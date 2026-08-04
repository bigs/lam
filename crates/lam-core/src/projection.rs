use std::collections::{BTreeMap, BTreeSet};

use crate::{
    ACTOR_EVENT_SCHEMA_VERSION, ActorEvent, ActorEventData, ContextEntry, ContextSequence,
    ContextTransition, DeliveryMode, EventBatch, JournalPage, MessageEnvelope, MessageError,
    MessageId, ModelDescriptor, ModelId, ModelSelection, Revision, RunId, RunProgress, StoredEvent,
};

/// One admitted message as viewed through the actor projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmittedMessage {
    /// Journal revision which admitted the message.
    pub revision: Revision,
    /// Durable envelope.
    pub envelope: MessageEnvelope,
    /// Context position which consumed the message, when delivered.
    pub consumed_at: Option<ContextSequence>,
}

/// One context item paired with its derived positions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectedContextEntry {
    /// Position in the logical context stream.
    pub sequence: ContextSequence,
    /// Actor-journal revision which stored the entry.
    pub revision: Revision,
    /// Model-visible item.
    pub entry: ContextEntry,
}

/// Pure current-state projection of one actor journal.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ActorState {
    revision: Revision,
    messages: Vec<AdmittedMessage>,
    context: Vec<ProjectedContextEntry>,
    selected_model: Option<Box<ModelSelection>>,
    model_descriptors: BTreeMap<ModelId, ModelDescriptor>,
    active_run: Option<RunId>,
    completed_runs: BTreeSet<RunId>,
    interrupted_runs: BTreeSet<RunId>,
}

impl ActorState {
    /// Constructs an empty actor projection.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the folded actor-journal head.
    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Returns the latest logical context position.
    #[must_use]
    pub fn context_sequence(&self) -> ContextSequence {
        self.context
            .last()
            .map_or(ContextSequence::ZERO, |entry| entry.sequence)
    }

    /// Returns all projected context entries in order.
    #[must_use]
    pub fn context(&self) -> &[ProjectedContextEntry] {
        &self.context
    }

    /// Returns the durable model currently selected by this actor.
    #[must_use]
    pub fn selected_model(&self) -> Option<&ModelSelection> {
        self.selected_model.as_deref()
    }

    /// Returns one admitted message by identity.
    #[must_use]
    pub fn message(&self, message_id: &MessageId) -> Option<&AdmittedMessage> {
        self.messages
            .iter()
            .find(|message| message.envelope.message_id() == message_id)
    }

    /// Iterates over pending messages in admission order.
    pub fn pending_messages(&self) -> impl Iterator<Item = &AdmittedMessage> {
        self.messages
            .iter()
            .filter(|message| message.consumed_at.is_none())
    }

    /// Returns all admitted messages in admission order.
    #[must_use]
    pub fn messages(&self) -> &[AdmittedMessage] {
        &self.messages
    }

    /// Returns messages currently eligible to enter context.
    pub fn eligible_messages(&self) -> impl Iterator<Item = &AdmittedMessage> {
        let active = self.active_run.is_some();
        self.pending_messages()
            .filter(move |message| !active || message.envelope.delivery() == DeliveryMode::Steer)
    }

    /// Returns the active run, if an intermediate entry has not terminated it.
    #[must_use]
    pub const fn active_run(&self) -> Option<&RunId> {
        self.active_run.as_ref()
    }

    /// Reports whether a terminal model entry completed `run_id`.
    #[must_use]
    pub fn is_run_completed(&self, run_id: &RunId) -> bool {
        self.completed_runs.contains(run_id)
    }

    /// Reports whether a durable interruption closed `run_id`.
    #[must_use]
    pub fn is_run_interrupted(&self, run_id: &RunId) -> bool {
        self.interrupted_runs.contains(run_id)
    }

    /// Finds the newest compaction marker accepted by `compatible`.
    pub fn latest_compaction_matching(
        &self,
        mut compatible: impl FnMut(&ContextEntry) -> bool,
    ) -> Option<&ProjectedContextEntry> {
        self.context.iter().rev().find(|projected| {
            matches!(
                projected.entry.transition,
                ContextTransition::Compaction { .. }
            ) && compatible(&projected.entry)
        })
    }

    /// Folds one bounded journal page into this projection.
    ///
    /// The state is consumed so a failed fold cannot expose a partially applied
    /// projection. Callers can rebuild from the authoritative journal.
    pub fn fold_page(mut self, page: JournalPage) -> Result<Self, StateError> {
        if page.head < self.revision {
            return Err(StateError::JournalContract {
                message: format!(
                    "journal head regressed from {} to {}",
                    self.revision.get(),
                    page.head.get()
                ),
            });
        }
        if page.events.is_empty() && page.head > self.revision {
            return Err(StateError::JournalContract {
                message: "journal returned an empty page before its observed head".to_owned(),
            });
        }

        let mut expected = self.revision;
        for stored in &page.events {
            expected = expected
                .checked_advance(1)
                .ok_or(StateError::RevisionExhausted)?;
            if stored.revision != expected {
                return Err(StateError::JournalContract {
                    message: format!(
                        "expected revision {}, received {}",
                        expected.get(),
                        stored.revision.get()
                    ),
                });
            }
            if stored.revision > page.head {
                return Err(StateError::JournalContract {
                    message: format!(
                        "event revision {} exceeds observed head {}",
                        stored.revision.get(),
                        page.head.get()
                    ),
                });
            }
        }

        for stored in page.events {
            self.apply_stored_event(stored)?;
        }
        Ok(self)
    }

    /// Plans idempotent message admission against the current projection.
    pub fn plan_admission(
        &self,
        message: MessageEnvelope,
    ) -> Result<AdmissionDecision, StateError> {
        message.validate()?;
        if let Some(existing) = self.message(message.message_id()) {
            if message.is_idempotent_retry_of(&existing.envelope) {
                return Ok(AdmissionDecision::Existing {
                    revision: existing.revision,
                });
            }
            return Err(StateError::MessageIdCollision {
                message_id: message.message_id().clone(),
            });
        }
        Ok(AdmissionDecision::Append(ActorEvent::message_admitted(
            message,
        )))
    }

    /// Validates and plans one context append at the current journal head.
    pub fn plan_context_append(&self, entry: ContextEntry) -> Result<ActorEvent, StateError> {
        self.revision
            .checked_advance(1)
            .ok_or(StateError::RevisionExhausted)?;
        self.validate_context_entry(&entry)?;
        Ok(ActorEvent::context_appended(entry))
    }

    /// Validates and plans a model selection at the current journal head.
    pub fn plan_model_selection(
        &self,
        selection: ModelSelection,
    ) -> Result<ActorEvent, StateError> {
        self.revision
            .checked_advance(1)
            .ok_or(StateError::RevisionExhausted)?;
        self.validate_model_selection(&selection)?;
        Ok(ActorEvent::model_selected(selection))
    }

    /// Validates an atomic event batch against each preceding event in order.
    ///
    /// This is required when later events depend on state established earlier
    /// in the same compare-and-append operation.
    pub fn validate_batch(&self, batch: &EventBatch) -> Result<(), StateError> {
        let mut preview = self.clone();
        let mut revision = self.revision;
        for event in batch.iter() {
            revision = revision
                .checked_advance(1)
                .ok_or(StateError::RevisionExhausted)?;
            preview.apply_stored_event(StoredEvent {
                revision,
                event: event.clone(),
            })?;
        }
        Ok(())
    }

    fn apply_stored_event(&mut self, stored: StoredEvent) -> Result<(), StateError> {
        let schema_version = stored.event.schema_version();
        if schema_version == 0 || schema_version > ACTOR_EVENT_SCHEMA_VERSION {
            return Err(StateError::UnsupportedEventVersion {
                revision: stored.revision,
                found: stored.event.schema_version(),
                supported: ACTOR_EVENT_SCHEMA_VERSION,
            });
        }

        let data = stored.event.into_data();
        if schema_version == 1 && matches!(data, ActorEventData::ModelSelected { .. }) {
            return Err(StateError::UnsupportedEventVersion {
                revision: stored.revision,
                found: schema_version,
                supported: ACTOR_EVENT_SCHEMA_VERSION,
            });
        }
        match data {
            ActorEventData::ModelSelected { selection } => {
                self.apply_model_selection(selection)?;
            }
            ActorEventData::MessageAdmitted { message } => {
                self.apply_message(stored.revision, message)?;
            }
            ActorEventData::ContextAppended { entry } => {
                self.apply_context(stored.revision, entry)?;
            }
        }
        self.revision = stored.revision;
        Ok(())
    }

    fn apply_model_selection(&mut self, selection: ModelSelection) -> Result<(), StateError> {
        self.validate_model_selection(&selection)?;
        self.model_descriptors
            .entry(selection.model_id.clone())
            .or_insert_with(|| selection.descriptor.clone());
        self.selected_model = Some(Box::new(selection));
        Ok(())
    }

    fn validate_model_selection(&self, selection: &ModelSelection) -> Result<(), StateError> {
        selection
            .descriptor
            .validate()
            .map_err(|error| StateError::InvalidModelDescriptor {
                model_id: selection.model_id.clone(),
                message: error.to_string(),
            })?;
        if self.selected_model.is_some()
            && let Some(run_id) = &self.active_run
        {
            return Err(StateError::ModelSwitchDuringRun {
                run_id: run_id.clone(),
            });
        }
        if let Some(existing) = self.model_descriptors.get(&selection.model_id)
            && existing != &selection.descriptor
        {
            return Err(StateError::ModelDescriptorChanged {
                model_id: selection.model_id.clone(),
                existing: Box::new(existing.clone()),
                actual: Box::new(selection.descriptor.clone()),
            });
        }
        Ok(())
    }

    fn apply_message(
        &mut self,
        revision: Revision,
        message: MessageEnvelope,
    ) -> Result<(), StateError> {
        message.validate()?;
        if self.message(message.message_id()).is_some() {
            return Err(StateError::DuplicateMessage {
                message_id: message.message_id().clone(),
            });
        }
        self.messages.push(AdmittedMessage {
            revision,
            envelope: message,
            consumed_at: None,
        });
        Ok(())
    }

    fn apply_context(&mut self, revision: Revision, entry: ContextEntry) -> Result<(), StateError> {
        let sequence = self.validate_context_entry(&entry)?;

        match &entry.transition {
            ContextTransition::Messages { run_id, .. } => {
                self.consume_eligible_messages(sequence);
                if self.active_run.is_none() {
                    self.active_run = Some(run_id.clone());
                }
            }
            ContextTransition::Model {
                run_id,
                progress: RunProgress::Complete,
            } => {
                self.active_run = None;
                self.completed_runs.insert(run_id.clone());
            }
            ContextTransition::Interrupted { run_id, .. } => {
                self.consume_eligible_messages(sequence);
                self.active_run = None;
                self.interrupted_runs.insert(run_id.clone());
            }
            ContextTransition::Model {
                progress: RunProgress::Continue,
                ..
            }
            | ContextTransition::Eval { .. }
            | ContextTransition::Compaction { .. } => {}
        }

        self.context.push(ProjectedContextEntry {
            sequence,
            revision,
            entry,
        });
        Ok(())
    }

    fn consume_eligible_messages(&mut self, sequence: ContextSequence) {
        let active = self.active_run.is_some();
        for message in &mut self.messages {
            if message.consumed_at.is_none()
                && (!active || message.envelope.delivery() == DeliveryMode::Steer)
            {
                message.consumed_at = Some(sequence);
            }
        }
    }

    fn validate_context_entry(&self, entry: &ContextEntry) -> Result<ContextSequence, StateError> {
        let sequence = self
            .context_sequence()
            .next()
            .ok_or(StateError::ContextSequenceExhausted)?;

        match &entry.transition {
            ContextTransition::Messages {
                run_id,
                consumed_message_ids,
            } => {
                self.validate_message_batch(consumed_message_ids)?;
                self.validate_continuing_run(run_id, true)?;
            }
            ContextTransition::Model {
                run_id,
                progress: RunProgress::Continue,
            }
            | ContextTransition::Eval { run_id } => {
                self.validate_continuing_run(run_id, false)?;
            }
            ContextTransition::Model {
                run_id,
                progress: RunProgress::Complete,
            } => self.validate_terminal_run(run_id)?,
            ContextTransition::Interrupted {
                run_id,
                consumed_message_ids,
            } => {
                self.validate_message_batch(consumed_message_ids)?;
                self.validate_active_run(run_id)?;
            }
            ContextTransition::Compaction {
                covers_through,
                run_id,
            } => {
                let current = self.context_sequence();
                if *covers_through > current {
                    return Err(StateError::CompactionCoversFuture {
                        covers_through: *covers_through,
                        current,
                    });
                }
                if let Some(run_id) = run_id {
                    self.validate_continuing_run(run_id, false)?;
                }
            }
        }
        Ok(sequence)
    }

    fn validate_message_batch(&self, actual: &[MessageId]) -> Result<(), StateError> {
        let expected = self
            .eligible_messages()
            .map(|message| message.envelope.message_id().clone())
            .collect::<Vec<_>>();
        if expected.is_empty() {
            return Err(StateError::NoEligibleMessages);
        }
        if expected != actual {
            return Err(StateError::MessageBatchMismatch {
                expected,
                actual: actual.to_vec(),
            });
        }
        Ok(())
    }

    fn validate_continuing_run(&self, run_id: &RunId, may_start: bool) -> Result<(), StateError> {
        if self.completed_runs.contains(run_id) {
            return Err(StateError::RunAlreadyCompleted {
                run_id: run_id.clone(),
            });
        }
        if self.interrupted_runs.contains(run_id) {
            return Err(StateError::RunAlreadyInterrupted {
                run_id: run_id.clone(),
            });
        }
        match &self.active_run {
            Some(active) if active != run_id => Err(StateError::RunMismatch {
                expected: active.clone(),
                actual: run_id.clone(),
            }),
            None if !may_start => Err(StateError::RunNotActive {
                run_id: run_id.clone(),
            }),
            Some(_) | None => Ok(()),
        }
    }

    fn validate_terminal_run(&self, run_id: &RunId) -> Result<(), StateError> {
        self.validate_active_run(run_id)?;
        let pending_steers = self
            .pending_messages()
            .filter(|message| message.envelope.delivery() == DeliveryMode::Steer)
            .map(|message| message.envelope.message_id().clone())
            .collect::<Vec<_>>();
        if pending_steers.is_empty() {
            Ok(())
        } else {
            Err(StateError::TerminalWithPendingSteer {
                message_ids: pending_steers,
            })
        }
    }

    fn validate_active_run(&self, run_id: &RunId) -> Result<(), StateError> {
        let Some(active) = &self.active_run else {
            return Err(StateError::TerminalWithoutActiveRun {
                run_id: run_id.clone(),
            });
        };
        if active != run_id {
            return Err(StateError::RunMismatch {
                expected: active.clone(),
                actual: run_id.clone(),
            });
        }
        Ok(())
    }
}

/// Pure outcome of planning message admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdmissionDecision {
    /// Append this event at the projection's current revision.
    Append(ActorEvent),
    /// The identical message is already durable at this revision.
    Existing {
        /// Original admission revision.
        revision: Revision,
    },
}

/// Actor events cannot be folded into a coherent state.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum StateError {
    /// A stored event uses an unsupported domain schema.
    #[error(
        "actor event at revision {revision:?} has schema {found}, but this runtime supports {supported}"
    )]
    UnsupportedEventVersion {
        /// Event revision.
        revision: Revision,
        /// Stored schema version.
        found: u32,
        /// Supported schema version.
        supported: u32,
    },
    /// A backend violated the ordered-page contract.
    #[error("journal contract violation: {message}")]
    JournalContract {
        /// Contract failure.
        message: String,
    },
    /// The durable message itself violates Lam's rules.
    #[error(transparent)]
    InvalidMessage(#[from] MessageError),
    /// The journal contains the same message identity twice.
    #[error("message `{message_id}` was admitted more than once")]
    DuplicateMessage {
        /// Repeated identity.
        message_id: MessageId,
    },
    /// A caller reused a message identity with different semantic content.
    #[error("message id `{message_id}` was reused with different content")]
    MessageIdCollision {
        /// Colliding identity.
        message_id: MessageId,
    },
    /// No pending messages are currently eligible for context.
    #[error("message context requires at least one eligible message")]
    NoEligibleMessages,
    /// A message batch did not consume exactly the eligible mailbox messages.
    #[error("message batch did not match eligible mailbox order")]
    MessageBatchMismatch {
        /// Required message order.
        expected: Vec<MessageId>,
        /// Supplied message order.
        actual: Vec<MessageId>,
    },
    /// A compaction marker attempted to cover context which does not exist yet.
    #[error("compaction covers future context")]
    CompactionCoversFuture {
        /// Requested inclusive boundary.
        covers_through: ContextSequence,
        /// Current context boundary.
        current: ContextSequence,
    },
    /// An entry attempted to switch runs without terminating the active one.
    #[error("active run `{expected}` does not match entry run `{actual}`")]
    RunMismatch {
        /// Active run.
        expected: RunId,
        /// Incoming run.
        actual: RunId,
    },
    /// An intermediate entry attempted to resume a completed run.
    #[error("run `{run_id}` is already complete")]
    RunAlreadyCompleted {
        /// Completed run.
        run_id: RunId,
    },
    /// An intermediate entry attempted to resume an interrupted run.
    #[error("run `{run_id}` was interrupted")]
    RunAlreadyInterrupted {
        /// Interrupted run.
        run_id: RunId,
    },
    /// A non-message entry attempted to begin a run.
    #[error("run `{run_id}` is not active")]
    RunNotActive {
        /// Incoming run.
        run_id: RunId,
    },
    /// A terminal model entry did not belong to an active run.
    #[error("run `{run_id}` cannot terminate because it is not active")]
    TerminalWithoutActiveRun {
        /// Incoming run.
        run_id: RunId,
    },
    /// A terminal model entry raced with one or more steering messages.
    #[error("run cannot terminate while steering messages are pending")]
    TerminalWithPendingSteer {
        /// Pending steering identities.
        message_ids: Vec<MessageId>,
    },
    /// A model switch was attempted while a tool-calling loop was active.
    #[error("cannot switch models while run `{run_id}` is active")]
    ModelSwitchDuringRun {
        /// Active run.
        run_id: RunId,
    },
    /// A stable model registry key was rebound to a different descriptor.
    #[error("model `{model_id}` descriptor changed from {existing:?} to {actual:?}")]
    ModelDescriptorChanged {
        /// Stable registry identity.
        model_id: ModelId,
        /// Descriptor first observed in this actor journal.
        existing: Box<ModelDescriptor>,
        /// Conflicting descriptor.
        actual: Box<ModelDescriptor>,
    },
    /// A durable model descriptor contained an empty identity field.
    #[error("model `{model_id}` has an invalid descriptor: {message}")]
    InvalidModelDescriptor {
        /// Stable registry identity.
        model_id: ModelId,
        /// Validation diagnostic.
        message: String,
    },
    /// Actor-journal revision arithmetic overflowed.
    #[error("actor journal revision space is exhausted")]
    RevisionExhausted,
    /// Logical context sequence arithmetic overflowed.
    #[error("actor context sequence space is exhausted")]
    ContextSequenceExhausted,
}
