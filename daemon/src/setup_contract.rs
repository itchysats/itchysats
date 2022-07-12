use crate::wallet;
use crate::wire::Msg0;
use crate::wire::Msg1;
use crate::wire::Msg2;
use crate::wire::Msg3;
use crate::wire::SetupMsg;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use bdk::bitcoin::util::psbt::PartiallySignedTransaction;
use bdk::bitcoin::Amount;
use bdk_ext::keypair;
use futures::Sink;
use futures::SinkExt;
use futures::Stream;
use futures::StreamExt;
use maia::commit_descriptor;
use maia::create_cfd_transactions;
use maia::lock_descriptor;
use maia_core::secp256k1_zkp::XOnlyPublicKey;
use maia_core::PartyParams;
use maia_core::PunishParams;
use model::olivia;
use model::shared_protocol::verify_adaptor_signature;
use model::shared_protocol::verify_cets;
use model::shared_protocol::verify_signature;
use model::Cet;
use model::Dlc;
use model::OraclePayouts;
use model::Payouts;
use model::Position;
use model::Role;
use model::SetupParams;
use model::TransactionExt;
use model::CET_TIMELOCK;
use std::collections::HashMap;
use std::time::Duration;
use tokio_extras::FutureExt;
use xtra::prelude::MessageChannel;

/// How long contract setup protocol waits for the next message before giving up
///
/// 120s are currently needed to ensure that we can outlive times when the maker/taker are under
/// heavy message load. Failed contract setups are annoying compared to failed rollovers so we allow
/// more time to see them less often.
const CONTRACT_SETUP_MSG_TIMEOUT: Duration = Duration::from_secs(120);

/// Given an initial set of parameters, sets up the CFD contract with
/// the counterparty.
#[allow(clippy::too_many_arguments)]
pub async fn new(
    mut sink: impl Sink<SetupMsg, Error = anyhow::Error> + Unpin,
    mut stream: impl Stream<Item = SetupMsg> + Unpin,
    (oracle_pk, announcements): (XOnlyPublicKey, Vec<olivia::Announcement>),
    setup_params: SetupParams,
    build_party_params_channel: MessageChannel<wallet::BuildPartyParams, Result<PartyParams>>,
    sign_channel: MessageChannel<wallet::Sign, Result<PartiallySignedTransaction>>,
    role: Role,
    position: Position,
    n_payouts: usize,
) -> Result<Dlc> {
    let (sk, pk) = keypair::new(&mut rand::thread_rng());
    let (rev_sk, rev_pk) = keypair::new(&mut rand::thread_rng());
    let (publish_sk, publish_pk) = keypair::new(&mut rand::thread_rng());

    let own_params = build_party_params_channel
        .send(wallet::BuildPartyParams {
            amount: setup_params.margin,
            identity_pk: pk,
            fee_rate: setup_params.tx_fee_rate,
        })
        .await
        .context("Failed to send message to wallet actor")?
        .context("Failed to build party params")?;

    let own_punish = PunishParams {
        revocation_pk: rev_pk,
        publish_pk,
    };

    fn stream_next_span(parent: &tracing::Span) -> tracing::Span {
        tracing::debug_span!(parent: parent, "SetupMsg stream next")
    }

    sink.send(SetupMsg::Msg0(Msg0::from((own_params.clone(), own_punish))))
        .await
        .context("Failed to send Msg0")?;
    let msg0 = stream
        .next()
        .timeout(CONTRACT_SETUP_MSG_TIMEOUT, stream_next_span)
        .await
        .with_context(|| {
            format!(
                "Expected Msg0 within {} seconds",
                CONTRACT_SETUP_MSG_TIMEOUT.as_secs()
            )
        })?
        .context("Empty stream instead of Msg0")?
        .try_into_msg0()?;

    tracing::info!("Exchanged setup parameters");

    let (counterparty, counterparty_punish) = msg0.into();

    let params = AllParams::new(
        own_params,
        own_punish,
        counterparty,
        counterparty_punish,
        role,
    );

    let expected_margin = setup_params.counterparty_margin;
    let actual_margin = params.counterparty.lock_amount;

    if actual_margin != expected_margin {
        bail!(
            "Amounts sent by counterparty don't add up, expected margin {expected_margin} but got {actual_margin}"
        )
    }

    let settlement_event_id = announcements.last().context("Empty announcements")?.id;

    let payouts = Payouts::new(
        position,
        role,
        setup_params.price,
        setup_params.quantity,
        setup_params.long_leverage,
        setup_params.short_leverage,
        n_payouts,
        setup_params.fee_account.settle(),
    )?;
    let payouts_per_event = OraclePayouts::new(payouts, announcements)?;

    let own_cfd_txs = tokio::task::spawn_blocking({
        let maker_params = params.maker().clone();
        let taker_params = params.taker().clone();
        let maker_punish = *params.maker_punish();
        let taker_punish = *params.taker_punish();

        move || {
            create_cfd_transactions(
                (maker_params, maker_punish),
                (taker_params, taker_punish),
                oracle_pk,
                (CET_TIMELOCK, setup_params.refund_timelock),
                payouts_per_event.into(),
                sk,
                setup_params.tx_fee_rate.to_u32(),
            )
        }
    })
    .await?
    .context("Failed to create CFD transactions")?;

    tracing::info!("Created CFD transactions");

    sink.send(SetupMsg::Msg1(Msg1::from(own_cfd_txs.clone())))
        .await
        .context("Failed to send Msg1")?;

    let msg1 = stream
        .next()
        .timeout(CONTRACT_SETUP_MSG_TIMEOUT, stream_next_span)
        .await
        .with_context(|| {
            format!(
                "Expected Msg1 within {} seconds",
                CONTRACT_SETUP_MSG_TIMEOUT.as_secs()
            )
        })?
        .context("Empty stream instead of Msg1")?
        .try_into_msg1()?;

    tracing::info!("Exchanged CFD transactions");

    let lock_desc = lock_descriptor(params.maker().identity_pk, params.taker().identity_pk);

    let lock_amount = params.maker().lock_amount + params.taker().lock_amount;

    let commit_desc = commit_descriptor(
        (
            params.maker().identity_pk,
            params.maker_punish().revocation_pk,
            params.maker_punish().publish_pk,
        ),
        (
            params.taker().identity_pk,
            params.taker_punish().revocation_pk,
            params.taker_punish().publish_pk,
        ),
    );

    let own_cets = own_cfd_txs.cets;
    let commit_tx = own_cfd_txs.commit.0.clone();

    let commit_amount = Amount::from_sat(commit_tx.output[0].value);

    verify_adaptor_signature(
        &commit_tx,
        &lock_desc,
        lock_amount,
        &msg1.commit,
        &params.own_punish.publish_pk,
        &params.counterparty.identity_pk,
    )
    .context("Commit adaptor signature does not verify")?;

    for own_grouped_cets in own_cets.clone() {
        let counterparty_cets = msg1
            .cets
            .get(&own_grouped_cets.event.id)
            .cloned()
            .context("Expect event to exist in msg")?;

        tokio::task::spawn_blocking({
            let commit_desc = commit_desc.clone();
            let params_counterparty = params.counterparty.clone();
            move || {
                verify_cets(
                    (oracle_pk, own_grouped_cets.event.nonce_pks.clone()),
                    params_counterparty,
                    own_grouped_cets.cets,
                    counterparty_cets,
                    commit_desc,
                    commit_amount,
                )
                .context("CET signatures don't verify")?;

                anyhow::Ok(())
            }
        })
        .await??;
    }

    let lock_tx = own_cfd_txs.lock;
    let refund_tx = own_cfd_txs.refund.0;

    verify_signature(
        &refund_tx,
        &commit_desc,
        commit_amount,
        &msg1.refund,
        &params.counterparty.identity_pk,
    )
    .context("Refund signature does not verify")?;

    tracing::info!("Verified all signatures");

    let mut signed_lock_tx = sign_channel
        .send(wallet::Sign { psbt: lock_tx })
        .await
        .context("Failed to send message to wallet actor")?
        .context("Failed to sign transaction")?;
    sink.send(SetupMsg::Msg2(Msg2 {
        signed_lock: signed_lock_tx.clone(),
    }))
    .await
    .context("Failed to send Msg2")?;
    let msg2 = stream
        .next()
        .timeout(CONTRACT_SETUP_MSG_TIMEOUT, stream_next_span)
        .await
        .with_context(|| {
            format!(
                "Expected Msg2 within {} seconds",
                CONTRACT_SETUP_MSG_TIMEOUT.as_secs()
            )
        })?
        .context("Empty stream instead of Msg2")?
        .try_into_msg2()?;
    signed_lock_tx
        .combine(msg2.signed_lock)
        .context("Failed to merge lock PSBTs")?;

    tracing::info!("Exchanged signed lock transaction");

    // TODO: In case we sign+send but never receive (the signed lock_tx from the counterparty)
    // we need some fallback handling (after x time) to spend the outputs in a different way so
    // the counterparty cannot hold us hostage

    let cets = tokio::task::spawn_blocking({
        let maker_address = params.maker().address.clone();
        let taker_address = params.taker().address.clone();
        let commit_tx = commit_tx.clone();
        let commit_desc = commit_desc.clone();
        move || {
        own_cets
            .into_iter()
            .map(|grouped_cets| {
                let event_id = grouped_cets.event.id;
                let counterparty_cets = msg1
                    .cets
                    .get(&event_id)
                    .with_context(|| format!("Counterparty CETs for event {event_id} missing"))?;
                let cets = grouped_cets
                    .cets
                    .into_iter()
                    .map(|(tx, _, digits)| {
                        let counterparty_encsig = counterparty_cets
                            .iter()
                            .find_map(|(counterparty_range, counterparty_encsig)| {
                                (counterparty_range == &digits.range()).then(|| counterparty_encsig)
                            })
                            .with_context(|| {
                                let range = digits.range();

                                format!(
                                    "Missing counterparty adaptor signature for CET corresponding to price range {range:?}",
                                )
                            })?;

                        let maker_amount = tx.find_output_amount(&maker_address.script_pubkey()).unwrap_or_default();
                        let taker_amount = tx.find_output_amount(&taker_address.script_pubkey()).unwrap_or_default();

                        let cet = Cet {
                            maker_amount,
                            taker_amount,
                            adaptor_sig: *counterparty_encsig,
                            range: digits.range(),
                            n_bits: digits.len(),
                            txid: tx.txid(),
                        };

                        debug_assert_eq!(
                            cet.to_tx((&commit_tx, &commit_desc), &maker_address, &taker_address)
                                .expect("can reconstruct CET")
                                .txid(),
                            tx.txid()
                        );

                        Ok(cet)
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok((event_id.parse()?, cets))
            })
            .collect::<Result<HashMap<_, _>>>()
    }})
    .await??;

    // TODO: Remove send- and receiving ACK messages once we are able to handle incomplete DLC
    // monitoring
    sink.send(SetupMsg::Msg3(Msg3))
        .await
        .context("Failed to send Msg3")?;
    let _ = stream
        .next()
        .timeout(CONTRACT_SETUP_MSG_TIMEOUT, stream_next_span)
        .await
        .with_context(|| {
            format!(
                "Expected Msg3 within {} seconds",
                CONTRACT_SETUP_MSG_TIMEOUT.as_secs()
            )
        })?
        .context("Empty stream instead of Msg3")?
        .try_into_msg3()?;

    Ok(Dlc {
        identity: sk,
        identity_counterparty: params.counterparty.identity_pk,
        revocation: rev_sk,
        revocation_pk_counterparty: counterparty_punish.revocation_pk,
        publish: publish_sk,
        publish_pk_counterparty: counterparty_punish.publish_pk,
        maker_address: params.maker().address.clone(),
        taker_address: params.taker().address.clone(),
        lock: (signed_lock_tx.extract_tx(), lock_desc),
        commit: (commit_tx, msg1.commit, commit_desc),
        cets,
        refund: (refund_tx, msg1.refund),
        maker_lock_amount: params.maker().lock_amount,
        taker_lock_amount: params.taker().lock_amount,
        revoked_commit: Vec::new(),
        settlement_event_id,
        refund_timelock: setup_params.refund_timelock,
    })
}

/// A convenience struct for storing PartyParams and PunishParams of both
/// parties and the role of the caller.
struct AllParams {
    pub own: PartyParams,
    pub own_punish: PunishParams,
    pub counterparty: PartyParams,
    pub counterparty_punish: PunishParams,
    pub own_role: Role,
}

impl AllParams {
    fn new(
        own: PartyParams,
        own_punish: PunishParams,
        counterparty: PartyParams,
        counterparty_punish: PunishParams,
        own_role: Role,
    ) -> Self {
        Self {
            own,
            own_punish,
            counterparty,
            counterparty_punish,
            own_role,
        }
    }

    fn maker(&self) -> &PartyParams {
        match self.own_role {
            Role::Maker => &self.own,
            Role::Taker => &self.counterparty,
        }
    }

    fn taker(&self) -> &PartyParams {
        match self.own_role {
            Role::Maker => &self.counterparty,
            Role::Taker => &self.own,
        }
    }

    fn maker_punish(&self) -> &PunishParams {
        match self.own_role {
            Role::Maker => &self.own_punish,
            Role::Taker => &self.counterparty_punish,
        }
    }
    fn taker_punish(&self) -> &PunishParams {
        match self.own_role {
            Role::Maker => &self.counterparty_punish,
            Role::Taker => &self.own_punish,
        }
    }
}
