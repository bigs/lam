use serde::{Deserialize, Serialize};

use crate::{ContextEntry, MessageEnvelope};

/// Current serialized schema version for actor events.
pub const ACTOR_EVENT_SCHEMA_VERSION: u32 = 1;

/// One versioned fact in an actor journal.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActorEvent {
    schema_version: u32,
    event: ActorEventData,
}

impl ActorEvent {
    /// Constructs a message-admission event.
    #[must_use]
    pub const fn message_admitted(message: MessageEnvelope) -> Self {
        Self {
            schema_version: ACTOR_EVENT_SCHEMA_VERSION,
            event: ActorEventData::MessageAdmitted { message },
        }
    }

    /// Constructs a context-append event.
    #[must_use]
    pub const fn context_appended(entry: ContextEntry) -> Self {
        Self {
            schema_version: ACTOR_EVENT_SCHEMA_VERSION,
            event: ActorEventData::ContextAppended { entry },
        }
    }

    /// Returns the serialized event-schema version.
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Returns the event value.
    #[must_use]
    pub const fn data(&self) -> &ActorEventData {
        &self.event
    }

    pub(crate) fn into_data(self) -> ActorEventData {
        self.event
    }
}

/// Initial closed actor-event vocabulary.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ActorEventData {
    /// A message became durable in the actor's mailbox.
    MessageAdmitted {
        /// Admitted envelope.
        message: MessageEnvelope,
    },
    /// A context item and its source messages were atomically recorded.
    ContextAppended {
        /// Appended model-visible entry.
        entry: ContextEntry,
    },
}
