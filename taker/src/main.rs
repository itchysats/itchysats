use crate::routes::IdentityInfo;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use daemon::bdk::bitcoin;
use daemon::bdk::bitcoin::Address;
use daemon::bdk::bitcoin::Amount;
use daemon::bdk::FeeRate;
use daemon::connection::connect;
use daemon::libp2p_utils::create_connect_tcp_multiaddr;
use daemon::libp2p_utils::libp2p_socket_from_legacy_networking;
use daemon::monitor;
use daemon::oracle;
use daemon::projection;
use daemon::seed::RandomSeed;
use daemon::seed::Seed;
use daemon::seed::UmbrelSeed;
use daemon::wallet;
use daemon::wallet::TAKER_WALLET_ID;
use daemon::Environment;
use daemon::TakerActorSystem;
use daemon::N_PAYOUTS;
use libp2p_core::PeerId;
use model::olivia;
use model::Identity;
use model::SETTLEMENT_INTERVAL;
use rocket::fairing::AdHoc;
use rocket::fairing::Fairing;
use shared_bin::catchers::default_catchers;
use shared_bin::fairings;
use shared_bin::logger;
use shared_bin::logger::LevelFilter;
use shared_bin::logger::LOCAL_COLLECTOR_ENDPOINT;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio_extras::Tasks;
use xtra::Actor;

mod routes;

pub const ANNOUNCEMENT_LOOKAHEAD: time::Duration = time::Duration::hours(24);

const MAINNET_ELECTRUM: &str = "ssl://blockstream.info:700";
const MAINNET_MAKER: &str = "mainnet.itchysats.network:10000";
const MAINNET_MAKER_ID: &str = "7e35e34801e766a6a29ecb9e22810ea4e3476c2b37bf75882edf94a68b1d9607";
const MAINNET_MAKER_PEER_ID: &str = "12D3KooWP3BN6bq9jPy8cP7Grj1QyUBfr7U6BeQFgMwfTTu12wuY";

const TESTNET_ELECTRUM: &str = "ssl://blockstream.info:993";
const TESTNET_MAKER: &str = "testnet.itchysats.network:9999";
const TESTNET_MAKER_ID: &str = "69a42aa90da8b065b9532b62bff940a3ba07dbbb11d4482c7db83a7e049a9f1e";
const TESTNET_MAKER_PEER_ID: &str = "12D3KooWEsK2X8Tp24XtyWh7DM65VfwXtNH2cmfs2JsWmkmwKbV1";

#[derive(Parser)]
struct Opts {
    /// The IP address or hostname of the other party (i.e. the maker).
    ///
    /// If not specified it defaults to the itchysats maker for the mainnet or testnet.
    #[clap(long)]
    maker: Option<String>,

    /// The public key of the maker as a 32 byte hex string.
    ///
    /// If not specified it defaults to the itchysats maker-id for mainnet or testnet.
    #[clap(long, parse(try_from_str = parse_x25519_pubkey))]
    maker_id: Option<x25519_dalek::PublicKey>,

    /// Maker's peer id, required for establishing libp2p encrypted connection.
    ///
    /// If not specified it defaults to the itchysats maker-peer-id for mainnet or testnet.
    #[clap(long)]
    maker_peer_id: Option<PeerId>,

    /// The IP address to listen on for the HTTP API.
    #[clap(long, default_value = "127.0.0.1:8000")]
    http_address: SocketAddr,

    /// Where to permanently store data, defaults to the current working directory.
    #[clap(long)]
    data_dir: Option<PathBuf>,

    /// If enabled logs will be in json format
    #[clap(short, long)]
    json: bool,

    /// If enabled, traces will be exported to the OTEL collector
    #[clap(long)]
    instrumentation: bool,

    /// OTEL collector endpoint address
    ///
    /// If not specified it defaults to the local collector endpoint.
    #[clap(long, default_value = LOCAL_COLLECTOR_ENDPOINT )]
    collector_endpoint: String,

    /// Configure the log level, e.g.: one of Error, Warn, Info, Debug, Trace
    #[clap(short, long, default_value = "Debug")]
    log_level: LevelFilter,

    /// Password for the web interface.
    ///
    /// If not provided, will be derived from the seed.
    #[clap(long)]
    password: Option<rocket_basicauth::Password>,

    #[clap(subcommand)]
    network: Option<Network>,

    #[clap(short, long, parse(try_from_str = parse_umbrel_seed))]
    umbrel_seed: Option<[u8; 32]>,
}

impl Opts {
    fn network(&self) -> Network {
        self.network.clone().unwrap_or_else(|| Network::Mainnet {
            electrum: MAINNET_ELECTRUM.to_string(),
            withdraw: None,
        })
    }

    fn maker(&self) -> Result<(String, x25519_dalek::PublicKey, PeerId)> {
        let network = self.network();

        let maker_url = match self.maker.clone() {
            Some(maker) => maker,
            None => match network {
                Network::Mainnet { .. } => MAINNET_MAKER.to_string(),
                Network::Testnet { .. } => TESTNET_MAKER.to_string(),
                Network::Signet { .. } | Network::Regtest { .. } => {
                    bail!("No maker default URL configured for {network:?}")
                }
            },
        };

        let maker_id = match self.maker_id {
            Some(maker_id) => maker_id,
            None => match network {
                Network::Mainnet { .. } => parse_x25519_pubkey(MAINNET_MAKER_ID)?,
                Network::Testnet { .. } => parse_x25519_pubkey(TESTNET_MAKER_ID)?,
                Network::Signet { .. } | Network::Regtest { .. } => {
                    bail!("No maker default public key configured for {network:?}")
                }
            },
        };

        let maker_peer_id = match self.maker_peer_id {
            Some(maker_peer_id) => maker_peer_id,
            None => match network {
                Network::Mainnet { .. } => MAINNET_MAKER_PEER_ID.parse()?,
                Network::Testnet { .. } => TESTNET_MAKER_PEER_ID.parse()?,
                Network::Signet { .. } | Network::Regtest { .. } => {
                    bail!("No maker default peer id configured for {network:?}")
                }
            },
        };

        Ok((maker_url, maker_id, maker_peer_id))
    }
}

fn parse_x25519_pubkey(s: &str) -> Result<x25519_dalek::PublicKey> {
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(s, &mut bytes)?;
    Ok(x25519_dalek::PublicKey::from(bytes))
}

fn parse_umbrel_seed(s: &str) -> Result<[u8; 32]> {
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(s, &mut bytes)?;
    Ok(bytes)
}

#[derive(Parser, Clone)]
enum Network {
    /// Run on mainnet (default)
    Mainnet {
        /// URL to the electrum backend to use for the wallet.
        #[clap(long, default_value = MAINNET_ELECTRUM)]
        electrum: String,

        #[clap(subcommand)]
        withdraw: Option<Withdraw>,
    },
    /// Run on testnet
    Testnet {
        /// URL to the electrum backend to use for the wallet.
        #[clap(long, default_value = TESTNET_ELECTRUM)]
        electrum: String,

        #[clap(subcommand)]
        withdraw: Option<Withdraw>,
    },
    /// Run on signet
    Signet {
        /// URL to the electrum backend to use for the wallet.
        #[clap(long)]
        electrum: String,

        #[clap(subcommand)]
        withdraw: Option<Withdraw>,
    },
    /// Run on regtest
    Regtest {
        /// URL to the electrum backend to use for the wallet.
        #[clap(long)]
        electrum: String,

        #[clap(subcommand)]
        withdraw: Option<Withdraw>,
    },
}

#[derive(Subcommand, Clone)]
enum Withdraw {
    Withdraw {
        /// Optionally specify the amount of Bitcoin to be withdrawn. If not specified the wallet
        /// will be drained. Amount is to be specified with denomination, e.g. "0.1 BTC"
        #[clap(long)]
        amount: Option<Amount>,
        /// Optionally specify the fee-rate for the transaction. The fee-rate is specified as sats
        /// per vbyte, e.g. 5.0
        #[clap(long)]
        fee: Option<f32>,
        /// The address to receive the Bitcoin.
        #[clap(long)]
        address: Address,
    },
}

impl Network {
    fn electrum(&self) -> &str {
        match self {
            Network::Mainnet { electrum, .. } => electrum,
            Network::Testnet { electrum, .. } => electrum,
            Network::Signet { electrum, .. } => electrum,
            Network::Regtest { electrum, .. } => electrum,
        }
    }

    fn bitcoin_network(&self) -> bitcoin::Network {
        match self {
            Network::Mainnet { .. } => bitcoin::Network::Bitcoin,
            Network::Testnet { .. } => bitcoin::Network::Testnet,
            Network::Signet { .. } => bitcoin::Network::Signet,
            Network::Regtest { .. } => bitcoin::Network::Regtest,
        }
    }

    fn price_feed_network(&self) -> xtra_bitmex_price_feed::Network {
        match self {
            Network::Mainnet { .. } => xtra_bitmex_price_feed::Network::Mainnet,
            Network::Testnet { .. } | Network::Signet { .. } | Network::Regtest { .. } => {
                xtra_bitmex_price_feed::Network::Testnet
            }
        }
    }

    fn data_dir(&self, base: PathBuf) -> PathBuf {
        match self {
            Network::Mainnet { .. } => base.join("mainnet"),
            Network::Testnet { .. } => base.join("testnet"),
            Network::Signet { .. } => base.join("signet"),
            Network::Regtest { .. } => base.join("regtest"),
        }
    }

    fn withdraw(&self) -> &Option<Withdraw> {
        match self {
            Network::Mainnet { withdraw, .. } => withdraw,
            Network::Testnet { withdraw, .. } => withdraw,
            Network::Signet { withdraw, .. } => withdraw,
            Network::Regtest { withdraw, .. } => withdraw,
        }
    }
}

impl std::fmt::Debug for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Network::Mainnet { .. } => "mainnet".fmt(f),
            Network::Testnet { .. } => "testnet".fmt(f),
            Network::Signet { .. } => "signet".fmt(f),
            Network::Regtest { .. } => "regtest".fmt(f),
        }
    }
}

#[rocket::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();

    let network = opts.network();
    let (maker_url, maker_id, maker_peer_id) = opts.maker()?;

    logger::init(
        opts.log_level,
        opts.json,
        opts.instrumentation,
        "taker",
        &opts.collector_endpoint,
    )
    .context("initialize logger")?;
    tracing::info!("Running version: {}", daemon::version::version());
    let settlement_interval_hours = SETTLEMENT_INTERVAL.whole_hours();

    tracing::info!(
        "CFDs created with this release will settle after {settlement_interval_hours} hours"
    );

    let data_dir = opts
        .data_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().expect("unable to get cwd"));

    let data_dir = network.data_dir(data_dir);

    if !data_dir.exists() {
        tokio::fs::create_dir_all(&data_dir).await?;
    }

    let maker_identity = Identity::new(maker_id);

    let bitcoin_network = network.bitcoin_network();
    let (ext_priv_key, identities, web_password) = match opts.umbrel_seed {
        Some(seed_bytes) => {
            let seed = UmbrelSeed::from(seed_bytes);
            let ext_priv_key = seed.derive_extended_priv_key(bitcoin_network)?;
            let identities = seed.derive_identities();
            let web_password = opts.password.unwrap_or_else(|| seed.derive_auth_password());
            (ext_priv_key, identities, web_password)
        }
        None => {
            let seed = RandomSeed::initialize(&data_dir.join("taker_seed")).await?;
            let ext_priv_key = seed.derive_extended_priv_key(bitcoin_network)?;
            let identities = seed.derive_identities();
            let web_password = opts.password.unwrap_or_else(|| seed.derive_auth_password());
            (ext_priv_key, identities, web_password)
        }
    };

    let mut tasks = Tasks::default();

    let mut wallet_dir = data_dir.clone();
    wallet_dir.push(TAKER_WALLET_ID);
    let (wallet, wallet_feed_receiver) = wallet::Actor::new(
        network.electrum(),
        ext_priv_key,
        wallet_dir,
        TAKER_WALLET_ID.to_string(),
    )?;

    let wallet = wallet.create(None).spawn(&mut tasks);

    if let Some(Withdraw::Withdraw {
        amount,
        address,
        fee,
    }) = network.withdraw()
    {
        wallet
            .send(wallet::Withdraw {
                amount: *amount,
                address: address.clone(),
                fee: fee.map(FeeRate::from_sat_per_vb),
            })
            .await??;

        return Ok(());
    }

    let auth_username = rocket_basicauth::Username("itchysats");
    tracing::info!("Authentication details: username='{auth_username}' password='{web_password}'");

    let figment = rocket::Config::figment()
        .merge(("address", opts.http_address.ip()))
        .merge(("port", opts.http_address.port()))
        .merge(("cli_colors", false));

    let db = sqlite_db::connect(data_dir.join("taker.sqlite"), true).await?;

    // Create actors

    let (projection_actor, projection_context) = xtra::Context::new(None);

    let possible_addresses = resolve_maker_addresses(maker_url.as_str()).await?;

    // Assume that the first resolved ipv4 address is good enough for libp2p.
    let first_maker_address = possible_addresses
        .iter()
        .find(|x| x.is_ipv4())
        .context("Could not resolve maker URL")?;

    let maker_libp2p_address = libp2p_socket_from_legacy_networking(first_maker_address);
    let maker_multiaddr = create_connect_tcp_multiaddr(&maker_libp2p_address, maker_peer_id)?;

    let identity_info = IdentityInfo {
        taker_id: hex::encode(identities.identity_pk.to_bytes()),
        taker_peer_id: identities.libp2p.public().to_peer_id().to_string(),
    };

    let environment = match env::var("ITCHYSATS_ENV") {
        Ok(environment) => Environment::from_str_or_unknown(environment.as_str()),
        Err(_) => Environment::Binary,
    };

    let price_feed_network = network.price_feed_network();
    let taker = TakerActorSystem::new(
        db.clone(),
        wallet.clone(),
        *olivia::PUBLIC_KEY,
        identities,
        |executor| oracle::Actor::new(db.clone(), executor),
        {
            |executor| {
                let electrum = network.electrum().to_string();
                monitor::Actor::new(db.clone(), electrum, executor)
            }
        },
        move || xtra_bitmex_price_feed::Actor::new(price_feed_network),
        N_PAYOUTS,
        Duration::from_secs(10),
        projection_actor.clone(),
        Identity::new(maker_id),
        maker_multiaddr,
        environment,
    )?;

    let (proj_actor, projection_feeds) = projection::Actor::new(
        db.clone(),
        bitcoin_network,
        taker.price_feed_actor.clone().into(),
    );
    tasks.add(projection_context.run(proj_actor));

    tasks.add(connect(
        taker.maker_online_status_feed_receiver.clone(),
        taker.connection_actor.clone(),
        maker_identity,
        possible_addresses,
    ));

    let mission_success = rocket::custom(figment)
        .manage(projection_feeds)
        .manage(wallet_feed_receiver)
        .manage(identity_info)
        .manage(bitcoin_network)
        .manage(taker.maker_online_status_feed_receiver.clone())
        .manage(taker.identify_info_feed_receiver.clone())
        .manage(taker)
        .manage(auth_username)
        .manage(web_password)
        .mount(
            "/api",
            rocket::routes![
                routes::feed,
                routes::post_order_request,
                routes::get_health_check,
                routes::post_cfd_action,
                routes::post_withdraw_request,
                routes::get_metrics,
                routes::put_sync_wallet,
                routes::get_version,
            ],
        )
        .register("/api", default_catchers())
        .mount("/", rocket::routes![routes::dist, routes::index])
        .register("/", default_catchers())
        .attach(fairings::log_launch())
        .attach(fairings::log_requests())
        .attach(ui_browser_launch())
        .launch()
        .await?;

    tracing::trace!(?mission_success, "Rocket has landed");

    db.close().await;

    Ok(())
}

async fn resolve_maker_addresses(maker_addr: &str) -> Result<Vec<SocketAddr>> {
    let possible_addresses = tokio::net::lookup_host(maker_addr)
        .await?
        .collect::<Vec<_>>();

    tracing::debug!(
        "Resolved {} to [{}]",
        maker_addr,
        itertools::join(possible_addresses.iter(), ",")
    );
    Ok(possible_addresses)
}

/// Attach this fairing to enable loading the UI in the system default browser
pub fn ui_browser_launch() -> impl Fairing {
    AdHoc::on_liftoff("ui browser launch", move |rocket| {
        Box::pin(async move {
            let (username, password) = match (
                rocket.state::<rocket_basicauth::Username>(),
                rocket.state::<rocket_basicauth::Password>(),
            ) {
                (Some(username), Some(password)) => (username, password),
                _ => {
                    tracing::warn!("Username and password not configured correctly");
                    return;
                }
            };

            let http_endpoint = format!(
                "http://{}:{}@{}:{}",
                username,
                password,
                rocket.config().address,
                rocket.config().port
            );

            match webbrowser::open(http_endpoint.as_str()) {
                Ok(()) => {
                    tracing::info!("The user interface was opened in your default browser");
                }
                Err(e) => {
                    tracing::debug!(
                        "Could not open user interface at {} in default browser because {e:#}",
                        http_endpoint
                    );
                }
            }
        })
    })
}
