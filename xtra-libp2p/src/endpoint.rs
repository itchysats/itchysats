use crate::multiaddress_ext::MultiaddrExt as _;
use crate::upgrade;
use crate::Connection;
use crate::Substream;
use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::AsyncRead;
use futures::AsyncWrite;
use futures::StreamExt;
use futures::TryStreamExt;
use libp2p_core::identity::Keypair;
use libp2p_core::transport::Boxed;
use libp2p_core::transport::ListenerEvent;
use libp2p_core::Multiaddr;
use libp2p_core::Negotiated;
use libp2p_core::PeerId;
use libp2p_core::Transport;
use multistream_select::NegotiationError;
use multistream_select::Version;
use std::collections::HashMap;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::pin::Pin;
use std::time::Duration;
use thiserror::Error;
use tokio_extras::Tasks;
use tracing::instrument;
use tracing::Instrument;
use xtra::message_channel::MessageChannel;
use xtra::Address;
use xtra::Context;
use xtra_productivity::xtra_productivity;
use xtras::SendAsyncNext;
use xtras::SendAsyncSafe;

/// An actor for managing multiplexed connections over a given transport thus representing an
/// _endpoint_.
///
/// The actor does not impose any policy on connection and/or protocol management.
/// New connections can be established by sending a [`Connect`] messages. Existing connections can
/// be disconnected by sending [`Disconnect`]. Listening for incoming connections is done by sending
/// a [`ListenOn`] message. To list the current state, send the [`GetConnectionStats`] message.
///
/// The combination of the above should make it possible to implement a fairly large number of
/// policies. For example, to maintain a connection to an another endpoint, you can regularly check
/// if the connection is still established by sending [`GetConnectionStats`] and react accordingly
/// (f.e. sending [`Connect`] in case the connection has disappeared).
///
/// Once a connection with a peer is established, both sides can open substreams on top of the
/// connection. Any incoming substream will - assuming the protocol is supported by the endpoint -
/// trigger a [`NewInboundSubstream`] message to the actor provided in the constructor.
/// Opening a new substream can be achieved by sending the [`OpenSubstream`] message.
pub struct Endpoint {
    transport_fn: Box<dyn Fn() -> Boxed<Connection> + Send + 'static>,
    controls: HashMap<PeerId, (yamux::Control, Tasks)>,
    inbound_substream_channels: HashMap<&'static str, MessageChannel<NewInboundSubstream, ()>>,
    listen_addresses: HashSet<Multiaddr>,
    inflight_connections: HashSet<PeerId>,
    connection_timeout: Duration,
    subscribers: Subscribers,
}

/// Open a substream to the provided peer.
///
/// Fails if we are not connected to the peer or the peer does not support any of the requested
/// protocols.
#[derive(Debug)]
pub struct OpenSubstream<P> {
    peer: PeerId,
    protocols: Vec<&'static str>,
    marker_num_protocols: PhantomData<P>,
}

/// Marker type denominating a single protocol.
#[derive(Clone, Copy, Debug)]
pub enum Single {}

/// Marker type denominating multiple protocols.
#[derive(Clone, Copy, Debug)]
pub enum Multiple {}

impl OpenSubstream<Single> {
    /// Constructs [`OpenSubstream`] with a single protocol.
    ///
    /// We will only attempt to negotiate the given protocol. If the endpoint does not speak this
    /// protocol, negotiation will fail.
    pub fn single_protocol(peer: PeerId, protocol: &'static str) -> Self {
        tracing::trace!("Open substream with peer {peer} on protocol {protocol}");

        Self {
            peer,
            protocols: vec![protocol],
            marker_num_protocols: PhantomData,
        }
    }
}

impl OpenSubstream<Multiple> {
    /// Constructs [`OpenSubstream`] with multiple protocols.
    ///
    /// The given protocols will be tried **in order**, with the first successful one being used.
    /// Specifying multiple protocols can useful to maintain backwards-compatibility. An endpoint
    /// can attempt to first establish a substream with a new protocol and falling back to older
    /// versions in case the new version is not supported.
    pub fn multiple_protocols(peer: PeerId, protocols: Vec<&'static str>) -> Self {
        tracing::trace!(
            "Open substream (multi protocol) with peer {peer} on protocol {protocols:?}"
        );
        Self {
            peer,
            protocols,
            marker_num_protocols: PhantomData,
        }
    }
}

/// Connect to the given [`Multiaddr`].
///
/// The address must contain a `/p2p` suffix.
/// Will fail if we are already connected to the peer.
#[derive(Debug)]
pub struct Connect(pub Multiaddr);

/// Disconnect from the given peer.
#[derive(Clone, Copy, Debug)]
pub struct Disconnect(pub PeerId);

/// Listen on the provided [`Multiaddr`].
///
/// For this to work, the [`Endpoint`] needs to be constructed with a compatible transport.
/// In other words, you cannot listen on a `/memory` address if you haven't configured a `/memory`
/// transport.
pub struct ListenOn(pub Multiaddr);

/// Retrieve [`ConnectionStats`] from the [`Endpoint`].
#[derive(Clone, Copy, Debug)]
pub struct GetConnectionStats;

#[derive(Debug, Default)]
pub struct ConnectionStats {
    pub connected_peers: HashSet<PeerId>,
    pub listen_addresses: HashSet<Multiaddr>,
}

/// Notifies an actor of a new, inbound substream from the given peer.
#[derive(Debug)]
pub struct NewInboundSubstream {
    pub peer: PeerId,
    pub stream: Substream,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("No connection to {0}")]
    NoConnection(PeerId),
    #[error("Timeout in protocol negotiation")]
    NegotiationTimeoutReached,
    #[error("Failed to negotiate protocol")]
    NegotiationFailed(#[from] NegotiationError), // TODO(public-api): Consider breaking this up.
    #[error("Bad connection")]
    BadConnection(#[from] yamux::ConnectionError), // TODO(public-api): Consider removing this.
    #[error("Address {0} does not end with a peer ID")]
    NoPeerIdInAddress(Multiaddr),
    #[error("Already trying to connect to peer {0}")]
    AlreadyTryingToConnected(PeerId),
}

/// Subscribers that get notified on connection changes
///
/// This allows other actors to get notified on connection changes such as a new connection being
/// established or dropped as well as listening addresses being added or removed.
#[derive(Default)]
pub struct Subscribers {
    connection_established: Vec<MessageChannel<ConnectionEstablished, ()>>,
    connection_dropped: Vec<MessageChannel<ConnectionDropped, ()>>,
    listen_address_added: Vec<MessageChannel<ListenAddressAdded, ()>>,
    listen_address_removed: Vec<MessageChannel<ListenAddressRemoved, ()>>,
}

impl Subscribers {
    pub fn new(
        connection_established: Vec<MessageChannel<ConnectionEstablished, ()>>,
        connection_dropped: Vec<MessageChannel<ConnectionDropped, ()>>,
        listen_address_added: Vec<MessageChannel<ListenAddressAdded, ()>>,
        listen_address_removed: Vec<MessageChannel<ListenAddressRemoved, ()>>,
    ) -> Self {
        Self {
            connection_established,
            connection_dropped,
            listen_address_added,
            listen_address_removed,
        }
    }
}

impl Endpoint {
    /// Construct a new [`Endpoint`] from the provided transport.
    ///
    /// An [`Endpoint`]s identity ([`PeerId`]) will be computed from the given [`Keypair`].
    ///
    /// The `connection_timeout` is applied to:
    /// 1. Dialing
    /// 2. Connection upgrades (i.e. noise handshake, yamux upgrade, etc)
    /// 3. Protocol negotiations
    ///
    /// The provided substream handlers are actors that will be given the fully-negotiated
    /// substreams whenever a peer opens a new substream for the provided protocol.
    pub fn new<T, const N: usize>(
        transport: Box<dyn Fn() -> T + Send + 'static>,
        identity: Keypair,
        connection_timeout: Duration,
        inbound_substream_handlers: [(&'static str, MessageChannel<NewInboundSubstream, ()>); N],
        subscribers: Subscribers,
    ) -> Self
    where
        T: Transport + Send + Sync + 'static,
        T::Output: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        T::Error: Send + Sync,
        T::Listener: Send + 'static,
        T::Dial: Send + 'static,
        T::ListenerUpgrade: Send + 'static,
    {
        let transport_fn = Box::new({
            let transport = Box::new(transport);
            let identity = identity;
            let handlers: Vec<&'static str> = inbound_substream_handlers
                .iter()
                .map(|(proto, _)| *proto)
                .collect();

            move || {
                upgrade::transport(
                    (transport)(),
                    &identity,
                    handlers.clone(),
                    connection_timeout,
                )
            }
        });

        Self {
            transport_fn,
            inbound_substream_channels: verify_unique_handlers(inbound_substream_handlers),
            controls: HashMap::default(),
            listen_addresses: HashSet::default(),
            inflight_connections: HashSet::default(),
            connection_timeout,
            subscribers,
        }
    }

    async fn drop_connection(&mut self, this: &Address<Self>, peer: &PeerId) {
        let (mut control, tasks) = match self.controls.remove(peer) {
            None => return,
            Some(control) => control,
        };

        // TODO: Evaluate whether dropping and closing has to be in a particular order.
        tokio_extras::spawn(this, async move {
            let _ = control.close().await;
            drop(tasks);
        });
        self.notify_connection_dropped(*peer).await;
    }

    #[instrument(skip(control, connection_timeout), err)]
    async fn open_substream(
        mut control: yamux::Control,
        peer: PeerId,
        protocols: Vec<&'static str>,
        connection_timeout: Duration,
    ) -> Result<(&'static str, Substream), Error> {
        let stream = control
            .open_stream()
            .instrument(tracing::debug_span!("open yamux stream"))
            .await?;

        let (protocol, stream) = tokio_extras::time::timeout(
            connection_timeout,
            multistream_select::dialer_select_proto(stream, protocols, Version::V1),
            || tracing::debug_span!("dialer_select_proto", version = ?Version::V1),
        )
        .await
        .map_err(|_timeout| Error::NegotiationTimeoutReached)?
        .map_err(Error::NegotiationFailed)?;

        Ok((
            protocol,
            Substream::new(stream, protocol, libp2p_core::Endpoint::Dialer),
        ))
    }
}

#[xtra_productivity]
impl Endpoint {
    async fn handle(&mut self, msg: NewConnection, ctx: &mut Context<Self>) {
        self.inflight_connections.remove(&msg.peer);
        let this = ctx.address().expect("we are alive");

        let NewConnection {
            peer,
            control,
            mut incoming_substreams,
            worker,
        } = msg;

        let mut tasks = Tasks::default();
        tasks.add(worker);
        tasks.add_fallible(
            {
                let inbound_substream_channels = self
                    .inbound_substream_channels
                    .iter()
                    .map(|(proto, channel)| (proto.to_owned(), channel.clone()))
                    .collect::<HashMap<_, _>>();

                async move {
                    loop {
                        let (stream, protocol) = match incoming_substreams.try_next().await {
                            Ok(Some(Ok((stream, protocol)))) => (stream, protocol),
                            Ok(Some(Err(upgrade::Error::NegotiationTimeoutReached))) => {
                                tracing::debug!("Hit timeout while negotiating substream");
                                continue;
                            }
                            Ok(Some(Err(upgrade::Error::NegotiationFailed(e)))) => {
                                tracing::debug!("Failed to negotiate substream: {}", e);
                                continue;
                            }
                            Ok(None) => bail!("Substream listener closed"),
                            Err(e) => bail!(e),
                        };

                        let channel = inbound_substream_channels
                            .get(&protocol)
                            .expect("Cannot negotiate a protocol that we don't support");

                        let stream =
                            Substream::new(stream, protocol, libp2p_core::Endpoint::Listener);

                        let substream = NewInboundSubstream { peer, stream };
                        let span =
                            tracing::debug_span!("Register new inbound substream", ?substream);
                        let _ = channel.send_async_safe(substream).instrument(span).await;
                    }
                }
            },
            move |error| async move {
                this.send_async_next(ExistingConnectionFailed { peer, error })
                    .await;
            },
        );

        self.controls.insert(peer, (control, tasks));
        self.notify_connection_established(peer).await;
    }

    async fn handle(&mut self, msg: ListenerFailed) {
        tracing::debug!("Listener failed: {:#}", msg.error);

        self.listen_addresses.remove(&msg.address);
        self.notify_listen_address_removed(msg.address).await;
    }

    async fn handle(&mut self, msg: FailedToConnect, ctx: &mut Context<Self>) {
        tracing::debug!("Failed to connect: {:#}", msg.error);
        let peer = msg.peer;

        self.inflight_connections.remove(&peer);
        self.drop_connection(&ctx.address().expect("self to be alive"), &peer)
            .await;
    }

    async fn handle(&mut self, msg: ExistingConnectionFailed, ctx: &mut Context<Self>) {
        tracing::debug!("Connection failed: {:#}", msg.error);
        let peer = msg.peer;

        self.drop_connection(&ctx.address().expect("self to be alive"), &peer)
            .await;
    }

    async fn handle(&mut self, _: GetConnectionStats) -> ConnectionStats {
        ConnectionStats {
            connected_peers: self.controls.keys().copied().collect(),
            listen_addresses: self.listen_addresses.clone(),
        }
    }

    async fn handle(&mut self, msg: Connect, ctx: &mut Context<Self>) -> Result<(), Error> {
        let this = ctx.address().expect("we are alive");

        let peer = msg
            .0
            .clone()
            .extract_peer_id()
            .ok_or_else(|| Error::NoPeerIdInAddress(msg.0.clone()))?;

        if self.inflight_connections.contains(&peer) || self.controls.contains_key(&peer) {
            return Err(Error::AlreadyTryingToConnected(peer));
        }

        let mut transport = (self.transport_fn)();

        self.inflight_connections.insert(peer);
        tokio_extras::spawn_fallible(
            &this.clone(),
            {
                let this = this.clone();
                let connection_timeout = self.connection_timeout;

                let fut = async move {
                    let (peer, control, incoming_substreams, worker) = tokio_extras::time::timeout(
                        connection_timeout,
                        transport.dial(msg.0)?,
                        || tracing::debug_span!("transport dial"),
                    )
                    .await
                    .context("Dialing timed out")??;

                    this.send_async_next(NewConnection {
                        peer,
                        control,
                        incoming_substreams,
                        worker,
                    })
                    .await;

                    anyhow::Ok(())
                };

                fut.instrument(tracing::debug_span!("Dial new connection").or_current())
            },
            move |error| async move {
                this.send_async_next(FailedToConnect { peer, error }).await;
            },
        );

        Ok(())
    }

    async fn handle(&mut self, msg: Disconnect, ctx: &mut Context<Self>) {
        self.drop_connection(&ctx.address().expect("self to be alive"), &msg.0)
            .await;
    }

    async fn handle(&mut self, msg: ListenOn, ctx: &mut Context<Self>) {
        let this = ctx.address().expect("we are alive");
        let listen_address = msg.0.clone();

        let mut transport = (self.transport_fn)();

        tokio_extras::spawn_fallible::<_, _, _, (), _, _, _>(
            &this.clone(),
            {
                let this = this.clone();
                let listen_address = listen_address.clone();

                async move {
                    let mut stream = transport
                        .listen_on(msg.0)
                        .context("cannot establish transport stream")?;

                    let mut tasks = Tasks::default();

                    this.send(NewListenAddress {
                        listen_address: listen_address.clone(),
                    })
                    .await?;

                    loop {
                        let event = stream.next().await.context("Listener closed")?;
                        match event {
                            Ok(ListenerEvent::Upgrade {
                                upgrade,
                                remote_addr,
                                ..
                            }) => {
                                let this = this.clone();
                                tasks.add_fallible(
                                    async move {
                                        let (peer, control, incoming_substreams, worker) =
                                            upgrade.await.with_context(|| {
                                                match PeerId::try_from_multiaddr(&remote_addr) {
                                                    Some(peer_id) => format!(
                                                        "Failed to connect with peer: {peer_id}"
                                                    ),
                                                    None => format!("Failed to connect with multi-address: {remote_addr}"),
                                                }
                                            })?;
                                        this.send_async_next(NewConnection {
                                            peer,
                                            control,
                                            incoming_substreams,
                                            worker,
                                        })
                                        .await;
                                        Ok(())
                                    },
                                    move |e: anyhow::Error| async move {
                                        tracing::warn!("Could not upgrade connection: {e:#}");
                                    },
                                );
                            }
                            Err(e) => {
                                tracing::error!("Listener emitted error: {e:#}");
                                continue;
                            }
                            _ => continue,
                        }
                    }
                }
            },
            |error| async move {
                let _ = this
                    .send(ListenerFailed {
                        address: listen_address,
                        error,
                    })
                    .await;
            },
        );
    }

    // The diff in this file allows us to go back to the behaviour
    // before patch 98b488f0db4e5867a1d1139a2e307ef40937cf67, without
    // needing to rebase in between. Apply and stash to go back and
    // forth.
    #[must_use]
    async fn handle(
        &mut self,
        msg: OpenSubstream<Single>,
        _: &mut Context<Self>,
    ) -> Result<Pin<Box<dyn futures::Future<Output = Result<Substream, Error>> + Send>>, Error>
    {
        let peer = msg.peer;
        let protocols = msg.protocols;

        debug_assert!(
            protocols.len() == 1,
            "Type-system enforces that we only try to negotiate one protocol"
        );

        let (control, _) = self.controls.get(&peer).ok_or(Error::NoConnection(peer))?;

        let fut = {
            let connection_timeout = self.connection_timeout;
            let control = control.clone();
            async move {
                let (protocol, stream) =
                    Self::open_substream(control, peer, protocols.clone(), connection_timeout)
                        .await?;

                debug_assert!(
                    protocol == protocols[0],
                    "If negotiation is successful, must have selected the only protocol we sent."
                );

                Result::<_, Error>::Ok(stream)
            }
        };

        // We await on the future directly and return a boxed future
        // that resolves immediately: just as if we went back in time
        // before patch 98b488f0db4e5867a1d1139a2e307ef40937cf67

        let stream = fut.await?;

        Ok(Box::pin(async { Ok(stream) }))
    }

    #[must_use]
    async fn handle(
        &mut self,
        msg: OpenSubstream<Multiple>,
    ) -> Result<
        Pin<Box<dyn futures::Future<Output = Result<(&'static str, Substream), Error>> + Send>>,
        Error,
    > {
        let peer = msg.peer;
        let protocols = msg.protocols;

        let (control, _) = self.controls.get(&peer).ok_or(Error::NoConnection(peer))?;

        let fut = {
            let connection_timeout = self.connection_timeout;
            let control = control.clone();
            async move {
                let (protocol, stream) =
                    Self::open_substream(control, peer, protocols, connection_timeout).await?;

                Ok((protocol, stream))
            }
        };

        Ok(Box::pin(fut))
    }

    async fn handle(&mut self, msg: NewListenAddress) {
        // FIXME: This address could be a "catch-all" like "0.0.0.0" which actually results in
        // listening on multiple interfaces.
        self.listen_addresses.insert(msg.listen_address.clone());
        self.notify_listen_address_added(msg.listen_address).await;
    }
}

impl Endpoint {
    async fn notify_connection_established(&mut self, peer: PeerId) {
        tracing::info!(%peer, "Connection established");

        for subscriber in &self.subscribers.connection_established {
            subscriber
                .send_async_next(ConnectionEstablished { peer })
                .await;
        }
    }

    async fn notify_connection_dropped(&mut self, peer: PeerId) {
        tracing::info!(%peer, "Connection dropped");

        for subscriber in &self.subscribers.connection_dropped {
            subscriber.send_async_next(ConnectionDropped { peer }).await
        }
    }

    async fn notify_listen_address_added(&mut self, added: Multiaddr) {
        tracing::info!(address=%added, "Listen address added");

        for subscriber in &self.subscribers.listen_address_added {
            subscriber
                .send_async_next(ListenAddressAdded {
                    address: added.clone(),
                })
                .await;
        }
    }

    async fn notify_listen_address_removed(&mut self, removed: Multiaddr) {
        tracing::info!(address=%removed, "Listen address removed");

        for subscriber in &self.subscribers.listen_address_removed {
            subscriber
                .send_async_next(ListenAddressRemoved {
                    address: removed.clone(),
                })
                .await
        }
    }
}

fn verify_unique_handlers<const N: usize>(
    inbound_substream_handlers: [(&str, MessageChannel<NewInboundSubstream, ()>); N],
) -> HashMap<&str, MessageChannel<NewInboundSubstream, ()>> {
    let mut map = HashMap::with_capacity(inbound_substream_handlers.len());

    for (protocol, handler) in inbound_substream_handlers {
        let previous_handler = map.insert(protocol, handler);

        debug_assert!(
            previous_handler.is_none(),
            "Duplicate handler declared for protocol {protocol}"
        );
    }

    map
}

#[async_trait]
impl xtra::Actor for Endpoint {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}

#[derive(Debug)]
struct ListenerFailed {
    address: Multiaddr,
    error: anyhow::Error,
}

#[derive(Debug)]
struct FailedToConnect {
    peer: PeerId,
    error: anyhow::Error,
}

#[derive(Debug)]
struct ExistingConnectionFailed {
    peer: PeerId,
    error: anyhow::Error,
}

struct NewListenAddress {
    listen_address: Multiaddr,
}

struct NewConnection {
    peer: PeerId,
    control: yamux::Control,
    #[allow(clippy::type_complexity)]
    incoming_substreams: BoxStream<
        'static,
        Result<
            Result<(Negotiated<yamux::Stream>, &'static str), upgrade::Error>,
            yamux::ConnectionError,
        >,
    >,
    worker: BoxFuture<'static, ()>,
}

#[derive(Clone, Copy)]
pub struct ConnectionEstablished {
    pub peer: PeerId,
}

#[derive(Clone, Copy)]
pub struct ConnectionDropped {
    pub peer: PeerId,
}

pub struct ListenAddressAdded {
    pub address: Multiaddr,
}

pub struct ListenAddressRemoved {
    pub address: Multiaddr,
}
