use crate::connection;
use crate::connection::NoConnection;
use anyhow::Context as _;
use anyhow::Result;
use daemon::command;
use daemon::oracle;
use daemon::oracle::NoAnnouncement;
use daemon::process_manager;
use daemon::setup_contract_deprecated;
use daemon::wire;
use futures::channel::mpsc;
use futures::channel::mpsc::UnboundedSender;
use futures::sink;
use futures::SinkExt;
use maia_core::secp256k1_zkp::XOnlyPublicKey;
use model::olivia::Announcement;
use model::Dlc;
use model::FundingFee;
use model::FundingRate;
use model::Identity;
use model::OrderId;
use model::Position;
use model::Role;
use model::RolloverVersion;
use model::TxFeeRate;
use xtra::prelude::MessageChannel;
use xtra_productivity::xtra_productivity;

/// Upon accepting Rollover maker sends the current estimated transaction fee and
/// funding rate
#[derive(Clone, Copy)]
pub struct AcceptRollover {
    pub tx_fee_rate: TxFeeRate,
    pub long_funding_rate: FundingRate,
    pub short_funding_rate: FundingRate,
}

#[derive(Clone, Copy)]
pub struct RejectRollover;

pub struct ProtocolMsg(pub wire::RolloverMsg);

/// Message sent from the spawned task to `rollover_taker::Actor` to
/// notify that rollover has finished successfully.
struct RolloverSucceeded {
    dlc: Dlc,
    funding_fee: FundingFee,
}

/// Message sent from the spawned task to `rollover_maker::Actor` to
/// notify that rollover has failed.
struct RolloverFailed {
    error: anyhow::Error,
}

pub struct Actor {
    order_id: OrderId,
    send_to_taker_actor: MessageChannel<connection::TakerMessage, Result<(), NoConnection>>,
    n_payouts: usize,
    taker_id: Identity,
    oracle_pk: XOnlyPublicKey,
    sent_from_taker: Option<UnboundedSender<wire::RolloverMsg>>,
    oracle_actor:
        MessageChannel<oracle::GetAnnouncements, Result<Vec<Announcement>, NoAnnouncement>>,
    register: MessageChannel<connection::RegisterRollover, ()>,
    executor: command::Executor,
    version: RolloverVersion,
}

impl Actor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        order_id: OrderId,
        n_payouts: usize,
        send_to_taker_actor: MessageChannel<connection::TakerMessage, Result<(), NoConnection>>,
        taker_id: Identity,
        oracle_pk: XOnlyPublicKey,
        oracle_actor: MessageChannel<
            oracle::GetAnnouncements,
            Result<Vec<Announcement>, NoAnnouncement>,
        >,
        process_manager: xtra::Address<process_manager::Actor>,
        register: MessageChannel<connection::RegisterRollover, ()>,
        db: sqlite_db::Connection,
        version: RolloverVersion,
    ) -> Self {
        Self {
            order_id,
            n_payouts,
            send_to_taker_actor,
            taker_id,
            oracle_pk,
            sent_from_taker: None,
            oracle_actor,
            register,
            executor: command::Executor::new(db, process_manager),
            version,
        }
    }

    async fn emit_complete(
        &mut self,
        dlc: Dlc,
        funding_fee: FundingFee,
        ctx: &mut xtra::Context<Self>,
    ) {
        if let Err(e) = self
            .executor
            .execute(self.order_id, |cfd| {
                Ok(cfd.complete_rollover(dlc, funding_fee, None))
            })
            .await
        {
            tracing::warn!(order_id = %self.order_id, "{:#}", e)
        }

        ctx.stop_self();
    }

    async fn emit_reject(&mut self, reason: anyhow::Error, ctx: &mut xtra::Context<Self>) {
        if let Err(e) = self
            .executor
            .execute(self.order_id, |cfd| Ok(cfd.reject_rollover(reason)))
            .await
        {
            tracing::warn!(order_id = %self.order_id, "{:#}", e)
        }

        ctx.stop_self();
    }

    async fn emit_fail(&mut self, error: anyhow::Error, ctx: &mut xtra::Context<Self>) {
        if let Err(e) = self
            .executor
            .execute(self.order_id, |cfd| Ok(cfd.fail_rollover(error)))
            .await
        {
            tracing::warn!(order_id = %self.order_id, "{:#}", e)
        }

        ctx.stop_self();
    }

    async fn accept(&mut self, msg: AcceptRollover, ctx: &mut xtra::Context<Self>) -> Result<()> {
        let order_id = self.order_id;
        let AcceptRollover {
            tx_fee_rate,
            long_funding_rate,
            short_funding_rate,
        } = msg;

        if self.sent_from_taker.is_some() {
            tracing::debug!(%order_id, "Rollover already active");
            return Ok(());
        }

        let (sender, receiver) = mpsc::unbounded();

        self.sent_from_taker = Some(sender);

        let (rollover_params, dlc, position, oracle_event_id, funding_rate) = self
            .executor
            .execute(self.order_id, |cfd| {
                let funding_rate = match cfd.position() {
                    Position::Long => long_funding_rate,
                    Position::Short => short_funding_rate,
                };

                let (event, params, dlc, position, event_id) =
                    cfd.accept_rollover_proposal(tx_fee_rate, funding_rate, None, self.version)?;
                Ok((event, params, dlc, position, event_id, funding_rate))
            })
            .await?;

        let taker_id = self.taker_id;

        // the maker computes the rollover fee and sends it over to the taker so that both parties
        // are on the same page
        let complete_fee = match rollover_params.version {
            RolloverVersion::V1 => {
                // Note there is actually a bug here, but we have to keep this as is to reach
                // agreement on the fee for the protocol V1 version.
                //
                // The current fee is supposed to be added here, but we never noticed because in V1
                // the fee is always charged for one hour using a static rate. This
                // results in applying the fee in the DLC only for the next rollover
                // (because we do apply the fee in the Cfd when loading the rollover
                // event). Effectively this means, that we always charged one
                // rollover too little.
                rollover_params.fee_account.settle()
            }
            RolloverVersion::V2 | RolloverVersion::V3 => rollover_params
                .fee_account
                .add_funding_fee(rollover_params.current_fee)
                .settle(),
        };

        self.send_to_taker_actor
            .send(connection::TakerMessage {
                taker_id,
                msg: wire::MakerToTaker::ConfirmRollover {
                    order_id,
                    oracle_event_id,
                    tx_fee_rate,
                    funding_rate,
                    complete_fee: complete_fee.into(),
                },
            })
            .await
            .context("Maker connection actor disconnected")?
            .context("Failed to send confirm rollover message")?;

        let announcement = self
            .oracle_actor
            .send(oracle::GetAnnouncements(vec![oracle_event_id]))
            .await
            .context("Oracle actor disconnected")?
            .context("Failed to get announcement")?;

        let funding_fee = *rollover_params.funding_fee();
        let send_to_taker_actor = self.send_to_taker_actor.clone();

        let rollover_fut = setup_contract_deprecated::roll_over(
            sink::unfold((), move |_, msg| {
                let send_to_taker_actor = send_to_taker_actor.clone();
                async move {
                    let msg = connection::TakerMessage {
                        taker_id,
                        msg: wire::MakerToTaker::RolloverProtocol { order_id, msg },
                    };

                    let _ = send_to_taker_actor.send(msg).split_receiver().await;
                    Ok(())
                }
            }),
            receiver,
            (self.oracle_pk, announcement[0].clone()),
            rollover_params,
            Role::Maker,
            position,
            dlc,
            self.n_payouts,
            complete_fee,
        );

        let this = ctx.address().expect("self to be alive");

        tokio_extras::spawn(&this.clone(), async move {
            let _: Result<(), xtra::Error> =
                match rollover_fut.await.context("Rollover protocol failed") {
                    Ok(dlc) => this.send(RolloverSucceeded { dlc, funding_fee }).await,
                    Err(source) => this.send(RolloverFailed { error: source }).await,
                };
        });

        Ok(())
    }

    async fn reject(&mut self, ctx: &mut xtra::Context<Self>) -> Result<()> {
        tracing::info!(id = %self.order_id, "Rejecting rollover proposal" );

        self.send_to_taker_actor
            .send(connection::TakerMessage {
                taker_id: self.taker_id,
                msg: wire::MakerToTaker::RejectRollover(self.order_id),
            })
            .await
            .context("Maker connection actor disconnected")?
            .context("Failed to send reject rollover message")?;

        self.emit_reject(anyhow::format_err!("unknown"), ctx).await;

        ctx.stop_self();

        Ok(())
    }

    pub async fn forward_protocol_msg(&mut self, msg: ProtocolMsg) -> Result<()> {
        self.sent_from_taker
            .as_mut()
            .context("Rollover task is not active")? // Sender is set once `Accepted` is sent.
            .send(msg.0)
            .await
            .context("Failed to forward message to rollover task")?;

        Ok(())
    }
}

#[async_trait::async_trait]
impl xtra::Actor for Actor {
    type Stop = ();
    async fn started(&mut self, ctx: &mut xtra::Context<Self>) {
        let order_id = self.order_id;
        let taker_id = self.taker_id;

        tracing::info!(
            %order_id,
            %taker_id,
            "Received rollover proposal"
        );

        let this = ctx.address().expect("self to be alive");
        let fut = async {
            // Register ourselves with the actor handling connections with
            // takers, so that it knows where to forward rollover messages
            // which correspond to this instance
            self.register
                .send(connection::RegisterRollover {
                    order_id,
                    address: this,
                })
                .await?;

            self.executor
                .execute(self.order_id, |cfd| cfd.start_rollover_deprecated())
                .await?;

            anyhow::Ok(())
        };

        if let Err(source) = fut.await {
            self.emit_fail(source, ctx).await;
        }
    }

    async fn stopped(self) -> Self::Stop {}
}

#[xtra_productivity]
impl Actor {
    async fn handle_accept_rollover(&mut self, msg: AcceptRollover, ctx: &mut xtra::Context<Self>) {
        if let Err(error) = self.accept(msg, ctx).await {
            self.emit_fail(error, ctx).await;
        };
    }

    async fn handle_reject_rollover(
        &mut self,
        _msg: RejectRollover,
        ctx: &mut xtra::Context<Self>,
    ) {
        if let Err(error) = self.reject(ctx).await {
            self.emit_fail(error, ctx).await;
        };
    }

    async fn handle_protocol_msg(&mut self, msg: ProtocolMsg, ctx: &mut xtra::Context<Self>) {
        if let Err(error) = self.forward_protocol_msg(msg).await {
            self.emit_fail(error, ctx).await;
        };
    }

    async fn handle_rollover_failed(&mut self, msg: RolloverFailed, ctx: &mut xtra::Context<Self>) {
        self.emit_fail(msg.error, ctx).await
    }

    async fn handle_rollover_succeeded(
        &mut self,
        msg: RolloverSucceeded,
        ctx: &mut xtra::Context<Self>,
    ) {
        self.emit_complete(msg.dlc, msg.funding_fee, ctx).await
    }
}
