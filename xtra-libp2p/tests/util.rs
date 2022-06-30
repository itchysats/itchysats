use async_trait::async_trait;
use libp2p_core::Multiaddr;
use std::collections::HashSet;
use std::time::Duration;
use xtra::message_channel::MessageChannel;
use xtra::spawn::TokioGlobalSpawnExt;
use xtra::Actor;
use xtra::Address;
use xtra_libp2p::endpoint;
use xtra_libp2p::endpoint::Subscribers;
use xtra_libp2p::libp2p::identity::Keypair;
use xtra_libp2p::libp2p::transport::MemoryTransport;
use xtra_libp2p::libp2p::PeerId;
use xtra_libp2p::Endpoint;
use xtra_libp2p::NewInboundSubstream;
use xtra_productivity::xtra_productivity;

/// Small aggregate dedicated to keep everything that's related to one party
/// (e.g. alice or bob) in one place
pub struct Node {
    pub peer_id: PeerId,
    pub endpoint: Address<Endpoint>,
    pub subscriber_stats: Address<EndpointSubscriberStats>,
}

pub fn make_node<const N: usize>(
    substream_handlers: [(&'static str, MessageChannel<NewInboundSubstream, ()>); N],
) -> Node {
    let id = Keypair::generate_ed25519();
    let peer_id = id.public().to_peer_id();

    let subscriber_stats = EndpointSubscriberStats::default()
        .create(None)
        .spawn_global();

    let endpoint = Endpoint::new(
        Box::new(MemoryTransport::default),
        id,
        Duration::from_secs(20),
        substream_handlers,
        Subscribers::new(
            vec![subscriber_stats.clone().into()],
            vec![subscriber_stats.clone().into()],
            vec![subscriber_stats.clone().into()],
            vec![subscriber_stats.clone().into()],
        ),
    )
    .create(None)
    .spawn_global();

    Node {
        peer_id,
        endpoint,
        subscriber_stats,
    }
}

/// A test actor subscribing to all the notifications
#[derive(Default)]
pub struct EndpointSubscriberStats {
    connected_peers: HashSet<PeerId>,
    listen_addresses: HashSet<Multiaddr>,
}

#[async_trait]
impl Actor for EndpointSubscriberStats {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}

#[xtra_productivity(message_impl = false)]
impl EndpointSubscriberStats {
    async fn handle(&mut self, msg: endpoint::ConnectionEstablished) {
        self.connected_peers.insert(msg.peer);
    }

    async fn handle(&mut self, msg: endpoint::ConnectionDropped) {
        self.connected_peers.remove(&msg.peer);
    }

    async fn handle(&mut self, msg: endpoint::ListenAddressAdded) {
        self.listen_addresses.insert(msg.address);
    }

    async fn handle(&mut self, msg: endpoint::ListenAddressRemoved) {
        self.listen_addresses.remove(&msg.address);
    }
}

#[xtra_productivity]
impl EndpointSubscriberStats {
    async fn handle(&mut self, _msg: GetConnectedPeers) -> HashSet<PeerId> {
        self.connected_peers.clone()
    }

    async fn handle(&mut self, _msg: GetListenAddresses) -> HashSet<Multiaddr> {
        self.listen_addresses.clone()
    }
}

/// Returns connected peers
#[derive(Clone, Copy)]
pub struct GetConnectedPeers;

/// Returns current listen addressess
#[derive(Clone, Copy)]
pub struct GetListenAddresses;
