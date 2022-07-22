use crate::util::make_node;
use crate::util::GetConnectedPeers;
use crate::util::GetListenAddresses;
use crate::util::Node;
use anyhow::Context as _;
use anyhow::Result;
use async_trait::async_trait;
use asynchronous_codec::Bytes;
use futures::SinkExt;
use futures::StreamExt;
use libp2p_core::multiaddr::Protocol;
use libp2p_core::Multiaddr;
use std::collections::HashSet;
use xtra::message_channel::MessageChannel;
use xtra::spawn::TokioGlobalSpawnExt;
use xtra::Actor;
use xtra::Context;
use xtra_libp2p::endpoint;
use xtra_libp2p::libp2p::PeerId;
use xtra_libp2p::Connect;
use xtra_libp2p::Disconnect;
use xtra_libp2p::GetConnectionStats;
use xtra_libp2p::ListenOn;
use xtra_libp2p::NewInboundSubstream;
use xtra_libp2p::OpenSubstream;
use xtra_productivity::xtra_productivity;

mod util;

#[tokio::test]
async fn hello_world() {
    let alice_hello_world_handler = HelloWorld::default().create(None).spawn_global();
    let (alice, bob, _) = alice_and_bob(
        [(
            "/hello-world/1.0.0",
            alice_hello_world_handler.clone().into(),
        )],
        [],
    )
    .await;

    let bob_to_alice = bob
        .endpoint
        .send(OpenSubstream::single_protocol(
            alice.peer_id,
            "/hello-world/1.0.0",
        ))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();

    let string = hello_world_dialer(bob_to_alice, "Bob").await.unwrap();

    assert_eq!(string, "Hello Bob!");
}

#[tokio::test]
async fn after_connect_see_each_other_as_connected() {
    let (alice, bob, _) = alice_and_bob([], []).await;

    let alice_stats = alice.endpoint.send(GetConnectionStats).await.unwrap();
    let bob_stats = bob.endpoint.send(GetConnectionStats).await.unwrap();

    assert_eq!(alice_stats.connected_peers, HashSet::from([bob.peer_id]));
    assert_eq!(bob_stats.connected_peers, HashSet::from([alice.peer_id]));
}

#[tokio::test]
async fn disconnect_is_reflected_in_stats() {
    let (alice, bob, _) = alice_and_bob([], []).await;

    alice.endpoint.send(Disconnect(bob.peer_id)).await.unwrap();

    let alice_stats = alice.endpoint.send(GetConnectionStats).await.unwrap();
    let bob_stats = bob.endpoint.send(GetConnectionStats).await.unwrap();

    assert_eq!(alice_stats.connected_peers, HashSet::from([]));
    assert_eq!(bob_stats.connected_peers, HashSet::from([]));
}

#[tokio::test]
async fn subscriber_stats_track_listen_addresses_properly() {
    let alice = make_node([]);

    let port = rand::random::<u16>();
    let alice_listen = format!("/memory/{port}").parse::<Multiaddr>().unwrap();

    let alice_listen_addresses = alice
        .subscriber_stats
        .send(GetListenAddresses)
        .await
        .unwrap();

    assert!(
        alice_listen_addresses.is_empty(),
        "Endpoint was not configured to listen yet"
    );

    alice
        .endpoint
        .send(ListenOn(alice_listen.clone()))
        .await
        .unwrap();

    let alice_listen_addresses = alice
        .subscriber_stats
        .send(GetListenAddresses)
        .await
        .unwrap();

    assert_eq!(
        alice_listen_addresses,
        HashSet::from([alice_listen]),
        "Alice endpoint is configured for a single address",
    );
}

#[tokio::test]
async fn subscriber_stats_track_connected_peers_properly() {
    let alice = make_node([]);
    let bob = make_node([]);

    let port = rand::random::<u16>();
    let alice_listen = format!("/memory/{port}").parse::<Multiaddr>().unwrap();

    alice
        .endpoint
        .send(ListenOn(alice_listen.clone()))
        .await
        .unwrap();

    let alice_peer_id = &alice.peer_id;

    let alice_connected_peers = alice
        .subscriber_stats
        .send(GetConnectedPeers)
        .await
        .unwrap();
    assert!(alice_connected_peers.is_empty());

    let bob_connected_peers = alice
        .subscriber_stats
        .send(GetConnectedPeers)
        .await
        .unwrap();
    assert!(bob_connected_peers.is_empty());

    bob.endpoint
        .send(Connect(
            format!("/memory/{port}/p2p/{alice_peer_id}")
                .parse()
                .unwrap(),
        ))
        .await
        .unwrap()
        .unwrap();

    // Peers should be connected to each other now

    let alice_connected_peers = alice
        .subscriber_stats
        .send(GetConnectedPeers)
        .await
        .unwrap();
    assert_eq!(alice_connected_peers, HashSet::from([bob.peer_id]),);

    let bob_connected_peers = bob.subscriber_stats.send(GetConnectedPeers).await.unwrap();
    assert_eq!(bob_connected_peers, HashSet::from([alice.peer_id]),);

    bob.endpoint.send(Disconnect(*alice_peer_id)).await.unwrap();

    let alice_connected_peers = alice
        .subscriber_stats
        .send(GetConnectedPeers)
        .await
        .unwrap();
    assert!(alice_connected_peers.is_empty());

    let bob_connected_peers = alice
        .subscriber_stats
        .send(GetConnectedPeers)
        .await
        .unwrap();
    assert!(bob_connected_peers.is_empty());

    // Peers should be disconnected now
}

#[tokio::test]
async fn listen_address_is_reflected_in_stats() {
    let (alice, _, listen_address) = alice_and_bob([], []).await;

    let alice_stats = alice.endpoint.send(GetConnectionStats).await.unwrap();

    assert_eq!(
        alice_stats.listen_addresses,
        HashSet::from([listen_address])
    );
}

#[tokio::test]
async fn cannot_open_substream_for_unhandled_protocol() {
    let (alice, bob, _) = alice_and_bob([], []).await;

    let error = alice
        .endpoint
        .send(OpenSubstream::single_protocol(
            bob.peer_id,
            "/foo/bar/1.0.0",
        ))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        xtra_libp2p::Error::NegotiationFailed(xtra_libp2p::NegotiationError::Failed)
    ))
}

#[tokio::test]
async fn cannot_connect_twice() {
    let (alice, bob, alice_listen) = alice_and_bob([], []).await;

    let error = bob
        .endpoint
        .send(Connect(
            alice_listen.with(Protocol::P2p(alice.peer_id.into())),
        ))
        .await
        .unwrap()
        .unwrap_err();

    assert!(matches!(
        error,
        xtra_libp2p::Error::AlreadyTryingToConnected(twin) if twin == alice.peer_id
    ))
}

#[tokio::test]
async fn chooses_first_protocol_in_list_of_multiple() {
    let alice_hello_world_handler = HelloWorld::default().create(None).spawn_global();
    let (alice, bob, _) = alice_and_bob(
        [(
            "/hello-world/1.0.0",
            alice_hello_world_handler.clone().into(),
        )],
        [],
    )
    .await;

    let (actual_protocol, _) = bob
        .endpoint
        .send(OpenSubstream::multiple_protocols(
            alice.peer_id,
            vec![
                "/hello-world/1.0.0",
                "/foo-bar/1.0.0", // This is unsupported by Alice.
            ],
        ))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();

    assert_eq!(actual_protocol, "/hello-world/1.0.0");
}

#[cfg_attr(debug_assertions, tokio::test)] // The assertion for duplicate handlers only runs in debug mode.
#[should_panic(expected = "Duplicate handler declared for protocol /hello-world/1.0.0")]
async fn disallow_duplicate_handlers() {
    let hello_world_handler = HelloWorld::default().create(None).spawn_global();

    make_node([
        ("/hello-world/1.0.0", hello_world_handler.clone().into()),
        ("/hello-world/1.0.0", hello_world_handler.into()),
    ]);
}

#[tokio::test]
async fn falls_back_to_next_protocol_if_unsupported() {
    let alice_hello_world_handler = HelloWorld::default().create(None).spawn_global();
    let (alice, bob, _) = alice_and_bob(
        [(
            "/hello-world/1.0.0",
            alice_hello_world_handler.clone().into(),
        )],
        [],
    )
    .await;

    let (actual_protocol, _) = bob
        .endpoint
        .send(OpenSubstream::multiple_protocols(
            alice.peer_id,
            vec![
                "/foo-bar/1.0.0", // This is unsupported by Alice.
                "/hello-world/1.0.0",
            ],
        ))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();

    assert_eq!(actual_protocol, "/hello-world/1.0.0");
}

async fn alice_and_bob<const AN: usize, const BN: usize>(
    alice_inbound_substream_handlers: [(&'static str, MessageChannel<NewInboundSubstream, ()>); AN],
    bob_inbound_substream_handlers: [(&'static str, MessageChannel<NewInboundSubstream, ()>); BN],
) -> (Node, Node, Multiaddr) {
    let port = rand::random::<u16>();

    let alice = make_node(alice_inbound_substream_handlers);
    let bob = make_node(bob_inbound_substream_handlers);

    let alice_listen = format!("/memory/{port}").parse::<Multiaddr>().unwrap();

    alice
        .endpoint
        .send(ListenOn(alice_listen.clone()))
        .await
        .unwrap();

    let alice_peer_id = &alice.peer_id;
    bob.endpoint
        .send(Connect(
            format!("/memory/{port}/p2p/{alice_peer_id}")
                .parse()
                .unwrap(),
        ))
        .await
        .unwrap()
        .unwrap();

    (alice, bob, alice_listen)
}

/// A test actor subscribing to all the notifications
#[derive(Default)]
struct EndpointSubscriberStats {
    connected_peers: HashSet<PeerId>,
    listen_addresses: HashSet<Multiaddr>,
}

#[async_trait]
impl Actor for EndpointSubscriberStats {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}

#[xtra_productivity]
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

#[derive(Default)]
struct HelloWorld;

#[xtra_productivity]
impl HelloWorld {
    async fn handle(&mut self, msg: NewInboundSubstream, ctx: &mut Context<Self>) {
        tracing::info!("New hello world stream from {}", msg.peer);

        tokio_extras::spawn_fallible(
            &ctx.address().unwrap(),
            hello_world_listener(msg.stream),
            move |e| async move {
                tracing::warn!("Hello world protocol with peer {} failed: {}", msg.peer, e);
            },
        );
    }
}

#[async_trait]
impl Actor for HelloWorld {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}

async fn hello_world_dialer(stream: xtra_libp2p::Substream, name: &'static str) -> Result<String> {
    let mut stream = asynchronous_codec::Framed::new(stream, asynchronous_codec::LengthCodec);

    stream.send(Bytes::from(name)).await?;
    let bytes = stream.next().await.context("Expected message")??;
    let message = String::from_utf8(bytes.to_vec())?;

    Ok(message)
}

async fn hello_world_listener(stream: xtra_libp2p::Substream) -> Result<()> {
    let mut stream =
        asynchronous_codec::Framed::new(stream, asynchronous_codec::LengthCodec).fuse();

    let bytes = stream.select_next_some().await?;
    let name = String::from_utf8(bytes.to_vec())?;

    stream.send(Bytes::from(format!("Hello {name}!"))).await?;

    Ok(())
}
