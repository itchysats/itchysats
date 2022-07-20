use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use clap::StructOpt;
use daemon::bdk::FeeRate;
use daemon::monitor;
use daemon::oracle;
use daemon::projection;
use daemon::seed::RandomSeed;
use daemon::seed::Seed;
use daemon::wallet;
use daemon::wallet::MAKER_WALLET_ID;
use daemon::HEARTBEAT_INTERVAL;
use daemon::N_PAYOUTS;
use maker::routes;
use maker::ActorSystem;
use maker::Opts;
use model::olivia;
use model::SETTLEMENT_INTERVAL;
use shared_bin::catchers::default_catchers;
use shared_bin::cli::Withdraw;
use shared_bin::fairings;
use shared_bin::logger;
use std::net::SocketAddr;
use tokio_extras::Tasks;
use xtras::supervisor::always_restart;
use xtras::supervisor::Supervisor;

#[rocket::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();
    let service_name = "maker_".to_string() + opts.network.kind();

    logger::init(
        opts.log_level,
        opts.json,
        opts.json_span_list,
        opts.instrumentation,
        opts.tokio_console,
        &service_name,
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

    let data_dir = opts.network.data_dir(data_dir);

    if !data_dir.exists() {
        tokio::fs::create_dir_all(&data_dir).await?;
    }

    let seed = RandomSeed::initialize(&data_dir.join("maker_seed")).await?;

    let bitcoin_network = opts.network.bitcoin_network();

    let ext_priv_key = match opts.wallet_xprv {
        Some(wallet_xprv) => {
            if wallet_xprv.network != bitcoin_network {
                let network = wallet_xprv.network;
                bail!("Invalid private key provided. Was '{network}' but should have been '{bitcoin_network}'");
            }
            wallet_xprv
        }
        None => seed.derive_extended_priv_key(bitcoin_network)?,
    };

    let mut tasks = Tasks::default();

    let mut wallet_dir = data_dir.clone();

    wallet_dir.push(MAKER_WALLET_ID);
    let (wallet, wallet_feed_receiver) = wallet::Actor::spawn(
        opts.network.electrum(),
        ext_priv_key,
        wallet_dir,
        MAKER_WALLET_ID.to_string(),
    )?;

    if let Some(Withdraw::Withdraw {
        amount,
        address,
        fee,
    }) = opts.network.withdraw()
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
    let auth_password = seed.derive_auth_password::<rocket_basicauth::Password>();

    let identities = seed.derive_identities();

    let peer_id = identities.peer_id();
    let hex_pk = hex::encode(identities.identity_pk.to_bytes());
    tracing::info!("Authentication details: username='{auth_username}' password='{auth_password}'");
    tracing::info!("Connection details: maker_id='{hex_pk}', peer_id='{peer_id}'");

    let figment = rocket::Config::figment()
        .merge(("address", opts.http_address.ip()))
        .merge(("port", opts.http_address.port()))
        .merge(("cli_colors", false));

    let p2p_port = opts.p2p_port;
    let p2p_socket = format!("0.0.0.0:{p2p_port}").parse::<SocketAddr>().unwrap();

    let db =
        sqlite_db::connect(data_dir.join("maker.sqlite"), opts.ignore_migration_errors).await?;

    // Create actors

    let (projection_actor, projection_context) = xtra::Context::new(None);

    let libp2p_socket = daemon::libp2p_utils::libp2p_socket_from_legacy_networking(&p2p_socket);
    let endpoint_listen = daemon::libp2p_utils::create_listen_tcp_multiaddr(
        &libp2p_socket.ip(),
        libp2p_socket.port(),
    )
    .expect("to parse properly");

    let maker = ActorSystem::new(
        db.clone(),
        wallet.clone(),
        *olivia::PUBLIC_KEY,
        |executor| oracle::Actor::new(db.clone(), executor),
        {
            |executor| {
                let electrum = opts.network.electrum().to_string();
                monitor::Actor::new(db.clone(), electrum, executor)
            }
        },
        SETTLEMENT_INTERVAL,
        N_PAYOUTS,
        projection_actor.clone(),
        identities,
        HEARTBEAT_INTERVAL,
        p2p_socket,
        endpoint_listen,
    )?;

    let (supervisor, price_feed) = Supervisor::with_policy(
        move || xtra_bitmex_price_feed::Actor::new(opts.network.bitmex_network()),
        always_restart::<xtra_bitmex_price_feed::Error>(),
    );

    tasks.add(supervisor.run_log_summary());

    let (proj_actor, projection_feeds) =
        projection::Actor::new(db.clone(), bitcoin_network, price_feed.clone().into());
    tasks.add(projection_context.run(proj_actor));

    let mission_success = rocket::custom(figment)
        .manage(projection_feeds)
        .manage(wallet_feed_receiver)
        .manage(maker)
        .manage(auth_username)
        .manage(auth_password)
        .manage(bitcoin_network)
        .mount(
            "/api",
            rocket::routes![
                routes::maker_feed,
                routes::put_offer_params,
                routes::post_cfd_action,
                routes::get_health_check,
                routes::get_cfds,
                routes::get_takers,
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
        .attach(fairings::ui_browser_launch())
        .launch()
        .await?;

    tracing::trace!(?mission_success, "Rocket has landed");

    db.close().await;

    Ok(())
}
