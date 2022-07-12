use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use bdk::bitcoin::secp256k1::ecdsa::Signature;
use bdk::bitcoin::secp256k1::SecretKey;
use bdk::bitcoin::secp256k1::SECP256K1;
use bdk::bitcoin::util::psbt::PartiallySignedTransaction;
use bdk::bitcoin::Amount;
use bdk::bitcoin::PublicKey;
use bdk::bitcoin::Transaction;
use bdk::bitcoin::Txid;
use bdk::descriptor::Descriptor;
use bdk::miniscript::DescriptorTrait;
use maia::commit_descriptor;
use maia::renew_cfd_transactions;
use maia_core::secp256k1_zkp;
use maia_core::secp256k1_zkp::EcdsaAdaptorSignature;
use maia_core::secp256k1_zkp::XOnlyPublicKey;
use maia_core::Cets;
use maia_core::CfdTransactions;
use maia_core::PartyParams;
use model::olivia;
use model::olivia::BitMexPriceEventId;
use model::shared_protocol::verify_adaptor_signature;
use model::shared_protocol::verify_cets;
use model::shared_protocol::verify_signature;
use model::Cet;
use model::Dlc;
use model::ExecuteOnCfd;
use model::FundingFee;
use model::FundingRate;
use model::OraclePayouts;
use model::OrderId;
use model::Payouts;
use model::Position;
use model::RevokedCommit;
use model::Role;
use model::RolloverParams;
use model::Timestamp;
use model::TransactionExt;
use model::TxFeeRate;
use model::CET_TIMELOCK;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::time::Duration;

/// How long rollover protocol waits for the next message before giving up
///
/// 60s timeout are acceptable here because rollovers are automatically retried; a few failed
/// rollovers are not a big deal.
pub(crate) const ROLLOVER_MSG_TIMEOUT: Duration = Duration::from_secs(60);

pub struct RolloverCompletedParams {
    pub dlc: Dlc,
    pub funding_fee: FundingFee,
}

#[derive(thiserror::Error, Debug)]
pub enum DialerError {
    #[error("Rollover got rejected")]
    Rejected,
    #[error("Rollover failed")]
    Failed {
        #[source]
        source: anyhow::Error,
    },
}

#[derive(Serialize, Deserialize)]
pub(crate) enum DialerMessage {
    Propose(Propose),
    RolloverMsg(Box<RolloverMsg>),
}

impl DialerMessage {
    pub fn into_propose(self) -> Result<Propose> {
        match self {
            DialerMessage::Propose(propose) => Ok(propose),
            DialerMessage::RolloverMsg(_) => bail!("Expected Propose but got RolloverMsg"),
        }
    }

    pub fn into_rollover_msg(self) -> Result<RolloverMsg> {
        match self {
            DialerMessage::RolloverMsg(rollover_msg) => Ok(*rollover_msg),
            DialerMessage::Propose(_) => bail!("Expected RolloverMsg but got Propose"),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub enum Decision {
    Confirm(Confirm),
    Reject(Reject),
}

#[derive(Serialize, Deserialize)]
pub(crate) enum ListenerMessage {
    Decision(Decision),
    RolloverMsg(Box<RolloverMsg>),
}

impl ListenerMessage {
    pub fn into_decision(self) -> Result<Decision> {
        match self {
            ListenerMessage::Decision(decision) => Ok(decision),
            ListenerMessage::RolloverMsg(_) => {
                Err(anyhow!("Expected Decision but got RolloverMsg"))
            }
        }
    }

    pub fn into_rollover_msg(self) -> Result<RolloverMsg> {
        match self {
            ListenerMessage::RolloverMsg(rollover_msg) => Ok(*rollover_msg),
            ListenerMessage::Decision(_) => Err(anyhow!("Expected RolloverMsg but got Decision")),
        }
    }
}

#[derive(Copy, Clone, Serialize, Deserialize)]
pub struct Propose {
    pub order_id: OrderId,
    pub timestamp: Timestamp,
    pub from_commit_txid: Txid,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Confirm {
    pub order_id: OrderId,
    pub oracle_event_ids: Vec<BitMexPriceEventId>,
    pub tx_fee_rate: TxFeeRate,
    pub funding_rate: FundingRate,
    pub complete_fee: CompleteFee,
}

#[derive(Copy, Clone, Serialize, Deserialize)]
pub struct Reject {
    pub order_id: OrderId,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub(crate) enum RolloverMsg {
    Msg0(RolloverMsg0),
    Msg1(RolloverMsg1),
    Msg2(RolloverMsg2),
}

impl RolloverMsg {
    pub fn try_into_msg0(self) -> Result<RolloverMsg0> {
        if let Self::Msg0(v) = self {
            Ok(v)
        } else {
            bail!("Not Msg0")
        }
    }

    pub fn try_into_msg1(self) -> Result<RolloverMsg1> {
        if let Self::Msg1(v) = self {
            Ok(v)
        } else {
            bail!("Not Msg1")
        }
    }

    pub fn try_into_msg2(self) -> Result<RolloverMsg2> {
        if let Self::Msg2(v) = self {
            Ok(v)
        } else {
            bail!("Not Msg2")
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub(crate) struct RolloverMsg0 {
    pub revocation_pk: PublicKey,
    pub publish_pk: PublicKey,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct RolloverMsg1 {
    pub commit: EcdsaAdaptorSignature,
    pub cets: HashMap<String, Vec<(RangeInclusive<u64>, EcdsaAdaptorSignature)>>,
    pub refund: Signature,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub(crate) struct RolloverMsg2 {
    pub revocation_sk: SecretKey,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub(crate) struct RolloverMsg3;

impl From<CfdTransactions> for RolloverMsg1 {
    fn from(txs: CfdTransactions) -> Self {
        let cets = txs
            .cets
            .into_iter()
            .map(|grouped_cets| {
                (
                    grouped_cets.event.id,
                    grouped_cets
                        .cets
                        .into_iter()
                        .map(|(_, encsig, digits)| (digits.range(), encsig))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();
        Self {
            commit: txs.commit.1,
            cets,
            refund: txs.refund.1,
        }
    }
}

/// Fee to be paid for the rollover.
///
/// The maker comes up with this amount so that both parties are on the same page
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CompleteFee {
    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_sat")]
    LongPaysShort(Amount),
    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_sat")]
    ShortPaysLong(Amount),
    Nein,
}

impl From<model::CompleteFee> for CompleteFee {
    fn from(complete_fee: model::CompleteFee) -> Self {
        match complete_fee {
            model::CompleteFee::LongPaysShort(a) => CompleteFee::LongPaysShort(a),
            model::CompleteFee::ShortPaysLong(a) => CompleteFee::ShortPaysLong(a),
            model::CompleteFee::None => CompleteFee::Nein,
        }
    }
}

impl From<CompleteFee> for model::CompleteFee {
    fn from(complete_fee: CompleteFee) -> Self {
        match complete_fee {
            CompleteFee::LongPaysShort(a) => model::CompleteFee::LongPaysShort(a),
            CompleteFee::ShortPaysLong(a) => model::CompleteFee::ShortPaysLong(a),
            CompleteFee::Nein => model::CompleteFee::None,
        }
    }
}

pub(crate) async fn emit_completed<E>(
    order_id: OrderId,
    dlc: Dlc,
    funding_fee: FundingFee,
    complete_fee: model::CompleteFee,
    executor: &E,
) where
    E: ExecuteOnCfd,
{
    if let Err(e) = executor
        .execute(order_id, |cfd| {
            Ok(cfd.complete_rollover(dlc, funding_fee, Some(complete_fee)))
        })
        .await
    {
        tracing::error!(%order_id, "Failed to execute rollover completed: {e:#}")
    }

    tracing::info!(%order_id, "Rollover completed");
}

pub(crate) async fn emit_rejected<E>(order_id: OrderId, executor: &E)
where
    E: ExecuteOnCfd,
{
    if let Err(e) = executor
        .execute(order_id, |cfd| {
            Ok(cfd.reject_rollover(anyhow!("maker decision")))
        })
        .await
    {
        tracing::error!(%order_id, "Failed to execute rollover rejected: {e:#}")
    }

    tracing::info!(%order_id, "Rollover rejected");
}

pub(crate) async fn emit_failed<E>(order_id: OrderId, e: anyhow::Error, executor: &E)
where
    E: ExecuteOnCfd,
{
    tracing::error!(%order_id, "Rollover failed: {e:#}");

    if let Err(e) = executor
        .execute(order_id, |cfd| Ok(cfd.fail_rollover(e)))
        .await
    {
        tracing::error!(%order_id, "Failed to execute rollover failed: {e:#}")
    }
}

#[derive(Debug, Copy, Clone)]
pub(crate) struct PunishParams {
    pub(crate) maker: maia_core::PunishParams,
    pub(crate) taker: maia_core::PunishParams,
}

impl PunishParams {
    pub(crate) fn new(
        maker_revocation: PublicKey,
        taker_revocation: PublicKey,
        maker_publish: PublicKey,
        taker_publish: PublicKey,
    ) -> Self {
        Self {
            maker: maia_core::PunishParams {
                revocation_pk: maker_revocation,
                publish_pk: maker_publish,
            },
            taker: maia_core::PunishParams {
                revocation_pk: taker_revocation,
                publish_pk: taker_publish,
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_own_cfd_transactions(
    dlc: &Dlc,
    rollover_params: RolloverParams,
    announcements: Vec<olivia::Announcement>,
    oracle_pk: XOnlyPublicKey,
    our_position: Position,
    n_payouts: usize,
    complete_fee: model::CompleteFee,
    punish_params: PunishParams,
    role: Role,
) -> Result<CfdTransactions> {
    let sk = dlc.identity;

    let maker_lock_amount = dlc.maker_lock_amount;
    let taker_lock_amount = dlc.taker_lock_amount;

    let payouts = Payouts::new(
        our_position,
        role,
        rollover_params.price,
        rollover_params.quantity,
        rollover_params.long_leverage,
        rollover_params.short_leverage,
        n_payouts,
        complete_fee,
    )?;
    let payouts_per_event = OraclePayouts::new(payouts, announcements)?;

    // unsign lock tx because PartiallySignedTransaction needs an unsigned tx
    let mut unsigned_lock_tx = dlc.lock.0.clone();
    unsigned_lock_tx
        .input
        .iter_mut()
        .for_each(|input| input.witness.clear());

    let lock_tx = PartiallySignedTransaction::from_unsigned_tx(unsigned_lock_tx)?;
    let maker_identity_pk = dlc.maker_identity_pk(role);
    let taker_identity_pk = dlc.taker_identity_pk(role);
    let own_cfd_txs = tokio::task::spawn_blocking({
        let maker_address = dlc.maker_address.clone();
        let taker_address = dlc.taker_address.clone();
        let lock_tx = lock_tx.clone();

        move || {
            renew_cfd_transactions(
                lock_tx,
                (
                    maker_identity_pk,
                    maker_lock_amount,
                    maker_address,
                    maia_core::PunishParams {
                        revocation_pk: punish_params.maker.revocation_pk,
                        publish_pk: punish_params.maker.publish_pk,
                    },
                ),
                (
                    taker_identity_pk,
                    taker_lock_amount,
                    taker_address,
                    maia_core::PunishParams {
                        revocation_pk: punish_params.taker.revocation_pk,
                        publish_pk: punish_params.taker.publish_pk,
                    },
                ),
                oracle_pk,
                (CET_TIMELOCK, rollover_params.refund_timelock),
                payouts_per_event.into(),
                sk,
                rollover_params.fee_rate.to_u32(),
            )
        }
    })
    .await?
    .context("Failed to create new CFD transactions")?;

    Ok(own_cfd_txs)
}

pub(crate) fn build_commit_descriptor(
    maker_identity: PublicKey,
    taker_identity: PublicKey,
    punish_params: PunishParams,
) -> Descriptor<PublicKey> {
    commit_descriptor(
        (
            maker_identity,
            punish_params.maker.revocation_pk,
            punish_params.maker.publish_pk,
        ),
        (
            taker_identity,
            punish_params.taker.revocation_pk,
            punish_params.taker.publish_pk,
        ),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_and_verify_cets_and_refund(
    dlc: &Dlc,
    oracle_pk: XOnlyPublicKey,
    publish_pk: PublicKey,
    our_role: Role,
    own_cfd_txs: &CfdTransactions,
    commit_desc: &Descriptor<PublicKey>,
    msg1: &RolloverMsg1,
) -> Result<(HashMap<BitMexPriceEventId, Vec<Cet>>, Transaction)> {
    let lock_amount = dlc.taker_lock_amount + dlc.maker_lock_amount;

    let own_cets = own_cfd_txs.cets.clone();
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

    for Cets { event, cets } in own_cets.iter() {
        let counterparty_cets = msg1
            .cets
            .get(&event.id)
            .cloned()
            .context("Expect event to exist in msg")?;

        verify_cets(
            (oracle_pk, event.nonce_pks.clone()),
            PartyParams {
                lock_psbt: own_cfd_txs.lock.clone(),
                identity_pk: dlc.identity_counterparty,
                lock_amount,
                address: counterparty_address.clone(),
            },
            cets.clone(),
            counterparty_cets,
            commit_desc.clone(),
            commit_amount,
        )
        .context("CET signatures don't verify")?;
    }

    let refund_tx = own_cfd_txs.refund.0.clone();

    verify_signature(
        &refund_tx,
        commit_desc,
        commit_amount,
        &msg1.refund,
        &dlc.identity_counterparty,
    )
    .context("Refund signature does not verify")?;

    let maker_address = &dlc.maker_address;
    let taker_address = &dlc.taker_address;
    let cets = own_cets
        .into_iter()
        .map(|Cets { event, cets }| {
            let event_id = event.id;
            let counterparty_cets = msg1
                .cets
                .get(&event_id)
                .with_context(|| format!("Counterparty CETs for event {event_id} missing"))?;
            let cets = cets
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
                        cet.to_tx((&commit_tx, commit_desc), maker_address, taker_address)
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

    Ok((cets, refund_tx))
}

pub(crate) fn finalize_revoked_commits(
    dlc: &Dlc,
    commit_tx_adaptor_sig: EcdsaAdaptorSignature,
    msg2: RolloverMsg2,
    complete_fee_before_rollover: model::CompleteFee,
) -> Result<Vec<RevokedCommit>> {
    let revocation_sk_theirs = msg2.revocation_sk;

    {
        let derived_rev_pk = PublicKey::new(secp256k1_zkp::PublicKey::from_secret_key(
            SECP256K1,
            &revocation_sk_theirs,
        ));

        if derived_rev_pk != dlc.revocation_pk_counterparty {
            bail!("Counterparty sent invalid revocation sk");
        }
    }

    let mut revoked_commit = dlc.revoked_commit.clone();
    let transaction = dlc.commit.0.clone();
    revoked_commit.push(RevokedCommit {
        encsig_ours: commit_tx_adaptor_sig,
        revocation_sk_theirs,
        publication_pk_theirs: dlc.publish_pk_counterparty,
        txid: transaction.txid(),
        script_pubkey: dlc.commit.2.script_pubkey(),
        settlement_event_id: Some(dlc.settlement_event_id),
        complete_fee: Some(complete_fee_before_rollover),
    });

    Ok(revoked_commit)
}

#[async_trait]
pub trait GetAnnouncements {
    async fn get_announcements(
        &self,
        events: Vec<BitMexPriceEventId>,
    ) -> Result<Vec<olivia::Announcement>>;
}

#[async_trait]
pub trait GetRates {
    async fn get_rates(&self) -> Result<Rates>;
}

/// Set of rates needed to accept rollover proposals.
#[derive(Clone, Copy)]
pub struct Rates {
    pub(crate) funding_rate_long: FundingRate,
    pub(crate) funding_rate_short: FundingRate,
    pub(crate) tx_fee_rate: TxFeeRate,
}

impl Rates {
    pub fn new(
        long_funding_rate: FundingRate,
        short_funding_rate: FundingRate,
        tx_fee_rate: TxFeeRate,
    ) -> Self {
        Self {
            funding_rate_long: long_funding_rate,
            funding_rate_short: short_funding_rate,
            tx_fee_rate,
        }
    }
}
