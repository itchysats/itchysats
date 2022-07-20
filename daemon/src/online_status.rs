use std::time::Duration;

use async_trait::async_trait;
use libp2p_core::PeerId;
use tokio::sync::watch;
use xtra::prelude::*;
use xtra_libp2p::endpoint;
use xtra_libp2p::Endpoint;
use xtra_libp2p::GetConnectionStats;
use xtra_productivity::xtra_productivity;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConnectionStatus {
    Online,
    Offline,
}

/// Actor that transmits updates of ConnectionStatus of a specified PeerId based on
/// information transmitted by the Endpoint via a watch channel.
pub struct Actor {
    endpoint: Address<Endpoint>,
    watched_peer: PeerId,
    sender: watch::Sender<ConnectionStatus>,
}

impl Actor {
    pub fn new(
        endpoint: Address<Endpoint>,
        watched_peer: PeerId,
        sender: watch::Sender<ConnectionStatus>,
    ) -> Self {
        Self {
            endpoint,
            watched_peer,
            sender,
        }
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    type Stop = ();

    #[tracing::instrument(name = "online_status::Actor started", skip_all)]
    async fn started(&mut self, ctx: &mut Context<Self>) {
        tracing::debug!(
            "Online status watch actor started. Monitoring for peer id changes: {:?}",
            self.watched_peer
        );

        match self.endpoint.send(GetConnectionStats).await {
            Ok(connection_stats) => {
                let status = if connection_stats
                    .connected_peers
                    .contains(&self.watched_peer)
                {
                    ConnectionStatus::Online
                } else {
                    ConnectionStatus::Offline
                };
                self.sender
                    .send(status)
                    .expect("Receiver to outlive this actor");
            }
            Err(e) => {
                tracing::error!(
                    "Unable to receive connection stats from the endpoint upon startup: {e:#}"
                );
                // This code path should not be hit, but in case we run into an error this sleep
                // prevents a continuous endless loup of restarts.
                self.sender
                    .send(ConnectionStatus::Offline)
                    .expect("Receiver to outlive this actor");
                tokio_extras::time::sleep(Duration::from_secs(2)).await;

                ctx.stop_self();
            }
        }
    }

    async fn stopped(self) -> Self::Stop {}
}

#[xtra_productivity]
impl Actor {
    async fn handle_connection_established(&mut self, msg: endpoint::ConnectionEstablished) {
        tracing::debug!(
            "Adding newly established connection to online_status: {:?}",
            msg.peer
        );
        if msg.peer == self.watched_peer {
            self.sender
                .send(ConnectionStatus::Online)
                .expect("Receiver to outlive this actor");
        }
    }

    async fn handle_connection_dropped(&mut self, msg: endpoint::ConnectionDropped) {
        tracing::debug!(
            "Remove dropped connection from online_status: {:?}",
            msg.peer
        );

        if msg.peer == self.watched_peer {
            self.sender
                .send(ConnectionStatus::Offline)
                .expect("Receiver to outlive this actor");
        }
    }
}
