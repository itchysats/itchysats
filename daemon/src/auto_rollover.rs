use crate::command;
use crate::rollover;
use crate::rollover::taker::ProposeRollover;
use crate::Txid;
use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use model::libp2p::PeerId;
use model::olivia::BitMexPriceEventId;
use model::OrderId;
use sqlite_db;
use std::time::Duration;
use time::OffsetDateTime;
use xtra::Address;
use xtra_productivity::xtra_productivity;
use xtras::SendAsyncSafe;
use xtras::SendInterval;

pub struct Actor {
    db: sqlite_db::Connection,
    libp2p_rollover: Address<rollover::taker::Actor<command::Executor>>,
}

impl Actor {
    pub fn new(
        db: sqlite_db::Connection,
        libp2p_rollover: Address<rollover::taker::Actor<command::Executor>>,
    ) -> Self {
        Self {
            db,
            libp2p_rollover,
        }
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle(&mut self, _msg: AutoRollover, ctx: &mut xtra::Context<Self>) {
        tracing::trace!("Checking all CFDs for rollover eligibility");

        // Auto-rollover is invoked periodically by `addr.send_interval()`,
        // which does not handle errors - forward implementation to allow `?` inside
        if let Err(e) = self.handle_auto_rollover_impl(ctx).await {
            tracing::error!("Auto-rollover failed: {:#}", e);
        }
    }

    async fn handle(
        &mut self,
        Rollover {
            order_id,
            maker_peer_id,
            from_commit_txid,
            from_settlement_event_id,
        }: Rollover,
    ) {
        if let Some(maker_peer_id) = maker_peer_id {
            if let Err(e) = self
                .libp2p_rollover
                .send(ProposeRollover {
                    order_id,
                    maker_peer_id,
                    from_commit_txid,
                    from_settlement_event_id,
                })
                .await
            {
                tracing::error!(%order_id, "Failed to dispatch proposal to libp2p rollover actor: {e:#}");
            }
        } else {
            unreachable!("this should not happen on the taker side, we always know the peer id ,")
        }
    }
}

impl Actor {
    async fn handle_auto_rollover_impl(
        &mut self,
        ctx: &mut xtra::Context<Actor>,
    ) -> Result<(), anyhow::Error> {
        let this = ctx
            .address()
            .expect("actor to be able to give address to itself");

        let mut stream = self.db.load_all_open_cfds::<model::Cfd>(());

        while let Some(cfd) = stream.next().await {
            let cfd: model::Cfd = match cfd {
                Ok(cfd) => cfd,
                Err(e) => {
                    tracing::warn!("Failed to load CFD from database: {e:#}");
                    continue;
                }
            };
            let id = cfd.id();
            let maker_peer_id = cfd.counterparty_peer_id();

            match cfd.can_auto_rollover_taker(OffsetDateTime::now_utc()) {
                Ok((from_commit_txid, from_settlement_event_id)) => {
                    // If we disconnect, we don't care.
                    let _ = this
                        .send_async_safe(Rollover {
                            order_id: id,
                            maker_peer_id,
                            from_commit_txid,
                            from_settlement_event_id,
                        })
                        .await;
                }
                Err(reason) => {
                    tracing::trace!(order_id = %id, %reason, "CFD is not eligible for auto-rollover");
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    type Stop = ();

    async fn started(&mut self, ctx: &mut xtra::Context<Self>) {
        let this = ctx.address().expect("we are alive");
        tokio_extras::spawn(
            &this.clone(),
            this.send_interval(Duration::from_secs(5 * 60), || AutoRollover),
        );
    }

    async fn stopped(self) -> Self::Stop {}
}

/// Message sent to ourselves at an interval to check if rollover can
/// be triggered for any of the CFDs in the database.
#[derive(Clone, Copy)]
pub struct AutoRollover;

/// Message used to trigger rollover internally within the `auto_rollover::Actor`
///
/// This helps us trigger rollover in the tests unconditionally of time.
#[derive(Clone, Copy)]
pub struct Rollover {
    pub order_id: OrderId,
    pub maker_peer_id: Option<PeerId>,
    pub from_commit_txid: Txid,
    pub from_settlement_event_id: BitMexPriceEventId,
}
