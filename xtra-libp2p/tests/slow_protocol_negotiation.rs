use anyhow::Result;
use async_trait::async_trait;
use hello_world::hello_world_dialer;
use hello_world::HelloWorld;
use libp2p_core::Multiaddr;
use libp2p_core::PeerId;
use util::make_node;
use util::Node;
use xtra::spawn::TokioGlobalSpawnExt;
use xtra::Actor;
use xtra_libp2p::Connect;
use xtra_libp2p::ListenOn;
use xtra_libp2p::NewInboundSubstream;
use xtra_libp2p::OpenSubstream;
use xtra_productivity::xtra_productivity;

mod hello_world;
mod util;

#[tokio::test]
async fn hanging_during_protocol_negotation_does_not_block_endpoint() {
    let alice = make_node([]);
    let alice_port = listen_on_random_port(&alice).await.unwrap();

    // Sloan has an endpoint which will get stuck when trying to
    // negotiate a protocol
    let infinite_negotiation_handler = InfiniteNegotiation.create(None).spawn_global();
    let slow_sloan = make_node([("/infinite-negotiation", infinite_negotiation_handler.into())]);
    connect_to_peer(&slow_sloan, alice.peer_id, alice_port)
        .await
        .unwrap();

    // Alice tries to negotiate a protocol with Sloan in the
    // background. This will never resolve in time!
    #[allow(clippy::disallowed_methods)]
    tokio::spawn({
        let alice = alice.clone();
        async move {
            let _stream = alice
                .endpoint
                .send(OpenSubstream::single_protocol(
                    slow_sloan.peer_id,
                    "/infinite-negotiation",
                ))
                .await
                .unwrap()
                .unwrap()
                .await;

            unreachable!("Protocol can never be negotiated in time")
        }
    });

    // In the meantime, Alice connects to a lot of Bobs to run the
    // hello-world protocol
    for _ in 0..1000 {
        let bob_handler = HelloWorld::default().create(None).spawn_global();
        let bob = make_node([(hello_world::PROTOCOL_NAME, bob_handler.clone().into())]);

        connect_to_peer(&bob, alice.peer_id, alice_port)
            .await
            .unwrap();

        let alice_to_bob = alice
            .endpoint
            .send(OpenSubstream::single_protocol(
                bob.peer_id,
                hello_world::PROTOCOL_NAME,
            ))
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap();

        let string = hello_world_dialer(alice_to_bob, "Alice").await.unwrap();

        assert_eq!(string, "Hello Alice!");
    }

    // If we succeed, it means that we were able to use the `Endpoint`
    // whilst waiting for a protocol negotiation to finish
}

async fn listen_on_random_port(node: &Node) -> Result<u16> {
    let port = rand::random::<u16>();
    let listen_address = format!("/memory/{port}").parse::<Multiaddr>()?;

    node.endpoint.send(ListenOn(listen_address)).await?;

    Ok(port)
}

async fn connect_to_peer(node: &Node, peer_id: PeerId, port: u16) -> Result<()> {
    node.endpoint
        .send(Connect(format!("/memory/{port}/p2p/{peer_id}").parse()?))
        .await??;

    Ok(())
}

struct InfiniteNegotiation;

#[async_trait]
impl Actor for InfiniteNegotiation {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}

#[xtra_productivity]
impl InfiniteNegotiation {
    fn handle(&mut self, _: NewInboundSubstream) {}
}
