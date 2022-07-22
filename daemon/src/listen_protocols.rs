use crate::collab_settlement;
use crate::identify;
use crate::rollover;
use std::collections::HashSet;
use xtra::message_channel::MessageChannel;
use xtra::Address;
use xtra_libp2p::NewInboundSubstream;
use xtra_libp2p_ping::pong;

pub const MAKER_LISTEN_PROTOCOLS: MakerListenProtocols = MakerListenProtocols::new(
    xtra_libp2p_ping::PROTOCOL_NAME,
    identify::PROTOCOL,
    rollover::PROTOCOL,
    collab_settlement::PROTOCOL,
);

pub const TAKER_LISTEN_PROTOCOLS: TakerListenProtocols = TakerListenProtocols::new(
    xtra_libp2p_ping::PROTOCOL_NAME,
    identify::PROTOCOL,
    xtra_libp2p_offer::PROTOCOL_NAME,
);

pub const REQUIRED_MAKER_LISTEN_PROTOCOLS: RequiredMakerListenProtocols =
    RequiredMakerListenProtocols::new(
        xtra_libp2p_ping::PROTOCOL_NAME,
        identify::PROTOCOL,
        rollover::PROTOCOL,
        collab_settlement::PROTOCOL,
    );

/// Verify if the listen protocols that the `maker` supports are
/// sufficient to fulfil the `requirements` of the taker.
pub fn does_maker_satisfy_taker_needs(
    maker: &HashSet<String>,
    requirements: RequiredMakerListenProtocols,
) -> Result<(), HashSet<String>> {
    let missing_protocols = maker
        .difference(&HashSet::<String>::from(requirements))
        .cloned()
        .collect::<HashSet<_>>();

    if !missing_protocols.is_empty() {
        return Err(missing_protocols);
    }

    Ok(())
}

/// The set of protocols that the maker's `Endpoint` is listening for.
pub struct MakerListenProtocols {
    ping_v1: &'static str,
    identify_v1: &'static str,
    rollover_v1: &'static str,
    collaborative_settlement_v1: &'static str,
}

impl MakerListenProtocols {
    pub const fn new(
        ping_v1: &'static str,
        identify_v1: &'static str,
        rollover_v1: &'static str,
        collaborative_settlement_v1: &'static str,
    ) -> Self {
        Self {
            ping_v1,
            identify_v1,
            rollover_v1,
            collaborative_settlement_v1,
        }
    }

    /// Construct a map of protocol identifiers to actor addresses.
    ///
    /// This is used so that the `Endpoint` knows who to delegate to
    /// when receiving new inbound substreams.
    pub fn inbound_substream_handlers(
        &self,
        ping_v1: Address<pong::Actor>,
        identify_v1: Address<identify::listener::Actor>,
        rollover_v1: Address<rollover::maker::Actor>,
        collaborative_settlement_v1: Address<collab_settlement::maker::Actor>,
    ) -> [(&'static str, MessageChannel<NewInboundSubstream, ()>); 4] {
        [
            (self.ping_v1, ping_v1.into()),
            (self.identify_v1, identify_v1.into()),
            (self.rollover_v1, rollover_v1.into()),
            (
                self.collaborative_settlement_v1,
                collaborative_settlement_v1.into(),
            ),
        ]
    }

    /// All the versions of the ping protocol supported by the maker.
    fn pings(&self) -> HashSet<String> {
        HashSet::from_iter([self.ping_v1.to_string()])
    }

    /// All the versions of the identify protocol supported by the
    /// maker.
    fn identifies(&self) -> HashSet<String> {
        HashSet::from_iter([self.identify_v1.to_string()])
    }

    /// All the versions of the rollover protocol supported by the
    /// maker.
    fn rollovers(&self) -> HashSet<String> {
        HashSet::from_iter([self.rollover_v1.to_string()])
    }

    /// All the versions of the collaborative settlement protocol
    /// supported by the maker.
    fn collaborative_settlements(&self) -> HashSet<String> {
        HashSet::from_iter([self.collaborative_settlement_v1.to_string()])
    }
}

impl From<MakerListenProtocols> for HashSet<String> {
    fn from(maker: MakerListenProtocols) -> Self {
        maker
            .pings()
            .union(&maker.identifies())
            .chain(maker.rollovers().union(&maker.collaborative_settlements()))
            .cloned()
            .collect()
    }
}

/// The set of protocols that the maker's `Endpoint` is expected to
/// be listening for.
pub struct RequiredMakerListenProtocols {
    ping: &'static str,
    identify: &'static str,
    rollover: &'static str,
    collaborative_settlement: &'static str,
}

impl RequiredMakerListenProtocols {
    pub const fn new(
        ping: &'static str,
        identify: &'static str,
        rollover: &'static str,
        collaborative_settlement: &'static str,
    ) -> Self {
        Self {
            ping,
            identify,
            rollover,
            collaborative_settlement,
        }
    }
}

impl From<RequiredMakerListenProtocols> for HashSet<String> {
    fn from(required: RequiredMakerListenProtocols) -> Self {
        HashSet::from_iter([
            required.ping.to_string(),
            required.identify.to_string(),
            required.rollover.to_string(),
            required.collaborative_settlement.to_string(),
        ])
    }
}

/// The set of protocols that the taker's `Endpoint` is listening for.
pub struct TakerListenProtocols {
    ping_v1: &'static str,
    identify_v1: &'static str,
    offer_v1: &'static str,
}

impl TakerListenProtocols {
    pub const fn new(
        ping_v1: &'static str,
        identify_v1: &'static str,
        offer_v1: &'static str,
    ) -> Self {
        Self {
            ping_v1,
            identify_v1,
            offer_v1,
        }
    }

    /// Construct a map of protocol identifiers to actor addresses.
    ///
    /// This is used so that the `Endpoint` knows who to delegate to
    /// when receiving new inbound substreams.
    pub fn inbound_substream_handlers(
        &self,
        ping_v1: Address<pong::Actor>,
        identify_v1: Address<identify::listener::Actor>,
        offer_v1: Address<xtra_libp2p_offer::taker::Actor>,
    ) -> [(&'static str, MessageChannel<NewInboundSubstream, ()>); 3] {
        [
            (self.ping_v1, ping_v1.into()),
            (self.identify_v1, identify_v1.into()),
            (self.offer_v1, offer_v1.into()),
        ]
    }
}

impl From<TakerListenProtocols> for HashSet<String> {
    fn from(protocols: TakerListenProtocols) -> Self {
        HashSet::from_iter([
            protocols.ping_v1.to_string(),
            protocols.identify_v1.to_string(),
            protocols.offer_v1.to_string(),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_expected_protocols_are_supported() {
        assert!(does_maker_satisfy_taker_needs(
            &MAKER_LISTEN_PROTOCOLS.into(),
            REQUIRED_MAKER_LISTEN_PROTOCOLS
        )
        .is_ok());
    }
}
