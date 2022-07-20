mod sqlx_ext; // Must come first because it is a macro.

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use chashmap_async::CHashMap;
use futures::future::BoxFuture;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use model::libp2p::PeerId;
use model::CfdEvent;
use model::EventKind;
use model::FundingRate;
use model::Identity;
use model::Leverage;
use model::OpeningFee;
use model::OrderId;
use model::Position;
use model::Price;
use model::Role;
use model::TxFeeRate;
use model::Usd;
use sqlx::migrate::MigrateError;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::Connection as _;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;
use std::any::Any;
use std::any::TypeId;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use time::Duration;

pub use closed::*;
pub use failed::*;
use model::EventKind::RolloverCompleted;

pub mod closed;
pub mod event_log;
pub mod failed;
mod impls;
mod models;
mod rollover;
pub mod time_to_first_position;

#[derive(Clone)]
pub struct Connection {
    inner: SqlitePool,
    aggregate_cache: Arc<CHashMap<(TypeId, OrderId), Box<dyn Any + Send + Sync + 'static>>>,
}

impl Connection {
    fn new(pool: SqlitePool) -> Self {
        Self {
            inner: pool,
            aggregate_cache: Arc::new(CHashMap::new()),
        }
    }

    pub async fn close(self) {
        self.inner.close().await;
    }
}

/// Connects to the SQLite database at the given path.
///
/// If the database does not exist, it will be created. If it does exist, we load it and apply all
/// pending migrations. If applying migrations fails, the old database is backed up next to it and a
/// new one is created.
pub fn connect(
    path: PathBuf,
    ignore_migration_errors: bool,
) -> BoxFuture<'static, Result<Connection>> {
    async move {
        let pool = SqlitePool::connect_with(
            SqliteConnectOptions::new()
                .create_if_missing(true)
                .filename(&path),
        )
        .await?;

        let path_display = path.display();

        // Attempt to migrate, early return if successful
        let error = match run_migrations(&pool).await {
            Ok(()) => {
                tracing::info!("Opened database at {path_display}");

                return Ok(Connection::new(pool));
            }
            Err(e) => e,
        };

        if !ignore_migration_errors {
            bail!("Could not access database due to '{error:#}'. Please backup your database and start again or disable failsafe mode. Your db path is: {path_display}");
        }

        // Attempt to recover from _some_ problems during migration.
        // These two can happen if someone tampered with the migrations or messed with the DB.
        if let Some(MigrateError::VersionMissing(_) | MigrateError::VersionMismatch(_)) =
            error.downcast_ref::<MigrateError>()
        {
            tracing::error!("{:#}", error);

            let unix_timestamp = time::OffsetDateTime::now_utc().unix_timestamp();

            let new_path = PathBuf::from(format!("{path_display}-{unix_timestamp}-backup"));
            let new_path_display = new_path.display();

            tracing::info!("Backing up old database at {path_display} to {new_path_display}");

            tokio::fs::rename(&path, &new_path)
                .await
                .context("Failed to rename database file")?;

            tracing::info!("Starting with a new database!");

            // recurse to reconnect (async recursion requires a `BoxFuture`)
            return connect(path, ignore_migration_errors).await;
        }

        Err(error)
    }
    .boxed()
}

pub async fn memory() -> Result<Connection> {
    // Note: Every :memory: database is distinct from every other. So, opening two database
    // connections each with the filename ":memory:" will create two independent in-memory
    // databases. see: https://www.sqlite.org/inmemorydb.html
    let pool = SqlitePool::connect(":memory:").await?;

    run_migrations(&pool).await?;

    Ok(Connection::new(pool))
}

async fn run_migrations(pool: &SqlitePool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .context("Failed to run migrations")?;

    Ok(())
}

impl Connection {
    pub async fn insert_cfd(&self, cfd: &model::Cfd) -> Result<()> {
        let mut conn = self.inner.acquire().await?;

        let id = models::OrderId::from(cfd.id());

        let role = models::Role::from(cfd.role());
        let quantity = models::Usd::from(cfd.quantity());
        let initial_price = models::Price::from(cfd.initial_price());
        let leverage = models::Leverage::from(cfd.taker_leverage());

        let position = models::Position::from(cfd.position());
        let counterparty_network_identity =
            models::Identity::from(cfd.counterparty_network_identity());
        let initial_funding_rate = models::FundingRate::from(cfd.initial_funding_rate());
        let opening_fee = models::OpeningFee::from(cfd.opening_fee());
        let tx_fee_rate = models::TxFeeRate::from(cfd.initial_tx_fee_rate());
        let counterparty_peer_id = cfd.counterparty_peer_id().map(models::PeerId::from);

        let query_result = sqlx::query(
            r#"
        insert into cfds (
            uuid,
            position,
            initial_price,
            leverage,
            settlement_time_interval_hours,
            quantity_usd,
            counterparty_network_identity,
            counterparty_peer_id,
            role,
            opening_fee,
            initial_funding_rate,
            initial_tx_fee_rate
        ) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"#,
        )
        .bind(&id)
        .bind(&position)
        .bind(&initial_price)
        .bind(&leverage)
        .bind(&cfd.settlement_time_interval_hours().whole_hours())
        .bind(&quantity)
        .bind(&counterparty_network_identity)
        .bind(&counterparty_peer_id.unwrap_or_else(|| {
            tracing::debug!(
                order_id=%cfd.id(),
                counterparty_identity=%cfd.counterparty_network_identity(),
                "Inserting deprecated CFD with placeholder peer-id"
            );
            models::PeerId::from(model::libp2p::PeerId::placeholder())
        }))
        .bind(&role)
        .bind(&opening_fee)
        .bind(&initial_funding_rate)
        .bind(&tx_fee_rate)
        .execute(&mut conn)
        .await?;

        if query_result.rows_affected() != 1 {
            anyhow::bail!("failed to insert cfd");
        }

        Ok(())
    }

    /// Appends an event to the `events` table.
    ///
    /// To make handling of `None` events more ergonomic, you can pass anything in here that
    /// implements `Into<Option>` event.
    pub async fn append_event(&self, event: impl Into<Option<CfdEvent>>) -> Result<()> {
        let mut conn = self.inner.acquire().await?;

        let event = match event.into() {
            Some(event) => event,
            None => return Ok(()),
        };

        let (event_name, event_data) = event.event.to_json();

        let order_id = models::OrderId::from(event.id);
        let timestamp = models::Timestamp::from(event.timestamp);
        let query_result = sqlx::query(
            r##"
        insert into events (
            cfd_id,
            name,
            data,
            created_at
        ) values (
            (select id from cfds where cfds.uuid = $1),
            $2, $3, $4
        )"##,
        )
        .bind(&order_id)
        .bind(&event_name)
        .bind(&event_data)
        .bind(&timestamp)
        .execute(&mut conn)
        .await?;

        if query_result.rows_affected() != 1 {
            anyhow::bail!("failed to insert event");
        }

        // if we have a rollover completed event we store it additionally in its own table
        if let RolloverCompleted { .. } = event.event {
            rollover::insert(&mut conn, query_result.last_insert_rowid(), event).await?;
        }

        tracing::info!(event = %event_name, %order_id, "Appended event to database");

        Ok(())
    }

    /// Load a CFD in its latest version from the database.
    pub async fn load_open_cfd<C>(&self, id: OrderId, args: C::CtorArgs) -> Result<C, Error>
    where
        C: CfdAggregate,
    {
        let mut conn = self.inner.acquire().await?;
        let mut db_tx = conn.begin().await?;

        let cache_key = (TypeId::of::<C>(), id);
        let aggregate = std::any::type_name::<C>();

        let cfd = match self.aggregate_cache.remove(&cache_key).await {
            None => {
                // No cache entry? Load the CFD row. Version will be 0 because we haven't applied
                // any events, thus all events will be loaded.
                let cfd = load_cfd_row(&mut db_tx, id).await?;

                C::new(args, cfd)
            }
            Some(cfd) => {
                // Got a cache entry: Downcast it to the type at hand.

                *cfd.downcast::<C>()
                    .expect("we index by type id, must be able to downcast")
            }
        };
        let cfd_version = cfd.version();

        let events = load_cfd_events(&mut db_tx, id, cfd_version)
            .await
            .with_context(|| format!("Could not load events for CFD {id}"))?;
        let num_events = events.len();

        tracing::trace!(target = "aggregate", order_id = %id, %aggregate, %cfd_version, %num_events, "Applying new events to CFD");

        let cfd = events.into_iter().fold(cfd, C::apply);

        self.aggregate_cache
            .insert(cache_key, Box::new(cfd.clone()))
            .await;

        db_tx.commit().await?;

        Ok(cfd)
    }

    pub fn load_all_cfds<C>(&self, args: C::CtorArgs) -> impl Stream<Item = Result<C>> + Unpin + '_
    where
        C: CfdAggregate + ClosedCfdAggregate + FailedCfdAggregate,
        C::CtorArgs: Clone + Send + Sync,
    {
        let stream = async_stream::stream! {
            let ids = self.load_open_cfd_ids().await?;
            for id in ids {
                let res = match self.load_open_cfd(id, args.clone()).await {
                    Err(Error::OpenCfdNotFound) => {
                        tracing::trace!(
                            order_id=%id,
                            target="db",
                            "Ignoring OpenCfdNotFound"
                        );
                        continue;
                    }
                    res => res.with_context(|| format!("Could not load open CFD {id}")),
                };

                yield res;
            }

            let ids = self.load_closed_cfd_ids().await?;
            for id in ids {
                yield self.load_closed_cfd(id, args.clone()).await
                    .with_context(|| format!("Failed to load closed CFD {id}"));
            }

            let ids = self.load_failed_cfd_ids().await?;
            for id in ids {
                yield self.load_failed_cfd(id, args.clone()).await
                    .with_context(|| format!("Failed to load failed CFD {id}"));
            }
        };

        stream.boxed()
    }

    /// Loads all CFDs where we are still able to append events
    ///
    /// This function is to be called when we only want to process CFDs where events can still be
    /// appended, but ignore all other CFDs.
    /// Open in this context means that the CFD is not final yet, i.e. we can still append events.
    /// In this context a CFD is not open anymore if one of the following happened:
    /// 1. Event of the confirmation of a payout (spend) transaction on the blockchain was recorded
    ///     Cases: Collaborative settlement, CET, Refund
    /// 2. Event that fails the CFD early was recorded, meaning it becomes irrelevant for processing
    ///     Cases: Setup failed, Taker's take order rejected
    pub fn load_all_open_cfds<C>(
        &self,
        args: C::CtorArgs,
    ) -> impl Stream<Item = Result<C>> + Unpin + '_
    where
        C: CfdAggregate + Unpin,
        C::CtorArgs: Clone + Send + Sync,
    {
        let stream = async_stream::stream! {
            let ids = self.load_open_cfd_ids().await?;

            for id in ids {
                let res = match self.load_open_cfd(id, args.clone()).await {
                    Err(Error::OpenCfdNotFound) => {
                        tracing::trace!(
                            order_id=%id,
                            target="db",
                            "Ignoring OpenCfdNotFound"
                        );
                        continue;
                    }
                    res => res.with_context(|| format!("Could not load open CFD {id}")),
                };

                yield res;
            }
        };

        stream.boxed()
    }

    /// Load the IDs for all the CFDs found in the `cfds` table.
    ///
    /// Importantly, callers **cannot** rely on the CFD IDs returned
    /// corresponding to open CFDs.
    pub async fn load_open_cfd_ids(&self) -> Result<Vec<OrderId>> {
        let mut conn = self.inner.acquire().await?;

        let ids = sqlx::query!(
            r#"
            SELECT
                uuid as "uuid: models::OrderId"
            FROM
                cfds
            "#
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|r| r.uuid.into())
        .collect();

        Ok(ids)
    }

    async fn closed_cfd_ids_according_to_the_blockchain(&self) -> Result<Vec<OrderId>> {
        let mut conn = self.inner.acquire().await?;

        let ids = sqlx::query!(
            r#"
            select
                id as cfd_id,
                uuid as "uuid: models::OrderId"
            from
                cfds
            where exists (
                select id from EVENTS as events
                where events.cfd_id = cfds.id and
                (
                    events.name = $1 or
                    events.name = $2 or
                    events.name= $3
                )
            )
            "#,
            EventKind::COLLABORATIVE_SETTLEMENT_CONFIRMED,
            EventKind::CET_CONFIRMED,
            EventKind::REFUND_CONFIRMED,
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|r| r.uuid.into())
        .collect();

        Ok(ids)
    }

    async fn failed_cfd_ids_according_to_events(&self) -> Result<Vec<OrderId>> {
        let mut conn = self.inner.acquire().await?;

        let ids = sqlx::query!(
            r#"
            select
                id as cfd_id,
                uuid as "uuid: models::OrderId"
            from
                cfds
            where exists (
                select id from EVENTS as events
                where events.cfd_id = cfds.id and
                (
                    events.name = $1 or
                    events.name = $2
                )
            )
            "#,
            model::EventKind::OFFER_REJECTED,
            model::EventKind::CONTRACT_SETUP_FAILED,
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|r| r.uuid.into())
        .collect();

        Ok(ids)
    }
}

// TODO: Make sqlx directly instantiate this struct instead of mapping manually. Need to create
// newtype for `settlement_interval`.
#[derive(Clone, Copy)]
pub struct Cfd {
    pub id: OrderId,
    pub position: Position,
    pub initial_price: Price,
    pub taker_leverage: Leverage,
    pub settlement_interval: Duration,
    pub quantity_usd: Usd,
    pub counterparty_network_identity: Identity,
    pub counterparty_peer_id: Option<PeerId>,
    pub role: Role,
    pub opening_fee: OpeningFee,
    pub initial_funding_rate: FundingRate,
    pub initial_tx_fee_rate: TxFeeRate,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("The CFD requested was not found in the open CFDs")]
    OpenCfdNotFound,
    #[error("{0:#}")]
    Sqlx(#[source] sqlx::Error),
    #[error("{0:#}")]
    Other(#[source] anyhow::Error),
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Error::Sqlx(e)
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Other(e)
    }
}

/// A trait for abstracting over an aggregate.
///
/// Aggregating all available events differs based on the module. Thus, to provide a single
/// interface we ask the caller to provide us with the bare minimum API so we can build (and
/// therefore cache) the aggregate for them.
pub trait CfdAggregate: Clone + Send + Sync + 'static {
    type CtorArgs;

    fn new(args: Self::CtorArgs, cfd: Cfd) -> Self;
    fn apply(self, event: CfdEvent) -> Self;
    fn version(&self) -> u32;
}

async fn load_cfd_row(conn: &mut Transaction<'_, Sqlite>, id: OrderId) -> Result<Cfd, Error> {
    let id = models::OrderId::from(id);

    let cfd_row = sqlx::query!(
        r#"
            select
                id as cfd_id,
                uuid as "uuid: models::OrderId",
                position as "position: models::Position",
                initial_price as "initial_price: models::Price",
                leverage as "leverage: models::Leverage",
                settlement_time_interval_hours,
                quantity_usd as "quantity_usd: models::Usd",
                counterparty_network_identity as "counterparty_network_identity: models::Identity",
                counterparty_peer_id as "counterparty_peer_id: models::PeerId",
                role as "role: models::Role",
                opening_fee as "opening_fee: models::OpeningFee",
                initial_funding_rate as "initial_funding_rate: models::FundingRate",
                initial_tx_fee_rate as "initial_tx_fee_rate: models::TxFeeRate"
            from
                cfds
            where
                cfds.uuid = $1
            "#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(Error::OpenCfdNotFound)?;

    let role = cfd_row.role.into();
    let counterparty_network_identity = cfd_row.counterparty_network_identity.into();
    let counterparty_peer_id = if cfd_row.counterparty_peer_id
        == models::PeerId::from(model::libp2p::PeerId::placeholder())
    {
        derive_known_peer_id(counterparty_network_identity, role)
    } else {
        Some(cfd_row.counterparty_peer_id.into())
    };

    Ok(Cfd {
        id: cfd_row.uuid.into(),
        position: cfd_row.position.into(),
        initial_price: cfd_row.initial_price.into(),
        taker_leverage: cfd_row.leverage.into(),
        settlement_interval: Duration::hours(cfd_row.settlement_time_interval_hours),
        quantity_usd: cfd_row.quantity_usd.into(),
        counterparty_network_identity,
        counterparty_peer_id,
        role,
        opening_fee: cfd_row.opening_fee.into(),
        initial_funding_rate: cfd_row.initial_funding_rate.into(),
        initial_tx_fee_rate: cfd_row.initial_tx_fee_rate.into(),
    })
}

/// Derive the peer id for known makers
///
/// This is used temporarily for rolling out the libp2p based network protocols.
/// The peer-id is defaulted to a placeholder dummy in the database. Based on the dummy we can
/// decide to default the peer-id according to known makers.
fn derive_known_peer_id(legacy_network_identity: Identity, role: Role) -> Option<PeerId> {
    const MAINNET_MAKER_ID: &str =
        "7e35e34801e766a6a29ecb9e22810ea4e3476c2b37bf75882edf94a68b1d9607";
    const MAINNET_MAKER_PEER_ID: &str = "12D3KooWP3BN6bq9jPy8cP7Grj1QyUBfr7U6BeQFgMwfTTu12wuY";

    const TESTNET_MAKER_ID: &str =
        "69a42aa90da8b065b9532b62bff940a3ba07dbbb11d4482c7db83a7e049a9f1e";
    const TESTNET_MAKER_PEER_ID: &str = "12D3KooWEsK2X8Tp24XtyWh7DM65VfwXtNH2cmfs2JsWmkmwKbV1";

    match role {
        // Default known maker PeerIds based on the legacy network identity
        Role::Taker => (match legacy_network_identity.to_string().as_str() {
            MAINNET_MAKER_ID => Some(MAINNET_MAKER_PEER_ID),
            TESTNET_MAKER_ID => Some(TESTNET_MAKER_PEER_ID),
            _ => None,
        })
        .map(|peer_id| PeerId::from_str(peer_id).expect("static peer-id to parse"))
        .map(PeerId::from),
        Role::Maker => None,
    }
}

/// Load events for a given CFD but only onwards from the specified version.
///
/// The version of a CFD is the number of events that have been applied. If we have an aggregate
/// instance in version 3, we can avoid loading the first 3 events and only apply the ones after.
///
/// Events will be sorted in chronological order.
async fn load_cfd_events(
    conn: &mut Transaction<'_, Sqlite>,
    id: OrderId,
    from_version: u32,
) -> Result<Vec<CfdEvent>> {
    let id = models::OrderId::from(id);

    let mut events = sqlx::query!(
        r#"

        select
            c.id as cfd_row_id,
            events.id as event_row_id,
            events.name,
            events.data,
            events.created_at as "created_at: models::Timestamp"
        from
            events
        join
            cfds c on c.id = events.cfd_id
        where
            uuid = $1
        order by
            events.id
        limit $2,-1
            "#,
        id,
        from_version
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|row| {
        Ok((
            row.cfd_row_id.context("CFD with id not found {id}")?,
            row.event_row_id,
            CfdEvent {
                timestamp: row.created_at.into(),
                id: id.into(),
                event: EventKind::from_json(row.name, row.data)?,
            },
        ))
    })
    .collect::<Result<Vec<(i64, i64, CfdEvent)>>>()?;

    for (cfd_row_id, event_row_id, event) in events.iter_mut() {
        if let RolloverCompleted { .. } = event.event {
            if let Some((dlc, funding_fee, complete_fee)) =
                rollover::load(conn, *cfd_row_id, *event_row_id).await?
            {
                event.event = RolloverCompleted {
                    dlc: Some(dlc),
                    funding_fee,
                    complete_fee,
                }
            }
        }
    }

    let events = events
        .into_iter()
        .map(|(_, _, event)| event)
        .collect::<Vec<CfdEvent>>();

    Ok(events)
}

async fn delete_from_cfds_table(conn: &mut Transaction<'_, Sqlite>, id: OrderId) -> Result<()> {
    let id = models::OrderId::from(id);
    let query_result = sqlx::query!(
        r#"
        DELETE FROM
            cfds
        WHERE
            cfds.uuid = $1
        "#,
        id,
    )
    .execute(&mut *conn)
    .await?;

    if query_result.rows_affected() != 1 {
        anyhow::bail!("failed to delete from cfds");
    }

    Ok(())
}

async fn delete_from_events_table(conn: &mut Transaction<'_, Sqlite>, id: OrderId) -> Result<()> {
    let id = models::OrderId::from(id);

    let query_result = sqlx::query!(
        r#"
        DELETE FROM
            events
        WHERE events.cfd_id IN
            (SELECT id FROM cfds WHERE cfds.uuid = $1)
        "#,
        id,
    )
    .execute(&mut *conn)
    .await?;

    if query_result.rows_affected() < 1 {
        anyhow::bail!("failed to delete from events");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bdk::bitcoin::Amount;
    use model::Cfd;
    use model::Leverage;
    use model::OpeningFee;
    use model::Position;
    use model::Price;
    use model::Role;
    use model::Timestamp;
    use model::TxFeeRate;
    use model::Usd;
    use pretty_assertions::assert_eq;
    use rust_decimal_macros::dec;

    #[tokio::test]
    async fn test_insert_and_load_cfd() {
        let db = memory().await.unwrap();

        let cfd = dummy_cfd();
        db.insert_cfd(&cfd).await.unwrap();
        let mut conn = db.inner.acquire().await.unwrap();
        let mut db_tx = conn.begin().await.unwrap();

        let super::Cfd {
            id,
            position,
            initial_price,
            taker_leverage: leverage,
            settlement_interval,
            quantity_usd,
            counterparty_network_identity,
            counterparty_peer_id,
            role,
            opening_fee,
            initial_funding_rate,
            initial_tx_fee_rate,
        } = load_cfd_row(&mut db_tx, cfd.id()).await.unwrap();

        db_tx.commit().await.unwrap();

        assert_eq!(cfd.id(), id);
        assert_eq!(cfd.position(), position);
        assert_eq!(cfd.initial_price(), initial_price);
        assert_eq!(cfd.taker_leverage(), leverage);
        assert_eq!(cfd.settlement_time_interval_hours(), settlement_interval);
        assert_eq!(cfd.quantity(), quantity_usd);
        assert_eq!(
            cfd.counterparty_network_identity(),
            counterparty_network_identity
        );
        assert_eq!(cfd.counterparty_peer_id(), counterparty_peer_id);
        assert_eq!(cfd.role(), role);
        assert_eq!(cfd.opening_fee(), opening_fee);
        assert_eq!(cfd.initial_funding_rate(), initial_funding_rate);
        assert_eq!(cfd.initial_tx_fee_rate(), initial_tx_fee_rate);
    }

    #[tokio::test]
    async fn test_append_events() {
        let db = memory().await.unwrap();

        let cfd = dummy_cfd();
        db.insert_cfd(&cfd).await.unwrap();

        let timestamp = Timestamp::now();

        let event1 = CfdEvent {
            timestamp,
            id: cfd.id(),
            event: EventKind::OfferRejected,
        };

        db.append_event(event1.clone()).await.unwrap();

        let mut conn = db.inner.acquire().await.unwrap();

        let mut db_tx = conn.begin().await.unwrap();
        let events = load_cfd_events(&mut db_tx, cfd.id(), 0).await.unwrap();
        db_tx.commit().await.unwrap();
        assert_eq!(events, vec![event1.clone()]);

        let event2 = CfdEvent {
            timestamp,
            id: cfd.id(),
            event: EventKind::RevokeConfirmed,
        };

        db.append_event(event2.clone()).await.unwrap();

        // let mut conn = db.inner.acquire().await.unwrap();
        let mut db_tx = conn.begin().await.unwrap();
        let events = load_cfd_events(&mut db_tx, cfd.id(), 0).await.unwrap();
        db_tx.commit().await.unwrap();
        assert_eq!(events, vec![event1, event2])
    }

    #[tokio::test]
    async fn given_insert_cfd_with_peer_id_then_peer_id_loaded() {
        let db = memory().await.unwrap();

        let cfd = dummy_taker_with_counterparty_peer_id();
        db.insert_cfd(&cfd).await.unwrap();
        let mut conn = db.inner.acquire().await.unwrap();
        let mut db_tx = conn.begin().await.unwrap();

        let super::Cfd {
            counterparty_peer_id,
            ..
        } = load_cfd_row(&mut db_tx, cfd.id()).await.unwrap();

        db_tx.commit().await.unwrap();

        assert_eq!(cfd.counterparty_peer_id(), counterparty_peer_id);
    }

    #[tokio::test]
    async fn given_insert_cfd_without_peer_id_when_known_mainnet_maker_then_peer_id_loaded() {
        let db = memory().await.unwrap();

        let cfd = dummy_taker_with_legacy_identity(
            "7e35e34801e766a6a29ecb9e22810ea4e3476c2b37bf75882edf94a68b1d9607",
        );
        db.insert_cfd(&cfd).await.unwrap();
        let mut conn = db.inner.acquire().await.unwrap();
        let mut db_tx = conn.begin().await.unwrap();

        let super::Cfd {
            counterparty_peer_id,
            ..
        } = load_cfd_row(&mut db_tx, cfd.id()).await.unwrap();

        db_tx.commit().await.unwrap();

        assert_eq!(
            "12D3KooWP3BN6bq9jPy8cP7Grj1QyUBfr7U6BeQFgMwfTTu12wuY",
            counterparty_peer_id.unwrap().inner().to_string().as_str()
        );
    }

    #[tokio::test]
    async fn given_insert_cfd_without_peer_id_when_known_testnet_maker_then_peer_id_loaded() {
        let db = memory().await.unwrap();

        let cfd = dummy_taker_with_legacy_identity(
            "69a42aa90da8b065b9532b62bff940a3ba07dbbb11d4482c7db83a7e049a9f1e",
        );
        db.insert_cfd(&cfd).await.unwrap();
        let mut conn = db.inner.acquire().await.unwrap();
        let mut db_tx = conn.begin().await.unwrap();

        let super::Cfd {
            counterparty_peer_id,
            ..
        } = load_cfd_row(&mut db_tx, cfd.id()).await.unwrap();

        db_tx.commit().await.unwrap();

        assert_eq!(
            "12D3KooWEsK2X8Tp24XtyWh7DM65VfwXtNH2cmfs2JsWmkmwKbV1",
            counterparty_peer_id.unwrap().inner().to_string().as_str()
        );
    }

    #[tokio::test]
    async fn given_insert_cfd_without_peer_id_when_unknown_maker_then_no_peer_id_loaded() {
        let db = memory().await.unwrap();

        let cfd = dummy_cfd();
        db.insert_cfd(&cfd).await.unwrap();
        let mut conn = db.inner.acquire().await.unwrap();
        let mut db_tx = conn.begin().await.unwrap();

        let super::Cfd {
            counterparty_peer_id,
            ..
        } = load_cfd_row(&mut db_tx, cfd.id()).await.unwrap();

        db_tx.commit().await.unwrap();

        assert_eq!(None, counterparty_peer_id);
    }

    pub fn dummy_cfd() -> Cfd {
        dummy_taker_with_legacy_identity(
            "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
        )
    }

    pub fn dummy_taker_with_counterparty_peer_id() -> Cfd {
        Cfd::new(
            OrderId::default(),
            Position::Long,
            Price::new(dec!(60_000)).unwrap(),
            Leverage::TWO,
            Duration::hours(24),
            Role::Taker,
            Usd::new(dec!(1_000)),
            "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"
                .parse()
                .unwrap(),
            Some(PeerId::random()),
            OpeningFee::new(Amount::from_sat(2000)),
            FundingRate::default(),
            TxFeeRate::default(),
        )
    }

    pub fn dummy_taker_with_legacy_identity(identity: &str) -> Cfd {
        Cfd::new(
            OrderId::default(),
            Position::Long,
            Price::new(dec!(60_000)).unwrap(),
            Leverage::TWO,
            Duration::hours(24),
            Role::Taker,
            Usd::new(dec!(1_000)),
            identity.parse().unwrap(),
            None,
            OpeningFee::new(Amount::from_sat(2000)),
            FundingRate::default(),
            TxFeeRate::default(),
        )
    }

    pub fn lock_confirmed(cfd: &Cfd) -> CfdEvent {
        CfdEvent {
            timestamp: Timestamp::now(),
            id: cfd.id(),
            event: EventKind::LockConfirmed,
        }
    }

    pub fn setup_failed(cfd: &Cfd) -> CfdEvent {
        CfdEvent {
            timestamp: Timestamp::now(),
            id: cfd.id(),
            event: EventKind::ContractSetupFailed,
        }
    }

    pub fn order_rejected(cfd: &Cfd) -> CfdEvent {
        CfdEvent {
            timestamp: Timestamp::now(),
            id: cfd.id(),
            event: EventKind::OfferRejected,
        }
    }
}
