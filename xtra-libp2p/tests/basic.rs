use crate::hello_world::hello_world_dialer;
use crate::hello_world::HelloWorld;
use libp2p_core::multiaddr::Protocol;
use libp2p_core::Multiaddr;
use std::collections::HashSet;
use util::make_node;
use util::GetConnectedPeers;
use util::GetListenAddresses;
use util::Node;
use xtra::message_channel::MessageChannel;
use xtra::spawn::TokioGlobalSpawnExt;
use xtra::Actor;
use xtra_libp2p::Connect;
use xtra_libp2p::Disconnect;
use xtra_libp2p::GetConnectionStats;
use xtra_libp2p::ListenOn;
use xtra_libp2p::NewInboundSubstream;
use xtra_libp2p::OpenSubstream;

mod hello_world;
mod util;

#[tokio::test]
async fn hello_world() {
    let alice_hello_world_handler = HelloWorld::default().create(None).spawn_global();
    let (alice, bob, _) = alice_and_bob(
        [(
            hello_world::PROTOCOL_NAME,
            alice_hello_world_handler.clone().into(),
        )],
        [],
    )
    .await;

    let bob_to_alice = bob
        .endpoint
        .send(OpenSubstream::single_protocol(
            alice.peer_id,
            hello_world::PROTOCOL_NAME,
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
            hello_world::PROTOCOL_NAME,
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
                hello_world::PROTOCOL_NAME,
                "/foo-bar/1.0.0", // This is unsupported by Alice.
            ],
        ))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();

    assert_eq!(actual_protocol, hello_world::PROTOCOL_NAME);
}

#[cfg_attr(debug_assertions, tokio::test)] // The assertion for duplicate handlers only runs in debug mode.
#[should_panic(expected = "Duplicate handler declared for protocol /hello-world/1.0.0")]
async fn disallow_duplicate_handlers() {
    let hello_world_handler = HelloWorld::default().create(None).spawn_global();

    make_node([
        (
            hello_world::PROTOCOL_NAME,
            hello_world_handler.clone().into(),
        ),
        (hello_world::PROTOCOL_NAME, hello_world_handler.into()),
    ]);
}

#[tokio::test]
async fn falls_back_to_next_protocol_if_unsupported() {
    let alice_hello_world_handler = HelloWorld::default().create(None).spawn_global();
    let (alice, bob, _) = alice_and_bob(
        [(
            hello_world::PROTOCOL_NAME,
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
                hello_world::PROTOCOL_NAME,
            ],
        ))
        .await
        .unwrap()
        .unwrap()
        .await
        .unwrap();

    assert_eq!(actual_protocol, hello_world::PROTOCOL_NAME);
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
