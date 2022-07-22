use crate::protocol;
use crate::PROTOCOL_NAME;
use conquer_once::Lazy;
use prometheus::register_histogram;
use prometheus::Histogram;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;
use tokio_extras::spawn_fallible;
use tracing::Instrument;
use xtra::prelude::async_trait;
use xtra::Address;
use xtra::Context;
use xtra_libp2p::endpoint;
use xtra_libp2p::libp2p::PeerId;
use xtra_libp2p::Endpoint;
use xtra_libp2p::GetConnectionStats;
use xtra_libp2p::OpenSubstream;
use xtra_productivity::xtra_productivity;
use xtras::SendAsyncNext;
use xtras::SendInterval;

/// An actor implementing the official ipfs/libp2p ping protocol.
///
/// The ping protocol serves two purposes:
///
/// 1. To measure the latency to other peers.
/// 2. To prevent an otherwise seldom-utilised connection from being closed by intermediary network
/// devices along the connection pathway.
///
/// When constructed with a `ping_interval`, the actor will request all connected peers from the
/// provided [`Endpoint`] and ping all peers.
///
/// This actor also implements the listening end of the ping protocol and will correctly handle
/// incoming pings even without a `ping_interval` set. This is useful if an application wants to
/// allow other peers in the network to measure their latency but is not interested in measuring
/// latencies itself or keeping connections alive otherwise.
pub struct Actor {
    endpoint: Address<Endpoint>,
    ping_interval: Duration,
    connected_peers: HashSet<PeerId>,
    latencies: HashMap<PeerId, Duration>,
}

impl Actor {
    pub fn new(endpoint: Address<Endpoint>, ping_interval: Duration) -> Self {
        Self {
            endpoint,
            ping_interval,
            connected_peers: HashSet::default(),
            latencies: HashMap::default(),
        }
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    type Stop = ();

    #[tracing::instrument("Ping actor started", skip_all)]
    async fn started(&mut self, ctx: &mut Context<Self>) {
        let this = ctx.address().expect("we just started");

        match self.endpoint.send(GetConnectionStats).await {
            Ok(connection_stats) => self
                .connected_peers
                .extend(connection_stats.connected_peers),
            Err(e) => {
                tracing::error!(
                    "Unable to receive connection stats from the endpoint upon startup: {e:#}"
                );
                // This code path should not be hit, but in case we run into an error this sleep
                // prevents a continuous endless loup of restarts.
                tokio_extras::time::sleep(Duration::from_secs(2)).await;

                ctx.stop_self();
            }
        }

        tokio_extras::spawn(
            &this.clone(),
            this.send_interval(self.ping_interval, || Ping, xtras::IncludeSpan::Always),
        );
    }

    async fn stopped(self) -> Self::Stop {}
}

/// Private message to ping all connected peers.
struct Ping;

/// Private message to record latency of a peer.
struct RecordLatency {
    peer: PeerId,
    latency: Duration,
}

/// Private message to get the latency of a peer.
///
/// Primarily used for testing. May be exposed publicly at some point.
pub(crate) struct GetLatency(pub PeerId);

#[xtra_productivity]
impl Actor {
    async fn handle(&mut self, _: Ping, ctx: &mut Context<Self>) {
        self.latencies.clear();

        let quiet = tracing::info_span!(target: "quiet_children", "sometimes_quiet_children");

        for peer in self.connected_peers.iter().copied() {
            let endpoint = self.endpoint.clone();
            let this = ctx.address().expect("we are alive");

            let ping_fut = {
                let this = this.clone();

                async move {
                    let stream = endpoint
                        .send(OpenSubstream::single_protocol(peer, PROTOCOL_NAME))
                        .await??
                        .await?;
                    let latency = protocol::send(stream).await?;

                    this.send_async_next(RecordLatency { peer, latency }).await;
                    anyhow::Ok(())
                }
            };

            let err_handler = move |e| async move {
                tracing::warn!(%peer, "Outbound ping protocol failed: {e:#}")
            };

            spawn_fallible(
                &this,
                ping_fut
                    .instrument(quiet.in_scope(|| tracing::debug_span!("Ping peer").or_current())),
                err_handler,
            );
        }
    }

    async fn handle(&mut self, msg: RecordLatency) {
        let RecordLatency { peer, latency } = msg;

        self.latencies.insert(peer, latency);

        let latency_milliseconds = latency.as_millis();

        tracing::trace!(%peer, %latency_milliseconds, "Received pong");

        let latency_seconds = latency_milliseconds.checked_div(1000).unwrap_or_default();
        PEER_LATENCY_HISTOGRAM.observe(latency_seconds as f64);
    }

    async fn handle(&mut self, GetLatency(peer): GetLatency) -> Option<Duration> {
        return self.latencies.get(&peer).copied();
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle_connection_established(&mut self, msg: endpoint::ConnectionEstablished) {
        tracing::trace!(
            "Adding newly established connection to ping: {:?}",
            msg.peer
        );
        self.connected_peers.insert(msg.peer);
    }

    async fn handle_connection_dropped(&mut self, msg: endpoint::ConnectionDropped) {
        tracing::trace!("Remove dropped connection from ping: {:?}", msg.peer);
        self.connected_peers.remove(&msg.peer);
    }
}

/// A histogram tracking the latency to all our connected peers.
///
/// There are two things to note about the design of this metric.
///
/// 1. We are not using any labels. It is tempting to track the latency _per peer_, however creating
/// labels for unbounded sets of values (like user IDs) is an anti-pattern (see https://prometheus.io/docs/practices/naming/#labels).
/// 2. We assume most latencies will be in the order of 10-100 milliseconds which is why most of our
/// histogram buckets focus on this range.
static PEER_LATENCY_HISTOGRAM: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "p2p_ping_latency_seconds",
        "The latency of ping messages to all connected peers in seconds.",
        vec![
            0.01, 0.02, 0.03, 0.04, 0.05, 0.06, 0.07, 0.08, 0.09, 0.1, 0.15, 0.2, 0.3, 0.5, 0.75,
            1.0, 2.0, 5.0
        ]
    )
    .unwrap()
});
