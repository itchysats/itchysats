use crate::mocks::monitor::MonitorActor;
use crate::mocks::oracle::OracleActor;
use crate::mocks::price_feed::PriceFeedActor;
use crate::mocks::wallet::WalletActor;
use daemon::auto_rollover;
use daemon::bdk::bitcoin::Amount;
use daemon::bdk::bitcoin::Network;
use daemon::bdk::bitcoin::Txid;
use daemon::connection::connect;
use daemon::connection::ConnectionStatus;
use daemon::libp2p_utils::create_connect_multiaddr;
use daemon::maia_core::secp256k1_zkp::XOnlyPublicKey;
use daemon::projection;
use daemon::projection::Cfd;
use daemon::projection::Feeds;
use daemon::projection::MakerOffers;
use daemon::seed::RandomSeed;
use daemon::seed::Seed;
use daemon::Environment;
use daemon::HEARTBEAT_INTERVAL;
use daemon::N_PAYOUTS;
use model::libp2p::PeerId;
use model::olivia::Announcement;
use model::olivia::BitMexPriceEventId;
use model::FundingRate;
use model::Identity;
use model::Leverage;
use model::OpeningFee;
use model::OrderId;
use model::Position;
use model::Price;
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
use tokio_extras::Tasks;
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use xtra::Actor;
use xtra_bitmex_price_feed::Quote;
use xtra_libp2p::libp2p::Multiaddr;
use xtra_libp2p::multiaddress_ext::MultiaddrExt;

pub mod flow;
pub mod maia;
pub mod mocks;

fn oracle_pk() -> XOnlyPublicKey {
    XOnlyPublicKey::from_str("ddd4636845a90185991826be5a494cde9f4a6947b1727217afedc6292fa4caf7")
        .unwrap()
}

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

#[derive(Clone, Copy)]
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

#[derive(Clone, Copy)]
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

    pub async fn set_offer_params(&mut self, offer_params: maker::cfd::OfferParams) {
        let maker::cfd::OfferParams {
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

    pub fn offers_feed(&mut self) -> &mut watch::Receiver<MakerOffers> {
        &mut self.feeds.offers
    }

    pub fn quote_feed(&mut self) -> &mut watch::Receiver<Option<projection::Quote>> {
        &mut self.feeds.quote
    }

    pub fn maker_status_feed(&mut self) -> &mut watch::Receiver<ConnectionStatus> {
        &mut self.system.maker_online_status_feed_receiver
    }

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
            maker_identity,
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

        let (proj_actor, feeds) =
            projection::Actor::new(db, Network::Testnet, taker.price_feed_actor.clone().into());
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
            _tasks: tasks,
        }
    }

    pub async fn trigger_rollover_with_latest_dlc_params(&mut self, id: OrderId) {
        let latest_dlc = self.first_cfd().aggregated().latest_dlc().clone().unwrap();
        self.system
            .auto_rollover_actor
            .send(auto_rollover::Rollover {
                order_id: id,
                maker_peer_id: Some(self.maker_peer_id),
                from_commit_txid: latest_dlc.commit.0.txid(),
                from_settlement_event_id: latest_dlc.settlement_event_id,
            })
            .await
            .unwrap();
    }

    pub async fn trigger_rollover_with_specific_params(
        &mut self,
        id: OrderId,
        from_commit_txid: Txid,
        from_settlement_event_id: BitMexPriceEventId,
    ) {
        self.system
            .auto_rollover_actor
            .send(auto_rollover::Rollover {
                order_id: id,
                maker_peer_id: Some(self.maker_peer_id),
                from_commit_txid,
                from_settlement_event_id,
            })
            .await
            .unwrap();
    }
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

pub fn dummy_quote() -> Quote {
    Quote {
        timestamp: OffsetDateTime::now_utc(),
        bid: dummy_price(),
        ask: dummy_price(),
    }
}

// Offer params allowing a single position, either short or long
pub fn dummy_offer_params(position_maker: Position) -> maker::cfd::OfferParams {
    let (price_long, price_short) = match position_maker {
        Position::Long => (Some(Price::new(dummy_price()).unwrap()), None),
        Position::Short => (None, Some(Price::new(dummy_price()).unwrap())),
    };

    maker::cfd::OfferParams {
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

fn dummy_price() -> Decimal {
    dec!(50_000)
}

pub fn init_tracing() -> DefaultGuard {
    let filter = EnvFilter::from_default_env()
        // apply warning level globally
        .add_directive(LevelFilter::WARN.into())
        // log traces from test itself
        .add_directive("happy_path=debug".parse().unwrap())
        .add_directive("wire=trace".parse().unwrap())
        .add_directive("taker=debug".parse().unwrap())
        .add_directive("maker=debug".parse().unwrap())
        .add_directive("daemon=debug".parse().unwrap())
        .add_directive("xtra_libp2p=debug".parse().unwrap())
        .add_directive("xtra_libp2p_offer=debug".parse().unwrap())
        .add_directive("xtra_libp2p_ping=debug".parse().unwrap())
        .add_directive("rocket=warn".parse().unwrap());

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_test_writer()
        .set_default()
}

pub async fn mock_oracle_announcements(
    maker: &mut Maker,
    taker: &mut Taker,
    announcements: Vec<Announcement>,
) {
    taker
        .mocks
        .mock_oracle_announcement_with(announcements.clone())
        .await;
    maker
        .mocks
        .mock_oracle_announcement_with(announcements)
        .await;
}
