use anyhow::Context;
use anyhow::Result;
use bdk::bitcoin::secp256k1::ecdsa::Signature;
use bdk::bitcoin::secp256k1::SECP256K1;
use bdk::bitcoin::Amount;
use bdk::bitcoin::Transaction;
use bdk::descriptor::Descriptor;
use maia::compute_adaptor_pk;
use maia::spending_tx_sighash;
use maia_core::interval;
use maia_core::secp256k1_zkp::EcdsaAdaptorSignature;
use maia_core::secp256k1_zkp::XOnlyPublicKey;
use maia_core::PartyParams;
use std::ops::RangeInclusive;

pub(crate) async fn verify_cets(
    (oracle_pk, nonce_pks): (XOnlyPublicKey, Vec<XOnlyPublicKey>),
    counterparty: PartyParams,
    own_cets: Vec<(Transaction, EcdsaAdaptorSignature, interval::Digits)>,
    counterparty_cets: Vec<(RangeInclusive<u64>, EcdsaAdaptorSignature)>,
    commit_desc: Descriptor<bdk::bitcoin::PublicKey>,
    commit_amount: Amount,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        for (tx, _, digits) in own_cets.iter() {
            let counterparty_encsig = counterparty_cets
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

pub(crate) fn verify_adaptor_signature(
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

pub(crate) fn verify_signature(
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

pub(crate) fn verify_cet_encsig(
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
