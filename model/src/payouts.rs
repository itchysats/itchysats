use crate::CompleteFee;
use crate::Leverage;
use crate::Position;
use crate::Price;
use crate::Role;
use crate::Usd;
use anyhow::Result;
use itertools::Itertools;
use maia_core::generate_payouts;
use maia_core::Payout;

mod payout_curve;

pub struct Payouts {
    /// The full range of payout combinations by which a CFD can be
    /// settled.
    settlement: Vec<Payout>,
}

impl Payouts {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        position: Position,
        role: Role,
        price: Price,
        quantity: Usd,
        long_leverage: Leverage,
        short_leverage: Leverage,
        n_payouts: usize,
        fee: CompleteFee,
    ) -> Result<Self> {
        let payouts = payout_curve::calculate(
            price,
            quantity,
            long_leverage,
            short_leverage,
            n_payouts,
            fee,
        )?;

        let settlement = match (position, role) {
            (Position::Long, Role::Taker) | (Position::Short, Role::Maker) => payouts
                .into_iter()
                .map(|payout| generate_payouts(payout.range, payout.short, payout.long))
                .flatten_ok()
                .try_collect()?,
            (Position::Short, Role::Taker) | (Position::Long, Role::Maker) => payouts
                .into_iter()
                .map(|payout| generate_payouts(payout.range, payout.long, payout.short))
                .flatten_ok()
                .try_collect()?,
        };

        Ok(Self { settlement })
    }

    pub fn settlement(&self) -> Vec<Payout> {
        self.settlement.clone()
    }
}
