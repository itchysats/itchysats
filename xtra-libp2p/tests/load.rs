use crate::util::make_node;
use crate::util::EndpointSubscriberStats;
use anyhow::Context as _;
use anyhow::Result;
use async_trait::async_trait;
use asynchronous_codec::Bytes;
use futures::SinkExt;
use futures::StreamExt;
use libp2p_core::identity::Keypair;
use libp2p_core::transport::MemoryTransport;
use libp2p_core::Multiaddr;
use std::time::Duration;
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::util::SubscriberInitExt;
use xtra::message_channel::MessageChannel;
use xtra::spawn::TokioGlobalSpawnExt;
use xtra::Actor;
use xtra::Address;
use xtra::Context;
use xtra_libp2p::endpoint::ConnectionEstablished;
use xtra_libp2p::endpoint::Subscribers;
use xtra_libp2p::Connect;
use xtra_libp2p::Endpoint;
use xtra_libp2p::ListenOn;
use xtra_libp2p::NewInboundSubstream;
use xtra_libp2p::OpenSubstream;
use xtra_productivity::xtra_productivity;
use xtras::SendAsyncSafe;

mod util;

#[tokio::test]
#[ignore]
async fn given_single_protocol_when_one_protocol_run_then_alice_handles_1000_bobs() {
    const BOBS: usize = 1000;
    const PROTOCOLS: usize = 1;
    const PROTOCOL_RUNS_PER_BOB: u64 = 1;
    const TEST_TIMEOUT_SECS: u64 = 30;

    test_runner::<BOBS, PROTOCOLS, PROTOCOL_RUNS_PER_BOB, TEST_TIMEOUT_SECS>().await;
}

#[tokio::test]
#[ignore]
async fn given_single_protocol_when_multiple_protocol_run_then_alice_handles_100_bobs_with_100_protocol_runs(
) {
    // Note: It can actually handle 1000 bobs x 1000 runs, but that takes significant time

    const BOBS: usize = 100;
    const PROTOCOLS: usize = 1;
    const PROTOCOL_RUNS_PER_BOB: u64 = 100;
    const TEST_TIMEOUT_SECS: u64 = 120;

    test_runner::<BOBS, PROTOCOLS, PROTOCOL_RUNS_PER_BOB, TEST_TIMEOUT_SECS>().await;
}

#[tokio::test]
#[ignore]
async fn given_multiple_protocol_when_one_protocol_run_then_alice_handles_1000_bobs() {
    const BOBS: usize = 1000;
    const PROTOCOLS: usize = BOBS; // One distinct protocol per Bob and distinct protocol handlers for each protocol on Alice
    const PROTOCOL_RUNS_PER_BOB: u64 = 1;
    const TEST_TIMEOUT_SECS: u64 = 40;

    test_runner::<BOBS, PROTOCOLS, PROTOCOL_RUNS_PER_BOB, TEST_TIMEOUT_SECS>().await;
}

async fn test_runner<
    const BOBS: usize,
    const PROTOCOLS: usize,
    const PROTOCOL_RUNS_PER_BOB: u64,
    const TEST_TIMEOUT_SECS: u64,
>() {
    init_tracing();

    // Currently only supports either one protocol for all bobs or one protocol per bob
    assert!(PROTOCOLS == 1 || PROTOCOLS == BOBS);

    const PROTOCOL_NAME: &str = "foo";

    let mut protocols: Vec<String> = Vec::new();
    let mut alice_inbound_substream_handlers: Vec<(
        &'static str,
        MessageChannel<NewInboundSubstream, ()>,
    )> = Vec::new();
    let mut alice_protocol_actors: Vec<Address<SomeMessageExchangeListener>> = Vec::new();

    // create one protocol for each Bob (including specific handler for alice for that protocol)
    for index in 0..PROTOCOLS {
        let protocol = format!("/{PROTOCOL_NAME}-{index}/1.0.0");

        let alice_protocol_handler = SomeMessageExchangeListener::new(protocol.to_string())
            .create(None)
            .spawn_global();

        alice_inbound_substream_handlers.push((
            Box::leak(protocol.clone().into_boxed_str()),
            alice_protocol_handler.clone().into(),
        ));
        alice_protocol_actors.push(alice_protocol_handler);
        protocols.push(protocol);
    }

    let alice_inbound_substream_handlers = into_arr::<
        (&'static str, MessageChannel<NewInboundSubstream, ()>),
        PROTOCOLS,
    >(alice_inbound_substream_handlers);

    let port = rand::random::<u16>();

    let alice = make_node(alice_inbound_substream_handlers);
    let alice_listen = format!("/memory/{port}").parse::<Multiaddr>().unwrap();
    alice
        .endpoint
        .send(ListenOn(alice_listen.clone()))
        .await
        .unwrap();
    let alice_peer_id = alice.peer_id;

    // Give Alice some time to start up and listen
    #[allow(clippy::disallowed_methods)]
    tokio::time::sleep(Duration::from_secs(1)).await;

    for bob in 0..BOBS {
        let protocol_name = if PROTOCOLS == 1 {
            format!("/{PROTOCOL_NAME}-0/1.0.0")
        } else {
            format!("/{PROTOCOL_NAME}-{bob}/1.0.0")
        };

        let dialer_tag = format!("Bob{}", bob);
        let dialer = async move {
            let (endpoint_addr, endpoint_context) = xtra::Context::new(None);

            let dialer_actor = SomeMessageExchangeDialer::new(
                dialer_tag,
                Box::leak(protocol_name.clone().into_boxed_str()),
                endpoint_addr.clone(),
                PROTOCOL_RUNS_PER_BOB,
            )
            .create(None)
            .spawn_global();
            let id = Keypair::generate_ed25519();

            let subscriber_stats = EndpointSubscriberStats::default()
                .create(None)
                .spawn_global();

            let endpoint = Endpoint::new(
                Box::new(MemoryTransport::default),
                id,
                Duration::from_secs(20),
                [],
                Subscribers::new(
                    vec![subscriber_stats.clone().into(), dialer_actor.clone().into()],
                    vec![subscriber_stats.clone().into()],
                    vec![subscriber_stats.clone().into()],
                    vec![subscriber_stats.clone().into()],
                ),
            );

            #[allow(clippy::disallowed_methods)]
            tokio::task::spawn(endpoint_context.run(endpoint));

            endpoint_addr
                .send(Connect(
                    format!("/memory/{port}/p2p/{alice_peer_id}")
                        .parse()
                        .unwrap(),
                ))
                .await
                .unwrap()
                .unwrap();
        };

        #[allow(clippy::disallowed_methods)]
        tokio::task::spawn(dialer);
    }

    let ensure_listener_done = {
        let check_listener_done_count = if PROTOCOLS == 1 {
            BOBS as u64 * PROTOCOL_RUNS_PER_BOB
        } else {
            PROTOCOL_RUNS_PER_BOB
        };

        async move {
            loop {
                let mut done = true;
                for actor in &alice_protocol_actors {
                    let actor_done = actor
                        .send(CheckListenerDone(check_listener_done_count))
                        .await
                        .unwrap();

                    if !actor_done {
                        done = false;
                    }
                }

                if done {
                    break;
                }

                #[allow(clippy::disallowed_methods)]
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    };

    tokio::time::timeout(Duration::from_secs(TEST_TIMEOUT_SECS), ensure_listener_done)
        .await
        .unwrap();
}

struct SomeMessageExchangeDialer {
    dialer_tag: String,
    protocol: &'static str,
    endpoint: Address<Endpoint>,
    protocol_trigger_times: u64,
}

impl SomeMessageExchangeDialer {
    pub fn new(
        dialer_tag: String,
        protocol: &'static str,
        endpoint: Address<Endpoint>,
        protocol_trigger_times: u64,
    ) -> Self {
        Self {
            dialer_tag,
            protocol,
            endpoint,
            protocol_trigger_times,
        }
    }
}

#[xtra_productivity(message_impl = false)]
impl SomeMessageExchangeDialer {
    async fn handle(&mut self, msg: ConnectionEstablished, ctx: &mut Context<Self>) {
        tracing::info!("{}: Connection established", self.dialer_tag);

        for trigger_time in 0..self.protocol_trigger_times {
            let future = {
                let peer = msg.peer;
                let endpoint = self.endpoint.clone();
                let protocol = self.protocol;
                let trigger_time = trigger_time;
                async move {
                    tracing::info!(
                        "Dialer staring trigger time {trigger_time} for protocol {protocol}"
                    );
                    let bob_to_alice = endpoint
                        .send(OpenSubstream::single_protocol(peer, protocol))
                        .await
                        .unwrap()
                        .unwrap();

                    some_message_exchange_dialer(bob_to_alice).await.unwrap();

                    tracing::info!(
                        "Dialer finished trigger time {trigger_time} for protocol {protocol}"
                    );
                    anyhow::Ok(())
                }
            };

            let this = ctx.address().expect("self to be alive");
            tokio_extras::spawn_fallible(&this, future, move |e| async move {
                tracing::error!("Dialer failed to spawn future: {e:#}");
            });
        }
    }
}

#[async_trait]
impl Actor for SomeMessageExchangeDialer {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}

struct SomeMessageExchangeListener {
    protocol: String,
    done_times_count: u64,
}

impl SomeMessageExchangeListener {
    pub fn new(protocol: String) -> Self {
        Self {
            protocol,
            done_times_count: 0,
        }
    }
}

#[xtra_productivity(message_impl = false)]
impl SomeMessageExchangeListener {
    async fn handle(&mut self, msg: NewInboundSubstream, ctx: &mut Context<Self>) {
        tracing::info!("{} handling NewInboundSubstream", self.protocol);

        let this = ctx.address().expect("self to be alive");

        let protocol = self.protocol.clone();
        let future = {
            let this = this.clone();
            async move {
                some_message_exchange_listener(msg.stream).await?;
                this.send_async_safe(ListenerDone).await?;

                tracing::info!("{} sent ListenerDone", protocol);

                anyhow::Ok(())
            }
        };

        tokio_extras::spawn_fallible(&this, future, move |e| async move {
            tracing::warn!("Parallel message with peer {} failed: {}", msg.peer, e);
        });
    }
}

#[xtra_productivity]
impl SomeMessageExchangeListener {
    async fn handle(&mut self, _: ListenerDone) {
        self.done_times_count += 1;

        tracing::info!(
            "{} received ListenerDone for trigger time {}",
            self.protocol,
            self.done_times_count
        );
    }

    async fn handle(&mut self, check_done: CheckListenerDone) -> bool {
        self.done_times_count == check_done.0
    }
}

#[async_trait]
impl Actor for SomeMessageExchangeListener {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}

struct ListenerDone;
struct CheckListenerDone(u64);

pub fn into_arr<T, const N: usize>(v: Vec<T>) -> [T; N] {
    v.try_into()
        .unwrap_or_else(|v: Vec<T>| panic!("Expected a Vec of length {} but it was {}", N, v.len()))
}

async fn some_message_exchange_dialer(stream: xtra_libp2p::Substream) -> Result<()> {
    let mut stream =
        asynchronous_codec::Framed::new(stream, asynchronous_codec::LengthCodec).fuse();

    stream.send(Bytes::from("Dialer")).await?;
    let bytes = stream
        .select_next_some()
        .await
        .context("Expected message")?;
    let message = String::from_utf8(bytes.to_vec())?;

    assert_eq!(message, "Listener");

    Ok(())
}

async fn some_message_exchange_listener(stream: xtra_libp2p::Substream) -> Result<()> {
    let mut stream =
        asynchronous_codec::Framed::new(stream, asynchronous_codec::LengthCodec).fuse();

    let bytes = stream.select_next_some().await?;
    let name = String::from_utf8(bytes.to_vec())?;

    stream.send(Bytes::from("Listener")).await?;

    assert_eq!(name, "Dialer");

    Ok(())
}

pub fn init_tracing() -> DefaultGuard {
    tracing_subscriber::fmt()
        .with_env_filter("WARN")
        .with_env_filter("load=debug")
        .with_env_filter("xtra=debug")
        .with_env_filter("xtra_libp2p=debug")
        .with_env_filter("xtras=debug")
        .with_test_writer()
        .set_default()
}
