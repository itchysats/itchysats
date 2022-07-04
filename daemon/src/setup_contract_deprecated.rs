use crate::transaction_ext::TransactionExt;
use crate::wallet;
use crate::wire::Msg0;
use crate::wire::Msg1;
use crate::wire::Msg2;
use crate::wire::Msg3;
use crate::wire::RolloverMsg;
use crate::wire::RolloverMsg0;
use crate::wire::RolloverMsg1;
use crate::wire::RolloverMsg2;
use crate::wire::RolloverMsg3;
use crate::wire::SetupMsg;
use anyhow::Context;
use anyhow::Result;
use bdk::bitcoin::secp256k1::ecdsa::Signature;
use bdk::bitcoin::secp256k1::SECP256K1;
use bdk::bitcoin::util::psbt::PartiallySignedTransaction;
use bdk::bitcoin::Amount;
use bdk::bitcoin::Transaction;
use bdk::descriptor::Descriptor;
use bdk::miniscript::DescriptorTrait;
use bdk_ext::keypair;
use futures::stream::FusedStream;
use futures::Sink;
use futures::SinkExt;
use futures::StreamExt;
use maia_core::interval;
use maia_core::secp256k1_zkp;
use maia_core::secp256k1_zkp::EcdsaAdaptorSignature;
use maia_core::secp256k1_zkp::XOnlyPublicKey;
use maia_core::Announcement;
use maia_core::PartyParams;
use maia_core::PunishParams;
use maia_deprecated::commit_descriptor;
use maia_deprecated::compute_adaptor_pk;
use maia_deprecated::create_cfd_transactions;
use maia_deprecated::lock_descriptor;
use maia_deprecated::renew_cfd_transactions;
use maia_deprecated::spending_tx_sighash;
use model::olivia;
use model::Cet;
use model::CompleteFee;
use model::Dlc;
use model::Payouts;
use model::Position;
use model::RevokedCommit;
use model::Role;
use model::RolloverParams;
use model::SetupParams;
use model::CET_TIMELOCK;
use std::collections::HashMap;
use std::iter::FromIterator;
use std::ops::RangeInclusive;
use std::time::Duration;
use tokio_extras::FutureExt;
use xtra::prelude::MessageChannel;

/// How long contract setup protocol waits for the next message before giving up
///
/// 120s are currently needed to ensure that we can outlive times when the maker/taker are under
/// heavy message load. Failed contract setups are annoying compared to failed rollovers so we allow
/// more time to see them less often.
const CONTRACT_SETUP_MSG_TIMEOUT: Duration = Duration::from_secs(120);

/// How long rollover protocol waits for the next message before giving up
///
/// 60s timeout are acceptable here because rollovers are automatically retried; a few failed
/// rollovers are not a big deal.
const ROLLOVER_MSG_TIMEOUT: Duration = Duration::from_secs(60);

/// Given an initial set of parameters, sets up the CFD contract with
/// the counterparty.
#[allow(clippy::too_many_arguments)]
pub async fn new(
    sink: impl Sink<SetupMsg, Error = anyhow::Error>,
    mut stream: impl FusedStream<Item = SetupMsg> + Unpin,
    (oracle_pk, announcement): (XOnlyPublicKey, olivia::Announcement),
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

    futures::pin_mut!(sink);

    sink.send(SetupMsg::Msg0(Msg0::from((own_params.clone(), own_punish))))
        .await
        .context("Failed to send Msg0")?;
    let msg0 = stream
        .select_next_some()
        .timeout(CONTRACT_SETUP_MSG_TIMEOUT, stream_select_next_span)
        .await
        .with_context(|| format_expect_msg_within("Msg0", CONTRACT_SETUP_MSG_TIMEOUT))?
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
        anyhow::bail!(
            "Amounts sent by counterparty don't add up, expected margin {expected_margin} but got {actual_margin}"
        )
    }

    let settlement_event_id = announcement.id;
    let payouts = HashMap::from_iter([(
        announcement.into(),
        Payouts::new(
            position,
            role,
            setup_params.price,
            setup_params.quantity,
            setup_params.long_leverage,
            setup_params.short_leverage,
            n_payouts,
            setup_params.fee_account.settle(),
        )?
        .settlement(),
    )]);

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
                payouts,
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
        .select_next_some()
        .timeout(CONTRACT_SETUP_MSG_TIMEOUT, stream_select_next_span)
        .await
        .with_context(|| format_expect_msg_within("Msg1", CONTRACT_SETUP_MSG_TIMEOUT))?
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

        verify_cets(
            (oracle_pk, own_grouped_cets.event.nonce_pks.clone()),
            params.counterparty.clone(),
            own_grouped_cets.cets,
            counterparty_cets,
            commit_desc.clone(),
            commit_amount,
        )
        .await
        .context("CET signatures don't verify")?;
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
        .select_next_some()
        .timeout(CONTRACT_SETUP_MSG_TIMEOUT, stream_select_next_span)
        .await
        .with_context(|| format_expect_msg_within("Msg2", CONTRACT_SETUP_MSG_TIMEOUT))?
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
        .select_next_some()
        .timeout(CONTRACT_SETUP_MSG_TIMEOUT, stream_select_next_span)
        .await
        .with_context(|| format_expect_msg_within("Msg3", CONTRACT_SETUP_MSG_TIMEOUT))?
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

#[allow(clippy::too_many_arguments)]
pub async fn roll_over(
    sink: impl Sink<RolloverMsg, Error = anyhow::Error>,
    mut stream: impl FusedStream<Item = RolloverMsg> + Unpin,
    (oracle_pk, announcement): (XOnlyPublicKey, olivia::Announcement),
    rollover_params: RolloverParams,
    our_role: Role,
    our_position: Position,
    dlc: Dlc,
    n_payouts: usize,
    complete_fee: CompleteFee,
) -> Result<Dlc> {
    let sk = dlc.identity;
    let pk =
        bdk::bitcoin::PublicKey::new(secp256k1_zkp::PublicKey::from_secret_key(SECP256K1, &sk));

    let (rev_sk, rev_pk) = keypair::new(&mut rand::thread_rng());
    let (publish_sk, publish_pk) = keypair::new(&mut rand::thread_rng());

    let own_punish = PunishParams {
        revocation_pk: rev_pk,
        publish_pk,
    };

    futures::pin_mut!(sink);

    sink.send(RolloverMsg::Msg0(RolloverMsg0 {
        revocation_pk: rev_pk,
        publish_pk,
    }))
    .await
    .context("Failed to send Msg0")?;
    let msg0 = stream
        .select_next_some()
        .timeout(ROLLOVER_MSG_TIMEOUT, stream_select_next_span)
        .await
        .with_context(|| format_expect_msg_within("Msg0", ROLLOVER_MSG_TIMEOUT))?
        .try_into_msg0()?;

    let maker_lock_amount = dlc.maker_lock_amount;
    let taker_lock_amount = dlc.taker_lock_amount;
    let payouts = HashMap::from_iter([(
        Announcement {
            id: announcement.id.to_string(),
            nonce_pks: announcement.nonce_pks.clone(),
        },
        Payouts::new(
            our_position,
            our_role,
            rollover_params.price,
            rollover_params.quantity,
            rollover_params.long_leverage,
            rollover_params.short_leverage,
            n_payouts,
            complete_fee,
        )?
        .settlement(),
    )]);

    // unsign lock tx because PartiallySignedTransaction needs an unsigned tx
    let mut unsigned_lock_tx = dlc.lock.0.clone();
    unsigned_lock_tx
        .input
        .iter_mut()
        .for_each(|input| input.witness.clear());

    let lock_tx = PartiallySignedTransaction::from_unsigned_tx(unsigned_lock_tx)?;
    let counterparty_punish_params = PunishParams {
        revocation_pk: msg0.revocation_pk,
        publish_pk: msg0.publish_pk,
    };
    let ((maker_identity, maker_punish_params), (taker_identity, taker_punish_params)) =
        match our_role {
            Role::Maker => (
                (pk, own_punish),
                (dlc.identity_counterparty, counterparty_punish_params),
            ),
            Role::Taker => (
                (dlc.identity_counterparty, counterparty_punish_params),
                (pk, own_punish),
            ),
        };
    let own_cfd_txs = tokio::task::spawn_blocking({
        let maker_address = dlc.maker_address.clone();
        let taker_address = dlc.taker_address.clone();
        let lock_tx = lock_tx.clone();

        move || {
            renew_cfd_transactions(
                lock_tx,
                (
                    maker_identity,
                    maker_lock_amount,
                    maker_address,
                    maker_punish_params,
                ),
                (
                    taker_identity,
                    taker_lock_amount,
                    taker_address,
                    taker_punish_params,
                ),
                oracle_pk,
                (CET_TIMELOCK, rollover_params.refund_timelock),
                payouts,
                sk,
                rollover_params.fee_rate.to_u32(),
            )
        }
    })
    .await?
    .context("Failed to create new CFD transactions")?;

    sink.send(RolloverMsg::Msg1(RolloverMsg1::from(own_cfd_txs.clone())))
        .await
        .context("Failed to send Msg1")?;

    let msg1 = stream
        .select_next_some()
        .timeout(ROLLOVER_MSG_TIMEOUT, stream_select_next_span)
        .await
        .with_context(|| format_expect_msg_within("Msg1", ROLLOVER_MSG_TIMEOUT))?
        .try_into_msg1()?;

    let lock_amount = taker_lock_amount + maker_lock_amount;

    let commit_desc = commit_descriptor(
        (
            maker_identity,
            maker_punish_params.revocation_pk,
            maker_punish_params.publish_pk,
        ),
        (
            taker_identity,
            taker_punish_params.revocation_pk,
            taker_punish_params.publish_pk,
        ),
    );

    let own_cets = own_cfd_txs.cets;
    let commit_tx = own_cfd_txs.commit.0.clone();

    let commit_amount = Amount::from_sat(commit_tx.output[0].value);

    verify_adaptor_signature(
        &commit_tx,
        &dlc.lock.1,
        lock_amount,
        &msg1.commit,
        &publish_pk,
        &dlc.identity_counterparty,
    )
    .context("Commit adaptor signature does not verify")?;

    let counterparty_address = match our_role {
        Role::Maker => dlc.taker_address.clone(),
        Role::Taker => dlc.maker_address.clone(),
    };

    for own_grouped_cets in own_cets.clone() {
        let counterparty_cets = msg1
            .cets
            .get(&own_grouped_cets.event.id)
            .cloned()
            .context("Expect event to exist in msg")?;

        verify_cets(
            (oracle_pk, announcement.nonce_pks.clone()),
            PartyParams {
                lock_psbt: lock_tx.clone(),
                identity_pk: dlc.identity_counterparty,
                lock_amount,
                address: counterparty_address.clone(),
            },
            own_grouped_cets.cets,
            counterparty_cets,
            commit_desc.clone(),
            commit_amount,
        )
        .await
        .context("CET signatures don't verify")?;
    }

    let refund_tx = own_cfd_txs.refund.0;

    verify_signature(
        &refund_tx,
        &commit_desc,
        commit_amount,
        &msg1.refund,
        &dlc.identity_counterparty,
    )
    .context("Refund signature does not verify")?;

    let maker_address = &dlc.maker_address;
    let taker_address = &dlc.taker_address;
    let cets = own_cets
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
                                "Missing counterparty adaptor signature for CET corresponding to
                                 price range {range:?}"
                            )
                        })?;

                    let maker_amount = tx
                        .find_output_amount(&maker_address.script_pubkey())
                        .unwrap_or_default();
                    let taker_amount = tx
                        .find_output_amount(&taker_address.script_pubkey())
                        .unwrap_or_default();
                    let cet = Cet {
                        maker_amount,
                        taker_amount,
                        adaptor_sig: *counterparty_encsig,
                        range: digits.range(),
                        n_bits: digits.len(),
                        txid: tx.txid(),
                    };

                    debug_assert_eq!(
                        cet.to_tx((&commit_tx, &commit_desc), maker_address, taker_address)
                            .expect("can reconstruct CET")
                            .txid(),
                        tx.txid()
                    );

                    Ok(cet)
                })
                .collect::<Result<Vec<_>>>()?;
            Ok((event_id.parse()?, cets))
        })
        .collect::<Result<HashMap<_, _>>>()?;

    // reveal revocation secrets to the counterparty
    sink.send(RolloverMsg::Msg2(RolloverMsg2 {
        revocation_sk: dlc.revocation,
    }))
    .await
    .context("Failed to send Msg2")?;

    let msg2 = stream
        .select_next_some()
        .timeout(ROLLOVER_MSG_TIMEOUT, stream_select_next_span)
        .await
        .with_context(|| format_expect_msg_within("Msg2", ROLLOVER_MSG_TIMEOUT))?
        .try_into_msg2()?;
    let revocation_sk_theirs = msg2.revocation_sk;

    {
        let derived_rev_pk = bdk::bitcoin::PublicKey::new(
            secp256k1_zkp::PublicKey::from_secret_key(SECP256K1, &revocation_sk_theirs),
        );

        if derived_rev_pk != dlc.revocation_pk_counterparty {
            anyhow::bail!("Counterparty sent invalid revocation sk");
        }
    }

    let mut revoked_commit = dlc.revoked_commit;
    let transaction = dlc.commit.0;
    revoked_commit.push(RevokedCommit {
        encsig_ours: dlc.commit.1,
        revocation_sk_theirs,
        publication_pk_theirs: dlc.publish_pk_counterparty,
        txid: transaction.txid(),
        script_pubkey: dlc.commit.2.script_pubkey(),
        // We don't allow the taker to rollover from a specific commit-tx in the deprecated version
        // because we cannot be sure this works side effect free.
        settlement_event_id: None,
        complete_fee: None,
    });

    // TODO: Remove send- and receiving ACK messages once we are able to handle incomplete DLC
    // monitoring
    sink.send(RolloverMsg::Msg3(RolloverMsg3))
        .await
        .context("Failed to send Msg3")?;
    let _ = stream
        .select_next_some()
        .timeout(ROLLOVER_MSG_TIMEOUT, stream_select_next_span)
        .await
        .with_context(|| format_expect_msg_within("Msg3", ROLLOVER_MSG_TIMEOUT))?
        .try_into_msg3()?;

    Ok(Dlc {
        identity: sk,
        identity_counterparty: dlc.identity_counterparty,
        revocation: rev_sk,
        revocation_pk_counterparty: counterparty_punish_params.revocation_pk,
        publish: publish_sk,
        publish_pk_counterparty: counterparty_punish_params.publish_pk,
        maker_address: dlc.maker_address,
        taker_address: dlc.taker_address,
        lock: dlc.lock.clone(),
        commit: (commit_tx, msg1.commit, commit_desc),
        cets,
        refund: (refund_tx, msg1.refund),
        maker_lock_amount,
        taker_lock_amount,
        revoked_commit,
        settlement_event_id: announcement.id,
        refund_timelock: rollover_params.refund_timelock,
    })
}

fn stream_select_next_span(parent: &tracing::Span) -> tracing::Span {
    tracing::debug_span!(parent: parent, "SetupMsg stream select_next_some")
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

async fn verify_cets(
    (oracle_pk, nonce_pks): (XOnlyPublicKey, Vec<XOnlyPublicKey>),
    counterparty: PartyParams,
    own_cets: Vec<(Transaction, EcdsaAdaptorSignature, interval::Digits)>,
    cets: Vec<(RangeInclusive<u64>, EcdsaAdaptorSignature)>,
    commit_desc: Descriptor<bdk::bitcoin::PublicKey>,
    commit_amount: Amount,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        for (tx, _, digits) in own_cets.iter() {
            let counterparty_encsig = cets
                .iter()
                .find_map(|(range, encsig)| (range == &digits.range()).then(|| encsig))
                .with_context(|| {
                    let range = digits.range();

                    format!("no enc sig from counterparty for price range {range:?}",)
                })?;

            verify_cet_encsig(
                tx,
                counterparty_encsig,
                digits,
                &counterparty.identity_pk,
                (&oracle_pk, &nonce_pks),
                &commit_desc,
                commit_amount,
            )
            .context("enc sig on CET does not verify")?;
        }

        anyhow::Ok(())
    })
    .await??;

    Ok(())
}

fn verify_adaptor_signature(
    tx: &Transaction,
    spent_descriptor: &Descriptor<bdk::bitcoin::PublicKey>,
    spent_amount: Amount,
    encsig: &EcdsaAdaptorSignature,
    encryption_point: &bdk::bitcoin::PublicKey,
    pk: &bdk::bitcoin::PublicKey,
) -> Result<()> {
    let sighash = spending_tx_sighash(tx, spent_descriptor, spent_amount)
        .context("could not obtain sighash")?;

    encsig
        .verify(SECP256K1, &sighash, &pk.inner, &encryption_point.inner)
        .context("failed to verify encsig spend tx")
}

fn verify_signature(
    tx: &Transaction,
    spent_descriptor: &Descriptor<bdk::bitcoin::PublicKey>,
    spent_amount: Amount,
    sig: &Signature,
    pk: &bdk::bitcoin::PublicKey,
) -> Result<()> {
    let sighash = spending_tx_sighash(tx, spent_descriptor, spent_amount)
        .context("could not obtain sighash")?;
    SECP256K1.verify_ecdsa(&sighash, sig, &pk.inner)?;
    Ok(())
}

fn verify_cet_encsig(
    tx: &Transaction,
    encsig: &EcdsaAdaptorSignature,
    digits: &interval::Digits,
    pk: &bdk::bitcoin::PublicKey,
    (oracle_pk, nonce_pks): (&XOnlyPublicKey, &[XOnlyPublicKey]),
    spent_descriptor: &Descriptor<bdk::bitcoin::PublicKey>,
    spent_amount: Amount,
) -> Result<()> {
    let index_nonce_pairs = &digits
        .to_indices()
        .into_iter()
        .zip(nonce_pks.iter().cloned())
        .collect::<Vec<_>>();
    let adaptor_point = compute_adaptor_pk(oracle_pk, index_nonce_pairs)
        .context("could not calculate adaptor point")?;
    verify_adaptor_signature(
        tx,
        spent_descriptor,
        spent_amount,
        encsig,
        &bdk::bitcoin::PublicKey::new(adaptor_point),
        pk,
    )
}

/// Wrapper for the msg
fn format_expect_msg_within(msg: &str, timeout: Duration) -> String {
    let seconds = timeout.as_secs();

    format!("Expected {msg} within {seconds} seconds")
}
