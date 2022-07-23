#![cfg_attr(not(test), warn(clippy::unwrap_used))]

use crate::bitcoin::util::psbt::PartiallySignedTransaction;
use crate::bitcoin::Txid;
use anyhow::Context as _;
use anyhow::Result;
use bdk::bitcoin;
use bdk::bitcoin::Amount;
use bdk::FeeRate;
use connection::ConnectionStatus;
use libp2p_core::Multiaddr;
use libp2p_tcp::TokioTcpConfig;
use maia_core::secp256k1_zkp::XOnlyPublicKey;
use model::libp2p::PeerId;
use model::olivia;
use model::Leverage;
use model::Order;
use model::OrderId;
use model::Price;
use model::Role;
use model::Usd;
use parse_display::Display;
use seed::Identities;
use std::time::Duration;
use time::ext::NumericalDuration;
use tokio::sync::watch;
use tokio_extras::Tasks;
use tracing::instrument;
use xtra::prelude::*;
use xtra_bitmex_price_feed::QUOTE_INTERVAL_MINUTES;
use xtra_libp2p::dialer;
use xtra_libp2p::endpoint;
use xtra_libp2p::multiaddress_ext::MultiaddrExt;
use xtra_libp2p::Endpoint;
use xtra_libp2p_ping::ping;
use xtra_libp2p_ping::pong;
use xtras::supervisor::always_restart;
use xtras::supervisor::always_restart_after;
use xtras::supervisor::Supervisor;
use xtras::HandlerTimeoutExt;

pub use bdk;
pub use maia;
pub use maia_core;

pub mod archive_closed_cfds;
pub mod archive_failed_cfds;
pub mod auto_rollover;
pub mod collab_settlement;
pub mod command;
pub mod connection;
pub mod libp2p_utils;
pub mod monitor;
pub mod noise;
mod online_status;
pub mod oracle;
pub mod order;
pub mod position_metrics;
pub mod process_manager;
pub mod projection;
pub mod rollover;
pub mod seed;
pub mod setup_contract;
// TODO: Remove setup_contract_deprecated module after phasing out legacy networking
pub mod setup_contract_deprecated;
pub mod setup_taker;
pub mod shared_protocol;
pub mod taker_cfd;
mod transaction_ext;
pub mod version;
pub mod wallet;
pub mod wire;

/// Duration between the heartbeats sent by the maker, used by the taker to
/// determine whether the maker is online.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Duration between the restart attempts after a supervised actor has quit with
/// a failure.
pub const RESTART_INTERVAL: Duration = Duration::from_secs(5);

pub const ENDPOINT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(20);
pub const PING_INTERVAL: Duration = Duration::from_secs(30);

pub const N_PAYOUTS: usize = 200;

pub struct TakerActorSystem<O, W, P> {
    pub cfd_actor: Address<taker_cfd::Actor>,
    pub connection_actor: Address<connection::Actor>,
    wallet_actor: Address<W>,
    _oracle_actor: Address<O>, // NOTE: Hard to supervise because of our tests
    pub auto_rollover_actor: Address<auto_rollover::Actor>,
    pub price_feed_actor: Address<P>,
    executor: command::Executor,
    _close_cfds_actor: Address<archive_closed_cfds::Actor>,
    _archive_failed_cfds_actor: Address<archive_failed_cfds::Actor>,
    _pong_actor: Address<pong::Actor>,
    _online_status_actor: Address<online_status::Actor>,

    pub maker_online_status_feed_receiver: watch::Receiver<ConnectionStatus>,

    _tasks: Tasks,
}

impl<O, W, P> TakerActorSystem<O, W, P>
where
    O: Handler<oracle::MonitorAttestation, Return = ()>
        + Handler<
            oracle::GetAnnouncement,
            Return = Result<olivia::Announcement, oracle::NoAnnouncement>,
        > + Actor<Stop = ()>,
    W: Handler<wallet::BuildPartyParams, Return = Result<maia_core::PartyParams>>
        + Handler<wallet::Sign, Return = Result<PartiallySignedTransaction>>
        + Handler<wallet::Withdraw, Return = Result<Txid>>
        + Handler<wallet::Sync, Return = ()>
        + Actor<Stop = ()>,
    P: Handler<xtra_bitmex_price_feed::LatestQuote, Return = Option<xtra_bitmex_price_feed::Quote>>
        + Actor<Stop = xtra_bitmex_price_feed::Error>,
{
    #[instrument(
        name = "Create TakerActorSystem",
        skip_all,
        fields(
            %n_payouts,
            connect_timeout_secs = %connect_timeout.as_secs(),
            %environment,
        )
        err,
    )]
    #[allow(clippy::too_many_arguments)]
    pub fn new<M>(
        db: sqlite_db::Connection,
        wallet_actor_addr: Address<W>,
        oracle_pk: XOnlyPublicKey,
        identity: Identities,
        oracle_constructor: impl FnOnce(command::Executor) -> O,
        monitor_constructor: impl FnOnce(command::Executor) -> Result<M>,
        price_feed_constructor: impl (Fn() -> P) + Send + 'static,
        n_payouts: usize,
        connect_timeout: Duration,
        projection_actor: Address<projection::Actor>,
        maker_multiaddr: Multiaddr,
        environment: Environment,
    ) -> Result<Self>
    where
        M: Handler<monitor::StartMonitoring, Return = ()>
            + Handler<monitor::Sync, Return = ()>
            + Handler<monitor::MonitorCollaborativeSettlement, Return = ()>
            + Handler<monitor::MonitorCetFinality, Return = Result<()>>
            + Handler<monitor::TryBroadcastTransaction, Return = Result<()>>
            + Actor<Stop = ()>,
    {
        let (maker_online_status_feed_sender, maker_online_status_feed_receiver) =
            watch::channel(ConnectionStatus::Offline { reason: None });

        let (monitor_addr, monitor_ctx) = Context::new(None);
        let (oracle_addr, oracle_ctx) = Context::new(None);
        let (process_manager_addr, process_manager_ctx) = Context::new(None);

        let executor = command::Executor::new(db.clone(), process_manager_addr.clone());

        let mut tasks = Tasks::default();

        let position_metrics_actor = position_metrics::Actor::new(db.clone())
            .create(None)
            .spawn(&mut tasks);

        tasks.add(process_manager_ctx.run(process_manager::Actor::new(
            db.clone(),
            Role::Taker,
            projection_actor.clone().into(),
            position_metrics_actor.into(),
            monitor_addr.clone().into(),
            monitor_addr.clone().into(),
            monitor_addr.clone().into(),
            monitor_addr.into(),
            oracle_addr.clone().into(),
        )));

        let (endpoint_addr, endpoint_context) = Context::new(None);

        let (order_supervisor, order) = Supervisor::new({
            let oracle = oracle_addr.clone();
            let db = db.clone();
            let process_manager = process_manager_addr;
            let wallet = wallet_actor_addr.clone();
            let projection = projection_actor.clone();
            let endpoint = endpoint_addr.clone();
            move || {
                order::taker::Actor::new(
                    n_payouts,
                    oracle_pk,
                    oracle.clone().into(),
                    (db.clone(), process_manager.clone()),
                    (wallet.clone().into(), wallet.clone().into()),
                    projection.clone(),
                    endpoint.clone(),
                )
            }
        });
        tasks.add(order_supervisor.run_log_summary());

        let (collab_settlement_supervisor, libp2p_collab_settlement_addr) = Supervisor::new({
            let endpoint_addr = endpoint_addr.clone();
            let executor = executor.clone();
            move || {
                collab_settlement::taker::Actor::new(
                    endpoint_addr.clone(),
                    executor.clone(),
                    n_payouts,
                )
            }
        });
        tasks.add(collab_settlement_supervisor.run_log_summary());

        let (connection_actor_addr, connection_actor_ctx) = Context::new(None);
        let cfd_actor_addr = taker_cfd::Actor::new(
            db.clone(),
            projection_actor,
            libp2p_collab_settlement_addr,
            order,
            PeerId::from(
                maker_multiaddr
                    .clone()
                    .extract_peer_id()
                    .context("Unable to extract peer id from maker address")?,
            ),
        )
        .create(None)
        .spawn(&mut tasks);

        let (rollover_supervisor, libp2p_rollover_addr) = Supervisor::new({
            let endpoint_addr = endpoint_addr.clone();
            let executor = executor.clone();
            let oracle_addr = oracle_addr.clone();
            move || {
                rollover::taker::Actor::new(
                    endpoint_addr.clone(),
                    executor.clone(),
                    oracle_pk,
                    oracle_addr.clone().into(),
                    n_payouts,
                )
            }
        });
        tasks.add(rollover_supervisor.run_log_summary());

        let auto_rollover_addr = auto_rollover::Actor::new(db.clone(), libp2p_rollover_addr)
            .create(None)
            .spawn(&mut tasks);

        let online_status_actor = online_status::Actor::new(
            endpoint_addr.clone(),
            maker_multiaddr
                .clone()
                .extract_peer_id()
                .expect("to be able to extract peer id"),
            maker_online_status_feed_sender,
        )
        .create(None)
        .spawn(&mut tasks);

        tasks.add(
            connection_actor_ctx
                .with_handler_timeout(Duration::from_secs(120))
                .run(connection::Actor::new(
                    identity.identity_sk.clone(),
                    identity.peer_id(),
                    connect_timeout,
                    environment,
                )),
        );

        tasks.add(monitor_ctx.run(monitor_constructor(executor.clone())?));
        tasks.add(oracle_ctx.run(oracle_constructor(executor.clone())));

        let dialer_constructor = {
            let endpoint_addr = endpoint_addr.clone();
            move || dialer::Actor::new(endpoint_addr.clone(), maker_multiaddr.clone())
        };
        let (dialer_supervisor, dialer_actor) = Supervisor::<_, dialer::Error>::with_policy(
            dialer_constructor,
            always_restart_after(RESTART_INTERVAL),
        );

        let (offers_supervisor, libp2p_offer_addr) = Supervisor::new({
            let cfd_actor_addr = cfd_actor_addr.clone();
            move || xtra_libp2p_offer::taker::Actor::new(cfd_actor_addr.clone().into())
        });

        let pong_address = pong::Actor.create(None).spawn(&mut tasks);

        let (supervisor, ping_actor) =
            Supervisor::new(move || ping::Actor::new(endpoint_addr.clone(), PING_INTERVAL));
        tasks.add(supervisor.run_log_summary());

        let endpoint = Endpoint::new(
            Box::new(TokioTcpConfig::new),
            identity.libp2p,
            ENDPOINT_CONNECTION_TIMEOUT,
            [
                (xtra_libp2p_ping::PROTOCOL_NAME, pong_address.clone().into()),
                (xtra_libp2p_offer::PROTOCOL_NAME, libp2p_offer_addr.into()),
            ],
            endpoint::Subscribers::new(
                vec![
                    online_status_actor.clone().into(),
                    ping_actor.clone().into(),
                ],
                vec![
                    dialer_actor.into(),
                    ping_actor.into(),
                    online_status_actor.clone().into(),
                ],
                vec![],
                vec![],
            ),
        );

        tasks.add(endpoint_context.run(endpoint));

        tasks.add(dialer_supervisor.run_log_summary());
        tasks.add(offers_supervisor.run_log_summary());

        let (supervisor, price_feed_actor) =
            Supervisor::<_, xtra_bitmex_price_feed::Error>::with_policy(
                price_feed_constructor,
                always_restart(),
            );

        tasks.add(supervisor.run_log_summary());

        let close_cfds_actor = archive_closed_cfds::Actor::new(db.clone())
            .create(None)
            .spawn(&mut tasks);
        let archive_failed_cfds_actor = archive_failed_cfds::Actor::new(db)
            .create(None)
            .spawn(&mut tasks);

        tracing::debug!("Taker actor system ready");

        Ok(Self {
            cfd_actor: cfd_actor_addr,
            connection_actor: connection_actor_addr,
            wallet_actor: wallet_actor_addr,
            _oracle_actor: oracle_addr,
            auto_rollover_actor: auto_rollover_addr,
            price_feed_actor,
            executor,
            _close_cfds_actor: close_cfds_actor,
            _archive_failed_cfds_actor: archive_failed_cfds_actor,
            _tasks: tasks,
            maker_online_status_feed_receiver,
            _online_status_actor: online_status_actor,
            _pong_actor: pong_address,
        })
    }

    #[instrument(skip(self), err)]
    pub async fn place_order(
        &self,
        offer_id: OrderId,
        quantity: Usd,
        leverage: Leverage,
    ) -> Result<OrderId> {
        let order_id = self
            .cfd_actor
            .send(taker_cfd::PlaceOrder {
                offer_id,
                quantity,
                leverage,
            })
            .await??;

        Ok(order_id)
    }

    #[instrument(skip(self), err)]
    pub async fn commit(&self, order_id: OrderId) -> Result<()> {
        self.executor
            .execute(order_id, |cfd| cfd.manual_commit_to_blockchain())
            .await?;

        Ok(())
    }

    #[instrument(skip(self), err)]
    pub async fn propose_settlement(&self, order_id: OrderId) -> Result<()> {
        let latest_quote = self
            .price_feed_actor
            .send(xtra_bitmex_price_feed::LatestQuote)
            .await
            .context("Price feed not available")?
            .context("No quote available")?;

        let quote_timestamp = latest_quote
            .timestamp
            .format(&time::format_description::well_known::Rfc3339)
            .context("Failed to format timestamp")?;

        let threshold = QUOTE_INTERVAL_MINUTES.minutes() * 2;

        if latest_quote.is_older_than(threshold) {
            anyhow::bail!(
                "Latest quote is older than {} minutes. Refusing to settle with old price.",
                threshold.whole_minutes()
            )
        }

        self.cfd_actor
            .send(taker_cfd::ProposeSettlement {
                order_id,
                bid: Price::new(latest_quote.bid())?,
                ask: Price::new(latest_quote.ask())?,
                quote_timestamp,
            })
            .await?
    }

    #[instrument(skip(self), err)]
    pub async fn withdraw(
        &self,
        amount: Option<Amount>,
        address: bitcoin::Address,
        fee_rate: FeeRate,
    ) -> Result<Txid> {
        self.wallet_actor
            .send(wallet::Withdraw {
                amount,
                address,
                fee: Some(fee_rate),
            })
            .await?
    }

    #[instrument(skip(self), err)]
    pub async fn sync_wallet(&self) -> Result<()> {
        self.wallet_actor.send(wallet::Sync).await?;
        Ok(())
    }
}

#[derive(Debug, Copy, Clone, Display)]
pub enum Environment {
    Umbrel,
    RaspiBlitz,
    Docker,
    Binary,
    Test,
    Legacy,
    Unknown,
}

impl Environment {
    pub fn from_str_or_unknown(s: &str) -> Environment {
        match s {
            "umbrel" => Environment::Umbrel,
            "raspiblitz" => Environment::RaspiBlitz,
            "docker" => Environment::Docker,
            _ => Environment::Unknown,
        }
    }
}
