use model::CfdEvent;
use model::EventKind;

pub(super) struct EventLog(pub Vec<EventLogEntry>);

impl EventLog {
    pub(super) fn new(events: &[CfdEvent]) -> Self {
        Self(events.iter().map(EventLogEntry::from).collect())
    }

    pub(super) fn contains(&self, event: &EventKind) -> bool {
        self.0
            .iter()
            .any(|EventLogEntry { name, .. }| name == &event.to_string())
    }
}

pub(super) struct EventLogEntry {
    pub name: String,
    pub created_at: i64,
}

impl From<&CfdEvent> for EventLogEntry {
    fn from(event: &CfdEvent) -> Self {
        let name = event.event.to_string();
        let created_at = event.timestamp.seconds();

        Self { name, created_at }
    }
}
