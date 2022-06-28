pub mod dialer;
pub mod listener;
pub mod protocol;

pub const PROTOCOL: &str = "/itchysats/id/1.0.0";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Environment;
    use futures::Future;
    use futures::FutureExt;
    use libp2p_core::PublicKey;
    use std::collections::HashSet;
    use std::time::Duration;
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

        let (maker_peer_id, maker_dialer_actor, maker_endpoint) = create_endpoint_with_identify(
            "0.4.22".to_string(),
            Environment::Unknown,
            Keypair::generate_ed25519().public(),
            HashSet::new(),
            HashSet::from(["some_maker_protocol".to_string()]),
        );
        let (taker_peer_id, taker_dialer_actor, taker_endpoint) = create_endpoint_with_identify(
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

        let taker_to_maker_peer_info = {
            || {
                let taker_dialer_actor = taker_dialer_actor.clone();
                async move {
                    taker_dialer_actor
                        .send(dialer::GetPeerInfo(maker_peer_id))
                        .map(|res| res.unwrap())
                        .await
                }
            }
        };
        let maker_peer_info = retry_until_some(taker_to_maker_peer_info).await;

        let maker_to_taker_peer_info = || {
            let maker_dialer_actor = maker_dialer_actor.clone();
            async move {
                maker_dialer_actor
                    .send(dialer::GetPeerInfo(taker_peer_id))
                    .map(|res| res.unwrap())
                    .await
            }
        };
        let taker_peer_info = retry_until_some(maker_to_taker_peer_info).await;

        let expected_maker_peer_info = dialer::PeerInfo {
            wire_version: "0.3.0".to_string(),
            daemon_version: "0.4.22".to_string(),
            environment: Environment::Unknown,
        };

        let expected_taker_peer_info = dialer::PeerInfo {
            wire_version: "0.3.0".to_string(),
            daemon_version: "0.4.22".to_string(),
            environment: Environment::Umbrel,
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
    ) -> (PeerId, Address<dialer::Actor>, Address<Endpoint>) {
        let (endpoint_address, endpoint_context) = Context::new(None);

        let id = Keypair::generate_ed25519();
        let identify_dialer = dialer::Actor::new(endpoint_address.clone())
            .create(None)
            .spawn_global();

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
                vec![identify_dialer.clone().into()],
                vec![],
                vec![],
            ),
        );

        #[allow(clippy::disallowed_methods)]
        tokio::spawn(endpoint_context.run(endpoint));

        (id.public().to_peer_id(), identify_dialer, endpoint_address)
    }

    async fn retry_until_some<F, FUT, T>(mut fut: F) -> T
    where
        F: FnMut() -> FUT,
        FUT: Future<Output = Option<T>>,
    {
        loop {
            match fut().await {
                Some(t) => return t,
                None => tokio_extras::time::sleep(Duration::from_millis(200)).await,
            }
        }
    }
}
