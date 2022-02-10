use crate::db;
use crate::olivia;
use crate::process_manager;
use crate::projection;
use crate::try_continue;
use anyhow::Context;
use anyhow::Result;
use model::cfd::Cfd;
use model::cfd::OrderId;
use sqlx::pool::PoolConnection;
use sqlx::Sqlite;
use sqlx::SqlitePool;

pub async fn insert_cfd_and_update_feed(
    cfd: &Cfd,
    conn: &mut PoolConnection<Sqlite>,
    projection_address: &xtra::Address<projection::Actor>,
) -> Result<()> {
    db::insert_cfd(cfd, conn).await?;
    projection_address
        .send(projection::CfdChanged(cfd.id()))
        .await?;
    Ok(())
}

/// Load a CFD from the database and rehydrate as the [`model::cfd::Cfd`] aggregate.
pub async fn load_cfd(order_id: OrderId, conn: &mut PoolConnection<Sqlite>) -> Result<Cfd> {
    let (
        db::Cfd {
            id,
            position,
            initial_price,
            leverage,
            settlement_interval,
            counterparty_network_identity,
            role,
            quantity_usd,
            opening_fee,
            initial_funding_rate,
            initial_tx_fee_rate,
        },
        events,
    ) = db::load_cfd(order_id, conn).await?;
    let cfd = Cfd::rehydrate(
        id,
        position,
        initial_price,
        leverage,
        settlement_interval,
        quantity_usd,
        counterparty_network_identity,
        role,
        opening_fee,
        initial_funding_rate,
        initial_tx_fee_rate,
        events,
    );
    Ok(cfd)
}

pub async fn handle_oracle_attestation(
    attestation: &olivia::Attestation,
    db: &SqlitePool,
    process_manager: &xtra::Address<process_manager::Actor>,
) -> Result<()> {
    let mut conn = db.acquire().await?;
    let price_event_id = attestation.id;

    tracing::debug!("Learnt latest oracle attestation for event: {price_event_id}");

    for id in db::load_all_cfd_ids(&mut conn).await? {
        let cfd = try_continue!(load_cfd(id, &mut conn).await);
        let event = try_continue!(cfd
            .decrypt_cet(attestation)
            .context("Failed to decrypt CET using attestation"));

        if let Some(event) = event {
            // Note: ? OK, because if the actor is disconnected we can fail the loop
            if let Err(e) = process_manager
                .send(process_manager::Event::new(event.clone()))
                .await?
            {
                tracing::error!("Sending event to process manager failed: {:#}", e);
            }
        }
    }

    Ok(())
}
