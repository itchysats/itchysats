use crate::address_map::ActorName;
use crate::cfd_actors::load_cfd;
use crate::maker_inc_connections;
use crate::maker_inc_connections::TakerMessage;
use crate::model::cfd::Completed;
use crate::model::cfd::Dlc;
use crate::model::cfd::Role;
use crate::model::cfd::RolloverCompleted;
use crate::model::cfd::RolloverError;
use crate::model::cfd::RolloverProposal;
use crate::model::Identity;
use crate::oracle;
use crate::oracle::GetAnnouncement;
use crate::process_manager;
use crate::schnorrsig;
use crate::setup_contract;
use crate::wire;
use crate::wire::MakerToTaker;
use crate::wire::RollOverMsg;
use crate::Stopping;
use crate::Tasks;
use anyhow::Context as _;
use anyhow::Result;
use futures::channel::mpsc;
use futures::channel::mpsc::UnboundedSender;
use futures::future;
use futures::SinkExt;
use xtra::prelude::MessageChannel;
use xtra::Context;
use xtra::KeepRunning;
use xtra_productivity::xtra_productivity;

pub struct AcceptRollOver;

pub struct RejectRollOver;

pub struct ProtocolMsg(pub wire::RollOverMsg);

/// Message sent from the spawned task to `rollover_taker::Actor` to
/// notify that rollover has finished successfully.
struct RolloverSucceeded {
    dlc: Dlc,
}

/// Message sent from the spawned task to `rollover_taker::Actor` to
/// notify that rollover has failed.
struct RolloverFailed {
    error: RolloverError,
}

pub struct Actor {
    proposal: RolloverProposal,
    send_to_taker_actor: Box<dyn MessageChannel<TakerMessage>>,
    n_payouts: usize,
    taker_id: Identity,
    oracle_pk: schnorrsig::PublicKey,
    sent_from_taker: Option<UnboundedSender<RollOverMsg>>,
    oracle_actor: Box<dyn MessageChannel<GetAnnouncement>>,
    on_stopping: Vec<Box<dyn MessageChannel<Stopping<Self>>>>,
    process_manager: xtra::Address<process_manager::Actor>,
    db: sqlx::SqlitePool,
    tasks: Tasks,
}

impl Actor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        proposal: RolloverProposal,
        n_payouts: usize,
        send_to_taker_actor: &(impl MessageChannel<TakerMessage> + 'static),
        taker_id: Identity,
        oracle_pk: schnorrsig::PublicKey,
        oracle_actor: &(impl MessageChannel<GetAnnouncement> + 'static),
        (on_stopping0, on_stopping1): (
            &(impl MessageChannel<Stopping<Self>> + 'static),
            &(impl MessageChannel<Stopping<Self>> + 'static),
        ),
        process_manager: xtra::Address<process_manager::Actor>,
        db: sqlx::SqlitePool,
    ) -> Self {
        Self {
            proposal,
            n_payouts,
            send_to_taker_actor: send_to_taker_actor.clone_channel(),
            taker_id,
            oracle_pk,
            sent_from_taker: None,
            oracle_actor: oracle_actor.clone_channel(),
            on_stopping: vec![on_stopping0.clone_channel(), on_stopping1.clone_channel()],
            process_manager,
            db,
            tasks: Tasks::default(),
        }
    }

    async fn handle_proposal(&mut self) -> Result<(), RolloverError> {
        let mut conn = self.db.acquire().await.context("Failed to connect to DB")?;
        let cfd = load_cfd(self.proposal.order_id, &mut conn)
            .await
            .context("Failed to load CFD")?;

        let event = cfd.receive_rollover_proposal()?;
        self.process_manager
            .send(process_manager::Event::new(event.clone()))
            .await
            .context("Process manager disconnected")?
            .with_context(|| format!("Process manager failed to process event {:?}", event))?;

        Ok(())
    }

    async fn complete(&mut self, completed: RolloverCompleted, ctx: &mut xtra::Context<Self>) {
        let order_id = self.proposal.order_id;
        let event_fut = async {
            let mut conn = self.db.acquire().await?;
            let cfd = load_cfd(order_id, &mut conn).await?;
            let event = cfd.roll_over(completed)?;

            anyhow::Ok(event)
        };

        match event_fut.await {
            Ok(event) => {
                if let Some(event) = event {
                    let _ = self
                        .process_manager
                        .send(process_manager::Event::new(event))
                        .await;
                }
            }
            Err(e) => {
                tracing::warn!(%order_id, "Failed to complete rollover: {:#}", e)
            }
        }

        ctx.stop();
    }

    async fn accept(&mut self, ctx: &mut xtra::Context<Self>) -> Result<(), RolloverError> {
        let order_id = self.proposal.order_id;

        if self.sent_from_taker.is_some() {
            tracing::warn!(%order_id, "Rollover already active");
            return Ok(());
        }

        let (sender, receiver) = mpsc::unbounded();

        self.sent_from_taker = Some(sender);

        tracing::debug!(%order_id, "Maker accepts a roll_over proposal" );

        let mut conn = self.db.acquire().await.context("Failed to connect to DB")?;
        let cfd = load_cfd(self.proposal.order_id, &mut conn)
            .await
            .context("Failed to load CFD")?;

        let (event, (rollover_params, dlc, interval)) = cfd.accept_rollover_proposal()?;
        self.process_manager
            .send(process_manager::Event::new(event.clone()))
            .await
            .context("Process manager actor disconnected")?
            .with_context(|| format!("Process manager failed to process event {:?}", event))?;

        let oracle_event_id =
            oracle::next_announcement_after(time::OffsetDateTime::now_utc() + interval)
                .context("Failed to calculate next BitMexPriceEventId")?;

        let taker_id = self.taker_id;

        self.send_to_taker_actor
            .send(maker_inc_connections::TakerMessage {
                taker_id,
                msg: wire::MakerToTaker::ConfirmRollOver {
                    order_id,
                    oracle_event_id,
                },
            })
            .await
            .context("Maker connection actor disconnected")?
            .context("Failed to send confirm rollover message")?;

        let announcement = self
            .oracle_actor
            .send(oracle::GetAnnouncement(oracle_event_id))
            .await
            .context("Oracle actor disconnected")?
            .context("Failed to get announcement")?;

        let rollover_fut = setup_contract::roll_over(
            self.send_to_taker_actor.sink().with(move |msg| {
                future::ok(maker_inc_connections::TakerMessage {
                    taker_id,
                    msg: wire::MakerToTaker::RollOverProtocol { order_id, msg },
                })
            }),
            receiver,
            (self.oracle_pk, announcement),
            rollover_params,
            Role::Maker,
            dlc,
            self.n_payouts,
        );

        let this = ctx.address().expect("self to be alive");

        self.tasks.add(async move {
            let _: Result<(), xtra::Disconnected> = match rollover_fut.await {
                Ok(dlc) => this.send(RolloverSucceeded { dlc }).await,
                Err(source) => {
                    this.send(RolloverFailed {
                        error: RolloverError::Protocol { source },
                    })
                    .await
                }
            };
        });

        Ok(())
    }

    async fn reject(&mut self, ctx: &mut xtra::Context<Self>) -> Result<(), RolloverError> {
        tracing::info!(id = %self.proposal.order_id, "Rejecting rollover proposal");

        self.send_to_taker_actor
            .send(TakerMessage {
                taker_id: self.taker_id,
                msg: MakerToTaker::RejectRollOver(self.proposal.order_id),
            })
            .await
            .context("Maker connection actor disconnected")?
            .context("Failed to send reject rollover message")?;

        self.complete(RolloverCompleted::rejected(self.proposal.order_id), ctx)
            .await;

        ctx.stop();

        Ok(())
    }

    pub async fn forward_protocol_msg(&mut self, msg: ProtocolMsg) -> Result<(), RolloverError> {
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
    async fn started(&mut self, ctx: &mut xtra::Context<Self>) {
        let order_id = self.proposal.order_id;

        tracing::info!(
            %order_id,
            "Received rollover proposal"
        );

        if let Err(error) = self.handle_proposal().await {
            self.complete(Completed::Failed { order_id, error }, ctx)
                .await;
        }
    }

    async fn stopping(&mut self, ctx: &mut Context<Self>) -> KeepRunning {
        let address = ctx.address().expect("acquired own actor address");

        for channel in self.on_stopping.iter() {
            let _ = channel
                .send(Stopping {
                    me: address.clone(),
                })
                .await;
        }

        KeepRunning::StopAll
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle_accept_rollover(
        &mut self,
        _msg: AcceptRollOver,
        ctx: &mut xtra::Context<Self>,
    ) {
        if let Err(error) = self.accept(ctx).await {
            self.complete(
                RolloverCompleted::Failed {
                    order_id: self.proposal.order_id,
                    error,
                },
                ctx,
            )
            .await;
        };
    }

    async fn handle_reject_rollover(
        &mut self,
        _msg: RejectRollOver,
        ctx: &mut xtra::Context<Self>,
    ) {
        if let Err(error) = self.reject(ctx).await {
            self.complete(
                RolloverCompleted::Failed {
                    order_id: self.proposal.order_id,
                    error,
                },
                ctx,
            )
            .await;
        };
    }

    async fn handle_protocol_msg(&mut self, msg: ProtocolMsg, ctx: &mut xtra::Context<Self>) {
        if let Err(error) = self.forward_protocol_msg(msg).await {
            self.complete(
                RolloverCompleted::Failed {
                    order_id: self.proposal.order_id,
                    error,
                },
                ctx,
            )
            .await;
        };
    }

    async fn handle_rollover_failed(&mut self, msg: RolloverFailed, ctx: &mut xtra::Context<Self>) {
        self.complete(
            RolloverCompleted::failed(self.proposal.order_id, msg.error),
            ctx,
        )
        .await
    }

    async fn handle_rollover_succeeded(
        &mut self,
        msg: RolloverSucceeded,
        ctx: &mut xtra::Context<Self>,
    ) {
        self.complete(
            RolloverCompleted::succeeded(self.proposal.order_id, msg.dlc),
            ctx,
        )
        .await
    }
}

impl ActorName for Actor {
    fn actor_name() -> String {
        "Maker rollover".to_string()
    }
}
