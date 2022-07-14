use crate::Environment;
use std::collections::HashSet;

pub mod dialer;
pub mod listener;
pub mod protocol;

pub const PROTOCOL: &str = "/itchysats/id/1.0.0";

#[derive(Debug, Clone, PartialEq)]
pub struct PeerInfo {
    pub wire_version: String,
    pub daemon_version: String,
    pub environment: Environment,
    pub protocols: HashSet<String>,
}

impl TryFrom<protocol::IdentifyMsg> for PeerInfo {
    type Error = ConversionError;

    fn try_from(identity_msg: protocol::IdentifyMsg) -> Result<Self, Self::Error> {
        let identity_info = PeerInfo {
            wire_version: identity_msg.wire_version(),
            daemon_version: identity_msg.daemon_version()?,
            environment: identity_msg.environment().into(),
            protocols: identity_msg.protocols(),
        };

        Ok(identity_info)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Conversion to identity info failed: {error}")]
pub struct ConversionError {
    #[from]
    error: anyhow::Error,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identify::PeerInfo;
    use crate::Environment;
    use libp2p_core::PublicKey;
    use std::collections::HashSet;
    use std::time::Duration;
    use tokio::sync::watch;
    use xtra::spawn::TokioGlobalSpawnExt;
    use xtra::Actor as _;
    use xtra::Address;
    use xtra::Context;
    use xtra_libp2p::endpoint::Subscribers;
    use xtra_libp2p::libp2p::identity::Keypair;
    use xtra_libp2p::libp2p::multiaddr::Protocol;
    use xtra_libp2p::libp2p::transport::MemoryTransport;
    use xtra_libp2p::libp2p::Multiaddr;
    use xtra_libp2p::libp2p::PeerId;
    use xtra_libp2p::Connect;
    use xtra_libp2p::Endpoint;
    use xtra_libp2p::ListenOn;

    #[tokio::test]
    async fn both_parties_request_identify_info_on_connection_established() {
        tracing_subscriber::fmt()
            .with_env_filter("daemon=trace")
            .with_test_writer()
            .init();

        let (maker_peer_id, maker_endpoint, maker_receiver) = create_endpoint_with_identify(
            "0.4.22".to_string(),
            Environment::Unknown,
            Keypair::generate_ed25519().public(),
            HashSet::new(),
            HashSet::from(["some_maker_protocol".to_string()]),
        );
        let (_, taker_endpoint, taker_receiver) = create_endpoint_with_identify(
            "0.4.22".to_string(),
            Environment::Umbrel,
            Keypair::generate_ed25519().public(),
            HashSet::new(),
            HashSet::from(["some_taker_protocol".to_string()]),
        );

        maker_endpoint
            .send(ListenOn(Multiaddr::empty().with(Protocol::Memory(1000))))
            .await
            .unwrap();
        taker_endpoint
            .send(Connect(
                Multiaddr::empty()
                    .with(Protocol::Memory(1000))
                    .with(Protocol::P2p(maker_peer_id.into())),
            ))
            .await
            .unwrap()
            .unwrap();

        let taker_to_maker_peer_info = || taker_receiver.borrow().clone();
        let maker_peer_info = retry_until_some(taker_to_maker_peer_info).await;

        let maker_to_taker_peer_info = || maker_receiver.borrow().clone();
        let taker_peer_info = retry_until_some(maker_to_taker_peer_info).await;

        let expected_maker_peer_info = PeerInfo {
            wire_version: "0.3.0".to_string(),
            daemon_version: "0.4.22".to_string(),
            environment: Environment::Unknown,
            protocols: HashSet::from(["some_maker_protocol".to_string()]),
        };

        let expected_taker_peer_info = PeerInfo {
            wire_version: "0.3.0".to_string(),
            daemon_version: "0.4.22".to_string(),
            environment: Environment::Umbrel,
            protocols: HashSet::from(["some_taker_protocol".to_string()]),
        };

        assert_eq!(maker_peer_info, expected_maker_peer_info);
        assert_eq!(taker_peer_info, expected_taker_peer_info);
    }

    #[allow(clippy::type_complexity)]
    fn create_endpoint_with_identify(
        daemon_version: String,
        environment: Environment,
        identity: PublicKey,
        listen_addrs: HashSet<Multiaddr>,
        protocols: HashSet<String>,
    ) -> (PeerId, Address<Endpoint>, watch::Receiver<Option<PeerInfo>>) {
        let (endpoint_address, endpoint_context) = Context::new(None);

        let id = Keypair::generate_ed25519();
        let (identify_dialer, receiver) =
            dialer::Actor::new_with_subscriber(endpoint_address.clone());
        let identify_dialer = identify_dialer.create(None).spawn_global();

        let identify_listener = listener::Actor::new(
            daemon_version,
            environment,
            identity,
            listen_addrs,
            protocols,
        )
        .create(None)
        .spawn_global();

        let endpoint = Endpoint::new(
            Box::new(MemoryTransport::default),
            id.clone(),
            Duration::from_secs(10),
            [(PROTOCOL, identify_listener.into())],
            Subscribers::new(
                vec![identify_dialer.clone().into()],
                vec![identify_dialer.into()],
                vec![],
                vec![],
            ),
        );

        #[allow(clippy::disallowed_methods)]
        tokio::spawn(endpoint_context.run(endpoint));

        (id.public().to_peer_id(), endpoint_address, receiver)
    }

    async fn retry_until_some<F, T>(mut f: F) -> T
    where
        F: FnMut() -> Option<T>,
    {
        loop {
            match f() {
                Some(t) => return t,
                None => tokio_extras::time::sleep(Duration::from_millis(200)).await,
            }
        }
    }
}
