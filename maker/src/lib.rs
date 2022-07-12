use bdk::bitcoin::util::bip32::ExtendedPrivKey;
use clap::Parser;
use clap::Subcommand;
use daemon::bdk;
use daemon::bdk::bitcoin;
use daemon::bdk::bitcoin::Amount;
use shared_bin::logger::LevelFilter;
use shared_bin::logger::LOCAL_COLLECTOR_ENDPOINT;
use std::net::SocketAddr;
use std::path::PathBuf;

pub use actor_system::ActorSystem;

mod actor_system;
pub mod cfd;
mod collab_settlement;
mod connection;
mod contract_setup;
mod legacy_rollover;
mod metrics;
pub mod routes;

#[derive(Parser)]
pub struct Opts {
    /// The port to listen on for p2p connections.
    #[clap(long, default_value = "9999")]
    pub p2p_port: u16,

    /// The IP address to listen on for the HTTP API.
    #[clap(long, default_value = "127.0.0.1:8001")]
    pub http_address: SocketAddr,

    /// Where to permanently store data, defaults to the current working directory.
    #[clap(long)]
    pub data_dir: Option<PathBuf>,

    /// If enabled logs will be in json format
    #[clap(short, long)]
    pub json: bool,

    /// If enabled, traces will be exported to the OTEL collector
    #[clap(long)]
    pub instrumentation: bool,

    /// If enabled, tokio runtime can be locally debugged with tokio_console
    #[clap(long)]
    pub tokio_console: bool,

    /// OTEL collector endpoint address
    ///
    /// If not specified it defaults to the local collector endpoint.
    #[clap(long, default_value = LOCAL_COLLECTOR_ENDPOINT )]
    pub collector_endpoint: String,

    /// If enabled the application will not fail if an error occurred during db migration.
    #[clap(short, long)]
    pub ignore_migration_errors: bool,

    /// If provided will be used for internal wallet instead of a random key. The keys will be
    /// derived according to Bip84
    #[clap(short, long)]
    pub wallet_xprv: Option<ExtendedPrivKey>,

    /// Configure the log level, e.g.: one of Error, Warn, Info, Debug, Trace
    #[clap(short, long, default_value = "Debug")]
    pub log_level: LevelFilter,

    #[clap(subcommand)]
    pub network: Network,
}

#[derive(Parser)]
pub enum Network {
    /// Run on mainnet.
    Mainnet {
        /// URL to the electrum backend to use for the wallet.
        #[clap(long, default_value = "ssl://blockstream.info:700")]
        electrum: String,

        #[clap(subcommand)]
        withdraw: Option<Withdraw>,
    },
    /// Run on testnet.
    Testnet {
        /// URL to the electrum backend to use for the wallet.
        #[clap(long, default_value = "ssl://blockstream.info:993")]
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

#[derive(Subcommand)]
pub enum Withdraw {
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
        address: bdk::bitcoin::Address,
    },
}

impl Network {
    pub fn electrum(&self) -> &str {
        match self {
            Network::Mainnet { electrum, .. } => electrum,
            Network::Testnet { electrum, .. } => electrum,
            Network::Signet { electrum, .. } => electrum,
            Network::Regtest { electrum, .. } => electrum,
        }
    }

    pub fn bitcoin_network(&self) -> bitcoin::Network {
        match self {
            Network::Mainnet { .. } => bitcoin::Network::Bitcoin,
            Network::Testnet { .. } => bitcoin::Network::Testnet,
            Network::Signet { .. } => bitcoin::Network::Signet,
            Network::Regtest { .. } => bitcoin::Network::Regtest,
        }
    }

    pub fn price_feed_network(&self) -> xtra_bitmex_price_feed::Network {
        match self {
            Network::Mainnet { .. } => xtra_bitmex_price_feed::Network::Mainnet,
            Network::Testnet { .. } | Network::Signet { .. } | Network::Regtest { .. } => {
                xtra_bitmex_price_feed::Network::Testnet
            }
        }
    }

    pub fn data_dir(&self, base: PathBuf) -> PathBuf {
        match self {
            Network::Mainnet { .. } => base.join("mainnet"),
            Network::Testnet { .. } => base.join("testnet"),
            Network::Signet { .. } => base.join("signet"),
            Network::Regtest { .. } => base.join("regtest"),
        }
    }

    pub fn withdraw(&self) -> &Option<Withdraw> {
        match self {
            Network::Mainnet { withdraw, .. } => withdraw,
            Network::Testnet { withdraw, .. } => withdraw,
            Network::Signet { withdraw, .. } => withdraw,
            Network::Regtest { withdraw, .. } => withdraw,
        }
    }
}
