use crate::olivia;
use crate::CompleteFee;
use crate::Leverage;
use crate::Position;
use crate::Price;
use crate::Role;
use crate::Usd;
use anyhow::bail;
use anyhow::Result;
use itertools::Itertools;
use maia_core::generate_payouts;
use maia_core::Announcement;
use maia_core::Payout;
use std::collections::HashMap;

mod payout_curve;

/// Payout combinations associated with the oracle events that may
/// trigger them.
pub struct OraclePayouts(HashMap<Announcement, Vec<Payout>>);

impl OraclePayouts {
    pub fn new(payouts: Payouts, announcements: Vec<olivia::Announcement>) -> Result<Self> {
        let announcements = Announcements::new(announcements)?;

        let settlement = (announcements.settlement, payouts.settlement);
        let long_liquidations = announcements
            .liquidation
            .clone()
            .into_iter()
            .map(|announcement| (announcement, vec![payouts.long_liquidation.clone()]));
        let short_liquidations = announcements
            .liquidation
            .into_iter()
            .map(|announcement| (announcement, vec![payouts.short_liquidation.clone()]));

        Ok(Self(HashMap::from_iter(
            [settlement]
                .into_iter()
                .chain(long_liquidations)
                .chain(short_liquidations),
        )))
    }
}

impl From<OraclePayouts> for HashMap<Announcement, Vec<Payout>> {
    fn from(from: OraclePayouts) -> Self {
        from.0
    }
}

pub struct Payouts {
    /// The full range of payout combinations by which a CFD can be
    /// settled.
    settlement: Vec<Payout>,
    /// The payout combination which corresponds to the party with the
    /// long position being liquidated.
    long_liquidation: Payout,
    /// The payout combination which corresponds to the party with the
    /// short position being liquidated.
    short_liquidation: Payout,
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

        let settlement: Vec<_> = match (position, role) {
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

        let long_liquidation = settlement.first().expect("several payouts").clone();
        let short_liquidation = settlement.last().expect("several payouts").clone();

        Ok(Self {
            settlement,
            long_liquidation,
            short_liquidation,
        })
    }

    pub fn settlement(&self) -> Vec<Payout> {
        self.settlement.clone()
    }

    pub fn long_liquidation(&self) -> &Payout {
        &self.long_liquidation
    }

    pub fn short_liquidation(&self) -> &Payout {
        &self.short_liquidation
    }
}

struct Announcements {
    /// The announcement which corresponds to the oracle event that
    /// will mark the end of an epoch for a CFD.
    settlement: Announcement,
    /// All the intermediate oracle announcements between the start
    /// and end of an epoch for a CFD.
    liquidation: Vec<Announcement>,
}

impl Announcements {
    fn new(announcements: Vec<olivia::Announcement>) -> Result<Self> {
        let announcements = announcements
            .into_iter()
            .sorted_by(|a, b| a.id.cmp(&b.id))
            .map(|announcement| Announcement {
                id: announcement.id.to_string(),
                nonce_pks: announcement.nonce_pks,
            })
            .collect_vec();

        let (liquidation, settlement) = match announcements.as_slice() {
            [] => bail!("Need at least one announcement to construct"),
            [beginning @ .., last] => (beginning.to_vec(), last.clone()),
        };

        Ok(Self {
            settlement,
            liquidation,
        })
    }
}
