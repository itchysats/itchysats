use anyhow::Context;
use anyhow::Result;
use opentelemetry::sdk::propagation::TraceContextPropagator;
use opentelemetry::sdk::trace;
use opentelemetry::sdk::Resource;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use time::macros::format_description;
use tracing_subscriber::filter::dynamic_filter_fn;
use tracing_subscriber::filter::Directive;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;

pub use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Default local collector endpoint, compatible with jaeger
pub const LOCAL_COLLECTOR_ENDPOINT: &str = "http://localhost:4317";

const RUST_LOG_ENV: &str = "RUST_LOG";

// because the logger is only initialized at the end of this function but we want to print a warning
#[allow(clippy::print_stdout, clippy::too_many_arguments)]
pub fn init(
    level: LevelFilter,
    json_format: bool,
    json_span_list: bool,
    instrumentation: bool,
    use_tokio_console: bool,
    verbose_spans: bool,
    service_name: &str,
    collector_endpoint: &str,
) -> Result<()> {
    if level == LevelFilter::OFF {
        return Ok(());
    }

    let is_terminal = atty::is(atty::Stream::Stderr);

    let filter = match std::env::var_os(RUST_LOG_ENV).map(|s| s.into_string()) {
        Some(Ok(env)) => {
            let mut filter = log_base_directives(EnvFilter::new(""))?;
            for directive in env.split(',') {
                match directive.parse() {
                    Ok(d) => filter = filter.add_directive(d),
                    Err(e) => println!("WARN ignoring log directive: `{directive}`: {e}"),
                };
            }
            filter
        }
        _ => log_base_directives(EnvFilter::from_env(RUST_LOG_ENV))?,
    };

    let filter = filter.add_directive(format!("{level}").parse()?);

    let filter = if use_tokio_console {
        filter
            .add_directive("tokio=trace".parse()?)
            .add_directive("runtime=trace".parse()?)
    } else {
        filter
    };

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(is_terminal);

    let fmt_layer = if json_format {
        fmt_layer
            .json()
            .with_span_list(json_span_list)
            .with_timer(UtcTime::rfc_3339())
            .boxed()
    } else {
        fmt_layer
            .with_timer(UtcTime::new(format_description!(
                "[year]-[month]-[day] [hour]:[minute]:[second]"
            )))
            .boxed()
    };

    let telemetry = if instrumentation {
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

        opentelemetry::global::set_error_handler(|error| {
            tracing::error!(target: "opentelemetry", "OpenTelemetry error occurred: {:#}", anyhow::anyhow!(error));
        })
        .expect("to be able to set error handler");

        let cfg = trace::Config::default().with_resource(Resource::new([KeyValue::new(
            "service.name",
            service_name.to_string(),
        )]));

        let tracer = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_trace_config(cfg)
            .with_exporter(
                opentelemetry_otlp::new_exporter()
                    .tonic()
                    .with_endpoint(collector_endpoint),
            )
            .install_batch(opentelemetry::runtime::Tokio)
            .context("Failed to initialise OTLP exporter")?;

        Some(tracing_opentelemetry::layer().with_tracer(tracer))
    } else {
        None
    };

    let console_layer = if use_tokio_console {
        Some(console_subscriber::spawn())
    } else {
        None
    };

    let disable_spans = dynamic_filter_fn(move |meta, ctx| {
        if meta.is_span() && !["tokio", "runtime"].contains(&meta.target()) {
            match ctx.lookup_current() {
                Some(parent) => {
                    !(parent.name() == "always_quiet_children"
                        || (!verbose_spans && parent.name() == "sometimes_quiet_children"))
                }
                _ => true,
            }
        } else {
            true
        }
    });

    tracing_subscriber::registry()
        .with(console_layer)
        .with(disable_spans)
        .with(filter)
        .with(telemetry)
        .with(fmt_layer)
        .try_init()
        .context("Failed to init logger")?;

    tracing::info!("Initialized logger");

    Ok(())
}

fn log_base_directives(env: EnvFilter) -> Result<EnvFilter> {
    let filter = env
        .add_directive(Directive::from(LevelFilter::INFO))
        .add_directive("quiet_children=info".parse()?)
        .add_directive("bdk=warn".parse()?) // bdk is quite spamy on debug
        .add_directive("sqlx=warn".parse()?) // sqlx logs all queries on INFO
        .add_directive("hyper=warn".parse()?)
        .add_directive("rustls=warn".parse()?)
        .add_directive("reqwest=warn".parse()?)
        .add_directive("tungstenite=warn".parse()?)
        .add_directive("tokio_tungstenite=warn".parse()?)
        .add_directive("electrum_client=warn".parse()?)
        .add_directive("want=warn".parse()?)
        .add_directive("mio=warn".parse()?)
        .add_directive("tokio_util=warn".parse()?)
        .add_directive("yamux=warn".parse()?)
        .add_directive("multistream_select=warn".parse()?)
        .add_directive("libp2p_noise=warn".parse()?)
        .add_directive("h2=info".parse()?) // h2 spans originate from rocket and can spam a lot
        .add_directive("tonic=info".parse()?)
        .add_directive("tower=info".parse()?)
        .add_directive("_=off".parse()?) // rocket logs headers on INFO and uses `_` as the log target for it?
        .add_directive("rocket=off".parse()?) // disable rocket logs: we have our own
        .add_directive("opentelemetry=off".parse()?) // enable via RUST_LOG if needed
        .add_directive("sled=warn".parse()?) // downgrade sled log level: it is spamming too much on DEBUG
        .add_directive("xtra_libp2p_offer=debug".parse()?)
        .add_directive("xtras=debug".parse()?)
        .add_directive("xtra=debug".parse()?)
        .add_directive("xtra_libp2p=debug".parse()?);
    Ok(filter)
}
