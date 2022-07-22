use crate::identify::protocol;
use crate::Environment;
use async_trait::async_trait;
use libp2p_core::Multiaddr;
use libp2p_core::PublicKey;
use std::collections::HashSet;
use tokio_extras::spawn_fallible;
use xtra::Context;
use xtra_libp2p::NewInboundSubstream;
use xtra_productivity::xtra_productivity;

pub struct Actor {
    daemon_version: String,
    environment: Environment,
    identity: PublicKey,
    listen_addrs: HashSet<Multiaddr>,
    protocols: HashSet<String>,
}

impl Actor {
    pub fn new(
        daemon_version: String,
        environment: Environment,
        identity: PublicKey,
        listen_addrs: HashSet<Multiaddr>,
        protocols: HashSet<String>,
    ) -> Self {
        Self {
            daemon_version,
            environment,
            identity,
            listen_addrs,
            protocols,
        }
    }
}

#[xtra_productivity(message_impl = false)]
impl Actor {
    async fn handle(&mut self, message: NewInboundSubstream, ctx: &mut Context<Self>) {
        let NewInboundSubstream { stream, peer } = message;

        // TODO: Set observed address according to the address we observed when establishing the
        //  connection
        let identify_msg = protocol::IdentifyMsg::new(
            self.daemon_version.clone(),
            self.environment.into(),
            self.identity.clone(),
            self.listen_addrs.clone(),
            Multiaddr::empty(),
            self.protocols.clone(),
        );

        let send_identify_msg_fut = protocol::send(stream, identify_msg);

        let err_handler = move |e| async move {
            tracing::debug!(peer_id=%peer, "Identify protocol failed upon response: {e:#}")
        };

        let this = ctx.address().expect("we are alive");
        spawn_fallible(&this, send_identify_msg_fut, err_handler);
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}
