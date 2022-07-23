use crate::wire::SetupMsg;
use anyhow::bail;
use anyhow::Result;
use model::olivia::BitMexPriceEventId;
use model::FundingRate;
use model::Leverage;
use model::OpeningFee;
use model::OrderId;
use model::Position;
use model::Price;
use model::TxFeeRate;
use model::Usd;
use serde::Deserialize;
use serde::Serialize;
use time::Duration;

#[derive(Serialize, Deserialize)]
pub(crate) enum TakerMessage {
    PlaceOrder {
        id: OrderId,
        quantity: Usd,
        leverage: Leverage,
        position: Position,
        opening_price: Price,
        settlement_interval: Duration,
        opening_fee: OpeningFee,
        funding_rate: FundingRate,
        tx_fee_rate: TxFeeRate,
        oracle_event_id: BitMexPriceEventId,
    },
    ContractSetupMsg(Box<SetupMsg>),
}

#[derive(Serialize, Deserialize)]
pub(crate) enum MakerMessage {
    Decision(Decision),
    ContractSetupMsg(Box<SetupMsg>),
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Decision {
    Accept,
    Reject,
}

impl TryFrom<MakerMessage> for SetupMsg {
    type Error = anyhow::Error;

    fn try_from(value: MakerMessage) -> Result<Self> {
        match value {
            MakerMessage::Decision(_) => bail!("Expected SetupMsg, got decision"),
            MakerMessage::ContractSetupMsg(msg) => Ok(*msg),
        }
    }
}

impl TryFrom<TakerMessage> for SetupMsg {
    type Error = anyhow::Error;

    fn try_from(value: TakerMessage) -> Result<Self> {
        match value {
            TakerMessage::PlaceOrder { .. } => bail!("Expected SetupMsg, got order placement"),
            TakerMessage::ContractSetupMsg(msg) => Ok(*msg),
        }
    }
}
