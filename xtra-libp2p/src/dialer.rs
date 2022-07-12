use crate::endpoint;
use crate::multiaddress_ext::MultiaddrExt;
use crate::Connect;
use crate::Endpoint;
use crate::GetConnectionStats;
use anyhow::anyhow;
use anyhow::ensure;
use anyhow::Result;
use async_trait::async_trait;
use libp2p_core::Multiaddr;
use libp2p_core::PeerId;
use std::time::Duration;
use tracing::instrument;
use xtra::Address;
use xtra_productivity::xtra_productivity;
use xtras::SendAsyncSafe;

/// If we're not connected by this time, stop the actor.
pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// xtra actor that takes care of dialing (connecting) to an Endpoint.
///
/// Polls Endpoint at startup to check whether connection got established correctly, and
/// then listens for ConnectionDropped message to stop itself.
/// Should be used in conjunction with supervisor maintaining resilient connection.
#[derive(Debug)]
pub struct Actor {
    endpoint: Address<Endpoint>,
    connect_address: Multiaddr,
    listener_peer_id: Option<PeerId>,
    stop_reason: Option<Error>,
}

impl Actor {
    pub fn new(endpoint: Address<Endpoint>, connect_address: Multiaddr) -> Self {
        Self {
            endpoint,
            connect_address,
            listener_peer_id: None,
            stop_reason: None,
        }
    }

    #[instrument(err)]
    async fn connect(&self) -> Result<(), Error> {
        self.endpoint
            .send(Connect(self.connect_address.clone()))
            .await
            .map_err(|_| Error::NoEndpoint)?
            .map_err(|e| Error::Failed { source: anyhow!(e) })
    }

    fn stop_with_error(&mut self, e: Error, ctx: &mut xtra::Context<Self>) {
        tracing::debug!("Stopping dialer with an error: {e:#}");
        self.stop_reason = Some(e);
        ctx.stop_self();
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    type Stop = Error;

    async fn started(&mut self, ctx: &mut xtra::Context<Self>) {
        tracing::debug!("Starting dialer actor");
        match self
            .connect_address
            .clone()
            .extract_peer_id()
            .ok_or(Error::InvalidPeerId)
        {
            Ok(peer_id) => self.listener_peer_id = Some(peer_id),
            Err(e) => {
                self.stop_with_error(e, ctx);
            }
        }

        let this = ctx.address().expect("self to be alive");
        this.send_async_safe(Dial).await.expect("self to be alive");
    }

    async fn stopped(self) -> Self::Stop {
        self.stop_reason.unwrap_or(Error::Unspecified)
    }
}

impl Actor {
    fn peer_id(&self) -> PeerId {
        self.listener_peer_id
            .expect("to always have peer id if successfully started")
    }

    #[instrument(ret, err)]
    async fn is_connection_established(&self) -> Result<bool> {
        Ok(self
            .endpoint
            .send(GetConnectionStats)
            .await?
            .connected_peers
            .contains(&self.peer_id()))
    }

    #[instrument(err)]
    async fn dial(&self) -> Result<()> {
        if self.is_connection_established().await? {
            tracing::info!("Connection is already established, no need to connect");
            return Ok(());
        }

        if let Err(e) = self.connect().await {
            tracing::warn!("Failed to request connection from endpoint: {e:#}");
        }

        // Only check the connection again after it had enough time to be established
        tokio_extras::time::sleep(CONNECTION_TIMEOUT).await;

        ensure!(
            self.is_connection_established().await?,
            "No connection after dialing attempt",
        );
        Ok(())
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle(&mut self, _msg: Dial, ctx: &mut xtra::Context<Self>) {
        if let Err(e) = self.dial().await {
            self.stop_with_error(Error::Failed { source: e }, ctx);
        }
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle(&mut self, msg: endpoint::ConnectionDropped, ctx: &mut xtra::Context<Self>) {
        if msg.peer == self.peer_id() {
            tracing::debug!("Dialer noticed connection got dropped");
            self.stop_with_error(Error::ConnectionDropped, ctx);
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Dialer failed")]
    Failed {
        #[from]
        source: anyhow::Error,
    },
    #[error("Endpoint actor is disconnected")]
    NoEndpoint,
    #[error("Connection dropped from endpoint")]
    ConnectionDropped,
    #[error("Invalid Peer Id")]
    InvalidPeerId,
    #[error("Stop reason was not specified")]
    Unspecified,
}

struct Dial;
