use daemon::bdk::bitcoin::Amount;
use daemon::identify;
use daemon::online_status;
use daemon::projection::Cfd;
use daemon::projection::Quote;
use daemon::ProtocolFactory;
use model::Identity;
use model::Timestamp;
use rocket::response::stream::Event;
use serde::Serialize;
use std::collections::HashSet;

pub trait ToSseEvent {
    fn to_sse_event(&self) -> Event;
}

impl ToSseEvent for Vec<Cfd> {
    fn to_sse_event(&self) -> Event {
        Event::json(&self).event("cfds")
    }
}

impl ToSseEvent for Vec<Identity> {
    fn to_sse_event(&self) -> Event {
        Event::json(&self).event("takers")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WalletInfo {
    #[serde(with = "daemon::bdk::bitcoin::util::amount::serde::as_btc")]
    balance: Amount,
    address: String,
    last_updated_at: Timestamp,
}

impl ToSseEvent for Option<model::WalletInfo> {
    fn to_sse_event(&self) -> Event {
        let wallet_info = self.as_ref().map(|wallet_info| WalletInfo {
            balance: wallet_info.balance,
            address: wallet_info.address.to_string(),
            last_updated_at: wallet_info.last_updated_at,
        });

        Event::json(&wallet_info).event("wallet")
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ConnectionStatus {
    online: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub enum ConnectionCloseReason {
    MakerVersionOutdated,
    TakerVersionOutdated,
}

impl ToSseEvent for online_status::ConnectionStatus {
    fn to_sse_event(&self) -> Event {
        let connected = match self {
            online_status::ConnectionStatus::Online => ConnectionStatus { online: true },
            online_status::ConnectionStatus::Offline => ConnectionStatus { online: false },
        };

        Event::json(&connected).event("maker_status")
    }
}

impl ToSseEvent for Option<Quote> {
    fn to_sse_event(&self) -> Event {
        Event::json(self).event("quote")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MakerCompatibility {
    /// Protocols that the maker version does not support, but the taker version requires
    unsupported_protocols: Option<HashSet<String>>,
}

impl ToSseEvent for Option<identify::PeerInfo> {
    fn to_sse_event(&self) -> Event {
        let maker_protocols_expected_by_taker = ProtocolFactory::taker_expects_from_maker();

        let unsupported_protocols = self.as_ref().map(|identified_peer| {
            maker_protocols_expected_by_taker
                .difference(&identified_peer.protocols)
                .cloned()
                .collect()
        });

        let maker_compatibility = MakerCompatibility {
            unsupported_protocols,
        };

        Event::json(&maker_compatibility).event("maker_compatibility")
    }
}
