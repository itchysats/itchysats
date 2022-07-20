use crate::noise;
use crate::setup_taker;
use crate::version;
use crate::wire;
use crate::wire::EncryptedJsonCodec;
use crate::wire::Version;
use crate::ConnectionStatus;
use crate::Environment;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use futures::SinkExt;
use futures::StreamExt;
use futures::TryStreamExt;
use model::libp2p::PeerId;
use model::Identity;
use model::Leverage;
use model::OrderId;
use model::Usd;
use rand::thread_rng;
use rand::Rng;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_extras::time::already_instrumented;
use tokio_extras::FutureExt;
use tokio_extras::Tasks;
use tokio_util::codec::Framed;
use tracing::Instrument;
use xtra_productivity::xtra_productivity;
use xtras::address_map::NotConnected;
use xtras::AddressMap;

/// Time between reconnection attempts
pub const MAX_RECONNECT_INTERVAL_SECONDS: u64 = 60;

const TCP_TIMEOUT: Duration = Duration::from_secs(10);

/// The "Connected" state of our connection with the maker.
#[allow(clippy::large_enum_variant)]
enum State {
    Connected {
        write: wire::Write<wire::MakerToTaker, wire::TakerToMaker>,
        _tasks: Tasks,
    },
    Disconnected,
}

impl State {
    async fn send(&mut self, msg: wire::TakerToMaker) -> Result<()> {
        let msg_str = msg.name();

        let write = match self {
            State::Connected { write, .. } => write,
            State::Disconnected => {
                bail!("Cannot send {msg_str}, not connected to maker");
            }
        };

        match msg.order_id() {
            Some(order_id) => {
                tracing::trace!(target: "wire", msg_name = msg_str, %order_id, "Sending")
            }
            None => tracing::trace!(target: "wire", msg_name = msg_str, "Sending"),
        };

        write
            .send(msg)
            .await
            .with_context(|| format!("Failed to send message {msg_str} to maker"))?;

        Ok(())
    }
}

pub struct Actor {
    identity_sk: x25519_dalek::StaticSecret,
    peer_id: PeerId,
    connect_timeout: Duration,
    state: State,
    setup_actors: AddressMap<OrderId, setup_taker::Actor>,
    environment: Environment,
}

#[derive(Clone, Copy)]
pub struct Connect {
    pub maker_identity: Identity,
    pub maker_addr: SocketAddr,
}

pub struct MakerStreamMessage {
    pub item: Result<wire::MakerToTaker>,
}

/// Message sent from the `setup_taker::Actor` to the
/// `connection::Actor` so that it can forward it to the maker.
///
/// Additionally, the address of this instance of the
/// `setup_taker::Actor` is included so that the `connection::Actor`
/// knows where to forward the contract setup messages from the maker
/// about this particular order.
pub struct TakeOrder {
    pub order_id: OrderId,
    pub quantity: Usd,
    pub leverage: Leverage,
    pub address: xtra::Address<setup_taker::Actor>,
}

impl Actor {
    pub fn new(
        identity_sk: x25519_dalek::StaticSecret,
        peer_id: PeerId,
        connect_timeout: Duration,
        environment: Environment,
    ) -> Self {
        Self {
            identity_sk,
            state: State::Disconnected,
            setup_actors: AddressMap::default(),
            connect_timeout,
            peer_id,
            environment,
        }
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle_taker_to_maker(&mut self, message: wire::TakerToMaker) {
        if let Err(e) = self.state.send(message).await {
            tracing::warn!("{:#}", e);
        }
    }

    async fn handle_take_order(&mut self, msg: TakeOrder) -> Result<()> {
        self.state
            .send(wire::TakerToMaker::TakeOrder {
                order_id: msg.order_id,
                quantity: msg.quantity,
                leverage: msg.leverage,
            })
            .await?;

        self.setup_actors.insert(msg.order_id, msg.address);

        Ok(())
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle_connect(
        &mut self,
        Connect {
            maker_addr,
            maker_identity,
        }: Connect,
        ctx: &mut xtra::Context<Self>,
    ) -> Result<()> {
        tracing::debug!(address = %maker_addr, "Connecting to maker");

        let (mut write, mut read) = {
            let mut connection = TcpStream::connect(&maker_addr)
                .timeout(self.connect_timeout, || {
                    tracing::debug_span!("TcpStream connect")
                })
                .await
                .with_context(|| {
                    let seconds = self.connect_timeout.as_secs();

                    format!("Connection attempt to {maker_addr} timed out after {seconds}s",)
                })?
                .with_context(|| format!("Failed to connect to {maker_addr}"))?;
            let noise = noise::initiator_handshake(
                &mut connection,
                &self.identity_sk,
                &maker_identity.pk(),
            )
            .timeout(TCP_TIMEOUT, already_instrumented)
            .await??;

            Framed::new(connection, EncryptedJsonCodec::new(noise)).split()
        };

        let proposed_version = Version::LATEST;
        write
            .send(wire::TakerToMaker::HelloV4 {
                proposed_wire_version: proposed_version.clone(),
                daemon_version: version::version().to_string(),
                peer_id: self.peer_id,
                environment: self.environment.into(),
            })
            .instrument(tracing::debug_span!("send hello"))
            .timeout(TCP_TIMEOUT, already_instrumented)
            .await??;

        match read
            .try_next()
            .timeout(TCP_TIMEOUT, already_instrumented)
            .await
            .with_context(|| {
                format!(
                    "Maker {maker_identity} did not send Hello within 10 seconds, dropping connection"
                )
            })?
            .with_context(|| format!("Failed to read first message from maker {maker_identity}"))? {
            Some(wire::MakerToTaker::Hello(actual_version)) => {
                tracing::info!(%maker_identity, %actual_version, "Received Hello message from maker");
                if proposed_version != actual_version {
                    bail!(
                        "Network version mismatch, we proposed {proposed_version} but maker wants to use {actual_version}"
                    )
                }
            }
            Some(unexpected_message) => {
                bail!(
                    "Unexpected message {} from maker {maker_identity}", unexpected_message.name()
                )
            }
            None => {
                bail!(
                    "Connection to maker {maker_identity} closed before receiving first message"
                )
            }
        }

        tracing::info!(address = %maker_addr, "Established connection to maker");

        let this = ctx.address().expect("self to be alive");
        let weak = this.downgrade();

        let sink = futures::sink::unfold((), move |(), message| {
            this.send(message)
                .instrument(tracing::debug_span!("Forward MakerStreamMessage"))
        });
        let mut tasks = Tasks::default();
        tasks.add_fallible(
            read.map(move |item| Ok(MakerStreamMessage { item }))
                .forward(sink),
            move |e| async move {
                tracing::warn!(address = ?weak, "Error forwarding MakerStreamMessage: {:#}", e);
            },
        );

        self.state = State::Connected {
            write,
            _tasks: tasks,
        };
        Ok(())
    }

    async fn handle_wire_message(&mut self, message: MakerStreamMessage) {
        let msg = match message.item {
            Ok(msg) => msg,
            Err(e) => {
                tracing::warn!("Error while receiving message from maker: {:#}", e);
                return;
            }
        };

        let msg_name = msg.name();

        match msg.order_id() {
            Some(order_id) => tracing::trace!(target: "wire", msg_name, %order_id, "Received"),
            None => tracing::trace!(target: "wire", msg_name, "Received"),
        }

        match msg {
            wire::MakerToTaker::Heartbeat => {
                tracing::trace!("legacy heartbeat handler - use libp2p instead");
            }
            wire::MakerToTaker::ConfirmOrder(order_id) => {
                if let Err(NotConnected(_)) = self
                    .setup_actors
                    .send_async(&order_id, setup_taker::Accepted)
                    .await
                {
                    tracing::warn!(%order_id, "No active setup actor");
                }
            }
            wire::MakerToTaker::RejectOrder(order_id) => {
                if let Err(NotConnected(_)) = self
                    .setup_actors
                    .send_async(&order_id, setup_taker::Rejected::without_reason())
                    .await
                {
                    tracing::warn!(%order_id, "No active setup actor");
                }
            }
            wire::MakerToTaker::Protocol { order_id, msg } => {
                if let Err(NotConnected(_)) = self.setup_actors.send_async(&order_id, msg).await {
                    tracing::warn!(%order_id, "No active setup actor");
                }
            }
            wire::MakerToTaker::InvalidOrderId(order_id) => {
                if let Err(NotConnected(_)) = self
                    .setup_actors
                    .send_async(&order_id, setup_taker::Rejected::invalid_order_id())
                    .await
                {
                    tracing::warn!(%order_id, "No active setup actor");
                }
            }
            wire::MakerToTaker::Settlement { .. } => {
                tracing::error!("legacy handler - use libp2p instead");
            }
            wire::MakerToTaker::ConfirmRollover { .. } => {
                tracing::error!("legacy handler - use libp2p rollover instead")
            }
            wire::MakerToTaker::RejectRollover(_) => {
                tracing::error!("legacy handler - use libp2p rollover instead")
            }
            wire::MakerToTaker::RolloverProtocol { .. } => {
                tracing::error!("legacy handler - use libp2p rollover instead")
            }
            wire::MakerToTaker::CurrentOffers(_) => {
                // The maker is still sending this because there is not decision logic to stop it
                tracing::trace!(
                    "ignoring legacy offers - using `/itchysats/offer` libp2p protocol instead"
                )
            }
            wire::MakerToTaker::CurrentOrder(_) => {
                // no-op, we support `CurrentOffers` message and can ignore this one.
            }
            wire::MakerToTaker::Hello(_) => {
                tracing::warn!("Ignoring unexpected Hello message from maker. Hello is only expected when opening a new connection.")
            }
            wire::MakerToTaker::Unknown => {
                // Ignore unknown message to be forwards-compatible. We are logging it above on
                // `trace` level already.
            }
        }
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}

// TODO: Move the reconnection logic inside the connection::Actor instead of
// depending on a watch channel
#[tracing::instrument(skip_all)]
pub async fn connect(
    mut maker_online_status_feed_receiver: watch::Receiver<ConnectionStatus>,
    connection_actor_addr: xtra::Address<Actor>,
    maker_identity: Identity,
    maker_addresses: Vec<SocketAddr>,
) {
    loop {
        maker_online_status_feed_receiver
            .changed()
            .await
            .expect("watch channel should outlive the future");

        let connection_status = *maker_online_status_feed_receiver.borrow();
        if matches!(connection_status, ConnectionStatus::Offline) {
            tracing::debug!("No connection to the maker");
            'connect: loop {
                for address in &maker_addresses {
                    let connect_msg = Connect {
                        maker_identity,
                        maker_addr: *address,
                    };

                    if let Err(e) = connection_actor_addr
                        .send(connect_msg)
                        .await
                        .expect("Taker actor to be present")
                    {
                        tracing::warn!(%address, "Failed to establish connection: {:#}", e);
                        continue;
                    }
                    break 'connect;
                }

                let num_addresses = maker_addresses.len();

                // Apply a random number of seconds between the reconnection
                // attempts so that all takers don't try to reconnect at the same time
                let seconds = thread_rng().gen_range(5, MAX_RECONNECT_INTERVAL_SECONDS);

                tracing::warn!(
                    "Tried connecting to {num_addresses} addresses without success, retrying in {seconds} seconds",
                );

                tokio_extras::time::sleep(Duration::from_secs(seconds)).await;
            }
        }
    }
}
