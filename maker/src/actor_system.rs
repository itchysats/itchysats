use crate::cfd;
use crate::connection;
use crate::metrics::time_to_first_position;
use anyhow::Result;
use bdk::bitcoin;
use bdk::bitcoin::util::psbt::PartiallySignedTransaction;
use bdk::bitcoin::Amount;
use bdk::bitcoin::Txid;
use daemon::archive_closed_cfds;
use daemon::archive_failed_cfds;
use daemon::collab_settlement;
use daemon::command;
use daemon::monitor;
use daemon::oracle;
use daemon::oracle::NoAnnouncement;
use daemon::position_metrics;
use daemon::process_manager;
use daemon::projection;
use daemon::seed::Identities;
use daemon::wallet;
use libp2p_tcp::TokioTcpConfig;
use maia_core::secp256k1_zkp::XOnlyPublicKey;
use maia_core::PartyParams;
use model::olivia::Announcement;
use model::FundingRate;
use model::Leverage;
use model::OpeningFee;
use model::OrderId;
use model::Price;
use model::Role;
use model::TxFeeRate;
use model::Usd;
use std::net::SocketAddr;
use std::time::Duration;
use tokio_extras::Tasks;
use xtra::Actor;
use xtra::Address;
use xtra::Context;
use xtra::Handler;
use xtra_libp2p::endpoint;
use xtra_libp2p::libp2p::Multiaddr;
use xtra_libp2p::listener;
use xtra_libp2p::Endpoint;
use xtra_libp2p_ping::ping;
use xtra_libp2p_ping::pong;
use xtras::supervisor::always_restart_after;
use xtras::supervisor::Supervisor;
use xtras::HandlerTimeoutExt;

const ENDPOINT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(20);
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Duration between the restart attempts after a supervised actor has quit with
/// a failure.
pub const RESTART_INTERVAL: Duration = Duration::from_secs(5);

pub struct ActorSystem<O: 'static, W: 'static> {
    pub cfd_actor: Address<cfd::Actor<O, connection::Actor, W>>,
    wallet_actor: Address<W>,
    pub rollover_actor: Address<
        rollover::maker::Actor<command::Executor, oracle::AnnouncementsChannel, cfd::RatesChannel>,
    >,
    _archive_closed_cfds_actor: Address<archive_closed_cfds::Actor>,
    _archive_failed_cfds_actor: Address<archive_failed_cfds::Actor>,
    executor: command::Executor,
    _tasks: Tasks,
    _pong_actor: Address<pong::Actor>,
}

impl<O, W> ActorSystem<O, W>
where
    O: Handler<oracle::MonitorAttestation, Return = ()>
        + Handler<oracle::GetAnnouncements, Return = Result<Vec<Announcement>, NoAnnouncement>>
        + Actor<Stop = ()>,
    W: Handler<wallet::BuildPartyParams, Return = Result<PartyParams>>
        + Handler<wallet::Sign, Return = Result<PartiallySignedTransaction>>
        + Handler<wallet::Withdraw, Return = Result<Txid>>
        + Handler<wallet::Sync, Return = ()>
        + Actor<Stop = ()>,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new<M>(
        db: sqlite_db::Connection,
        wallet_addr: Address<W>,
        oracle_pk: XOnlyPublicKey,
        oracle_constructor: impl FnOnce(command::Executor) -> O,
        monitor_constructor: impl FnOnce(command::Executor) -> Result<M>,
        settlement_interval: time::Duration,
        n_payouts: usize,
        projection_actor: Address<projection::Actor>,
        identity: Identities,
        heartbeat_interval: Duration,
        p2p_socket: SocketAddr,
        listen_multiaddr: Multiaddr,
    ) -> Result<Self>
    where
        M: Handler<monitor::StartMonitoring, Return = ()>
            + Handler<monitor::Sync, Return = ()>
            + Handler<monitor::MonitorCollaborativeSettlement, Return = ()>
            + Handler<monitor::TryBroadcastTransaction, Return = Result<()>>
            + Handler<monitor::MonitorCetFinality, Return = Result<()>>
            + Actor<Stop = ()>,
    {
        let (monitor_addr, monitor_ctx) = Context::new(None);
        let (oracle_addr, oracle_ctx) = Context::new(None);
        let (inc_conn_addr, inc_conn_ctx) = Context::new(None);
        let (process_manager_addr, process_manager_ctx) = Context::new(None);
        let (time_to_first_position_addr, time_to_first_position_ctx) = Context::new(None);

        let executor = command::Executor::new(db.clone(), process_manager_addr.clone());

        let mut tasks = Tasks::default();

        let position_metrics_actor = position_metrics::Actor::new(db.clone())
            .create(None)
            .spawn(&mut tasks);

        tasks.add(process_manager_ctx.run(process_manager::Actor::new(
            db.clone(),
            Role::Maker,
            projection_actor.clone().into(),
            position_metrics_actor.into(),
            monitor_addr.clone().into(),
            monitor_addr.clone().into(),
            monitor_addr.clone().into(),
            monitor_addr.into(),
            oracle_addr.clone().into(),
        )));

        let (collab_settlement_supervisor, libp2p_collab_settlement_addr) = Supervisor::new({
            let executor = executor.clone();
            move || collab_settlement::maker::Actor::new(executor.clone(), n_payouts)
        });
        tasks.add(collab_settlement_supervisor.run_log_summary());

        let (endpoint_addr, endpoint_context) = Context::new(None);

        let (supervisor, maker_offer_address) = Supervisor::new({
            let endpoint_addr = endpoint_addr.clone();
            move || xtra_libp2p_offer::maker::Actor::new(endpoint_addr.clone())
        });
        tasks.add(supervisor.run_log_summary());

        let cfd_actor_addr = cfd::Actor::new(
            db.clone(),
            wallet_addr.clone(),
            settlement_interval,
            oracle_pk,
            projection_actor,
            process_manager_addr,
            inc_conn_addr,
            oracle_addr.clone(),
            time_to_first_position_addr,
            n_payouts,
            libp2p_collab_settlement_addr.clone(),
            maker_offer_address.clone(),
        )
        .create(None)
        .spawn(&mut tasks);

        let (rollover_supervisor, libp2p_rollover_addr) = Supervisor::new({
            let executor = executor.clone();
            let oracle_addr = oracle_addr;
            let cfd_actor_addr = cfd_actor_addr.clone();
            move || {
                rollover::maker::Actor::new(
                    executor.clone(),
                    oracle_pk,
                    oracle::AnnouncementsChannel::new(oracle_addr.clone().into()),
                    cfd::RatesChannel::new(cfd_actor_addr.clone().into()),
                    n_payouts,
                )
            }
        });
        tasks.add(rollover_supervisor.run_log_summary());

        let (ping_supervisor, ping_address) = Supervisor::new({
            let endpoint_addr = endpoint_addr.clone();
            move || ping::Actor::new(endpoint_addr.clone(), PING_INTERVAL)
        });

        let (listener_supervisor, listener_actor) = Supervisor::<_, listener::Error>::with_policy(
            move || listener::Actor::new(endpoint_addr.clone(), listen_multiaddr.clone()),
            always_restart_after(RESTART_INTERVAL),
        );

        let pong_address = pong::Actor.create(None).spawn(&mut tasks);

        let endpoint = Endpoint::new(
            Box::new(TokioTcpConfig::new),
            identity.libp2p,
            ENDPOINT_CONNECTION_TIMEOUT,
            [
                (rollover::PROTOCOL, libp2p_rollover_addr.clone().into()),
                (
                    collab_settlement::PROTOCOL,
                    libp2p_collab_settlement_addr.into(),
                ),
                (xtra_libp2p_ping::PROTOCOL_NAME, pong_address.clone().into()),
            ],
            endpoint::Subscribers::new(
                vec![
                    ping_address.clone().into(),
                    maker_offer_address.clone().into(),
                ],
                vec![ping_address.into(), maker_offer_address.into()],
                vec![],
                vec![listener_actor.into()],
            ),
        );

        tasks.add(endpoint_context.run(endpoint));

        tasks.add(listener_supervisor.run_log_summary());
        tasks.add(ping_supervisor.run_log_summary());

        tasks.add(
            inc_conn_ctx
                .with_handler_timeout(Duration::from_secs(120))
                .run(connection::Actor::new(
                    cfd_actor_addr.clone().into(),
                    cfd_actor_addr.clone().into(),
                    cfd_actor_addr.clone().into(),
                    identity.identity_sk,
                    heartbeat_interval,
                    p2p_socket,
                )),
        );

        tasks.add(monitor_ctx.run(monitor_constructor(executor.clone())?));

        tasks.add(oracle_ctx.run(oracle_constructor(executor.clone())));

        let archive_closed_cfds_actor = archive_closed_cfds::Actor::new(db.clone())
            .create(None)
            .spawn(&mut tasks);

        let archive_failed_cfds_actor = archive_failed_cfds::Actor::new(db.clone())
            .create(None)
            .spawn(&mut tasks);

        tasks.add(time_to_first_position_ctx.run(time_to_first_position::Actor::new(db)));

        tracing::debug!("Maker actor system ready");

        Ok(Self {
            cfd_actor: cfd_actor_addr,
            wallet_actor: wallet_addr,
            rollover_actor: libp2p_rollover_addr,
            _archive_closed_cfds_actor: archive_closed_cfds_actor,
            _archive_failed_cfds_actor: archive_failed_cfds_actor,
            executor,
            _tasks: tasks,
            _pong_actor: pong_address,
        })
    }

    /// Adjust the parameters which create offers for the connected takers.
    ///
    /// Once one offer is taken, another one with the same parameters is created.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_offer_params(
        &self,
        price_long: Option<Price>,
        price_short: Option<Price>,
        min_quantity: Usd,
        max_quantity: Usd,
        tx_fee_rate: TxFeeRate,
        funding_rate_long: FundingRate,
        funding_rate_short: FundingRate,
        opening_fee: OpeningFee,
        leverage_choices: Vec<Leverage>,
    ) -> Result<()> {
        self.cfd_actor
            .send(cfd::OfferParams {
                price_long,
                price_short,
                min_quantity,
                max_quantity,
                tx_fee_rate,
                funding_rate_long,
                funding_rate_short,
                opening_fee,
                leverage_choices,
            })
            .await??;

        Ok(())
    }

    pub async fn accept_order(&self, order_id: OrderId) -> Result<()> {
        self.cfd_actor.send(cfd::AcceptOrder { order_id }).await??;
        Ok(())
    }

    pub async fn reject_order(&self, order_id: OrderId) -> Result<()> {
        self.cfd_actor.send(cfd::RejectOrder { order_id }).await??;
        Ok(())
    }

    pub async fn accept_settlement(&self, order_id: OrderId) -> Result<()> {
        self.cfd_actor
            .send(cfd::AcceptSettlement { order_id })
            .await??;
        Ok(())
    }

    pub async fn reject_settlement(&self, order_id: OrderId) -> Result<()> {
        self.cfd_actor
            .send(cfd::RejectSettlement { order_id })
            .await??;
        Ok(())
    }

    pub async fn accept_rollover(&self, order_id: OrderId) -> Result<()> {
        self.cfd_actor
            .send(cfd::AcceptRollover { order_id })
            .await??;
        Ok(())
    }

    pub async fn reject_rollover(&self, order_id: OrderId) -> Result<()> {
        self.cfd_actor
            .send(cfd::RejectRollover { order_id })
            .await??;
        Ok(())
    }

    pub async fn commit(&self, order_id: OrderId) -> Result<()> {
        self.executor
            .execute(order_id, |cfd| cfd.manual_commit_to_blockchain())
            .await?;

        Ok(())
    }

    pub async fn withdraw(
        &self,
        amount: Option<Amount>,
        address: bitcoin::Address,
        fee: f32,
    ) -> Result<Txid> {
        self.wallet_actor
            .send(wallet::Withdraw {
                amount,
                address,
                fee: Some(bdk::FeeRate::from_sat_per_vb(fee)),
            })
            .await?
    }

    pub async fn sync_wallet(&self) -> Result<()> {
        self.wallet_actor.send(wallet::Sync).await?;
        Ok(())
    }

    pub async fn update_rollover_configuration(&self, is_accepting_rollovers: bool) -> Result<()> {
        self.rollover_actor
            .send(rollover::maker::UpdateConfiguration::new(
                is_accepting_rollovers,
            ))
            .await?;
        Ok(())
    }
}
