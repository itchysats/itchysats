use crate::bitcoin::util::bip32::ExtendedPrivKey;
use crate::routes::IdentityInfo;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use daemon::bdk::bitcoin;
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
use shared_bin::cli::Network;
use shared_bin::cli::Withdraw;
use shared_bin::fairings;
use shared_bin::logger;
use shared_bin::logger::LevelFilter;
use shared_bin::logger::LOCAL_COLLECTOR_ENDPOINT;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio_extras::Tasks;

mod routes;

pub const ANNOUNCEMENT_LOOKAHEAD: time::Duration = time::Duration::hours(24);

const MAINNET_MAKER: &str = "mainnet.itchysats.network:10000";
const MAINNET_MAKER_ID: &str = "7e35e34801e766a6a29ecb9e22810ea4e3476c2b37bf75882edf94a68b1d9607";
const MAINNET_MAKER_PEER_ID: &str = "12D3KooWP3BN6bq9jPy8cP7Grj1QyUBfr7U6BeQFgMwfTTu12wuY";

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

    /// If enabled, logs in json format will contain a list of all ancestor spans of log events.
    /// This **only** has an effect when `json` is also enabled.
    #[clap(long)]
    pub json_span_list: bool,

    /// If enabled, traces will be exported to the OTEL collector
    #[clap(long)]
    instrumentation: bool,

    /// If enabled, tokio runtime can be locally debugged with tokio_console
    #[clap(long)]
    pub tokio_console: bool,

    /// If enabled, libp2p ping and offer broadcast spans will be included in the traces exported
    /// by the application.
    #[clap(long)]
    pub verbose_spans: bool,

    /// OTEL collector endpoint address
    ///
    /// If not specified it defaults to the local collector endpoint.
    #[clap(long, default_value = LOCAL_COLLECTOR_ENDPOINT )]
    collector_endpoint: String,

    /// Service name for OTEL.
    ///
    /// If not specified it defaults to the binary name.
    #[clap(long, default_value = "taker")]
    service_name: String,

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

    /// If provided will be used for internal wallet instead of a random key or umbrel_seed. The
    /// keys will be derived according to Bip84.
    #[clap(short, long)]
    pub wallet_xprv: Option<ExtendedPrivKey>,
}

impl Opts {
    fn network(&self) -> Network {
        self.network.clone().unwrap_or_default()
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

#[rocket::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();
    let (maker_url, maker_id, maker_peer_id) = opts.maker()?;

    logger::init(
        opts.log_level,
        opts.json,
        opts.json_span_list,
        opts.instrumentation,
        opts.tokio_console,
        opts.verbose_spans,
        &opts.service_name,
        &opts.collector_endpoint,
    )
    .context("initialize logger")?;
    tracing::info!("Running version: {}", daemon::version::version());
    let settlement_interval_hours = SETTLEMENT_INTERVAL.whole_hours();

    tracing::info!(
        "CFDs created with this release will settle after {settlement_interval_hours} hours"
    );

    let network = opts.network();

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

    let ext_priv_key = match opts.wallet_xprv {
        Some(wallet_xprv) => wallet_xprv,
        None => ext_priv_key,
    };

    let mut tasks = Tasks::default();

    let mut wallet_dir = data_dir.clone();
    wallet_dir.push(TAKER_WALLET_ID);
    let (wallet, wallet_feed_receiver) = wallet::Actor::spawn(
        network.electrum(),
        ext_priv_key,
        wallet_dir,
        TAKER_WALLET_ID.to_string(),
    )?;

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

    let hex_pk = hex::encode(identities.identity_pk.to_bytes());
    let peer_id = identities.libp2p.public().to_peer_id().to_string();

    tracing::info!("Connection details: taker_id='{hex_pk}', peer_id='{peer_id}'");

    let identity_info = IdentityInfo {
        taker_id: hex_pk,
        taker_peer_id: peer_id,
    };

    let environment = match env::var("ITCHYSATS_ENV") {
        Ok(environment) => Environment::from_str_or_unknown(environment.as_str()),
        Err(_) => Environment::Binary,
    };

    let bitmex_network = network.bitmex_network();
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
        move || xtra_bitmex_price_feed::Actor::new(bitmex_network),
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
