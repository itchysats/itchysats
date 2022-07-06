use crate::command;
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use maia_core::secp256k1_zkp::XOnlyPublicKey;
use model::olivia;
use model::olivia::next_announcement_after;
use model::olivia::BitMexPriceEventId;
use model::CfdEvent;
use model::EventKind;
use sqlite_db;
use std::collections::HashMap;
use std::collections::HashSet;
use time::Duration;
use time::OffsetDateTime;
use xtra::message_channel::MessageChannel;
use xtra_productivity::xtra_productivity;
use xtras::SendInterval;

/// Timout to be passed into the reqwest client for doing http requests against the oracle.
///
/// 10 seconds was chosen arbitrarily. It should be plenty to fetch from the oracle and does not let
/// us wait forever.
const REQWEST_TIMEOUT: core::time::Duration = core::time::Duration::from_secs(10);

/// We only have to sync for new announcements once an hour.
///
/// Syncing every 60 seconds might still be an overkill but should not hurt us.
const SYNC_ANNOUNCEMENTS_INTERVAL: core::time::Duration = std::time::Duration::from_secs(60);

/// We want to sync attestations fast but don't spam our internal actor. Hence, we chose 30 seconds.
const SYNC_ATTESTATIONS_INTERVAL: core::time::Duration = std::time::Duration::from_secs(30);

pub struct Actor {
    announcements: HashMap<BitMexPriceEventId, (OffsetDateTime, Vec<XOnlyPublicKey>)>,
    pending_attestations: HashSet<BitMexPriceEventId>,
    executor: command::Executor,
    db: sqlite_db::Connection,
    client: reqwest::Client,
}

/// We want to fetch at least this much announcements into the future
///
/// For a rollover to happen successfully we need to know the oracle announcement details.
/// Our actor is checking if a new announcement can be fetched every SYNC_ANNOUNCEMENTS_INTERVAL and
/// ANNOUNCEMENT_LOOKAHEAD hours into the future. Given we rollover every for
/// hour model::SETTLEMENT_INTERVAL into the future, we want to have at least
/// model::SETTLEMENT_INTERVAL announcements ready. Due to sync interval coincidence, it might
/// happen that we do not have synced for a specific announcement yet. Hence, we need to fetch more
/// announcements. We fetch model::SETTLEMENT_INTERVAL + 2 announcement into the future because of
/// this example:
///
/// Assume the last fetch was at 01.01.2022 00:59:55, i.e. 5 seconds before midnight and we would
/// have synced for model::SETTLEMENT_INTERVAL+1 we would have synced announcements until 02.01.2022
/// 01:00:00. A rollover request happening exactly at 01.01.2022 01:00:00 would ask for the
/// announcement at 02.01.2022 02:00:00 because of how olivia::next_announcement_after works. Note:
/// even if the underlying logic of olivia::next_announcement_after changes, fetching model::
/// SETTLEMENT_INTERVAL + 2 won't hurt.
const ANNOUNCEMENT_LOOKAHEAD: Duration = Duration::hours(26);

#[derive(Clone, Copy)]
pub struct SyncAnnouncements;

#[derive(Clone, Copy)]
pub struct SyncAttestations;

#[derive(Clone, Copy)]
pub struct MonitorAttestation {
    pub event_id: BitMexPriceEventId,
}

#[derive(Clone)]
struct MonitorAttestations {
    pub event_ids: Vec<BitMexPriceEventId>,
}

/// Message used to request `Announcement`s from the `oracle::Actor`'s
/// local state.
///
/// Each `Announcement` corresponds to a [`BitMexPriceEventId`]
/// included in the message.
#[derive(Clone)]
pub struct GetAnnouncements(pub Vec<BitMexPriceEventId>);

#[derive(Debug, Clone)]
pub struct Attestation(olivia::Attestation);

/// A module-private message to allow parallelization of fetching announcements.
#[derive(Debug)]
struct NewAnnouncementFetched {
    id: BitMexPriceEventId,
    expected_outcome_time: OffsetDateTime,
    nonce_pks: Vec<XOnlyPublicKey>,
}

/// A module-private message to allow parallelization of fetching attestations.
#[derive(Debug)]
struct NewAttestationFetched {
    id: BitMexPriceEventId,
    attestation: Attestation,
}

#[derive(Default, Clone)]
struct Cfd {
    pending_attestation: Option<BitMexPriceEventId>,
    version: u32,
}

impl Cfd {
    fn apply(mut self, event: CfdEvent) -> Self {
        self.version += 1;

        let settlement_event_id = match event.event {
            EventKind::ContractSetupCompleted { dlc: None, .. } => return self,
            EventKind::ContractSetupCompleted { dlc: Some(dlc), .. } => dlc.settlement_event_id,
            EventKind::RolloverCompleted { dlc: None, .. } => return self,
            EventKind::RolloverCompleted { dlc: Some(dlc), .. } => dlc.settlement_event_id,
            // TODO: There might be a few cases where we do not need to monitor the attestation,
            // e.g. when we already agreed to collab. settle. Ignoring it for now
            // because I don't want to think about it and it doesn't cause much harm to do the
            // monitoring :)
            _ => return self,
        };

        // we can comfortably overwrite what was there because events are processed in order, thus
        // old attestations don't matter.
        Self {
            pending_attestation: Some(settlement_event_id),
            ..self
        }
    }
}

impl sqlite_db::CfdAggregate for Cfd {
    type CtorArgs = ();

    fn new(_: Self::CtorArgs, _: sqlite_db::Cfd) -> Self {
        Self::default()
    }

    fn apply(self, event: CfdEvent) -> Self {
        self.apply(event)
    }

    fn version(&self) -> u32 {
        self.version
    }
}

impl Actor {
    pub fn new(db: sqlite_db::Connection, executor: command::Executor) -> Self {
        Self {
            announcements: HashMap::new(),
            pending_attestations: HashSet::new(),
            executor,
            db,
            client: reqwest::Client::new(),
        }
    }

    fn ensure_having_announcements(&mut self, ctx: &mut xtra::Context<Self>) {
        for hour in 1..ANNOUNCEMENT_LOOKAHEAD.whole_hours() {
            let event_id =
                next_announcement_after(OffsetDateTime::now_utc() + Duration::hours(hour));

            if self.announcements.get(&event_id).is_some() {
                continue;
            }
            let this = ctx.address().expect("self to be alive");
            let client = self.client.clone();

            tokio_extras::spawn_fallible(
                &this.clone(),
                async move {
                    let url = event_id.to_olivia_url();

                    tracing::debug!(event_id = %event_id, "Fetching announcement");

                    let response = client
                        .get(url.clone())
                        .send()
                        .await
                        .with_context(|| format!("Failed to GET {url}"))?;

                    let code = response.status();
                    if !code.is_success() {
                        anyhow::bail!("GET {url} responded with {code}");
                    }

                    let announcement = response
                        .json::<olivia::Announcement>()
                        .await
                        .context("Failed to deserialize as Announcement")?;

                    this.send(NewAnnouncementFetched {
                        id: event_id,
                        nonce_pks: announcement.nonce_pks,
                        expected_outcome_time: announcement.expected_outcome_time,
                    })
                    .await?;

                    Ok(())
                },
                |e| async move {
                    tracing::debug!("Failed to fetch announcement: {:#}", e);
                },
            );
        }
    }

    fn update_pending_attestations(&mut self, ctx: &mut xtra::Context<Self>) {
        for event_id in self.pending_attestations.iter().copied() {
            if !event_id.has_likely_occurred() {
                tracing::trace!("Skipping {event_id} because it likely hasn't occurred yet");

                continue;
            }

            let this = ctx.address().expect("self to be alive");
            let client = self.client.clone();

            tokio_extras::spawn_fallible(
                &this.clone(),
                async move {
                    let url = event_id.to_olivia_url();

                    tracing::debug!("Fetching attestation for {event_id}");

                    let response = client
                        .get(url.clone())
                        .timeout(REQWEST_TIMEOUT)
                        .send()
                        .await
                        .with_context(|| format!("Failed to GET {url}"))?;

                    let code = response.status();
                    if !code.is_success() {
                        anyhow::bail!("GET {url} responded with {code}");
                    }

                    let attestation = response
                        .json::<olivia::Attestation>()
                        .await
                        .context("Failed to deserialize as Attestation")?;

                    this.send(NewAttestationFetched {
                        id: event_id,
                        attestation: Attestation(attestation),
                    })
                    .await??;

                    Ok(())
                },
                |e| async move {
                    tracing::debug!("Failed to fetch attestation: {:#}", e);
                },
            )
        }
    }

    fn add_pending_attestation(&mut self, event_id: BitMexPriceEventId) {
        if !self.pending_attestations.insert(event_id) {
            tracing::trace!("Attestation for {event_id} already being monitored");
        }
    }
}

#[xtra_productivity]
impl Actor {
    fn handle_monitor_attestation(&mut self, msg: MonitorAttestation) {
        self.add_pending_attestation(msg.event_id)
    }

    fn handle_monitor_attestations(&mut self, msg: MonitorAttestations) {
        for id in msg.event_ids.into_iter() {
            self.add_pending_attestation(id);
        }
    }

    fn handle_get_announcements(
        &mut self,
        GetAnnouncements(ids): GetAnnouncements,
    ) -> Result<Vec<olivia::Announcement>, NoAnnouncement> {
        let announcements = ids
            .iter()
            .map(|id| {
                self.announcements
                    .get_key_value(id)
                    .map(|(id, (time, nonce_pks))| olivia::Announcement {
                        id: *id,
                        expected_outcome_time: *time,
                        nonce_pks: nonce_pks.clone(),
                    })
                    .ok_or(NoAnnouncement(*id))
            })
            .collect::<Result<_, _>>()?;

        Ok(announcements)
    }

    fn handle_new_announcement_fetched(&mut self, msg: NewAnnouncementFetched) {
        self.announcements
            .insert(msg.id, (msg.expected_outcome_time, msg.nonce_pks));
    }

    fn handle_sync_announcements(&mut self, _: SyncAnnouncements, ctx: &mut xtra::Context<Self>) {
        self.ensure_having_announcements(ctx);
    }

    fn handle_sync_attestations(&mut self, _: SyncAttestations, ctx: &mut xtra::Context<Self>) {
        self.update_pending_attestations(ctx);
    }

    async fn handle_new_attestation_fetched(&mut self, msg: NewAttestationFetched) -> Result<()> {
        let NewAttestationFetched { id, attestation } = msg;

        tracing::info!("Fetched new attestation for {id}");

        for id in self.db.load_open_cfd_ids().await? {
            if let Err(err) = self
                .executor
                .execute(id, |cfd| cfd.decrypt_cet(&attestation.0))
                .await
            {
                tracing::warn!(order_id = %id, "Failed to decrypt CET using attestation: {err:#}")
            }
        }

        self.pending_attestations.remove(&id);

        Ok(())
    }
}

#[derive(Debug, Clone, thiserror::Error, Copy)]
#[error("Announcement {0} not found")]
pub struct NoAnnouncement(pub BitMexPriceEventId);

#[async_trait]
impl xtra::Actor for Actor {
    type Stop = ();
    async fn started(&mut self, ctx: &mut xtra::Context<Self>) {
        let this = ctx.address().expect("we are alive");
        tokio_extras::spawn(
            &this,
            this.clone()
                .send_interval(SYNC_ANNOUNCEMENTS_INTERVAL, || SyncAnnouncements),
        );

        tokio_extras::spawn(&this.clone(), {
            let db = self.db.clone();
            async move {
                let pending_attestations = db
                    .load_all_open_cfds::<Cfd>(())
                    .filter_map(|res| async move {
                        match res {
                            Ok(Cfd {
                                pending_attestation,
                                ..
                            }) => pending_attestation,
                            Err(e) => {
                                tracing::warn!("Failed to load CFD from database: {e:#}");
                                None
                            }
                        }
                    })
                    .collect::<Vec<_>>()
                    .await;

                let _: Result<(), xtra::Error> = this
                    .send(MonitorAttestations {
                        event_ids: pending_attestations,
                    })
                    .await;

                this.send_interval(SYNC_ATTESTATIONS_INTERVAL, || SyncAttestations)
                    .await;
            }
        });
    }

    async fn stopped(self) -> Self::Stop {}
}

impl Attestation {
    pub fn new(attestation: olivia::Attestation) -> Self {
        Self(attestation)
    }

    pub fn as_inner(&self) -> &olivia::Attestation {
        &self.0
    }

    pub fn into_inner(self) -> olivia::Attestation {
        self.0
    }

    pub fn id(&self) -> BitMexPriceEventId {
        self.0.id
    }
}

/// Source of announcements based on their event IDs.
///
/// This is just a wrapper around a `MessageChannel` which would
/// provide the same use. It is needed so that we can implement
/// foreign traits on it to fulfil the requirements of external APIs.
#[derive(Clone)]
pub struct AnnouncementsChannel(
    MessageChannel<GetAnnouncements, Result<Vec<olivia::Announcement>, NoAnnouncement>>,
);

impl AnnouncementsChannel {
    pub fn new(
        channel: MessageChannel<
            GetAnnouncements,
            Result<Vec<olivia::Announcement>, NoAnnouncement>,
        >,
    ) -> Self {
        Self(channel)
    }
}

#[async_trait]
impl crate::rollover::protocol::GetAnnouncements for AnnouncementsChannel {
    async fn get_announcements(
        &self,
        events: Vec<BitMexPriceEventId>,
    ) -> Result<Vec<olivia::Announcement>> {
        let announcements = self
            .0
            .send(GetAnnouncements(events))
            .await
            .context("Oracle actor disconnected")?
            .context("Failed to get announcements")?;

        Ok(announcements)
    }
}

#[cfg(test)]
pub mod tests {
    #[test]
    fn ensure_lookahead_constant() {
        use time::Duration;
        assert_eq!(
            crate::oracle::ANNOUNCEMENT_LOOKAHEAD,
            model::SETTLEMENT_INTERVAL + Duration::hours(2)
        );
    }
}
