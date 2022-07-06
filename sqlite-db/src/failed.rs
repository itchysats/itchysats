//! This module allows us to move failed CFDs to a separate table so
//! that we can reason about loading them independently from open
//! CFDs.
//!
//! Failed CFDs are ones that did not reach the point of having a DLC
//! on chain. This can happen either because the taker's order was
//! rejected or contract setup did not finish successfully.
//!
//! Therefore, it also provides an interface to load failed CFDs: the
//! `FailedCfdAggregate` trait. Implementers of the trait will be able
//! to call the `crate::db::load_all_cfds` API, which loads all types
//! of CFD.

use crate::delete_from_cfds_table;
use crate::delete_from_events_table;
use crate::derive_known_peer_id;
use crate::event_log::EventLog;
use crate::event_log::EventLogEntry;
use crate::load_cfd_events;
use crate::load_cfd_row;
use crate::models;
use crate::Cfd;
use crate::CfdAggregate;
use crate::Connection;
use anyhow::bail;
use anyhow::Result;
use model::libp2p::PeerId;
use model::long_and_short_leverage;
use model::EventKind;
use model::FailedCfd;
use model::FeeAccount;
use model::FundingFee;
use model::OrderId;
use model::Timestamp;
use models::FailedKind;
use sqlx::pool::PoolConnection;
use sqlx::Connection as _;
use sqlx::Sqlite;
use sqlx::Transaction;

/// A trait for building an aggregate based on a `FailedCfd`.
pub trait FailedCfdAggregate: CfdAggregate {
    fn new_failed(args: Self::CtorArgs, cfd: FailedCfd) -> Self;
}

impl Connection {
    pub async fn move_to_failed_cfds(&self) -> Result<()> {
        let ids = self.failed_cfd_ids_according_to_events().await?;

        if !ids.is_empty() {
            tracing::debug!("Moving failed CFDs to failed_cfds table: {ids:?}");
        }

        for id in ids.into_iter() {
            let pool = self.inner.clone();

            let fut = async move {
                let mut conn = pool.acquire().await?;
                let mut db_tx = conn.begin().await?;

                let cfd = load_cfd_row(&mut db_tx, id).await?;

                let events = load_cfd_events(&mut db_tx, id, 0).await?;
                let event_log = EventLog::new(&events);

                insert_failed_cfd(&mut db_tx, cfd, &event_log).await?;
                insert_event_log(&mut db_tx, id, event_log).await?;

                delete_from_events_table(&mut db_tx, id).await?;
                delete_from_cfds_table(&mut db_tx, id).await?;

                db_tx.commit().await?;

                anyhow::Ok(())
            };

            match fut.await {
                Ok(()) => tracing::debug!(order_id = %id, "Moved CFD to `failed_cfds` table"),
                Err(e) => tracing::warn!(order_id = %id, "Failed to move failed CFD: {e:#}"),
            }
        }

        Ok(())
    }

    /// Load a failed CFD from the database.
    pub async fn load_failed_cfd<C>(&self, id: OrderId, args: C::CtorArgs) -> Result<C>
    where
        C: FailedCfdAggregate,
    {
        let mut conn = self.inner.acquire().await?;

        let inner_id = models::OrderId::from(id);
        let cfd = sqlx::query!(
            r#"
            SELECT
                uuid as "id: models::OrderId",
                position as "position: models::Position",
                initial_price as "initial_price: models::Price",
                taker_leverage as "taker_leverage: models::Leverage",
                n_contracts as "n_contracts: models::Contracts",
                counterparty_network_identity as "counterparty_network_identity: models::Identity",
                counterparty_peer_id as "counterparty_peer_id: models::PeerId",
                role as "role: models::Role",
                fees as "fees: models::Fees",
                kind as "kind: models::FailedKind"
            FROM
                failed_cfds
            WHERE
                failed_cfds.uuid = $1
            "#,
            inner_id
        )
        .fetch_one(&mut conn)
        .await?;

        let creation_timestamp = load_creation_timestamp(&mut conn, id).await?;

        let cfd = FailedCfd {
            id,
            position: cfd.position.into(),
            initial_price: cfd.initial_price.into(),
            taker_leverage: cfd.taker_leverage.into(),
            n_contracts: cfd.n_contracts.into(),
            counterparty_network_identity: cfd.counterparty_network_identity.into(),
            counterparty_peer_id: cfd.counterparty_peer_id.into(),
            role: cfd.role.into(),
            fees: cfd.fees.into(),
            kind: cfd.kind.into(),
            creation_timestamp,
        };

        Ok(C::new_failed(args, cfd))
    }

    pub(crate) async fn load_failed_cfd_ids(&self) -> Result<Vec<OrderId>> {
        let mut conn = self.inner.acquire().await?;

        let ids = sqlx::query!(
            r#"
            SELECT
                uuid as "uuid: models::OrderId"
            FROM
                failed_cfds
            "#
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|r| r.uuid.into())
        .collect();

        Ok(ids)
    }
}

async fn insert_failed_cfd(
    conn: &mut Transaction<'_, Sqlite>,
    cfd: Cfd,
    event_log: &EventLog,
) -> Result<()> {
    let kind = if event_log.contains(&EventKind::OfferRejected) {
        FailedKind::OfferRejected
    } else if event_log.contains(&EventKind::ContractSetupFailed) {
        FailedKind::ContractSetupFailed
    } else {
        bail!("Failed CFD does not have expected event")
    };

    let n_contracts = cfd
        .quantity_usd
        .try_into_u64()
        .expect("number of contracts to fit into a u64");
    let n_contracts = models::Contracts::from(n_contracts);

    let fees = {
        let (long_leverage, short_leverage) =
            long_and_short_leverage(cfd.taker_leverage, cfd.role, cfd.position);

        let initial_funding_fee = FundingFee::calculate(
            cfd.initial_price,
            cfd.quantity_usd,
            long_leverage,
            short_leverage,
            cfd.initial_funding_rate,
            cfd.settlement_interval.whole_hours(),
        )
        .expect("values from db to be sane");

        let fee_account = FeeAccount::new(cfd.position, cfd.role)
            .add_opening_fee(cfd.opening_fee)
            .add_funding_fee(initial_funding_fee);

        models::Fees::from(fee_account.balance())
    };

    let counterparty_peer_id = match cfd.counterparty_peer_id {
        None => derive_known_peer_id(cfd.counterparty_network_identity, cfd.role)
            .unwrap_or_else(PeerId::placeholder),
        Some(peer_id) => peer_id,
    };

    let id = models::OrderId::from(cfd.id);
    let role = models::Role::from(cfd.role);
    let initial_price = models::Price::from(cfd.initial_price);
    let taker_leverage = models::Leverage::from(cfd.taker_leverage);
    let position = models::Position::from(cfd.position);
    let counterparty_network_identity = models::Identity::from(cfd.counterparty_network_identity);
    let counterparty_peer_id = models::PeerId::from(counterparty_peer_id);

    let query_result = sqlx::query!(
        r#"
        INSERT INTO failed_cfds
        (
            uuid,
            position,
            initial_price,
            taker_leverage,
            n_contracts,
            counterparty_network_identity,
            counterparty_peer_id,
            role,
            fees,
            kind
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        "#,
        id,
        position,
        initial_price,
        taker_leverage,
        n_contracts,
        counterparty_network_identity,
        counterparty_peer_id,
        role,
        fees,
        kind,
    )
    .execute(&mut *conn)
    .await?;

    if query_result.rows_affected() != 1 {
        bail!("failed to insert into failed_cfds");
    }

    Ok(())
}

async fn insert_event_log(
    conn: &mut Transaction<'_, Sqlite>,
    id: OrderId,
    event_log: EventLog,
) -> Result<()> {
    let id = models::OrderId::from(id);

    for EventLogEntry { name, created_at } in event_log.0.iter() {
        let query_result = sqlx::query!(
            r#"
            INSERT INTO event_log_failed (
                cfd_id,
                name,
                created_at
            )
            VALUES
            (
                (SELECT id FROM failed_cfds WHERE failed_cfds.uuid = $1),
                $2, $3
            )
            "#,
            id,
            name,
            created_at
        )
        .execute(&mut *conn)
        .await?;

        if query_result.rows_affected() != 1 {
            bail!("failed to insert into event_log_failed");
        }
    }

    Ok(())
}

/// Obtain the time at which the failed CFD was created, according to
/// the `event_log_failed` table.
///
/// We use the timestamp of the first event for a particular CFD `id`
/// in the `event_log_failed` table.
async fn load_creation_timestamp(
    conn: &mut PoolConnection<Sqlite>,
    id: OrderId,
) -> Result<Timestamp> {
    let id = models::OrderId::from(id);

    let row = sqlx::query!(
        r#"
        SELECT
            event_log_failed.created_at as "created_at!: i64"
        FROM
            event_log_failed
        JOIN
            failed_cfds on failed_cfds.id = event_log_failed.cfd_id
        WHERE
            failed_cfds.uuid = $1
        ORDER BY event_log_failed.created_at ASC
        LIMIT 1
        "#,
        id,
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok(Timestamp::new(row.created_at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory;
    use crate::tests::dummy_cfd;
    use crate::tests::lock_confirmed;
    use crate::tests::order_rejected;
    use crate::tests::setup_failed;
    use model::CfdEvent;

    #[tokio::test]
    async fn given_offer_rejected_when_move_cfds_to_failed_table_then_can_load_cfd_as_failed() {
        let db = memory().await.unwrap();

        let cfd = dummy_cfd();
        let order_id = cfd.id();

        db.insert_cfd(&cfd).await.unwrap();

        db.append_event(order_rejected(&cfd)).await.unwrap();

        db.move_to_failed_cfds().await.unwrap();

        let load_from_open = db.load_open_cfd::<DummyAggregate>(order_id, ()).await;
        let load_from_events = {
            let mut conn = db.inner.acquire().await.unwrap();
            let mut db_tx = conn.begin().await.unwrap();
            let res = load_cfd_events(&mut db_tx, order_id, 0).await.unwrap();
            db_tx.commit().await.unwrap();

            res
        };
        let load_from_failed = db.load_failed_cfd::<DummyAggregate>(order_id, ()).await;

        assert!(load_from_open.is_err());
        assert!(load_from_events.is_empty());
        assert!(load_from_failed.is_ok());
    }

    #[tokio::test]
    async fn given_contract_setup_failed_when_move_cfds_to_failed_table_then_can_load_cfd_as_failed(
    ) {
        let db = memory().await.unwrap();

        let cfd = dummy_cfd();
        let order_id = cfd.id();

        db.insert_cfd(&cfd).await.unwrap();

        db.append_event(setup_failed(&cfd)).await.unwrap();

        db.move_to_failed_cfds().await.unwrap();

        let load_from_open = db.load_open_cfd::<DummyAggregate>(order_id, ()).await;
        let load_from_events = {
            let mut conn = db.inner.acquire().await.unwrap();
            let mut db_tx = conn.begin().await.unwrap();
            let res = load_cfd_events(&mut db_tx, order_id, 0).await.unwrap();
            db_tx.commit().await.unwrap();

            res
        };
        let load_from_failed = db.load_failed_cfd::<DummyAggregate>(order_id, ()).await;

        assert!(load_from_open.is_err());
        assert!(load_from_events.is_empty());
        assert!(load_from_failed.is_ok());
    }

    #[tokio::test]
    async fn given_cfd_without_failed_events_when_move_cfds_to_failed_table_then_cannot_load_cfd_as_failed(
    ) {
        let db = memory().await.unwrap();

        let cfd = dummy_cfd();
        let order_id = cfd.id();

        db.insert_cfd(&cfd).await.unwrap();

        // appending an event which does not imply that the CFD failed
        db.append_event(lock_confirmed(&cfd)).await.unwrap();

        db.move_to_failed_cfds().await.unwrap();

        let load_from_open = db.load_open_cfd::<DummyAggregate>(order_id, ()).await;
        let load_from_events = {
            let mut conn = db.inner.acquire().await.unwrap();
            let mut db_tx = conn.begin().await.unwrap();
            let res = load_cfd_events(&mut db_tx, order_id, 0).await.unwrap();
            db_tx.commit().await.unwrap();

            res
        };
        let load_from_failed = db.load_failed_cfd::<DummyAggregate>(order_id, ()).await;

        assert!(load_from_open.is_ok());
        assert_eq!(load_from_events.len(), 1);
        assert!(load_from_failed.is_err());
    }

    #[tokio::test]
    async fn given_contract_setup_failed_when_move_cfds_to_failed_table_then_creation_timestamp_is_that_of_contract_setup_started_event(
    ) {
        let db = memory().await.unwrap();

        let cfd = dummy_cfd();
        let id = cfd.id();

        db.insert_cfd(&cfd).await.unwrap();

        let contract_setup_started_timestamp = Timestamp::new(1);
        let contract_setup_started = CfdEvent {
            timestamp: contract_setup_started_timestamp,
            id,
            event: EventKind::ContractSetupStarted,
        };
        db.append_event(contract_setup_started).await.unwrap();

        let contract_setup_failed_timestamp = Timestamp::new(2);
        let contract_setup_failed = CfdEvent {
            timestamp: contract_setup_failed_timestamp,
            id,
            event: EventKind::ContractSetupFailed,
        };
        db.append_event(contract_setup_failed).await.unwrap();

        db.move_to_failed_cfds().await.unwrap();

        let mut conn = db.inner.acquire().await.unwrap();
        let creation_timestamp = load_creation_timestamp(&mut conn, id).await.unwrap();

        assert_ne!(creation_timestamp, contract_setup_failed_timestamp);
        assert_eq!(creation_timestamp, contract_setup_started_timestamp);
    }

    #[derive(Debug, Clone)]
    struct DummyAggregate;

    impl CfdAggregate for DummyAggregate {
        type CtorArgs = ();

        fn new(_: Self::CtorArgs, _: Cfd) -> Self {
            Self
        }

        fn apply(self, _: CfdEvent) -> Self {
            Self
        }

        fn version(&self) -> u32 {
            0
        }
    }

    impl FailedCfdAggregate for DummyAggregate {
        fn new_failed(_: Self::CtorArgs, _: FailedCfd) -> Self {
            Self
        }
    }
}
