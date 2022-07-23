use crate::collab_settlement;
use crate::connection;
use crate::connection::NoConnection;
use crate::contract_setup;
use crate::metrics::time_to_first_position;
use crate::rollover;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use bdk::bitcoin::util::psbt::PartiallySignedTransaction;
use daemon::command;
use daemon::libp2p_utils::can_use_libp2p;
use daemon::oracle;
use daemon::oracle::NoAnnouncement;
use daemon::order;
use daemon::process_manager;
use daemon::projection;
use daemon::wallet;
use daemon::wire;
use maia_core::secp256k1_zkp::XOnlyPublicKey;
use maia_core::PartyParams;
use model::libp2p::PeerId;
use model::olivia;
use model::olivia::Announcement;
use model::olivia::BitMexPriceEventId;
use model::Cfd;
use model::FundingRate;
use model::Identity;
use model::Leverage;
use model::MakerOffers;
use model::OpeningFee;
use model::Order;
use model::OrderId;
use model::Origin;
use model::Position;
use model::Price;
use model::Role;
use model::RolloverVersion;
use model::SettlementProposal;
use model::Timestamp;
use model::TxFeeRate;
use model::Usd;
use sqlite_db;
use std::collections::HashSet;
use time::Duration;
use tokio_extras::FutureExt;
use tracing::instrument;
use xtra::Actor as _;
use xtra_productivity::xtra_productivity;
use xtras::address_map::NotConnected;
use xtras::AddressMap;
use xtras::SendAsyncSafe;

const HANDLE_ACCEPT_CONTRACT_SETUP_MESSAGE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10);

#[derive(Clone)]
pub struct NewOffers {
    pub params: OfferParams,
}

#[derive(Clone, Copy)]
pub struct AcceptOrder {
    pub order_id: OrderId,
}

#[derive(Clone, Copy)]
pub struct RejectOrder {
    pub order_id: OrderId,
}

#[derive(Clone, Copy)]
pub struct AcceptSettlement {
    pub order_id: OrderId,
}

#[derive(Clone, Copy)]
pub struct RejectSettlement {
    pub order_id: OrderId,
}

#[derive(Clone, Copy)]
pub struct AcceptRollover {
    pub order_id: OrderId,
}

#[derive(Clone, Copy)]
pub struct RejectRollover {
    pub order_id: OrderId,
}

#[derive(Clone, Copy)]
pub struct TakerConnected {
    pub id: Identity,
}

#[derive(Clone, Copy)]
pub struct TakerDisconnected {
    pub id: Identity,
}

#[derive(Clone)]
pub struct OfferParams {
    pub price_long: Option<Price>,
    pub price_short: Option<Price>,
    pub min_quantity: Usd,
    pub max_quantity: Usd,
    pub tx_fee_rate: TxFeeRate,
    pub funding_rate_long: FundingRate,
    pub funding_rate_short: FundingRate,
    pub opening_fee: OpeningFee,
    pub leverage_choices: Vec<Leverage>,
}

impl OfferParams {
    fn pick_oracle_event_id(settlement_interval: Duration) -> BitMexPriceEventId {
        olivia::next_announcement_after(time::OffsetDateTime::now_utc() + settlement_interval)
    }

    pub fn create_long_order(&self, settlement_interval: Duration) -> Option<Order> {
        self.price_long.map(|price_long| {
            Order::new(
                Position::Long,
                price_long,
                self.min_quantity,
                self.max_quantity,
                Origin::Ours,
                Self::pick_oracle_event_id(settlement_interval),
                settlement_interval,
                self.tx_fee_rate,
                self.funding_rate_long,
                self.opening_fee,
                self.leverage_choices.clone(),
            )
        })
    }

    pub fn create_short_order(&self, settlement_interval: Duration) -> Option<Order> {
        self.price_short.map(|price_short| {
            Order::new(
                Position::Short,
                price_short,
                self.min_quantity,
                self.max_quantity,
                Origin::Ours,
                Self::pick_oracle_event_id(settlement_interval),
                settlement_interval,
                self.tx_fee_rate,
                self.funding_rate_short,
                self.opening_fee,
                self.leverage_choices.clone(),
            )
        })
    }
}

fn create_maker_offers(offer_params: OfferParams, settlement_interval: Duration) -> MakerOffers {
    MakerOffers {
        long: offer_params.create_long_order(settlement_interval),
        short: offer_params.create_short_order(settlement_interval),
        tx_fee_rate: offer_params.tx_fee_rate,
        funding_rate_long: offer_params.funding_rate_long,
        funding_rate_short: offer_params.funding_rate_short,
    }
}

pub struct FromTaker {
    pub taker_id: Identity,
    pub peer_id: Option<PeerId>,
    pub msg: wire::TakerToMaker,
}

/// Proposed rollover
#[derive(Debug, Clone, PartialEq)]
struct RolloverProposal {
    pub order_id: OrderId,
    pub timestamp: Timestamp,
}

pub struct Actor<O: 'static, T: 'static, W: 'static> {
    db: sqlite_db::Connection,
    wallet: xtra::Address<W>,
    settlement_interval: Duration,
    oracle_pk: XOnlyPublicKey,
    projection: xtra::Address<projection::Actor>,
    process_manager: xtra::Address<process_manager::Actor>,
    executor: command::Executor,
    rollover_actors: AddressMap<OrderId, rollover::Actor>,
    takers: xtra::Address<T>,
    current_offers: Option<MakerOffers>,
    setup_actors: AddressMap<OrderId, contract_setup::Actor>,
    settlement_actors: AddressMap<OrderId, collab_settlement::Actor>,
    oracle: xtra::Address<O>,
    time_to_first_position: xtra::Address<time_to_first_position::Actor>,
    connected_takers: HashSet<Identity>,
    n_payouts: usize,
    libp2p_rollover: xtra::Address<daemon::rollover::maker::Actor>,
    libp2p_collab_settlement: xtra::Address<daemon::collab_settlement::maker::Actor>,
    libp2p_offer: xtra::Address<xtra_libp2p_offer::maker::Actor>,
    order: xtra::Address<order::maker::Actor>,
}

impl<O, T, W> Actor<O, T, W> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: sqlite_db::Connection,
        wallet: xtra::Address<W>,
        settlement_interval: Duration,
        oracle_pk: XOnlyPublicKey,
        projection: xtra::Address<projection::Actor>,
        process_manager: xtra::Address<process_manager::Actor>,
        takers: xtra::Address<T>,
        oracle: xtra::Address<O>,
        time_to_first_position: xtra::Address<time_to_first_position::Actor>,
        n_payouts: usize,
        libp2p_rollover: xtra::Address<daemon::rollover::maker::Actor>,
        libp2p_collab_settlement: xtra::Address<daemon::collab_settlement::maker::Actor>,
        libp2p_offer: xtra::Address<xtra_libp2p_offer::maker::Actor>,
        order: xtra::Address<order::maker::Actor>,
    ) -> Self {
        Self {
            db: db.clone(),
            wallet,
            settlement_interval,
            oracle_pk,
            projection,
            process_manager: process_manager.clone(),
            executor: command::Executor::new(db, process_manager),
            rollover_actors: AddressMap::default(),
            takers,
            current_offers: None,
            setup_actors: AddressMap::default(),
            oracle,
            time_to_first_position,
            n_payouts,
            connected_takers: HashSet::new(),
            settlement_actors: AddressMap::default(),
            libp2p_rollover,
            libp2p_collab_settlement,
            libp2p_offer,
            order,
        }
    }

    #[tracing::instrument(skip(self))]
    async fn update_connected_takers(&mut self) -> Result<()> {
        self.projection
            .send_async_safe(projection::Update(
                self.connected_takers
                    .clone()
                    .into_iter()
                    .collect::<Vec<Identity>>(),
            ))
            .await?;
        Ok(())
    }
}

impl<O, T, W> Actor<O, T, W>
where
    T: xtra::Handler<connection::TakerMessage, Return = Result<(), NoConnection>>,
{
    async fn handle_taker_connected(&mut self, taker_id: Identity) -> Result<()> {
        self.takers
            .send_async_safe(connection::TakerMessage {
                taker_id,
                msg: wire::MakerToTaker::CurrentOffers(self.current_offers.clone()),
            })
            .await?;

        if !self.connected_takers.insert(taker_id) {
            tracing::warn!("Taker already connected: {:?}", &taker_id);
        }
        self.update_connected_takers().await?;
        self.time_to_first_position
            .send_async_safe(time_to_first_position::Connected::new(taker_id))
            .await?;
        Ok(())
    }

    async fn handle_taker_disconnected(&mut self, taker_id: Identity) -> Result<()> {
        if !self.connected_takers.remove(&taker_id) {
            tracing::warn!("Removed unknown taker: {:?}", &taker_id);
        }
        self.update_connected_takers().await?;
        Ok(())
    }
}

impl<O, T, W> Actor<O, T, W>
where
    O: xtra::Handler<oracle::GetAnnouncement, Return = Result<Announcement, NoAnnouncement>>
        + xtra::Handler<oracle::MonitorAttestation, Return = ()>,
    T: xtra::Handler<connection::TakerMessage, Return = Result<(), NoConnection>>
        + xtra::Handler<connection::RegisterRollover, Return = ()>,
    W: 'static,
{
    #[instrument(skip(self, taker_id, this), err)]
    async fn handle_propose_rollover(
        &mut self,
        RolloverProposal { order_id, .. }: RolloverProposal,
        taker_id: Identity,
        version: RolloverVersion,
        this: &xtra::Address<Self>,
    ) -> Result<()> {
        let (rollover_actor_addr, fut) = rollover::Actor::new(
            order_id,
            self.n_payouts,
            self.takers.clone().into(),
            taker_id,
            self.oracle_pk,
            self.oracle.clone().into(),
            self.process_manager.clone(),
            self.takers.clone().into(),
            self.db.clone(),
            version,
        )
        .create(None)
        .run();

        tokio_extras::spawn(this, fut);

        self.rollover_actors.insert(order_id, rollover_actor_addr);

        Ok(())
    }
}

impl<O, T, W> Actor<O, T, W>
where
    O: xtra::Handler<oracle::GetAnnouncement, Return = Result<Announcement, NoAnnouncement>>
        + xtra::Handler<oracle::MonitorAttestation>,
    T: xtra::Handler<connection::ConfirmOrder, Return = Result<()>>
        + xtra::Handler<connection::TakerMessage, Return = Result<(), NoConnection>>
        + xtra::Handler<connection::BroadcastOffers, Return = ()>,
    W: xtra::Handler<wallet::Sign, Return = Result<PartiallySignedTransaction>>
        + xtra::Handler<wallet::BuildPartyParams, Return = Result<PartyParams>>,
{
    #[instrument(skip(self, taker_id, this), err)]
    async fn handle_take_order(
        &mut self,
        taker_id: Identity,
        taker_peer_id: Option<PeerId>,
        order_id: OrderId,
        quantity: Usd,
        leverage: Leverage,
        this: &xtra::Address<Self>,
    ) -> Result<()> {
        tracing::debug!(%taker_id, %quantity, %order_id, "Taker wants to take an order");

        let disconnected = self
            .setup_actors
            .get_disconnected(order_id)
            .with_context(|| {
                format!("Contract setup for order {order_id} is already in progress")
            })?;

        // 1. Validate if order is still valid
        let order_to_take = self
            .current_offers
            .as_ref()
            .and_then(|offers| offers.pick_order_to_take(order_id));

        let order_to_take = if let Some(order_to_take) = order_to_take {
            order_to_take
        } else {
            // An outdated order on the taker side does not require any state change on the
            // maker. notifying the taker with a specific message should be sufficient.
            // Since this is a scenario that we should rarely see we log
            // a warning to be sure we don't trigger this code path frequently.
            tracing::warn!("Taker tried to take order with outdated id {order_id}");

            self.takers
                .send(connection::TakerMessage {
                    taker_id,
                    msg: wire::MakerToTaker::InvalidOrderId(order_id),
                })
                .await??;

            return Ok(());
        };

        let cfd = Cfd::from_order(
            &order_to_take,
            quantity,
            taker_id,
            taker_peer_id.map(PeerId::from),
            Role::Maker,
            leverage,
        );

        // 2. Replicate the orders in the offers with new ones to allow other takers to use
        // the same offer
        if let Some(offers) = &self.current_offers {
            self.current_offers = Some(offers.replicate());
        }

        if let Err(e) = self
            .takers
            .send_async_safe(connection::BroadcastOffers(self.current_offers.clone()))
            .await
        {
            tracing::warn!("{e:#}");
        }

        if let Err(e) = self
            .libp2p_offer
            .send_async_safe(xtra_libp2p_offer::maker::NewOffers::new(
                self.current_offers.clone(),
            ))
            .await
        {
            tracing::warn!("{e:#}");
        }

        self.projection
            .send(projection::Update(self.current_offers.clone()))
            .await?;

        self.db.insert_cfd(&cfd).await?;
        self.projection
            .send(projection::CfdChanged(cfd.id()))
            .await?;

        // 4. Try to get the oracle announcement, if that fails we should exit prior to changing any
        // state
        let announcement = self
            .oracle
            .send(oracle::GetAnnouncement(order_to_take.oracle_event_id))
            .await??;

        // 5. Start up contract setup actor
        let (addr, fut) = contract_setup::Actor::new(
            self.db.clone(),
            self.process_manager.clone(),
            (order_to_take.clone(), cfd.quantity(), self.n_payouts),
            (self.oracle_pk, announcement),
            self.wallet.clone().into(),
            self.wallet.clone().into(),
            (
                self.takers.clone().into(),
                self.takers.clone().into(),
                taker_id,
            ),
            self.time_to_first_position.clone(),
        )
        .create(None)
        .run();

        tokio_extras::spawn(this, fut);

        disconnected.insert(addr);

        Ok(())
    }
}

#[xtra_productivity]
impl<O, T, W> Actor<O, T, W> {
    async fn handle_accept_order(&mut self, msg: AcceptOrder) -> Result<()> {
        let AcceptOrder { order_id } = msg;

        match self
            .order
            .send(order::maker::Decision::Accept(order_id))
            .await
        {
            Ok(Ok(_)) => return Ok(()),
            Err(e) => {
                tracing::warn!("Libp2p order actor is down, trying legacy mechanism: {e:#}");
            }
            Ok(Err(e)) => {
                tracing::debug!(
                    "Could not accept order via libp2p, trying legacy mechanism: {e:#}"
                );
            }
        };

        match self
            .setup_actors
            .send(&order_id, contract_setup::Accepted)
            .timeout(HANDLE_ACCEPT_CONTRACT_SETUP_MESSAGE_TIMEOUT, || {
                tracing::debug_span!("send contract_setup::Accepted")
            })
            .await
        {
            Ok(Ok(())) => {
                tracing::debug!(%order_id, "Maker accepts order");
                Ok(())
            }
            Ok(Err(NotConnected(e))) => {
                self.executor
                    .execute(order_id, |cfd| Ok(cfd.fail_contract_setup(anyhow!(e))))
                    .await?;

                bail!("Accept failed: No active contract setup for order {order_id}")
            }
            Err(elapsed) => {
                self.executor
                    .execute(
                        order_id,
                        |cfd| Ok(cfd.fail_contract_setup(anyhow!(elapsed))),
                    )
                    .await?;

                bail!("Accept failed: Contract setup stale for order {order_id}")
            }
        }
    }

    async fn handle_reject_order(&mut self, msg: RejectOrder) -> Result<()> {
        let RejectOrder { order_id } = msg;

        tracing::debug!(%order_id, "Maker rejects order");

        match self
            .order
            .send(order::maker::Decision::Reject(order_id))
            .await
        {
            Ok(Ok(_)) => return Ok(()),
            Err(e) => {
                tracing::warn!("Libp2p order actor is down, trying legacy mechanism: {e:#}");
            }
            Ok(Err(e)) => {
                tracing::debug!(
                    "Could not reject order via libp2p, trying legacy mechanism: {e:#}"
                );
            }
        };

        match self
            .setup_actors
            .send(&order_id, contract_setup::Rejected)
            .timeout(HANDLE_ACCEPT_CONTRACT_SETUP_MESSAGE_TIMEOUT, || {
                tracing::debug_span!("send contract_setup::Rejected")
            })
            .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(NotConnected(e))) => {
                self.executor
                    .execute(order_id, |cfd| Ok(cfd.fail_contract_setup(anyhow!(e))))
                    .await?;

                bail!("Reject failed: No active contract setup for order {order_id}")
            }
            Err(elapsed) => {
                self.executor
                    .execute(
                        order_id,
                        |cfd| Ok(cfd.fail_contract_setup(anyhow!(elapsed))),
                    )
                    .await?;

                bail!("Reject failed: Contract setup stale for order {order_id}")
            }
        }
    }

    async fn handle_accept_settlement(&mut self, msg: AcceptSettlement) -> Result<()> {
        let AcceptSettlement { order_id } = msg;

        match self
            .libp2p_collab_settlement
            .send(daemon::collab_settlement::maker::Accept { order_id })
            .await
        {
            // Return early if dispatch to libp2p settlement worked
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => {
                tracing::debug!("Try fallback to legacy collab settlement because unable to handle accept via libp2p: {error:#}");
            }
            Err(error) => {
                // we should never see this given that the libp2p actor is always running
                tracing::error!("Try fallback to legacy collab settlement because unable to dispatch accept to libp2p actor: {error:#}");
            }
        }

        // We fallback to dispatch to legacy collab settlement in case libp2p settlement failed
        match self
            .settlement_actors
            .send_async(&order_id, collab_settlement::Accepted)
            .await
        {
            Ok(_) => Ok(()),
            Err(NotConnected(e)) => {
                self.executor
                    .execute(order_id, |cfd| {
                        Ok(cfd.fail_collaborative_settlement(anyhow!(e)))
                    })
                    .await?;

                bail!("Accept failed: No settlement in progress for order {order_id}")
            }
        }
    }

    async fn handle_reject_settlement(&mut self, msg: RejectSettlement) -> Result<()> {
        let RejectSettlement { order_id } = msg;

        match self
            .libp2p_collab_settlement
            .send(daemon::collab_settlement::maker::Reject { order_id })
            .await
        {
            // Return early if dispatch to libp2p settlement worked
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => {
                tracing::debug!("Try fallback to legacy collab settlement because unable to handle reject via libp2p: {error:#}");
            }
            Err(error) => {
                // we should never see this given that the libp2p actor is always running
                tracing::error!("Try fallback to legacy collab settlement because unable to dispatch reject to libp2p actor: {error:#}");
            }
        }

        // We fallback to dispatch to legacy collab settlement in case libp2p settlement failed
        match self
            .settlement_actors
            .send_async(&order_id, collab_settlement::Rejected)
            .await
        {
            Ok(_) => Ok(()),
            Err(NotConnected(e)) => {
                self.executor
                    .execute(order_id, |cfd| {
                        Ok(cfd.fail_collaborative_settlement(anyhow!(e)))
                    })
                    .await?;

                bail!("Reject failed: No settlement in progress for order {order_id}")
            }
        }
    }

    async fn handle_accept_rollover(&mut self, msg: AcceptRollover) -> Result<()> {
        let current_offers = self
            .current_offers
            .as_ref()
            .context("Cannot accept rollover without current offer, as we need up-to-date fees")?;

        let order_id = msg.order_id;

        // We try to dispatch to libp2p rollover first
        // Using send here is fine because we dispatch to a task internally
        match self
            .libp2p_rollover
            .send(daemon::rollover::maker::Accept {
                order_id,
                tx_fee_rate: current_offers.tx_fee_rate,
                long_funding_rate: current_offers.funding_rate_long,
                short_funding_rate: current_offers.funding_rate_short,
            })
            .await
        {
            // Return early if dispatch to libp2p rollover worked
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => {
                let cfd = self.db.load_open_cfd::<Cfd>(order_id, ()).await?;
                if can_use_libp2p(&cfd) {
                    tracing::error!("Failed to accept rollover for libp2p: {error:#}");
                    return Err(error);
                }

                tracing::debug!("Try fallback to legacy rollover because unable to handle accept via libp2p: {error:#}");
            }
            Err(error) => {
                let cfd = self.db.load_open_cfd::<Cfd>(order_id, ()).await?;
                if can_use_libp2p(&cfd) {
                    tracing::error!(
                        "Failed to dispatch accept to libp2p rollover actor: {error:#}"
                    );
                    return Err(anyhow!(error));
                }

                // we should never see this given that the libp2p rollover actor is always running
                tracing::error!("Try fallback to legacy rollover because unable to dispatch accept to libp2p rollover actor: {error:#}");
            }
        }

        // We fallback to dispatch to legacy rollover in case libp2p rollover failed and we don't
        // know the peer-id
        match self
            .rollover_actors
            .send_async(
                &order_id,
                rollover::AcceptRollover {
                    tx_fee_rate: current_offers.tx_fee_rate,
                    long_funding_rate: current_offers.funding_rate_long,
                    short_funding_rate: current_offers.funding_rate_short,
                },
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(NotConnected(e)) => {
                self.executor
                    .execute(order_id, |cfd| Ok(cfd.fail_rollover(anyhow!(e))))
                    .await?;

                bail!("Accept failed: No active rollover for order {order_id}")
            }
        }
    }

    async fn handle_reject_rollover(&mut self, msg: RejectRollover) -> Result<()> {
        let order_id = msg.order_id;

        // We try to dispatch to libp2p rollover first
        // Using send here is fine because we dispatch to a task internally
        match self
            .libp2p_rollover
            .send(daemon::rollover::maker::Reject { order_id })
            .await
        {
            // Return early if dispatch to libp2p rollover worked
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => {
                let cfd = self.db.load_open_cfd::<Cfd>(order_id, ()).await?;

                if can_use_libp2p(&cfd) {
                    tracing::error!("Failed to reject rollover for libp2p: {error:#}");
                    return Err(error);
                }

                tracing::debug!("Try fallback to legacy rollover because unable to handle reject via libp2p: {error:#}");
            }
            Err(error) => {
                let cfd = self.db.load_open_cfd::<Cfd>(order_id, ()).await?;
                if can_use_libp2p(&cfd) {
                    tracing::error!(
                        "Failed to dispatch reject to libp2p rollover actor: {error:#}"
                    );
                    return Err(anyhow!(error));
                }

                // we should never see this given that the libp2p rollover actor is always running
                tracing::error!("Try fallback to legacy rollover because unable to dispatch reject to libp2p rollover actor: {error:#}");
            }
        }

        // We fallback to dispatch to legacy rollover in case libp2p rollover failed and we don't
        // know the peer-id
        match self
            .rollover_actors
            .send_async(&order_id, rollover::RejectRollover)
            .await
        {
            Ok(_) => Ok(()),
            Err(NotConnected(e)) => {
                self.executor
                    .execute(order_id, |cfd| Ok(cfd.fail_rollover(anyhow!(e))))
                    .await?;

                bail!("Reject failed: No active rollover for order {order_id}")
            }
        }
    }
}

impl<O, T, W> Actor<O, T, W>
where
    O: xtra::Handler<oracle::MonitorAttestation, Return = ()>,
    T: xtra::Handler<connection::settlement::Response, Return = Result<()>>,
    W: 'static + Send,
{
    #[instrument(skip(self, taker_id, this), err)]
    async fn handle_propose_settlement(
        &mut self,
        taker_id: Identity,
        proposal: SettlementProposal,
        this: &xtra::Address<Self>,
    ) -> Result<()> {
        let order_id = proposal.order_id;

        let disconnected = self
            .settlement_actors
            .get_disconnected(order_id)
            .with_context(|| format!("Settlement for order {order_id} is already in progress",))?;

        let (addr, fut) = collab_settlement::Actor::new(
            proposal,
            taker_id,
            self.takers.clone().into(),
            self.process_manager.clone(),
            self.db.clone(),
            self.n_payouts,
        )
        .create(None)
        .run();

        tokio_extras::spawn(this, fut);
        disconnected.insert(addr);

        Ok(())
    }
}

#[xtra_productivity]
impl<O, T, W> Actor<O, T, W>
where
    O: xtra::Handler<oracle::GetAnnouncement, Return = Result<Announcement, NoAnnouncement>>
        + xtra::Handler<oracle::MonitorAttestation, Return = ()>,
    T: xtra::Handler<connection::ConfirmOrder, Return = Result<()>>
        + xtra::Handler<connection::TakerMessage, Return = Result<(), NoConnection>>
        + xtra::Handler<connection::BroadcastOffers, Return = ()>
        + xtra::Handler<connection::settlement::Response, Return = Result<()>>
        + xtra::Handler<connection::RegisterRollover, Return = ()>,
    W: xtra::Handler<wallet::Sign, Return = Result<PartiallySignedTransaction>>
        + xtra::Handler<wallet::BuildPartyParams, Return = Result<PartyParams>>,
{
    async fn handle_offer_params(&mut self, msg: OfferParams) -> Result<()> {
        // 1. Update actor state to current order
        self.current_offers
            .replace(create_maker_offers(msg, self.settlement_interval));

        // 2. Notify UI via feed
        self.projection
            .send(projection::Update(self.current_offers.clone()))
            .await?;

        // 3. Inform connected takers
        if let Err(e) = self
            .takers
            .send_async_safe(connection::BroadcastOffers(self.current_offers.clone()))
            .await
        {
            tracing::warn!("{e:#}");
        }

        if let Err(e) = self
            .libp2p_offer
            .send_async_safe(xtra_libp2p_offer::maker::NewOffers::new(
                self.current_offers.clone(),
            ))
            .await
        {
            tracing::warn!("{e:#}");
        }

        Ok(())
    }

    async fn handle(&mut self, msg: TakerConnected) -> Result<()> {
        self.handle_taker_connected(msg.id).await
    }

    async fn handle(&mut self, msg: TakerDisconnected) -> Result<()> {
        self.handle_taker_disconnected(msg.id).await
    }

    async fn handle(
        &mut self,
        FromTaker {
            taker_id,
            peer_id,
            msg,
        }: FromTaker,
        ctx: &mut xtra::Context<Self>,
    ) {
        let this = ctx.address().expect("self to be alive");

        match msg {
            wire::TakerToMaker::DeprecatedTakeOrder { order_id, quantity } => {
                // Old clients do not send over leverage. Hence we default to Leverage::TWO which
                // was the default before.
                let leverage = Leverage::TWO;

                if let Err(e) = self
                    .handle_take_order(taker_id, peer_id, order_id, quantity, leverage, &this)
                    .await
                {
                    tracing::error!("Error when handling order take request: {:#}", e)
                }
            }
            wire::TakerToMaker::TakeOrder {
                order_id,
                quantity,
                leverage,
            } => {
                if let Err(e) = self
                    .handle_take_order(taker_id, peer_id, order_id, quantity, leverage, &this)
                    .await
                {
                    tracing::error!("Error when handling order take request: {:#}", e)
                }
            }
            wire::TakerToMaker::Settlement {
                order_id,
                msg:
                    wire::taker_to_maker::Settlement::Propose {
                        timestamp: _,
                        taker,
                        maker,
                        price,
                    },
            } => {
                if let Err(e) = self
                    .handle_propose_settlement(
                        taker_id,
                        SettlementProposal {
                            order_id,
                            taker,
                            maker,
                            price,
                        },
                        &this,
                    )
                    .await
                {
                    tracing::warn!(%order_id, "Failed to handle settlement proposal: {:#}", e);
                }
            }
            wire::TakerToMaker::Settlement {
                msg: wire::taker_to_maker::Settlement::Initiate { .. },
                ..
            } => {
                unreachable!("Handled within `collab_settlement::Actor");
            }
            wire::TakerToMaker::ProposeRollover {
                order_id,
                timestamp,
            } => {
                if let Err(e) = self
                    .handle_propose_rollover(
                        RolloverProposal {
                            order_id,
                            timestamp,
                        },
                        taker_id,
                        RolloverVersion::V1,
                        &this,
                    )
                    .await
                {
                    tracing::warn!("Failed to handle rollover proposal: {:#}", e);
                }
            }
            wire::TakerToMaker::ProposeRolloverV2 {
                order_id,
                timestamp,
            } => {
                if let Err(e) = self
                    .handle_propose_rollover(
                        RolloverProposal {
                            order_id,
                            timestamp,
                        },
                        taker_id,
                        RolloverVersion::V2,
                        &this,
                    )
                    .await
                {
                    tracing::warn!("Failed to handle rollover proposal V2: {:#}", e);
                }
            }
            wire::TakerToMaker::ProposeRolloverV3 {
                order_id,
                timestamp,
            } => {
                if let Err(e) = self
                    .handle_propose_rollover(
                        RolloverProposal {
                            order_id,
                            timestamp,
                        },
                        taker_id,
                        RolloverVersion::V3,
                        &this,
                    )
                    .await
                {
                    tracing::warn!("Failed to handle rollover proposal V3: {:#}", e);
                }
            }
            wire::TakerToMaker::RolloverProtocol { .. }
            | wire::TakerToMaker::Protocol { .. }
            | wire::TakerToMaker::Hello(_)
            | wire::TakerToMaker::HelloV2 { .. }
            | wire::TakerToMaker::HelloV3 { .. }
            | wire::TakerToMaker::HelloV4 { .. }
            | wire::TakerToMaker::Unknown => {
                if cfg!(debug_assertions) {
                    unreachable!("Message {} is not dispatched to this actor", msg.name())
                }
            }
        }
    }
}

#[async_trait]
impl<O: Send + 'static, T: Send + 'static, W: Send + 'static> xtra::Actor for Actor<O, T, W> {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}
