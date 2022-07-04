use crate::libp2p::PeerId;
use anyhow::Context;
use anyhow::Result;
use bdk::bitcoin::Address;
use bdk::bitcoin::Amount;
use bdk::bitcoin::Denomination;
use bdk::bitcoin::SignedAmount;
use bdk::bitcoin::Txid;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::de::Error as _;
use serde::Deserialize;
use serde::Serialize;
use std::cmp::Ordering;
use std::convert::TryInto;
use std::fmt;
use std::num::NonZeroU32;
use std::num::NonZeroU8;
use std::ops::Add;
use std::ops::Div;
use std::ops::Mul;
use std::ops::Sub;
use std::str;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use time::OffsetDateTime;

mod cfd;
mod contract_setup;
pub mod hex_transaction;
pub mod libp2p;
pub mod olivia;
pub mod payouts;
mod rollover;

pub use cfd::*;
pub use contract_setup::SetupParams;
pub use rollover::RolloverParams;
pub use rollover::Version as RolloverVersion;

/// The time-to-live of a CFD after it is first created or rolled
/// over.
///
/// This variable determines what oracle event ID will be associated
/// with the non-collaborative settlement of the CFD.
pub const SETTLEMENT_INTERVAL: time::Duration = time::Duration::hours(24);

#[derive(thiserror::Error, Debug, Clone, Copy)]
pub enum Error {
    #[error("Price of zero is not allowed.")]
    ZeroPrice,
    #[error("Negative Price is unimplemented.")]
    NegativePrice,
}

/// Represents "quantity" or "contract size" in Cfd terms
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Usd(Decimal);

impl Usd {
    pub const ZERO: Usd = Usd(Decimal::ZERO);

    pub fn new(value: Decimal) -> Self {
        Self(value)
    }

    pub fn try_into_u64(&self) -> Result<u64> {
        self.0.to_u64().context("could not fit decimal into u64")
    }

    #[must_use]
    pub fn into_decimal(self) -> Decimal {
        self.0
    }
}

impl fmt::Display for Usd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.round_dp(2).fmt(f)
    }
}

impl str::FromStr for Usd {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let dec = Decimal::from_str(s)?;
        Ok(Usd(dec))
    }
}

#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Price(Decimal);

impl Price {
    const INFINITE: Price = Price(rust_decimal_macros::dec!(21_000_000));

    pub fn new(value: Decimal) -> Result<Self, Error> {
        if value == Decimal::ZERO {
            return Result::Err(Error::ZeroPrice);
        }

        if value < Decimal::ZERO {
            return Result::Err(Error::NegativePrice);
        }

        Ok(Self(value))
    }

    pub fn try_into_u64(&self) -> Result<u64> {
        self.0.to_u64().context("Could not fit decimal into u64")
    }

    pub fn try_into_f64(&self) -> Result<f64> {
        self.0.to_f64().context("Could not fit decimal into f64")
    }

    #[must_use]
    pub fn into_decimal(self) -> Decimal {
        self.0
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl str::FromStr for Price {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let dec = Decimal::from_str(s)?;
        Ok(Price(dec))
    }
}

#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct InversePrice(Decimal);

impl InversePrice {
    pub fn new(value: Price) -> Result<Self, Error> {
        if value.0 == Decimal::ZERO {
            return Result::Err(Error::ZeroPrice);
        }

        if value.0 < Decimal::ZERO {
            return Result::Err(Error::NegativePrice);
        }

        Ok(Self(Decimal::ONE / value.0))
    }

    pub fn try_into_u64(&self) -> Result<u64> {
        self.0.to_u64().context("Could not fit decimal into u64")
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq)]
pub struct Leverage(u8);

impl Leverage {
    pub fn new(value: u8) -> Result<Self> {
        let val = NonZeroU8::new(value).context("Cannot use non-positive values")?;
        Ok(Self(u8::from(val)))
    }

    pub fn get(&self) -> u8 {
        self.0
    }

    pub const ONE: Self = Self(1);

    pub const TWO: Self = Self(2);
}

impl fmt::Display for Leverage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let leverage = self.0;

        write!(f, "x{leverage}")
    }
}

// add impl's to do algebra with Usd, Leverage, and ExhangeRate as required
impl Mul<Leverage> for Usd {
    type Output = Usd;

    fn mul(self, rhs: Leverage) -> Self::Output {
        let value = self.0 * Decimal::from(rhs.0);
        Self(value)
    }
}

impl Div<Leverage> for Usd {
    type Output = Usd;

    fn div(self, rhs: Leverage) -> Self::Output {
        Self(self.0 / Decimal::from(rhs.0))
    }
}

impl Mul<Usd> for Leverage {
    type Output = Usd;

    fn mul(self, rhs: Usd) -> Self::Output {
        let value = Decimal::from(self.0) * rhs.0;
        Usd(value)
    }
}

impl Mul<u8> for Usd {
    type Output = Usd;

    fn mul(self, rhs: u8) -> Self::Output {
        let value = self.0 * Decimal::from(rhs);
        Self(value)
    }
}

impl Div<u8> for Usd {
    type Output = Usd;

    fn div(self, rhs: u8) -> Self::Output {
        let value = self.0 / Decimal::from(rhs);
        Self(value)
    }
}

impl Div<u8> for Price {
    type Output = Price;

    fn div(self, rhs: u8) -> Self::Output {
        let value = self.0 / Decimal::from(rhs);
        Self(value)
    }
}

impl Add<Usd> for Usd {
    type Output = Usd;

    fn add(self, rhs: Usd) -> Self::Output {
        let value = self.0 + rhs.0;
        Self(value)
    }
}

impl Sub<Usd> for Usd {
    type Output = Usd;

    fn sub(self, rhs: Usd) -> Self::Output {
        let value = self.0 - rhs.0;
        Self(value)
    }
}

impl Div<Price> for Usd {
    type Output = Amount;

    fn div(self, rhs: Price) -> Self::Output {
        let mut btc = self.0 / rhs.0;
        btc.rescale(8);
        Amount::from_str_in(&btc.to_string(), Denomination::Bitcoin)
            .expect("Error computing BTC amount")
    }
}

impl Mul<Leverage> for Price {
    type Output = Price;

    fn mul(self, rhs: Leverage) -> Self::Output {
        let value = self.0 * Decimal::from(rhs.0);
        Self(value)
    }
}

impl Mul<Price> for Leverage {
    type Output = Price;

    fn mul(self, rhs: Price) -> Self::Output {
        let value = Decimal::from(self.0) * rhs.0;
        Price(value)
    }
}

impl Div<Leverage> for Price {
    type Output = Price;

    fn div(self, rhs: Leverage) -> Self::Output {
        let value = self.0 / Decimal::from(rhs.0);
        Self(value)
    }
}

impl Mul<InversePrice> for Usd {
    type Output = Amount;

    fn mul(self, rhs: InversePrice) -> Self::Output {
        let mut btc = self.0 * rhs.0;
        btc.rescale(8);
        Amount::from_str_in(&btc.to_string(), Denomination::Bitcoin)
            .expect("Error computing BTC amount")
    }
}

impl Mul<Leverage> for InversePrice {
    type Output = InversePrice;

    fn mul(self, rhs: Leverage) -> Self::Output {
        let value = self.0 * Decimal::from(rhs.0);
        Self(value)
    }
}

impl Mul<InversePrice> for Leverage {
    type Output = InversePrice;

    fn mul(self, rhs: InversePrice) -> Self::Output {
        let value = Decimal::from(self.0) * rhs.0;
        InversePrice(value)
    }
}

impl Div<Leverage> for InversePrice {
    type Output = InversePrice;

    fn div(self, rhs: Leverage) -> Self::Output {
        let value = self.0 / Decimal::from(rhs.0);
        Self(value)
    }
}

impl Add<Price> for Price {
    type Output = Price;

    fn add(self, rhs: Price) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub<Price> for Price {
    type Output = Price;

    fn sub(self, rhs: Price) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

impl Add<InversePrice> for InversePrice {
    type Output = InversePrice;

    fn add(self, rhs: InversePrice) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl Sub<InversePrice> for InversePrice {
    type Output = InversePrice;

    fn sub(self, rhs: InversePrice) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

impl Add<u8> for Leverage {
    type Output = Leverage;

    fn add(self, rhs: u8) -> Self::Output {
        Self(self.0 + rhs)
    }
}

impl Sub<u8> for Leverage {
    type Output = Leverage;

    fn sub(self, rhs: u8) -> Self::Output {
        Self(self.0 - rhs)
    }
}

impl Add<Leverage> for u8 {
    type Output = Leverage;

    fn add(self, rhs: Leverage) -> Self::Output {
        Leverage(self + rhs.0)
    }
}

impl Div<Leverage> for Leverage {
    type Output = Decimal;

    fn div(self, rhs: Leverage) -> Self::Output {
        Decimal::from(self.0) / Decimal::from(rhs.0)
    }
}

impl PartialEq<u8> for Leverage {
    #[inline]
    fn eq(&self, other: &u8) -> bool {
        self.0.eq(other)
    }
}

impl PartialOrd<u8> for Leverage {
    #[inline]
    fn partial_cmp(&self, other: &u8) -> Option<Ordering> {
        self.0.partial_cmp(other)
    }
    #[inline]
    fn lt(&self, other: &u8) -> bool {
        self.0.lt(other)
    }
    #[inline]
    fn le(&self, other: &u8) -> bool {
        self.0.le(other)
    }
    #[inline]
    fn gt(&self, other: &u8) -> bool {
        self.0.gt(other)
    }
    #[inline]
    fn ge(&self, other: &u8) -> bool {
        self.0.ge(other)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Serialize, Deserialize)]
pub struct Percent(Decimal);

impl Percent {
    #[must_use]
    pub fn round_dp(self, digits: u32) -> Self {
        Self(self.0.round_dp(digits))
    }
}

impl fmt::Display for Percent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.round_dp(2).fmt(f)
    }
}

impl From<Decimal> for Percent {
    fn from(decimal: Decimal) -> Self {
        Percent(decimal)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum TradingPair {
    BtcUsd,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum Position {
    Long,
    Short,
}

impl Position {
    /// Determines the counter position to the current position.
    pub fn counter_position(&self) -> Position {
        match self {
            Position::Long => Position::Short,
            Position::Short => Position::Long,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct Identity(x25519_dalek::PublicKey);

impl Identity {
    pub fn new(key: x25519_dalek::PublicKey) -> Self {
        Self(key)
    }

    pub fn pk(&self) -> x25519_dalek::PublicKey {
        self.0
    }
}

impl Serialize for Identity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Identity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let hex = String::deserialize(deserializer)?;

        let mut bytes = [0u8; 32];
        hex::decode_to_slice(&hex, &mut bytes).map_err(D::Error::custom)?;

        Ok(Self(x25519_dalek::PublicKey::from(bytes)))
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = hex::encode(self.0.as_bytes());

        write!(f, "{hex}")
    }
}

impl str::FromStr for Identity {
    type Err = hex::FromHexError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut key = [0u8; 32];

        hex::decode_to_slice(s, &mut key)?;

        Ok(Self(key.into()))
    }
}

#[derive(Debug, Clone)]
pub struct WalletInfo {
    pub balance: Amount,
    pub address: Address,
    pub last_updated_at: Timestamp,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(i64);

impl Timestamp {
    pub fn new(seconds: i64) -> Self {
        Self(seconds)
    }

    pub fn now() -> Self {
        let seconds: i64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time not to go backwards")
            .as_secs()
            .try_into()
            .expect("seconds of system time to fit into i64");

        Self(seconds)
    }

    pub fn seconds(&self) -> i64 {
        self.0
    }

    pub fn seconds_u64(&self) -> Result<u64> {
        let out = self.0.try_into().context("Unable to convert i64 to u64")?;
        Ok(out)
    }
}

/// Funding rate per SETTLEMENT_INTERVAL
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FundingRate(Decimal);

impl FundingRate {
    pub fn new(rate: Decimal) -> Result<Self> {
        anyhow::ensure!(
            rate.abs() <= Decimal::ONE,
            "Funding rate can't be higher than 100%"
        );

        Ok(Self(rate))
    }

    pub fn to_decimal(&self) -> Decimal {
        self.0
    }

    pub fn short_pays_long(&self) -> bool {
        self.0.is_sign_negative()
    }
}

impl Default for FundingRate {
    fn default() -> Self {
        Self::new(Decimal::ZERO).expect("hard-coded values to be valid")
    }
}

impl fmt::Display for FundingRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl str::FromStr for FundingRate {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let dec = Decimal::from_str(s)?;
        Ok(FundingRate(dec))
    }
}

#[derive(thiserror::Error, Debug, Clone, Copy)]
pub enum ConversionError {
    #[error("Underflow")]
    Underflow,
    #[error("Overflow")]
    Overflow,
}

/// Fee paid for the right to open a CFD.
///
/// This fee is paid by the taker to the maker.
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct OpeningFee {
    #[serde(with = "bdk::bitcoin::util::amount::serde::as_sat")]
    fee: Amount,
}

impl OpeningFee {
    pub fn new(fee: Amount) -> Self {
        Self { fee }
    }

    pub fn to_inner(self) -> Amount {
        self.fee
    }
}

impl fmt::Display for OpeningFee {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fee.as_sat().fmt(f)
    }
}

impl str::FromStr for OpeningFee {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let amount_sat: u64 = s.parse()?;
        Ok(OpeningFee {
            fee: Amount::from_sat(amount_sat),
        })
    }
}

impl Default for OpeningFee {
    fn default() -> Self {
        Self { fee: Amount::ZERO }
    }
}

/// Fee paid between takers and makers periodically.
///
/// The `fee` field represents the absolute value of this fee.
///
/// The sign of the `rate` field determines the direction of payment:
///
/// - If positive, the fee is paid from long to short.
/// - If negative, the fee is paid from short to long.
///
/// The reason for the existence of this fee is so that the party that
/// is betting against the market trend is passively rewarded for
/// keeping the CFD open.
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FundingFee {
    #[serde(with = "bdk::bitcoin::util::amount::serde::as_sat")]
    pub fee: Amount,
    pub rate: FundingRate,
}

impl FundingFee {
    pub fn calculate(
        price: Price,
        quantity: Usd,
        long_leverage: Leverage,
        short_leverage: Leverage,
        funding_rate: FundingRate,
        hours_to_charge: i64,
    ) -> Result<Self> {
        if funding_rate.0.is_zero() {
            return Ok(Self {
                fee: Amount::ZERO,
                rate: funding_rate,
            });
        }

        let margin = if funding_rate.short_pays_long() {
            calculate_margin(price, quantity, long_leverage)
        } else {
            calculate_margin(price, quantity, short_leverage)
        };

        let fraction_of_funding_period =
            if hours_to_charge as i64 == SETTLEMENT_INTERVAL.whole_hours() {
                Decimal::ONE
            } else {
                Decimal::from(hours_to_charge)
                    .checked_div(Decimal::from(SETTLEMENT_INTERVAL.whole_hours()))
                    .context("can't establish a fraction")?
            };

        let funding_fee = Decimal::from(margin.as_sat())
            * funding_rate.to_decimal().abs()
            * fraction_of_funding_period;
        let funding_fee = funding_fee
            .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::AwayFromZero)
            .to_u64()
            .context("Failed to represent as u64")?;

        Ok(Self {
            fee: Amount::from_sat(funding_fee),
            rate: funding_rate,
        })
    }

    /// Calculate the fee paid or earned for a party in a particular
    /// position.
    ///
    /// A positive sign means that the party in the `position` passed
    /// as an argument is paying the funding fee; a negative sign
    /// means that they are earning the funding fee.
    fn compute_relative(&self, position: Position) -> SignedAmount {
        let funding_rate = self.rate.0;
        let fee = self.fee.to_signed().expect("fee to fit in SignedAmount");

        // long pays short
        if funding_rate.is_sign_positive() {
            match position {
                Position::Long => fee,
                Position::Short => fee * (-1),
            }
        }
        // short pays long
        else {
            match position {
                Position::Long => fee * (-1),
                Position::Short => fee,
            }
        }
    }

    #[cfg(test)]
    fn new(fee: Amount, rate: FundingRate) -> Self {
        Self { fee, rate }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CompleteFee {
    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_sat")]
    LongPaysShort(Amount),
    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_sat")]
    ShortPaysLong(Amount),
    None,
}

/// Our own accumulated fees
///
/// The balance being positive means we owe this amount to the other party.
/// The balance being negative means that the other party owes this amount to us.
/// The counterparty fee-account balance is always the inverse of the balance.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct FeeAccount {
    balance: SignedAmount,
    position: Position,
    role: Role,
}

impl FeeAccount {
    pub fn new(position: Position, role: Role) -> Self {
        Self {
            position,
            role,
            balance: SignedAmount::ZERO,
        }
    }

    pub fn settle(&self) -> CompleteFee {
        let absolute = self.balance.as_sat().unsigned_abs();
        let absolute = Amount::from_sat(absolute);

        if self.balance == SignedAmount::ZERO {
            CompleteFee::None
        } else if (self.position == Position::Long && self.balance.is_positive())
            || (self.position == Position::Short && self.balance.is_negative())
        {
            CompleteFee::LongPaysShort(absolute)
        } else {
            CompleteFee::ShortPaysLong(absolute)
        }
    }

    pub fn balance(&self) -> SignedAmount {
        self.balance
    }

    #[must_use]
    pub fn add_opening_fee(self, opening_fee: OpeningFee) -> Self {
        let fee: i64 = opening_fee
            .fee
            .as_sat()
            .try_into()
            .expect("not to overflow");

        let signed_fee = match self.role {
            Role::Maker => -fee,
            Role::Taker => fee,
        };

        let signed_fee = SignedAmount::from_sat(signed_fee);
        let sum = self.balance + signed_fee;

        Self {
            balance: sum,
            position: self.position,
            role: self.role,
        }
    }

    #[must_use]
    pub fn add_funding_fee(self, funding_fee: FundingFee) -> Self {
        let fee: i64 = funding_fee
            .fee
            .as_sat()
            .try_into()
            .expect("not to overflow");

        let signed_fee = if (self.position == Position::Long
            && funding_fee.rate.0.is_sign_positive())
            || (self.position == Position::Short && funding_fee.rate.0.is_sign_negative())
        {
            fee
        } else {
            -fee
        };

        let signed_fee = SignedAmount::from_sat(signed_fee);
        let sum = self.balance + signed_fee;

        Self {
            balance: sum,
            position: self.position,
            role: self.role,
        }
    }

    #[must_use]
    pub fn from_complete_fee(self, fee_flow: CompleteFee) -> Self {
        match fee_flow {
            CompleteFee::LongPaysShort(amount) => {
                let fee: i64 = amount.as_sat().try_into().expect("not to overflow");

                let fee = match self.position {
                    Position::Long => fee,
                    Position::Short => -fee,
                };

                Self {
                    balance: SignedAmount::from_sat(fee),
                    ..self
                }
            }
            CompleteFee::ShortPaysLong(amount) => {
                let fee: i64 = amount.as_sat().try_into().expect("not to overflow");

                let fee = match self.position {
                    Position::Long => -fee,
                    Position::Short => fee,
                };

                Self {
                    balance: SignedAmount::from_sat(fee),
                    ..self
                }
            }
            CompleteFee::None => Self {
                balance: SignedAmount::ZERO,
                ..self
            },
        }
    }
}

/// Transaction fee in satoshis per vbyte
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct TxFeeRate(NonZeroU32);

impl TxFeeRate {
    pub fn new(fee_rate: NonZeroU32) -> Self {
        Self(fee_rate)
    }

    pub fn to_u32(self) -> u32 {
        self.0.into()
    }

    pub fn inner(&self) -> NonZeroU32 {
        self.0
    }
}

impl From<TxFeeRate> for bdk::FeeRate {
    fn from(fee_rate: TxFeeRate) -> Self {
        Self::from_sat_per_vb(fee_rate.to_u32() as f32)
    }
}

impl Default for TxFeeRate {
    fn default() -> Self {
        Self(NonZeroU32::new(1).expect("1 to be non-zero"))
    }
}

impl fmt::Display for TxFeeRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl str::FromStr for TxFeeRate {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let fee_sat = s.parse()?;
        Ok(TxFeeRate(fee_sat))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vout(u32);

impl Vout {
    pub fn new(vout: u32) -> Self {
        Self(vout)
    }
    pub fn inner(&self) -> u32 {
        self.0
    }
}

impl From<Vout> for u32 {
    fn from(vout: Vout) -> Self {
        vout.0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Fees(SignedAmount);

impl Fees {
    pub fn new(fees: SignedAmount) -> Self {
        Self(fees)
    }

    pub fn inner(self) -> SignedAmount {
        self.0
    }
}

impl From<Fees> for SignedAmount {
    fn from(fees: Fees) -> Self {
        fees.0
    }
}

impl TryFrom<i64> for Fees {
    type Error = anyhow::Error;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        Ok(Self::new(SignedAmount::from_sat(value)))
    }
}

impl From<&Fees> for i64 {
    fn from(fees: &Fees) -> Self {
        fees.0.as_sat() as i64
    }
}

/// The number of contracts per position.
#[derive(Debug, Clone, Copy)]
pub struct Contracts(u64);

impl Contracts {
    pub fn new(contracts: u64) -> Self {
        Self(contracts)
    }
}

impl From<Contracts> for u64 {
    fn from(contracts: Contracts) -> Self {
        contracts.0
    }
}

impl TryFrom<i64> for Contracts {
    type Error = anyhow::Error;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        let contracts = u64::try_from(value)?;

        Ok(Self::new(contracts))
    }
}

impl From<&Contracts> for i64 {
    fn from(contracts: &Contracts) -> Self {
        contracts.0 as i64
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Payout(Amount);

impl Payout {
    pub fn new(payout: Amount) -> Self {
        Self(payout)
    }

    pub fn inner(&self) -> Amount {
        self.0
    }
}

impl From<Payout> for SignedAmount {
    fn from(payout: Payout) -> Self {
        payout.0.to_signed().expect("Amount to fit in SignedAmount")
    }
}

impl TryFrom<i64> for Payout {
    type Error = anyhow::Error;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        let sats = u64::try_from(value)?;

        Ok(Self::new(Amount::from_sat(sats)))
    }
}

impl From<&Payout> for i64 {
    fn from(payout: &Payout) -> Self {
        payout.0.as_sat() as i64
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FailedCfd {
    pub id: OrderId,
    pub position: Position,
    pub initial_price: Price,
    pub taker_leverage: Leverage,
    pub n_contracts: Contracts,
    pub counterparty_network_identity: Identity,
    pub counterparty_peer_id: PeerId,
    pub role: Role,
    pub fees: Fees,
    pub kind: FailedKind,
    pub creation_timestamp: Timestamp,
}

/// The type of failed CFD.
#[derive(Debug, Clone, Copy)]
pub enum FailedKind {
    OfferRejected,
    ContractSetupFailed,
}

/// Representation of how a closed CFD was settled.
///
/// It is represented using an `enum` rather than a series of optional
/// fields so that only sane combinations of transactions can be
/// loaded from the database.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Settlement {
    Collaborative {
        txid: Txid,
        vout: Vout,
        payout: Payout,
        price: Price,
    },
    Cet {
        commit_txid: Txid,
        txid: Txid,
        vout: Vout,
        payout: Payout,
        price: Price,
    },
    Refund {
        commit_txid: Txid,
        txid: Txid,
        vout: Vout,
        payout: Payout,
    },
}

/// Data loaded from the database about a closed CFD.
#[derive(Debug, Clone, Copy)]
pub struct ClosedCfd {
    pub id: OrderId,
    pub position: Position,
    pub initial_price: Price,
    pub taker_leverage: Leverage,
    pub n_contracts: Contracts,
    pub counterparty_network_identity: Identity,
    pub counterparty_peer_id: PeerId,
    pub role: Role,
    pub fees: Fees,
    pub expiry_timestamp: OffsetDateTime,
    pub lock: Lock,
    pub settlement: Settlement,
    pub creation_timestamp: Timestamp,
}

/// Data loaded from the database about the lock transaction of a
/// closed CFD.
#[derive(Debug, Clone, Copy)]
pub struct Lock {
    pub txid: Txid,
    pub dlc_vout: Vout,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn algebra_with_usd() {
        let usd_0 = Usd::new(dec!(1.234));
        let usd_1 = Usd::new(dec!(9.876));

        let usd_sum = usd_0 + usd_1;
        let usd_diff = usd_0 - usd_1;
        let half = usd_0 / 2;
        let double = usd_1 * 2;

        assert_eq!(usd_sum.0, dec!(11.110));
        assert_eq!(usd_diff.0, dec!(-8.642));
        assert_eq!(half.0, dec!(0.617));
        assert_eq!(double.0, dec!(19.752));
    }

    #[test]
    fn usd_for_1_btc_buys_1_btc() {
        let usd = Usd::new(dec!(61234.5678));
        let price = Price::new(dec!(61234.5678)).unwrap();
        let inv_price = InversePrice::new(price).unwrap();
        let res_0 = usd / price;
        let res_1 = usd * inv_price;

        assert_eq!(res_0, Amount::ONE_BTC);
        assert_eq!(res_1, Amount::ONE_BTC);
    }

    #[test]
    fn leverage_does_not_alter_type() {
        let usd = Usd::new(dec!(61234.5678));
        let leverage = Leverage::new(3).unwrap();
        let res = usd * leverage / leverage;

        assert_eq!(res.0, usd.0);
    }

    #[test]
    fn test_algebra_with_types() {
        let usd = Usd::new(dec!(61234.5678));
        let leverage = Leverage::new(5).unwrap();
        let price = Price::new(dec!(61234.5678)).unwrap();
        let expected_buyin = Amount::from_str_in("0.2", Denomination::Bitcoin).unwrap();

        let liquidation_price = price * leverage / (leverage + 1);
        let inv_price = InversePrice::new(price).unwrap();
        let inv_liquidation_price = InversePrice::new(liquidation_price).unwrap();

        let long_buyin = usd / (price * leverage);
        let long_payout =
            (usd / leverage) * ((leverage + 1) * inv_price - leverage * inv_liquidation_price);

        assert_eq!(long_buyin, expected_buyin);
        assert_eq!(long_payout, Amount::ZERO);
    }

    #[test]
    fn roundtrip_identity_serde() {
        let id = Identity::new(x25519_dalek::PublicKey::from([42u8; 32]));

        serde_test::assert_tokens(
            &id,
            &[serde_test::Token::String(
                "2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a",
            )],
        );
    }

    #[test]
    fn long_taker_pays_opening_fee_to_maker() {
        let opening_fee = OpeningFee::new(Amount::from_sat(500));

        let long_taker = FeeAccount::new(Position::Long, Role::Taker)
            .add_opening_fee(opening_fee)
            .settle();
        let short_maker = FeeAccount::new(Position::Short, Role::Maker)
            .add_opening_fee(opening_fee)
            .settle();

        assert_eq!(
            long_taker,
            CompleteFee::LongPaysShort(Amount::from_sat(500))
        );
        assert_eq!(
            short_maker,
            CompleteFee::LongPaysShort(Amount::from_sat(500))
        );
    }

    #[test]
    fn short_taker_pays_opening_fee_to_maker() {
        let opening_fee = OpeningFee::new(Amount::from_sat(500));

        let short_taker = FeeAccount::new(Position::Short, Role::Taker)
            .add_opening_fee(opening_fee)
            .settle();
        let long_maker = FeeAccount::new(Position::Long, Role::Maker)
            .add_opening_fee(opening_fee)
            .settle();

        assert_eq!(
            short_taker,
            CompleteFee::ShortPaysLong(Amount::from_sat(500))
        );
        assert_eq!(
            long_maker,
            CompleteFee::ShortPaysLong(Amount::from_sat(500))
        );
    }

    #[test]
    fn long_pays_short_with_positive_funding_rate() {
        let funding_fee = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(0.001)).unwrap(),
        );

        let long_taker = FeeAccount::new(Position::Long, Role::Taker)
            .add_funding_fee(funding_fee)
            .add_funding_fee(funding_fee)
            .settle();
        let short_maker = FeeAccount::new(Position::Short, Role::Maker)
            .add_funding_fee(funding_fee)
            .add_funding_fee(funding_fee)
            .settle();

        assert_eq!(
            long_taker,
            CompleteFee::LongPaysShort(Amount::from_sat(1000))
        );
        assert_eq!(
            short_maker,
            CompleteFee::LongPaysShort(Amount::from_sat(1000))
        );
    }

    #[test]
    fn fee_account_handles_balance_of_zero() {
        let funding_fee_with_positive_rate = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(0.001)).unwrap(),
        );
        let funding_fee_with_negative_rate = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(-0.001)).unwrap(),
        );

        let long_taker = FeeAccount::new(Position::Long, Role::Taker)
            .add_funding_fee(funding_fee_with_positive_rate)
            .add_funding_fee(funding_fee_with_negative_rate)
            .settle();
        let short_maker = FeeAccount::new(Position::Short, Role::Maker)
            .add_funding_fee(funding_fee_with_positive_rate)
            .add_funding_fee(funding_fee_with_negative_rate)
            .settle();

        assert_eq!(long_taker, CompleteFee::None);
        assert_eq!(short_maker, CompleteFee::None);
    }

    #[test]
    fn fee_account_handles_negative_funding_rate() {
        let funding_fee = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(-0.001)).unwrap(),
        );

        let long_taker = FeeAccount::new(Position::Long, Role::Taker)
            .add_funding_fee(funding_fee)
            .add_funding_fee(funding_fee)
            .settle();
        let short_maker = FeeAccount::new(Position::Short, Role::Maker)
            .add_funding_fee(funding_fee)
            .add_funding_fee(funding_fee)
            .settle();

        assert_eq!(
            long_taker,
            CompleteFee::ShortPaysLong(Amount::from_sat(1000))
        );
        assert_eq!(
            short_maker,
            CompleteFee::ShortPaysLong(Amount::from_sat(1000))
        );
    }

    #[test]
    fn long_taker_short_maker_roundtrip() {
        let opening_fee = OpeningFee::new(Amount::from_sat(100));
        let funding_fee_with_positive_rate = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(0.001)).unwrap(),
        );
        let funding_fee_with_negative_rate = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(-0.001)).unwrap(),
        );

        let long_taker = FeeAccount::new(Position::Long, Role::Taker)
            .add_opening_fee(opening_fee)
            .add_funding_fee(funding_fee_with_positive_rate);
        let short_maker = FeeAccount::new(Position::Short, Role::Maker)
            .add_opening_fee(opening_fee)
            .add_funding_fee(funding_fee_with_positive_rate);

        assert_eq!(
            long_taker.settle(),
            CompleteFee::LongPaysShort(Amount::from_sat(600))
        );
        assert_eq!(
            short_maker.settle(),
            CompleteFee::LongPaysShort(Amount::from_sat(600))
        );

        let long_taker = long_taker.add_funding_fee(funding_fee_with_negative_rate);
        let short_maker = short_maker.add_funding_fee(funding_fee_with_negative_rate);

        assert_eq!(
            long_taker.settle(),
            CompleteFee::LongPaysShort(Amount::from_sat(100))
        );
        assert_eq!(
            short_maker.settle(),
            CompleteFee::LongPaysShort(Amount::from_sat(100))
        );

        let long_taker = long_taker.add_funding_fee(funding_fee_with_negative_rate);
        let short_maker = short_maker.add_funding_fee(funding_fee_with_negative_rate);

        assert_eq!(
            long_taker.settle(),
            CompleteFee::ShortPaysLong(Amount::from_sat(400))
        );
        assert_eq!(
            short_maker.settle(),
            CompleteFee::ShortPaysLong(Amount::from_sat(400))
        );
    }

    #[test]
    fn long_maker_short_taker_roundtrip() {
        let opening_fee = OpeningFee::new(Amount::from_sat(100));
        let funding_fee_with_positive_rate = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(0.001)).unwrap(),
        );
        let funding_fee_with_negative_rate = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(-0.001)).unwrap(),
        );

        let long_maker = FeeAccount::new(Position::Long, Role::Maker)
            .add_opening_fee(opening_fee)
            .add_funding_fee(funding_fee_with_positive_rate);
        let short_taker = FeeAccount::new(Position::Short, Role::Taker)
            .add_opening_fee(opening_fee)
            .add_funding_fee(funding_fee_with_positive_rate);

        assert_eq!(
            long_maker.settle(),
            CompleteFee::LongPaysShort(Amount::from_sat(400))
        );
        assert_eq!(
            short_taker.settle(),
            CompleteFee::LongPaysShort(Amount::from_sat(400))
        );

        let long_maker = long_maker.add_funding_fee(funding_fee_with_negative_rate);
        let short_taker = short_taker.add_funding_fee(funding_fee_with_negative_rate);

        assert_eq!(
            long_maker.settle(),
            CompleteFee::ShortPaysLong(Amount::from_sat(100))
        );
        assert_eq!(
            short_taker.settle(),
            CompleteFee::ShortPaysLong(Amount::from_sat(100))
        );

        let long_maker = long_maker.add_funding_fee(funding_fee_with_negative_rate);
        let short_taker = short_taker.add_funding_fee(funding_fee_with_negative_rate);

        assert_eq!(
            long_maker.settle(),
            CompleteFee::ShortPaysLong(Amount::from_sat(600))
        );
        assert_eq!(
            short_taker.settle(),
            CompleteFee::ShortPaysLong(Amount::from_sat(600))
        );
    }

    #[test]
    fn given_positive_rate_then_positive_taker_long_balance() {
        let funding_fee = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(0.001)).unwrap(),
        );

        let balance = FeeAccount::new(Position::Long, Role::Taker)
            .add_funding_fee(funding_fee)
            .add_funding_fee(funding_fee)
            .balance();

        assert_eq!(balance, SignedAmount::from_sat(1000))
    }

    #[test]
    fn given_positive_rate_then_negative_short_maker_balance() {
        let funding_fee = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(0.001)).unwrap(),
        );

        let balance = FeeAccount::new(Position::Short, Role::Maker)
            .add_funding_fee(funding_fee)
            .add_funding_fee(funding_fee)
            .balance();

        assert_eq!(balance, SignedAmount::from_sat(-1000))
    }

    #[test]
    fn given_negative_rate_then_negative_taker_long_balance() {
        let funding_fee = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(-0.001)).unwrap(),
        );

        let balance = FeeAccount::new(Position::Long, Role::Taker)
            .add_funding_fee(funding_fee)
            .add_funding_fee(funding_fee)
            .balance();

        assert_eq!(balance, SignedAmount::from_sat(-1000))
    }

    #[test]
    fn given_negative_rate_then_positive_short_maker_balance() {
        let funding_fee = FundingFee::new(
            Amount::from_sat(500),
            FundingRate::new(dec!(-0.001)).unwrap(),
        );

        let balance = FeeAccount::new(Position::Short, Role::Maker)
            .add_funding_fee(funding_fee)
            .add_funding_fee(funding_fee)
            .balance();

        assert_eq!(balance, SignedAmount::from_sat(1000))
    }

    #[test]
    fn proportional_funding_fees_if_sign_of_funding_rate_changes() {
        let long_leverage = Leverage::TWO;
        let short_leverage = Leverage::ONE;

        let funding_rate_pos = FundingRate::new(dec!(0.01)).unwrap();
        let long_pays_short_fee = FundingFee::calculate(
            dummy_price(),
            dummy_n_contracts(),
            long_leverage,
            short_leverage,
            funding_rate_pos,
            dummy_settlement_interval(),
        )
        .unwrap();

        let funding_rate_neg = FundingRate::new(dec!(-0.01)).unwrap();
        let short_pays_long_fee = FundingFee::calculate(
            dummy_price(),
            dummy_n_contracts(),
            long_leverage,
            short_leverage,
            funding_rate_neg,
            dummy_settlement_interval(),
        )
        .unwrap();

        let epsilon = (long_pays_short_fee.fee.as_sat() as i64)
            - (short_pays_long_fee.fee.as_sat() as i64) * (long_leverage.get() as i64);
        assert!(epsilon.abs() < 5)
    }

    #[test]
    fn zero_funding_fee_if_zero_funding_rate() {
        let zero_funding_rate = FundingRate::new(Decimal::ZERO).unwrap();

        let dummy_leverage = Leverage::new(1).unwrap();
        let fee = FundingFee::calculate(
            dummy_price(),
            dummy_n_contracts(),
            dummy_leverage,
            dummy_leverage,
            zero_funding_rate,
            dummy_settlement_interval(),
        )
        .unwrap();

        assert_eq!(fee.fee, Amount::ZERO)
    }

    #[test]
    fn given_positive_funding_rate_when_position_long_then_relative_fee_is_positive() {
        let positive_funding_rate = FundingRate::new(dec!(0.001)).unwrap();
        let long = Position::Long;

        let funding_fee = FundingFee::new(dummy_amount(), positive_funding_rate);
        let relative = funding_fee.compute_relative(long);

        assert!(relative.is_positive())
    }

    #[test]
    fn given_positive_funding_rate_when_position_short_then_relative_fee_is_negative() {
        let positive_funding_rate = FundingRate::new(dec!(0.001)).unwrap();
        let short = Position::Short;

        let funding_fee = FundingFee::new(dummy_amount(), positive_funding_rate);
        let relative = funding_fee.compute_relative(short);

        assert!(relative.is_negative())
    }

    #[test]
    fn given_negative_funding_rate_when_position_long_then_relative_fee_is_negative() {
        let negative_funding_rate = FundingRate::new(dec!(-0.001)).unwrap();
        let long = Position::Long;

        let funding_fee = FundingFee::new(dummy_amount(), negative_funding_rate);
        let relative = funding_fee.compute_relative(long);

        assert!(relative.is_negative())
    }

    #[test]
    fn given_negative_funding_rate_when_position_short_then_relative_fee_is_positive() {
        let negative_funding_rate = FundingRate::new(dec!(-0.001)).unwrap();
        let short = Position::Short;

        let funding_fee = FundingFee::new(dummy_amount(), negative_funding_rate);
        let relative = funding_fee.compute_relative(short);

        assert!(relative.is_positive())
    }

    #[test]
    fn given_long_fee_account_when_long_pays_short_from_complete_fee_then_same_after_settle() {
        let fee_account = FeeAccount::new(Position::Long, Role::Taker);

        let complete_fee = CompleteFee::LongPaysShort(Amount::from_sat(100));
        let fee_account = fee_account.from_complete_fee(complete_fee);

        let expected_complete_fee = fee_account.settle();

        assert_eq!(complete_fee, expected_complete_fee)
    }

    #[test]
    fn given_long_fee_account_when_short_pays_long_from_complete_fee_then_same_after_settle() {
        let fee_account = FeeAccount::new(Position::Long, Role::Taker);

        let complete_fee = CompleteFee::ShortPaysLong(Amount::from_sat(100));
        let fee_account = fee_account.from_complete_fee(complete_fee);

        let expected_complete_fee = fee_account.settle();

        assert_eq!(complete_fee, expected_complete_fee)
    }

    #[test]
    fn given_short_fee_account_when_long_pays_short_from_complete_fee_then_same_after_settle() {
        let fee_account = FeeAccount::new(Position::Short, Role::Taker);

        let complete_fee = CompleteFee::LongPaysShort(Amount::from_sat(100));
        let fee_account = fee_account.from_complete_fee(complete_fee);

        let expected_complete_fee = fee_account.settle();

        assert_eq!(complete_fee, expected_complete_fee)
    }

    #[test]
    fn given_short_fee_account_when_short_pays_long_from_complete_fee_then_same_after_settle() {
        let fee_account = FeeAccount::new(Position::Short, Role::Taker);

        let complete_fee = CompleteFee::ShortPaysLong(Amount::from_sat(100));
        let fee_account = fee_account.from_complete_fee(complete_fee);

        let expected_complete_fee = fee_account.settle();

        assert_eq!(complete_fee, expected_complete_fee)
    }

    #[test]
    fn given_fee_account_that_contains_funds_when_from_complete_fee_then_complete_fee() {
        let fee_account = FeeAccount::new(Position::Short, Role::Taker)
            .add_opening_fee(OpeningFee::new(Amount::from_sat(100)));

        let complete_fee = CompleteFee::ShortPaysLong(Amount::from_sat(100));
        let fee_account = fee_account.from_complete_fee(complete_fee);

        let expected_complete_fee = fee_account.settle();

        assert_eq!(complete_fee, expected_complete_fee)
    }

    fn dummy_amount() -> Amount {
        Amount::from_sat(500)
    }

    fn dummy_price() -> Price {
        Price::new(dec!(35_000)).expect("to not fail")
    }

    fn dummy_n_contracts() -> Usd {
        Usd::new(dec!(100))
    }

    fn dummy_settlement_interval() -> i64 {
        8
    }
}
