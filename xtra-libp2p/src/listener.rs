use crate::endpoint;
use crate::Endpoint;
use crate::GetConnectionStats;
use crate::ListenOn;
use anyhow::ensure;
use anyhow::Result;
use async_trait::async_trait;
use libp2p_core::Multiaddr;
use std::time::Duration;
use xtra::Address;
use xtra_productivity::xtra_productivity;
use xtras::SendAsyncSafe;

/// If we're not connected by this time, stop the actor.
pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// xtra actor that taker care of listening for incoming connections to the Endpoint.
///
/// Polls Endpoint at startup to check whether listening got started correctly, and
/// then listens for ListenerRemoved message to stop itself.
/// Should be used in conjunction with supervisor for continuous and resilient listening.
pub struct Actor {
    endpoint: Address<Endpoint>,
    listen_address: Multiaddr,
    /// Contains the reason we are stopping.
    stop_reason: Option<Error>,
}

impl Actor {
    pub fn new(endpoint: Address<Endpoint>, listen_address: Multiaddr) -> Self {
        Self {
            endpoint,
            listen_address,
            stop_reason: None,
        }
    }

    fn stop_with_error(&mut self, e: Error, ctx: &mut xtra::Context<Self>) {
        tracing::debug!("Stopping listener with an error: {e:#}");
        self.stop_reason = Some(e);
        ctx.stop_self();
    }

    async fn is_listening(&self) -> Result<bool> {
        Ok(self
            .endpoint
            .send(GetConnectionStats)
            .await?
            .listen_addresses
            .contains(&self.listen_address))
    }

    async fn listen(&self) -> Result<()> {
        ensure!(
            !self.is_listening().await?,
            "We should not be listening yet"
        );

        self.endpoint
            .send(ListenOn(self.listen_address.clone()))
            .await?;
        tokio_extras::time::sleep(CONNECTION_TIMEOUT).await;

        ensure!(
            self.is_listening().await?,
            "Endpoint did not start listening in time"
        );
        Ok(())
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    type Stop = Error;

    async fn started(&mut self, ctx: &mut xtra::Context<Self>) {
        let this = ctx.address().expect("self to be alive");
        this.send_async_safe(Listen)
            .await
            .expect("self to be alive");
    }

    async fn stopped(self) -> Self::Stop {
        self.stop_reason.unwrap_or(Error::Unspecified)
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle(&mut self, _msg: Listen, ctx: &mut xtra::Context<Self>) {
        if let Err(e) = self.listen().await {
            self.stop_with_error(Error::Failed { source: e }, ctx);
        }
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle(&mut self, msg: endpoint::ListenAddressRemoved, ctx: &mut xtra::Context<Self>) {
        if msg.address == self.listen_address {
            self.stop_with_error(Error::ConnectionDropped, ctx);
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Listener failed")]
    Failed {
        #[from]
        source: anyhow::Error,
    },
    #[error("Connection dropped from endpoint")]
    ConnectionDropped,
    #[error("Stop reason was not specified")]
    Unspecified,
}

struct Listen;
