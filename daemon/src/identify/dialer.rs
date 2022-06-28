use crate::identify::protocol;
use crate::identify::PROTOCOL;
use crate::Environment;
use async_trait::async_trait;
use std::collections::HashMap;
use tokio_extras::spawn_fallible;
use xtra::Address;
use xtra::Context;
use xtra_libp2p::endpoint;
use xtra_libp2p::libp2p::PeerId;
use xtra_libp2p::Endpoint;
use xtra_libp2p::OpenSubstream;
use xtra_productivity::xtra_productivity;

pub struct Actor {
    endpoint: Address<Endpoint>,
    peer_infos: HashMap<PeerId, PeerInfo>,
}

impl Actor {
    pub fn new(endpoint: Address<Endpoint>) -> Self {
        NUM_LIBP2P_CONNECTIONS_GAUGE.reset();

        Self {
            endpoint,
            peer_infos: HashMap::default(),
        }
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}

pub(crate) struct GetPeerInfo(pub PeerId);

pub(crate) struct IdentifyMsgReceived {
    peer_id: PeerId,
    identify_msg: protocol::IdentifyMsg,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PeerInfo {
    pub wire_version: String,
    pub daemon_version: String,
    pub environment: Environment,
}

#[derive(Debug, thiserror::Error)]
#[error("Conversion to peer info failed: {error}")]
pub struct ConversionError {
    #[from]
    error: anyhow::Error,
}

impl TryFrom<protocol::IdentifyMsg> for PeerInfo {
    type Error = ConversionError;

    fn try_from(identify_msg: protocol::IdentifyMsg) -> Result<Self, Self::Error> {
        let peer_info = PeerInfo {
            wire_version: identify_msg.wire_version(),
            daemon_version: identify_msg.daemon_version()?,
            environment: identify_msg.environment().into(),
        };

        Ok(peer_info)
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle(&mut self, msg: GetPeerInfo) -> Option<PeerInfo> {
        let peer_id = msg.0;
        self.peer_infos.get(&peer_id).cloned()
    }

    async fn handle(&mut self, msg: IdentifyMsgReceived) {
        let peer_id = msg.peer_id;
        let peer_info = match PeerInfo::try_from(msg.identify_msg.clone()) {
            Ok(peer_info) => peer_info,
            Err(e) => {
                tracing::error!("Peer info discarded {:?}: {e:#}", msg.identify_msg);
                return;
            }
        };

        let wire_version = peer_info.wire_version.clone();
        let daemon_version = peer_info.daemon_version.clone();
        let environment = peer_info.environment;

        NUM_LIBP2P_CONNECTIONS_GAUGE
            .with(&HashMap::from([
                (WIRE_VERSION_LABEL, wire_version.as_str()),
                (DAEMON_VERSION_LABEL, daemon_version.as_str()),
                (ENVIRONMENT_LABEL, environment.to_string().as_str()),
            ]))
            .inc();

        tracing::info!(%peer_id, %daemon_version, %environment, %wire_version, "New identify message received");
        self.peer_infos.insert(peer_id, peer_info);
    }
}

#[xtra_productivity(message_impl = false)]
impl Actor {
    async fn handle_connections_established(
        &mut self,
        msg: endpoint::ConnectionEstablished,
        ctx: &mut Context<Self>,
    ) {
        let peer_id = msg.peer;
        let endpoint = self.endpoint.clone();
        let this = ctx.address().expect("we are alive");

        let request_identify_msg_fut = {
            let this = this.clone();
            async move {
                let stream = endpoint
                    .send(OpenSubstream::single_protocol(peer_id, PROTOCOL))
                    .await??;

                let identify_msg = protocol::recv(stream).await?;

                this.send(IdentifyMsgReceived {
                    peer_id,
                    identify_msg,
                })
                .await?;

                anyhow::Ok(())
            }
        };

        let err_handler = move |e| async move {
            tracing::debug!(%peer_id, "Identify protocol failed upon request: {e:#}")
        };

        spawn_fallible(&this, request_identify_msg_fut, err_handler);
    }

    async fn handle_connections_dropped(&mut self, msg: endpoint::ConnectionDropped) {
        let peer_id = msg.peer;
        tracing::trace!(%peer_id, "Remove peer-info because connection dropped");
        if let Some(peer_info) = self.peer_infos.remove(&peer_id) {
            NUM_LIBP2P_CONNECTIONS_GAUGE
                .with(&HashMap::from([
                    (WIRE_VERSION_LABEL, peer_info.wire_version.as_str()),
                    (DAEMON_VERSION_LABEL, peer_info.daemon_version.as_str()),
                    (
                        ENVIRONMENT_LABEL,
                        peer_info.environment.to_string().as_str(),
                    ),
                ]))
                .dec();
        }
    }
}

const WIRE_VERSION_LABEL: &str = "wire_version";
const DAEMON_VERSION_LABEL: &str = "daemon_version";
const ENVIRONMENT_LABEL: &str = "environment";

static NUM_LIBP2P_CONNECTIONS_GAUGE: conquer_once::Lazy<prometheus::IntGaugeVec> =
    conquer_once::Lazy::new(|| {
        prometheus::register_int_gauge_vec!(
            "libp2p_connections_total",
            "The number of active libp2p connections.",
            &[WIRE_VERSION_LABEL, DAEMON_VERSION_LABEL, ENVIRONMENT_LABEL]
        )
        .unwrap()
    });
