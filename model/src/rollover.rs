use crate::CompleteFee;
use crate::FeeAccount;
use crate::FundingFee;
use crate::Leverage;
use crate::Price;
use crate::TxFeeRate;
use crate::Usd;

#[derive(Debug, Clone, Copy)]
pub enum Version {
    /// Version one of the rollover protocol
    ///
    /// This version cannot handle charging for "missed" rollovers yet, i.e. the hours to charge is
    /// always set to 1 hour. This version is needed for clients that are <= daemon version
    /// `0.4.7`.
    V1,
    /// Version two of the rollover protocol
    ///
    /// This version can handle charging for "missed" rollovers, i.e. we calculate the hours to
    /// charge based on the oracle event timestamp of the last successful rollover.
    V2,
    /// Version three of the rollover protocol
    ///
    /// This version calculates the time to extend the settlement time
    /// by using the `BitMexPriceEventId` of the settlement event
    /// associated with the rollover.
    V3,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match *self {
            Version::V1 => write!(f, "V1"),
            Version::V2 => write!(f, "V2"),
            Version::V3 => write!(f, "V3"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RolloverParams {
    pub price: Price,
    pub quantity: Usd,
    pub long_leverage: Leverage,
    pub short_leverage: Leverage,
    pub refund_timelock: u32,
    pub fee_rate: TxFeeRate,
    pub fee_account: FeeAccount,
    pub current_fee: FundingFee,
    pub version: Version,
}

impl RolloverParams {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        price: Price,
        quantity: Usd,
        long_leverage: Leverage,
        short_leverage: Leverage,
        refund_timelock: u32,
        fee_rate: TxFeeRate,
        fee_account: FeeAccount,
        current_fee: FundingFee,
        version: Version,
    ) -> Self {
        Self {
            price,
            quantity,
            long_leverage,
            short_leverage,
            refund_timelock,
            fee_rate,
            fee_account,
            current_fee,
            version,
        }
    }

    pub fn funding_fee(&self) -> &FundingFee {
        &self.current_fee
    }

    pub fn complete_fee_before_rollover(&self) -> CompleteFee {
        self.fee_account.settle()
    }
}
