use crate::olivia;
use crate::olivia::BitMexPriceEventId;
use crate::rollover;
use crate::Cfd;
use crate::CfdEvent;
use crate::CompleteFee;
use crate::Dlc;
use crate::EventKind;
use crate::FeeAccount;
use crate::FundingFee;
use crate::FundingRate;
use crate::Position;
use crate::Role;
use crate::RolloverParams;
use crate::TxFeeRate;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use time::OffsetDateTime;

impl Cfd {
    pub fn accept_rollover_proposal_single_event(
        self,
        tx_fee_rate: TxFeeRate,
        funding_rate: FundingRate,
        from_params: Option<(BitMexPriceEventId, CompleteFee)>,
        version: rollover::Version,
    ) -> Result<(CfdEvent, RolloverParams, Dlc, Position, BitMexPriceEventId)> {
        if !self.during_rollover {
            bail!("The CFD is not rolling over");
        }

        if self.role != Role::Maker {
            bail!("Can only accept proposal as a maker");
        }

        let now = OffsetDateTime::now_utc();
        let to_event_id = olivia::next_announcement_after(now + self.settlement_interval);

        // If a `from_event_id` was specified we use it, otherwise we use the
        // `settlement_event_id` of the current dlc to calculate the costs.
        let (from_event_id, rollover_fee_account) = match from_params {
            None => {
                let from_event_id = self
                    .dlc
                    .as_ref()
                    .context("Cannot roll over without DLC")?
                    .settlement_event_id;

                (from_event_id, self.fee_account)
            }
            Some((from_event_id, from_complete_fee)) => {
                // If we have rollover params we make sure to use the complete_fee as decided by the
                // params
                let rollover_fee_account =
                    FeeAccount::new(self.position, self.role).from_complete_fee(from_complete_fee);
                (from_event_id, rollover_fee_account)
            }
        };

        let hours_to_charge = match version {
            rollover::Version::V1 => 1,
            rollover::Version::V2 => self.hours_to_extend_in_rollover(now)?,
            rollover::Version::V3 => {
                self.hours_to_extend_in_rollover_based_on_event(to_event_id, now, from_event_id)?
            }
        };

        let funding_fee = FundingFee::calculate(
            self.initial_price,
            self.quantity,
            self.long_leverage,
            self.short_leverage,
            funding_rate,
            hours_to_charge as i64,
        )?;

        tracing::debug!(
            order_id = %self.id,
            rollover_version = %version,
            %hours_to_charge,
            funding_fee = %funding_fee.compute_relative(self.position),
            "Accepting rollover proposal"
        );

        Ok((
            CfdEvent::new(self.id, EventKind::RolloverAccepted),
            RolloverParams::new(
                self.initial_price,
                self.quantity,
                self.long_leverage,
                self.short_leverage,
                self.refund_timelock_in_blocks(),
                tx_fee_rate,
                rollover_fee_account,
                funding_fee,
                version,
            ),
            self.dlc.clone().context("No DLC present")?,
            self.position,
            to_event_id,
        ))
    }

    pub fn handle_rollover_accepted_taker_single_event(
        &self,
        tx_fee_rate: TxFeeRate,
        funding_rate: FundingRate,
        from_event_id: BitMexPriceEventId,
    ) -> Result<(CfdEvent, RolloverParams, Dlc, Position)> {
        if !self.during_rollover {
            bail!("The CFD is not rolling over");
        }

        if self.role != Role::Taker {
            bail!("Can only handle accepted proposal as a taker");
        }

        self.can_rollover()?;

        let now = OffsetDateTime::now_utc();

        let to_event_id = olivia::next_announcement_after(now + self.settlement_interval);

        // TODO: This should not be calculated here but we should just rely on `complete_fee`
        //  This requires more refactoring because the `RolloverCompleted` event currently depends
        //  on the `funding_fee` from the `RolloverParams`.
        let hours_to_charge =
            self.hours_to_extend_in_rollover_based_on_event(to_event_id, now, from_event_id)?;
        let funding_fee = FundingFee::calculate(
            self.initial_price,
            self.quantity,
            self.long_leverage,
            self.short_leverage,
            funding_rate,
            hours_to_charge as i64,
        )?;

        Ok((
            self.event(EventKind::RolloverAccepted),
            RolloverParams::new(
                self.initial_price,
                self.quantity,
                self.long_leverage,
                self.short_leverage,
                self.refund_timelock_in_blocks(),
                tx_fee_rate,
                self.fee_account,
                funding_fee,
                rollover::Version::V2,
            ),
            self.dlc.clone().context("No DLC present")?,
            self.position,
        ))
    }
}
