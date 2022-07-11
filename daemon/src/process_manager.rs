use crate::monitor::MonitorCetFinality;
use crate::monitor::MonitorCollaborativeSettlement;
use crate::monitor::MonitorParams;
use crate::monitor::StartMonitoring;
use crate::monitor::TransactionKind;
use crate::monitor::TryBroadcastTransaction;
use crate::oracle;
use crate::position_metrics;
use crate::projection;
use anyhow::Result;
use async_trait::async_trait;
use model::CfdEvent;
use model::EventKind;
use model::Role;
use sqlite_db;
use xtra::prelude::MessageChannel;
use xtra_productivity::xtra_productivity;
use xtras::SendAsyncSafe;

pub struct Actor {
    db: sqlite_db::Connection,
    role: Role,
    cfds_changed: MessageChannel<projection::CfdChanged, ()>,
    cfd_changed_metrics: MessageChannel<position_metrics::CfdChanged, ()>,
    try_broadcast_transaction: MessageChannel<TryBroadcastTransaction, Result<()>>,
    start_monitoring: MessageChannel<StartMonitoring, ()>,
    monitor_cet_finality: MessageChannel<MonitorCetFinality, Result<()>>,
    monitor_collaborative_settlement: MessageChannel<MonitorCollaborativeSettlement, ()>,
    monitor_attestation: MessageChannel<oracle::MonitorAttestations, ()>,
}

pub struct Event(CfdEvent);

impl Event {
    pub fn new(event: CfdEvent) -> Self {
        Self(event)
    }
}

impl Actor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: sqlite_db::Connection,
        role: Role,
        cfds_changed: MessageChannel<projection::CfdChanged, ()>,
        cfd_changed_metrics: MessageChannel<position_metrics::CfdChanged, ()>,
        try_broadcast_transaction: MessageChannel<TryBroadcastTransaction, Result<()>>,
        start_monitoring: MessageChannel<StartMonitoring, ()>,
        monitor_cet_finality: MessageChannel<MonitorCetFinality, Result<()>>,
        monitor_collaborative_settlement: MessageChannel<MonitorCollaborativeSettlement, ()>,
        monitor_attestation: MessageChannel<oracle::MonitorAttestations, ()>,
    ) -> Self {
        Self {
            db,
            role,
            cfds_changed,
            cfd_changed_metrics,
            try_broadcast_transaction,
            start_monitoring,
            monitor_cet_finality,
            monitor_collaborative_settlement,
            monitor_attestation,
        }
    }
}

#[xtra_productivity]
impl Actor {
    fn handle(&mut self, msg: Event) -> Result<()> {
        let event = msg.0;

        // 1. Safe in DB
        self.db.append_event(event.clone()).await?;

        // 2. Post process event
        use EventKind::*;
        match event.event {
            ContractSetupCompleted { dlc: Some(dlc), .. } => {
                let lock_tx = dlc.lock.0.clone();
                self.try_broadcast_transaction
                    .send_async_safe(TryBroadcastTransaction {
                        tx: lock_tx,
                        kind: TransactionKind::Lock,
                    })
                    .await?;

                self.start_monitoring
                    .send_async_safe(StartMonitoring {
                        id: event.id,
                        params: MonitorParams::new(dlc.clone()),
                    })
                    .await?;

                self.monitor_attestation
                    .send_async_safe(oracle::MonitorAttestations {
                        event_ids: dlc.event_ids(),
                    })
                    .await?;
            }
            CollaborativeSettlementCompleted {
                spend_tx, script, ..
            } => {
                let txid = spend_tx.txid();

                match self.role {
                    Role::Maker => {
                        self.try_broadcast_transaction
                            .send_async_safe(TryBroadcastTransaction {
                                tx: spend_tx,
                                kind: TransactionKind::CollaborativeClose,
                            })
                            .await?;
                    }
                    Role::Taker => {
                        // TODO: Publish the tx once the collaborative settlement is symmetric,
                        // allowing the taker to publish as well.
                    }
                };

                self.monitor_collaborative_settlement
                    .send_async_safe(MonitorCollaborativeSettlement {
                        order_id: event.id,
                        tx: (txid, script),
                    })
                    .await?;
            }
            CetTimelockExpiredPostOracleAttestation { cet } => {
                let _ = self
                    .monitor_cet_finality
                    .send_async_safe(MonitorCetFinality {
                        order_id: event.id,
                        cet: cet.clone(),
                    })
                    .await?;
                self.try_broadcast_transaction
                    .send_async_safe(TryBroadcastTransaction {
                        tx: cet,
                        kind: TransactionKind::Cet,
                    })
                    .await?;
            }
            OracleAttestedPostCetTimelock { cet, .. } => {
                let _ = self
                    .monitor_cet_finality
                    .send_async_safe(MonitorCetFinality {
                        order_id: event.id,
                        cet: cet.clone(),
                    })
                    .await?;
                self.try_broadcast_transaction
                    .send_async_safe(TryBroadcastTransaction {
                        tx: cet,
                        kind: TransactionKind::Cet,
                    })
                    .await?;
            }
            OracleAttestedPriorCetTimelock {
                commit_tx: Some(commit_tx),
                ..
            } => {
                self.try_broadcast_transaction
                    .send_async_safe(TryBroadcastTransaction {
                        tx: commit_tx,
                        kind: TransactionKind::Commit,
                    })
                    .await?;
            }
            ManualCommit { tx } => {
                self.try_broadcast_transaction
                    .send_async_safe(TryBroadcastTransaction {
                        tx,
                        kind: TransactionKind::Commit,
                    })
                    .await?;
            }
            OracleAttestedPriorCetTimelock {
                commit_tx: None,
                timelocked_cet: cet,
                ..
            } => {
                let _ = self
                    .monitor_cet_finality
                    .send_async_safe(MonitorCetFinality {
                        order_id: event.id,
                        cet,
                    })
                    .await?;
            }
            RolloverCompleted { dlc: Some(dlc), .. } => {
                self.start_monitoring
                    .send_async_safe(StartMonitoring {
                        id: event.id,
                        params: MonitorParams::new(dlc.clone()),
                    })
                    .await?;

                self.monitor_attestation
                    .send_async_safe(oracle::MonitorAttestations {
                        event_ids: dlc.event_ids(),
                    })
                    .await?;
            }
            RefundTimelockExpired { refund_tx: tx } => {
                self.try_broadcast_transaction
                    .send_async_safe(TryBroadcastTransaction {
                        tx,
                        kind: TransactionKind::Refund,
                    })
                    .await?;
            }
            ContractSetupCompleted { dlc: None, .. }
            | RolloverCompleted { dlc: None, .. }
            | RefundConfirmed
            | CollaborativeSettlementStarted { .. }
            | ContractSetupStarted
            | ContractSetupFailed
            | OfferRejected
            | RolloverStarted
            | RolloverAccepted
            | RolloverRejected
            | RolloverFailed
            | CollaborativeSettlementProposalAccepted
            | LockConfirmed
            | LockConfirmedAfterFinality
            | CommitConfirmed
            | CetConfirmed
            | RevokeConfirmed
            | CollaborativeSettlementConfirmed
            | CollaborativeSettlementRejected
            | CollaborativeSettlementFailed
            | CetTimelockExpiredPriorOracleAttestation => {}
        }

        // 3. Update UI
        self.cfds_changed
            .send_async_safe(projection::CfdChanged(event.id))
            .await?;

        // 4. Update metrics
        self.cfd_changed_metrics
            .send_async_safe(position_metrics::CfdChanged(event.id))
            .await?;

        Ok(())
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}
