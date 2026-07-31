use serde::{Deserialize, Serialize};

use crate::{ActorId, ComponentId, EncodedPayload, MessageId, PrincipalId, Timestamp};

/// Authenticated provenance of an admitted message.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MessageSource {
    /// Input supplied through a user-facing host API.
    User {
        /// Optional host-defined principal identity.
        principal: Option<PrincipalId>,
    },
    /// Input supplied by a trusted host component.
    Host {
        /// Component which supplied the message.
        component: ComponentId,
    },
    /// Input sent by another authenticated Lam actor.
    Actor {
        /// Sending actor.
        actor_id: ActorId,
    },
}

/// Determines when an admitted message becomes eligible for context.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DeliveryMode {
    /// Become eligible at the next safe model-request boundary.
    Steer,
    /// Wait until the active run reaches a terminal result.
    Queue,
}

/// A durable mailbox message.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEnvelope {
    message_id: MessageId,
    source: MessageSource,
    delivery: DeliveryMode,
    payload: EncodedPayload,
    received_at: Timestamp,
}

impl MessageEnvelope {
    /// Constructs an envelope after validating source-specific delivery rules.
    pub fn new(
        message_id: MessageId,
        source: MessageSource,
        delivery: DeliveryMode,
        payload: EncodedPayload,
        received_at: Timestamp,
    ) -> Result<Self, MessageError> {
        if matches!(source, MessageSource::Actor { .. }) && delivery != DeliveryMode::Steer {
            return Err(MessageError::ActorMustSteer);
        }
        Ok(Self {
            message_id,
            source,
            delivery,
            payload,
            received_at,
        })
    }

    /// Returns the stable message identity.
    #[must_use]
    pub fn message_id(&self) -> &MessageId {
        &self.message_id
    }

    /// Returns authenticated message provenance.
    #[must_use]
    pub const fn source(&self) -> &MessageSource {
        &self.source
    }

    /// Returns the delivery policy.
    #[must_use]
    pub const fn delivery(&self) -> DeliveryMode {
        self.delivery
    }

    /// Returns the structured payload.
    #[must_use]
    pub const fn payload(&self) -> &EncodedPayload {
        &self.payload
    }

    /// Returns the host-observed receipt time.
    #[must_use]
    pub const fn received_at(&self) -> Timestamp {
        self.received_at
    }

    pub(crate) fn is_idempotent_retry_of(&self, existing: &Self) -> bool {
        self.message_id == existing.message_id
            && self.source == existing.source
            && self.delivery == existing.delivery
            && self.payload == existing.payload
    }

    pub(crate) fn validate(&self) -> Result<(), MessageError> {
        if matches!(self.source, MessageSource::Actor { .. })
            && self.delivery != DeliveryMode::Steer
        {
            Err(MessageError::ActorMustSteer)
        } else {
            Ok(())
        }
    }
}

/// A message envelope violates Lam's delivery contract.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum MessageError {
    /// Inter-actor messages always steer.
    #[error("messages sent by an actor must use steer delivery")]
    ActorMustSteer,
}
