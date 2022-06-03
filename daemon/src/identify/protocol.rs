use crate::Multiaddr;
use anyhow::Context;
use anyhow::Result;
use asynchronous_codec::FramedRead;
use asynchronous_codec::FramedWrite;
use asynchronous_codec::JsonCodec;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use futures::SinkExt;
use futures::StreamExt;
use libp2p_core::identity::PublicKey;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;
use std::time::Duration;
use tokio_extras::FutureExt;

// Start libp2p based protocols from 0.3.0 since the last wire version was 0.2.1
const PROTOCOL_VERSION: &str = "0.3.0";

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentifyMsg {
    protocol_version: String,
    agent_version: String,
    public_key: Vec<u8>,
    listen_addrs: HashSet<Multiaddr>,
    observed_addr: Multiaddr,
    protocols: HashSet<String>,

    /// Optional environment field that is not part of the identify spec
    environment: Option<Environment>,
}

impl IdentifyMsg {
    pub fn new(
        daemon_version: String,
        environment: Environment,
        public_key: PublicKey,
        listen_addrs: HashSet<Multiaddr>,
        observed_addr: Multiaddr,
        protocols: HashSet<String>,
    ) -> Self {
        let agent_version = format!("itchysats/{}", daemon_version);

        Self {
            protocol_version: PROTOCOL_VERSION.to_string(),
            agent_version,
            public_key: public_key.to_protobuf_encoding(),
            listen_addrs,
            observed_addr,
            protocols,
            environment: Some(environment),
        }
    }

    pub fn daemon_version(&self) -> Result<String> {
        let splitted = self.agent_version.split('/').collect::<Vec<_>>();
        splitted
            .get(1)
            .map(|str| str.to_string())
            .context("Unable to extract daemon version")
    }

    pub fn environment(&self) -> Environment {
        self.environment.unwrap_or(Environment::Unknown)
    }

    pub fn wire_version(&self) -> String {
        self.protocol_version.clone()
    }
}

pub(crate) async fn recv<S>(stream: S) -> Result<IdentifyMsg>
where
    S: AsyncReadExt + Unpin,
{
    let mut framed = FramedRead::new(stream, JsonCodec::<(), IdentifyMsg>::new());

    let identify_msg = framed
        .next()
        .timeout(TIMEOUT, |parent| {
            tracing::debug_span!(parent: parent, "Received identify msg")
        })
        .await
        .context("Waiting for identify msg timed out")?
        .context("Receive identify msg failed")?
        .context("Failed to decode identify msg")?;

    Ok(identify_msg)
}

pub(crate) async fn send<S>(stream: S, identify_msg: IdentifyMsg) -> Result<()>
where
    S: AsyncWriteExt + Unpin,
{
    let mut framed = FramedWrite::new(stream, JsonCodec::<IdentifyMsg, ()>::new());
    framed
        .send(identify_msg)
        .await
        .context("Failed to send identify msg")?;

    Ok(())
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub enum Environment {
    Umbrel,
    RaspiBlitz,
    Docker,
    Binary,
    Test,
    Legacy,
    Unknown,
}

impl From<crate::Environment> for Environment {
    fn from(environment: crate::Environment) -> Self {
        match environment {
            crate::Environment::Umbrel => Environment::Umbrel,
            crate::Environment::RaspiBlitz => Environment::RaspiBlitz,
            crate::Environment::Docker => Environment::Docker,
            crate::Environment::Binary => Environment::Binary,
            crate::Environment::Test => Environment::Test,
            crate::Environment::Legacy => Environment::Legacy,
            crate::Environment::Unknown => Environment::Unknown,
        }
    }
}

impl From<Environment> for crate::Environment {
    fn from(environment: Environment) -> Self {
        match environment {
            Environment::Umbrel => crate::Environment::Umbrel,
            Environment::RaspiBlitz => crate::Environment::RaspiBlitz,
            Environment::Docker => crate::Environment::Docker,
            Environment::Binary => crate::Environment::Binary,
            Environment::Test => crate::Environment::Test,
            Environment::Legacy => crate::Environment::Legacy,
            Environment::Unknown => crate::Environment::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p_core::identity::Keypair;

    #[test]
    fn extract_daemon_version() {
        let msg = IdentifyMsg::new(
            "0.4.3".to_string(),
            Environment::Umbrel,
            Keypair::generate_ed25519().public(),
            HashSet::new(),
            Multiaddr::empty(),
            HashSet::new(),
        );

        let daemon_version = msg.daemon_version().unwrap();

        assert_eq!(daemon_version, "0.4.3".to_string());
    }
}
