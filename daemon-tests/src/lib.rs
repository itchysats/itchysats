use crate::flow::is_next_offers_none;
use crate::flow::next_maker_offers;
use crate::flow::next_with;
use crate::flow::one_cfd_with_state;
use crate::mocks::monitor::MonitorActor;
use crate::mocks::oracle::OracleActor;
use crate::mocks::price_feed::PriceFeedActor;
use crate::mocks::wallet::WalletActor;
use anyhow::Context;
use daemon::auto_rollover;
use daemon::bdk::bitcoin::Amount;
use daemon::bdk::bitcoin::Network;
use daemon::bdk::bitcoin::SignedAmount;
use daemon::bdk::bitcoin::Txid;
use daemon::connection::connect;
use daemon::connection::ConnectionStatus;
use daemon::libp2p_utils::create_connect_multiaddr;
use daemon::maia_core::secp256k1_zkp::XOnlyPublicKey;
use daemon::projection;
use daemon::projection::Cfd;
use daemon::projection::CfdState;
use daemon::projection::Feeds;
use daemon::projection::MakerOffers;
use daemon::seed::RandomSeed;
use daemon::seed::Seed;
use daemon::Environment;
use daemon::HEARTBEAT_INTERVAL;
use daemon::N_PAYOUTS;
use maker::cfd::OfferParams;
use model::libp2p::PeerId;
use model::olivia::Announcement;
use model::olivia::BitMexPriceEventId;
use model::CfdEvent;
use model::CompleteFee;
use model::Dlc;
use model::EventKind;
use model::FeeAccount;
use model::FundingFee;
use model::FundingRate;
use model::Identity;
use model::Leverage;
use model::OpeningFee;
use model::OrderId;
use model::Position;
use model::Price;
use model::Role;
use model::TxFeeRate;
use model::Usd;
use model::SETTLEMENT_INTERVAL;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_extras::time::sleep;
use tokio_extras::Tasks;
use tracing::instrument;
use xtra::Actor;
use xtra_bitmex_price_feed::Quote;
use xtra_libp2p::libp2p::Multiaddr;
use xtra_libp2p::multiaddress_ext::MultiaddrExt;

pub mod flow;
pub mod maia;
pub mod mocks;

#[macro_export]
macro_rules! confirm {
    (lock transaction, $id:expr, $maker:expr, $taker:expr) => {
        $maker
            .mocks
            .monitor()
            .await
            .confirm_lock_transaction($id)
            .await;
        $taker
            .mocks
            .monitor()
            .await
            .confirm_lock_transaction($id)
            .await;
    };
    (commit transaction, $id:expr, $maker:expr, $taker:expr) => {
        $maker
            .mocks
            .monitor()
            .await
            .confirm_commit_transaction($id)
            .await;
        $taker
            .mocks
            .monitor()
            .await
            .confirm_commit_transaction($id)
            .await;
    };
    (refund transaction, $id:expr, $maker:expr, $taker:expr) => {
        $maker
            .mocks
            .monitor()
            .await
            .confirm_refund_transaction($id)
            .await;
        $taker
            .mocks
            .monitor()
            .await
            .confirm_refund_transaction($id)
            .await;
    };
    (close transaction, $id:expr, $maker:expr, $taker:expr) => {
        $maker
            .mocks
            .monitor()
            .await
            .confirm_close_transaction($id)
            .await;
        $taker
            .mocks
            .monitor()
            .await
            .confirm_close_transaction($id)
            .await;
    };
    (cet, $id:expr, $maker:expr, $taker:expr) => {
        $maker.mocks.monitor().await.confirm_cet($id).await;
        $taker.mocks.monitor().await.confirm_cet($id).await;
    };
}

#[macro_export]
macro_rules! expire {
    (cet timelock, $id:expr, $maker:expr, $taker:expr) => {
        $maker.mocks.monitor().await.expire_cet_timelock($id).await;
        $taker.mocks.monitor().await.expire_cet_timelock($id).await;
    };
    (refund timelock, $id:expr, $maker:expr, $taker:expr) => {
        $maker
            .mocks
            .monitor()
            .await
            .expire_refund_timelock($id)
            .await;
        $taker
            .mocks
            .monitor()
            .await
            .expire_refund_timelock($id)
            .await;
    };
}

/// Simulate oracle attestation for both actor systems
#[macro_export]
macro_rules! simulate_attestation {
    ($maker:expr, $taker:expr, $order_id:expr, $attestation:expr) => {{
        tracing::debug!("Simulating attestation: {:?}", $attestation);

        $maker
            .mocks
            .oracle()
            .await
            .simulate_attestation($order_id, $attestation)
            .await;

        $taker
            .mocks
            .oracle()
            .await
            .simulate_attestation($order_id, $attestation)
            .await;
    }};
}

/// Waits until the CFDs for both maker and taker are in the given state.
#[macro_export]
macro_rules! wait_next_state {
    ($id:expr, $maker:expr, $taker:expr, $maker_state:expr, $taker_state:expr) => {
        let wait_until_taker = next_with($taker.cfd_feed(), |maybe_cfds| {
            maybe_cfds.and_then(one_cfd_with_state($taker_state))
        });
        let wait_until_maker = next_with($maker.cfd_feed(), |maybe_cfds| {
            maybe_cfds.and_then(one_cfd_with_state($maker_state))
        });

        let (taker_cfd, maker_cfd) = tokio::join!(wait_until_taker, wait_until_maker);
        let taker_cfd = taker_cfd.unwrap();
        let maker_cfd = maker_cfd.unwrap();

        assert_eq!(
            taker_cfd.order_id, maker_cfd.order_id,
            "order id mismatch between maker and taker"
        );
        assert_eq!(taker_cfd.order_id, $id, "unexpected order id in the taker");
        assert_eq!(maker_cfd.order_id, $id, "unexpected order id in the maker");
    };
    ($id:expr, $maker:expr, $taker:expr, $state:expr) => {
        wait_next_state!($id, $maker, $taker, $state, $state)
    };
}

/// Hide the implementation detail of arriving at the Cfd open state.
/// Useful when reading tests that should start at this point.
/// For convenience, returns also OrderId of the opened Cfd.
/// `announcement` is used during Cfd's creation.
pub async fn start_from_open_cfd_state(
    announcement: Announcement,
    position_maker: Position,
) -> (Maker, Taker, OrderId, FeeStructure) {
    let mut maker = Maker::start(&MakerConfig::default()).await;
    let mut taker = Taker::start(
        &TakerConfig::default(),
        maker.listen_addr,
        maker.identity,
        maker.connect_addr.clone(),
    )
    .await;

    is_next_offers_none(taker.offers_feed()).await.unwrap();

    let offer_params = dummy_offer_params(position_maker);

    let quantity = Usd::new(dec!(100));
    let taker_leverage = Leverage::TWO;

    let fee_structure = FeeStructure::new(
        offer_params.clone(),
        quantity,
        taker_leverage,
        position_maker,
    );

    maker.set_offer_params(offer_params).await;

    let (_, received) = next_maker_offers(maker.offers_feed(), taker.offers_feed())
        .await
        .unwrap();

    taker
        .mocks
        .mock_oracle_announcement_with(announcement.clone())
        .await;
    maker
        .mocks
        .mock_oracle_announcement_with(announcement)
        .await;

    let offer_to_take = match position_maker {
        Position::Short => received.short,
        Position::Long => received.long,
    }
    .context("Order for expected position not set")
    .unwrap();

    let offer_id = offer_to_take.id;

    let order_id = taker
        .system
        .place_order(offer_id, quantity, Leverage::TWO)
        .await
        .unwrap();
    wait_next_state!(order_id, maker, taker, CfdState::PendingSetup);

    maker.mocks.mock_party_params().await;
    taker.mocks.mock_party_params().await;

    maker.mocks.mock_wallet_sign_and_broadcast().await;
    taker.mocks.mock_wallet_sign_and_broadcast().await;

    maker.system.accept_order(order_id).await.unwrap();
    wait_next_state!(order_id, maker, taker, CfdState::ContractSetup);

    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition
    wait_next_state!(order_id, maker, taker, CfdState::PendingOpen);

    confirm!(lock transaction, order_id, maker, taker);
    wait_next_state!(order_id, maker, taker, CfdState::Open);

    (maker, taker, order_id, fee_structure)
}
pub struct FeeStructure {
    /// Opening fee charged by the maker
    opening_fee: OpeningFee,

    /// Funding fee for the first 24h calculated when opening a Cfd
    initial_funding_fee: FundingFee,

    /// The maker's position for the Cfd
    maker_position: Position,

    offer_params: OfferParams,
    quantity: Usd,
    taker_leverage: Leverage,
}

impl FeeStructure {
    pub fn new(
        offer_params: OfferParams,
        quantity: Usd,
        taker_leverage: Leverage,
        maker_position: Position,
    ) -> Self {
        let initial_funding_fee = match maker_position {
            Position::Long => FundingFee::calculate(
                offer_params.price_long.unwrap(),
                quantity,
                Leverage::ONE,
                taker_leverage,
                offer_params.funding_rate_long,
                SETTLEMENT_INTERVAL.whole_hours(),
            )
            .unwrap(),
            Position::Short => FundingFee::calculate(
                offer_params.price_short.unwrap(),
                quantity,
                taker_leverage,
                Leverage::ONE,
                offer_params.funding_rate_short,
                SETTLEMENT_INTERVAL.whole_hours(),
            )
            .unwrap(),
        };

        Self {
            opening_fee: offer_params.opening_fee,
            initial_funding_fee,
            maker_position,
            offer_params,
            quantity,
            taker_leverage,
        }
    }

    pub fn predict_fees(
        &self,
        accumulated_rollover_hours_to_charge: i64,
    ) -> (SignedAmount, SignedAmount) {
        if accumulated_rollover_hours_to_charge == 0 {
            tracing::info!("Predicting fees before first rollover")
        } else {
            tracing::info!(
                "Predicting fee for {} hours",
                accumulated_rollover_hours_to_charge
            );
        }

        tracing::debug!("Opening fee: {}", self.opening_fee);

        let mut maker_fee_account = FeeAccount::new(self.maker_position, Role::Maker)
            .add_opening_fee(self.opening_fee)
            .add_funding_fee(self.initial_funding_fee);

        let taker_position = self.maker_position.counter_position();
        let mut taker_fee_account = FeeAccount::new(taker_position, Role::Taker)
            .add_opening_fee(self.opening_fee)
            .add_funding_fee(self.initial_funding_fee);

        tracing::debug!(
            "Maker fees including opening and initial funding fee: {}",
            maker_fee_account.balance()
        );

        tracing::debug!(
            "Taker fees including opening and initial funding fee: {}",
            taker_fee_account.balance()
        );

        let accumulated_hours_to_charge = match self.maker_position {
            Position::Long => FundingFee::calculate(
                self.offer_params.price_long.unwrap(),
                self.quantity,
                Leverage::ONE,
                self.taker_leverage,
                self.offer_params.funding_rate_long,
                accumulated_rollover_hours_to_charge,
            )
            .unwrap(),
            Position::Short => FundingFee::calculate(
                self.offer_params.price_short.unwrap(),
                self.quantity,
                self.taker_leverage,
                Leverage::ONE,
                self.offer_params.funding_rate_short,
                accumulated_rollover_hours_to_charge,
            )
            .unwrap(),
        };

        maker_fee_account = maker_fee_account.add_funding_fee(accumulated_hours_to_charge);
        taker_fee_account = taker_fee_account.add_funding_fee(accumulated_hours_to_charge);

        tracing::debug!(
            "Maker fees including all fees: {}",
            maker_fee_account.balance()
        );

        tracing::debug!(
            "Taker fees including all fees: {}",
            taker_fee_account.balance()
        );

        (maker_fee_account.balance(), taker_fee_account.balance())
    }
}

fn oracle_pk() -> XOnlyPublicKey {
    XOnlyPublicKey::from_str("ddd4636845a90185991826be5a494cde9f4a6947b1727217afedc6292fa4caf7")
        .unwrap()
}

#[instrument]
pub async fn start_both() -> (Maker, Taker) {
    let maker = Maker::start(&MakerConfig::default()).await;
    let taker = Taker::start(
        &TakerConfig::default(),
        maker.listen_addr,
        maker.identity,
        maker.connect_addr.clone(),
    )
    .await;
    (maker, taker)
}

#[derive(Clone, Copy, Debug)]
pub struct MakerConfig {
    oracle_pk: XOnlyPublicKey,
    seed: RandomSeed,
    pub heartbeat_interval: Duration,
    n_payouts: usize,
    dedicated_port: Option<u16>,
    dedicated_libp2p_port: Option<u16>,
}

impl MakerConfig {
    pub fn with_dedicated_port(self, port: u16) -> Self {
        Self {
            dedicated_port: Some(port),
            ..self
        }
    }

    pub fn with_dedicated_libp2p_port(self, port: u16) -> Self {
        Self {
            dedicated_libp2p_port: Some(port),
            ..self
        }
    }
}

impl Default for MakerConfig {
    fn default() -> Self {
        Self {
            oracle_pk: oracle_pk(),
            seed: RandomSeed::default(),
            heartbeat_interval: HEARTBEAT_INTERVAL,
            n_payouts: N_PAYOUTS,
            dedicated_port: None,
            dedicated_libp2p_port: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TakerConfig {
    oracle_pk: XOnlyPublicKey,
    seed: RandomSeed,
    n_payouts: usize,
}

impl Default for TakerConfig {
    fn default() -> Self {
        Self {
            oracle_pk: oracle_pk(),
            seed: RandomSeed::default(),
            n_payouts: N_PAYOUTS,
        }
    }
}

/// Maker Test Setup
pub struct Maker {
    pub system: maker::ActorSystem<OracleActor, WalletActor>,
    pub mocks: mocks::Mocks,
    pub feeds: Feeds,
    pub listen_addr: SocketAddr,
    pub identity: Identity,
    /// The address on which taker can dial in with libp2p protocols (includes
    /// maker's PeerId)
    pub connect_addr: Multiaddr,
    _tasks: Tasks,
}

impl Maker {
    pub fn cfd_feed(&mut self) -> &mut watch::Receiver<Option<Vec<Cfd>>> {
        &mut self.feeds.cfds
    }

    pub fn first_cfd(&mut self) -> Cfd {
        self.cfd_feed()
            .borrow()
            .as_ref()
            .unwrap()
            .first()
            .unwrap()
            .clone()
    }

    pub fn latest_commit_txid(&mut self) -> Txid {
        self.first_cfd()
            .aggregated()
            .latest_dlc()
            .as_ref()
            .unwrap()
            .commit
            .0
            .txid()
    }

    pub fn offers_feed(&mut self) -> &mut watch::Receiver<MakerOffers> {
        &mut self.feeds.offers
    }

    pub fn connected_takers_feed(&mut self) -> &mut watch::Receiver<Vec<Identity>> {
        &mut self.feeds.connected_takers
    }

    #[instrument(name = "Start maker", skip_all)]
    pub async fn start(config: &MakerConfig) -> Self {
        let port = match config.dedicated_port {
            Some(port) => port,
            None => find_random_free_port().await,
        };
        let libp2p_port = match config.dedicated_libp2p_port {
            Some(port) => port,
            None => find_random_free_port().await,
        };

        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

        let db = sqlite_db::memory().await.unwrap();

        let (wallet, wallet_mock) = WalletActor::new();
        let (price_feed, price_feed_mock) = PriceFeedActor::new();

        let mut tasks = Tasks::default();

        let wallet_addr = wallet.create(None).spawn(&mut tasks);

        let (price_feed_addr, price_feed_fut) = price_feed.create(None).run();
        tasks.add(async move {
            let _ = price_feed_fut.await;
        });

        let settlement_interval = SETTLEMENT_INTERVAL;

        let identities = config.seed.derive_identities();

        let (projection_actor, projection_context) = xtra::Context::new(None);

        let mut monitor_mock = None;
        let mut oracle_mock = None;

        let endpoint_listen =
            daemon::libp2p_utils::create_listen_tcp_multiaddr(&address.ip(), libp2p_port)
                .expect("to parse properly");

        let maker = maker::ActorSystem::new(
            db.clone(),
            wallet_addr,
            config.oracle_pk,
            |executor| {
                let (oracle, mock) = OracleActor::new(executor);
                oracle_mock = Some(mock);

                oracle
            },
            |executor| {
                let (monitor, mock) = MonitorActor::new(executor);
                monitor_mock = Some(mock);

                Ok(monitor)
            },
            settlement_interval,
            config.n_payouts,
            projection_actor,
            identities.clone(),
            config.heartbeat_interval,
            address,
            endpoint_listen.clone(),
        )
        .unwrap();

        let mocks = mocks::Mocks::new(
            wallet_mock,
            price_feed_mock,
            monitor_mock.unwrap(),
            oracle_mock.unwrap(),
        );

        let (proj_actor, feeds) =
            projection::Actor::new(db, Network::Testnet, price_feed_addr.into());
        tasks.add(projection_context.run(proj_actor));

        Self {
            system: maker,
            feeds,
            identity: model::Identity::new(identities.identity_pk),
            listen_addr: address,
            mocks,
            _tasks: tasks,
            connect_addr: create_connect_multiaddr(&endpoint_listen, &identities.peer_id().inner())
                .expect("to parse properly"),
        }
    }

    pub async fn set_offer_params(&mut self, offer_params: OfferParams) {
        let OfferParams {
            price_long,
            price_short,
            min_quantity,
            max_quantity,
            tx_fee_rate,
            funding_rate_long,
            funding_rate_short,
            opening_fee,
            leverage_choices,
        } = offer_params;
        self.system
            .set_offer_params(
                price_long,
                price_short,
                min_quantity,
                max_quantity,
                tx_fee_rate,
                funding_rate_long,
                funding_rate_short,
                opening_fee,
                leverage_choices,
            )
            .await
            .unwrap();
    }
}

async fn find_random_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Taker Test Setup
pub struct Taker {
    pub id: Identity,
    pub system: daemon::TakerActorSystem<OracleActor, WalletActor, PriceFeedActor>,
    pub mocks: mocks::Mocks,
    pub feeds: Feeds,
    pub maker_peer_id: PeerId,
    db: sqlite_db::Connection,
    _tasks: Tasks,
}

impl Taker {
    pub fn cfd_feed(&mut self) -> &mut watch::Receiver<Option<Vec<Cfd>>> {
        &mut self.feeds.cfds
    }

    pub fn first_cfd(&mut self) -> Cfd {
        self.cfd_feed()
            .borrow()
            .as_ref()
            .unwrap()
            .first()
            .unwrap()
            .clone()
    }

    pub fn latest_commit_txid(&mut self) -> Txid {
        self.first_cfd()
            .aggregated()
            .latest_dlc()
            .as_ref()
            .unwrap()
            .commit
            .0
            .txid()
    }

    pub fn latest_dlc(&mut self) -> Dlc {
        self.first_cfd()
            .aggregated()
            .latest_dlc()
            .as_ref()
            .unwrap()
            .clone()
    }

    pub fn latest_fees(&mut self) -> CompleteFee {
        self.first_cfd().aggregated().latest_fees()
    }

    pub fn offers_feed(&mut self) -> &mut watch::Receiver<MakerOffers> {
        &mut self.feeds.offers
    }

    pub fn quote_feed(&mut self) -> &mut watch::Receiver<Option<projection::Quote>> {
        &mut self.feeds.quote
    }

    pub fn maker_status_feed(&mut self) -> &mut watch::Receiver<ConnectionStatus> {
        &mut self.system.maker_online_status_feed_receiver
    }

    #[instrument(name = "Start taker", skip_all)]
    pub async fn start(
        config: &TakerConfig,
        maker_address: SocketAddr,
        maker_identity: Identity,
        maker_multiaddr: Multiaddr,
    ) -> Self {
        let identities = config.seed.derive_identities();

        let db = sqlite_db::memory().await.unwrap();

        let mut tasks = Tasks::default();

        let (wallet, wallet_mock) = WalletActor::new();
        let (price_feed, price_feed_mock) = PriceFeedActor::new();

        let wallet_addr = wallet.create(None).spawn(&mut tasks);

        let (projection_actor, projection_context) = xtra::Context::new(None);

        let mut oracle_mock = None;
        let mut monitor_mock = None;
        tracing::info!("Connecting to maker {maker_multiaddr}");

        let taker = daemon::TakerActorSystem::new(
            db.clone(),
            wallet_addr,
            config.oracle_pk,
            identities.clone(),
            |executor| {
                let (oracle, mock) = OracleActor::new(executor);
                oracle_mock = Some(mock);

                oracle
            },
            |executor| {
                let (monitor, mock) = MonitorActor::new(executor);
                monitor_mock = Some(mock);

                Ok(monitor)
            },
            move || price_feed.clone(),
            config.n_payouts,
            Duration::from_secs(10),
            projection_actor,
            maker_multiaddr.clone(),
            Environment::Test,
        )
        .unwrap();

        let mocks = mocks::Mocks::new(
            wallet_mock,
            price_feed_mock,
            monitor_mock.unwrap(),
            oracle_mock.unwrap(),
        );

        let (proj_actor, feeds) = projection::Actor::new(
            db.clone(),
            Network::Testnet,
            taker.price_feed_actor.clone().into(),
        );
        tasks.add(projection_context.run(proj_actor));

        tasks.add(connect(
            taker.maker_online_status_feed_receiver.clone(),
            taker.connection_actor.clone(),
            maker_identity,
            vec![maker_address],
        ));

        Self {
            id: model::Identity::new(identities.identity_pk),
            system: taker,
            feeds,
            mocks,
            maker_peer_id: maker_multiaddr
                .extract_peer_id()
                .expect("to have peer id")
                .into(),
            db,
            _tasks: tasks,
        }
    }

    pub async fn trigger_rollover_with_latest_dlc_params(&mut self, order_id: OrderId) {
        let latest_dlc = self.first_cfd().aggregated().latest_dlc().clone().unwrap();
        self.system
            .auto_rollover_actor
            .send(auto_rollover::Rollover {
                order_id,
                maker_peer_id: Some(self.maker_peer_id),
                from_commit_txid: latest_dlc.commit.0.txid(),
                from_settlement_event_id: latest_dlc.settlement_event_id,
            })
            .await
            .unwrap();
    }

    pub async fn trigger_rollover_with_specific_params(
        &mut self,
        order_id: OrderId,
        from_commit_txid: Txid,
        from_settlement_event_id: BitMexPriceEventId,
    ) {
        self.system
            .auto_rollover_actor
            .send(auto_rollover::Rollover {
                order_id,
                maker_peer_id: Some(self.maker_peer_id),
                from_commit_txid,
                from_settlement_event_id,
            })
            .await
            .unwrap();
    }

    /// Appends an event that overwrites the current DLC
    ///
    /// Note that the projection does not get updated, this change only manipulates the database!
    /// When triggering another rollover this data will be loaded and used.
    pub async fn simulate_previous_rollover(
        &mut self,
        id: OrderId,
        dlc: Dlc,
        complete_fee: CompleteFee,
    ) {
        tracing::info!(commit_txid = %dlc.commit.0.txid(), "Manually setting latest DLC");

        self.db
            .append_event(CfdEvent::new(
                id,
                EventKind::RolloverCompleted {
                    dlc: Some(dlc),
                    // Funding fee irrelevant because only CompleteFee is used
                    funding_fee: dummy_funding_fee(),
                    complete_fee: Some(complete_fee),
                },
            ))
            .await
            .unwrap()
    }
}

pub fn dummy_quote() -> Quote {
    Quote {
        timestamp: OffsetDateTime::now_utc(),
        bid: dummy_price(),
        ask: dummy_price(),
    }
}

// Offer params allowing a single position, either short or long
pub fn dummy_offer_params(position_maker: Position) -> OfferParams {
    let (price_long, price_short) = match position_maker {
        Position::Long => (Some(Price::new(dummy_price()).unwrap()), None),
        Position::Short => (None, Some(Price::new(dummy_price()).unwrap())),
    };

    OfferParams {
        price_long,
        price_short,
        min_quantity: Usd::new(dec!(100)),
        max_quantity: Usd::new(dec!(1000)),
        tx_fee_rate: TxFeeRate::default(),
        // 8.76% annualized = rate of 0.0876 annualized = rate of 0.00024 daily
        funding_rate_long: FundingRate::new(dec!(0.00024)).unwrap(),
        funding_rate_short: FundingRate::new(dec!(0.00024)).unwrap(),
        opening_fee: OpeningFee::new(Amount::from_sat(2)),
        leverage_choices: vec![Leverage::TWO],
    }
}

fn dummy_funding_fee() -> FundingFee {
    FundingFee::calculate(
        Price::new(dec!(10000)).unwrap(),
        Usd::ZERO,
        Leverage::ONE,
        Leverage::ONE,
        Default::default(),
        0,
    )
    .unwrap()
}

fn dummy_price() -> Decimal {
    dec!(50_000)
}

pub async fn mock_oracle_announcements(
    maker: &mut Maker,
    taker: &mut Taker,
    announcement: Announcement,
) {
    taker
        .mocks
        .mock_oracle_announcement_with(announcement.clone())
        .await;
    maker
        .mocks
        .mock_oracle_announcement_with(announcement)
        .await;
}
