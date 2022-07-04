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

#[allow(clippy::too_many_arguments)]
pub fn calculate(
    position: Position,
    role: Role,
    price: Price,
    quantity: Usd,
    long_leverage: Leverage,
    short_leverage: Leverage,
    n_payouts: usize,
    fee: CompleteFee,
) -> Result<Vec<Payout>> {
    let payouts = payout_curve::calculate(
        price,
        quantity,
        long_leverage,
        short_leverage,
        n_payouts,
        fee,
    )?;

    match (position, role) {
        (Position::Long, Role::Taker) | (Position::Short, Role::Maker) => payouts
            .into_iter()
            .map(|payout| generate_payouts(payout.range, payout.short, payout.long))
            .flatten_ok()
            .collect(),
        (Position::Short, Role::Taker) | (Position::Long, Role::Maker) => payouts
            .into_iter()
            .map(|payout| generate_payouts(payout.range, payout.long, payout.short))
            .flatten_ok()
            .collect(),
    }
}
