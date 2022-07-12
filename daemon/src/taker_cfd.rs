use crate::bitcoin::util::psbt::PartiallySignedTransaction;
use crate::collab_settlement;
use crate::collab_settlement::taker::Settle;
use crate::connection;
use crate::oracle;
use crate::process_manager;
use crate::projection;
use crate::setup_taker;
use crate::wallet;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use maia_core::secp256k1_zkp::XOnlyPublicKey;
use maia_core::PartyParams;
use model::libp2p::PeerId;
use model::market_closing_price;
use model::olivia;
use model::Cfd;
use model::Identity;
use model::Leverage;
use model::MakerOffers;
use model::OrderId;
use model::Origin;
use model::Price;
use model::Role;
use model::Usd;
use sqlite_db;
use time::OffsetDateTime;
use xtra::Actor as _;
use xtra_productivity::xtra_productivity;
use xtras::AddressMap;

#[derive(Clone)]
pub struct CurrentMakerOffers(pub Option<MakerOffers>);

#[derive(Clone, Copy)]
pub struct TakeOffer {
    pub order_id: OrderId,
    pub quantity: Usd,
    pub leverage: Leverage,
}

#[derive(Clone)]
pub struct ProposeSettlement {
    pub order_id: OrderId,
    pub bid: Price,
    pub ask: Price,
    pub quote_timestamp: String,
}

pub struct Actor<O, W> {
    db: sqlite_db::Connection,
    wallet: xtra::Address<W>,
    oracle_pk: XOnlyPublicKey,
    projection_actor: xtra::Address<projection::Actor>,
    process_manager_actor: xtra::Address<process_manager::Actor>,
    conn_actor: xtra::Address<connection::Actor>,
    setup_actors: AddressMap<OrderId, setup_taker::Actor>,
    libp2p_collab_settlement_actor: xtra::Address<collab_settlement::taker::Actor>,
    oracle_actor: xtra::Address<O>,
    n_payouts: usize,
    current_maker_offers: Option<MakerOffers>,
    maker_identity: Identity,
    maker_peer_id: PeerId,
}

impl<O, W> Actor<O, W>
where
    W: xtra::Handler<wallet::Sign> + xtra::Handler<wallet::BuildPartyParams>,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: sqlite_db::Connection,
        wallet: xtra::Address<W>,
        oracle_pk: XOnlyPublicKey,
        projection_actor: xtra::Address<projection::Actor>,
        process_manager_actor: xtra::Address<process_manager::Actor>,
        conn_actor: xtra::Address<connection::Actor>,
        oracle_actor: xtra::Address<O>,
        libp2p_collab_settlement_actor: xtra::Address<collab_settlement::taker::Actor>,
        n_payouts: usize,
        maker_identity: Identity,
        maker_peer_id: PeerId,
    ) -> Self {
        Self {
            db,
            wallet,
            oracle_pk,
            projection_actor,
            process_manager_actor,
            conn_actor,
            oracle_actor,
            libp2p_collab_settlement_actor,
            n_payouts,
            setup_actors: AddressMap::default(),
            current_maker_offers: None,
            maker_identity,
            maker_peer_id,
        }
    }
}

#[xtra_productivity]
impl<O, W> Actor<O, W> {
    async fn handle_current_offers(&mut self, msg: xtra_libp2p_offer::taker::LatestMakerOffers) {
        let takers_perspective_of_maker_offers = msg.0.map(|mut maker_offers| {
            maker_offers.long = maker_offers.long.map(|mut long| {
                long.origin = Origin::Theirs;
                long
            });
            maker_offers.short = maker_offers.short.map(|mut short| {
                short.origin = Origin::Theirs;
                short
            });

            maker_offers
        });

        self.current_maker_offers = takers_perspective_of_maker_offers.clone();
        tracing::trace!("new maker offers {:?}", takers_perspective_of_maker_offers);

        if let Err(e) = self
            .projection_actor
            .send(projection::Update(
                takers_perspective_of_maker_offers.clone(),
            ))
            .await
        {
            tracing::warn!("Failed to send current offers to projection actor: {e:#}");
        };
    }
}

#[xtra_productivity]
impl<O, W> Actor<O, W> {
    async fn handle_propose_settlement(&mut self, msg: ProposeSettlement) -> Result<()> {
        let ProposeSettlement {
            order_id,
            bid,
            ask,
            quote_timestamp,
        } = msg;

        let cfd = self.db.load_open_cfd::<Cfd>(order_id, ()).await?;

        let proposal_closing_price = market_closing_price(bid, ask, Role::Taker, cfd.position());

        tracing::debug!(%order_id, %proposal_closing_price, %bid, %ask, %quote_timestamp, "Proposing settlement of contract");

        // Wait for the response to check for invariants (ie. whether it is possible to settle)
        self.libp2p_collab_settlement_actor
            .send(Settle {
                order_id,
                price: proposal_closing_price,
                maker_peer_id: cfd
                    .counterparty_peer_id()
                    .context("No counterparty peer id found")?,
            })
            .await??;

        Ok(())
    }
}

#[xtra_productivity]
impl<O, W> Actor<O, W>
where
    O: xtra::Handler<
            oracle::GetAnnouncements,
            Return = Result<Vec<olivia::Announcement>, oracle::NoAnnouncement>,
        > + xtra::Handler<oracle::MonitorAttestations>,
    W: xtra::Handler<wallet::BuildPartyParams, Return = Result<PartyParams>>
        + xtra::Handler<wallet::Sign, Return = Result<PartiallySignedTransaction>>,
{
    async fn handle_take_offer(
        &mut self,
        msg: TakeOffer,
        ctx: &mut xtra::Context<Self>,
    ) -> Result<()> {
        let TakeOffer {
            order_id,
            quantity,
            leverage,
        } = msg;

        let disconnected = self
            .setup_actors
            .get_disconnected(order_id)
            .with_context(|| {
                format!("Contract setup for order {order_id} is already in progress")
            })?;

        let (order_to_take, maker_offers) = self
            .current_maker_offers
            .clone()
            .context("No maker offers available to take")?
            .take_order(order_id);

        let order_to_take = order_to_take.context("Order to take could not be found in current maker offers, you might have an outdated offer")?;

        // The offer we are instructed to take is removed from the
        // set of available offers immediately so that we don't attempt
        // to take it more than once
        {
            self.current_maker_offers.replace(maker_offers);
            self.projection_actor
                .send(projection::Update(self.current_maker_offers.clone()))
                .await?;
        }

        if !order_to_take.is_safe_to_take(OffsetDateTime::now_utc()) {
            bail!("The maker's offer appears to be outdated, refusing to take offer",);
        }

        tracing::info!("Taking current order: {:?}", &order_to_take);

        // We create the cfd here without any events yet, only static data
        // Once the contract setup completes (rejected / accepted / failed) the first event will be
        // recorded
        let cfd = Cfd::from_order(
            &order_to_take,
            quantity,
            self.maker_identity,
            Some(self.maker_peer_id),
            Role::Taker,
            leverage,
        );

        self.db.insert_cfd(&cfd).await?;
        self.projection_actor
            .send(projection::CfdChanged(cfd.id()))
            .await?;

        let price_event_id = order_to_take.oracle_event_id;
        let announcement = self
            .oracle_actor
            .send(oracle::GetAnnouncements(vec![price_event_id]))
            .await?
            .with_context(|| format!("Announcement {price_event_id} not found"))?;

        let (addr, fut) = setup_taker::Actor::new(
            self.db.clone(),
            self.process_manager_actor.clone(),
            (
                cfd.id(),
                cfd.quantity(),
                cfd.taker_leverage(),
                self.n_payouts,
            ),
            (self.oracle_pk, announcement[0].clone()),
            self.wallet.clone().into(),
            self.wallet.clone().into(),
            self.conn_actor.clone(),
        )
        .create(None)
        .run();

        tokio_extras::spawn(&ctx.address().expect("self to be alive"), fut);
        tracing::info!("Spawned taker setup actor");
        disconnected.insert(addr);

        Ok(())
    }
}

#[async_trait]
impl<O: 'static, W: 'static> xtra::Actor for Actor<O, W> {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}
