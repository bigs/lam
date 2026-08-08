use std::fmt;
use std::str::FromStr;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// A string-backed Lam identifier failed validation.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{kind} must not be empty")]
pub struct InvalidIdentifier {
    kind: &'static str,
}

macro_rules! string_identifier {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Validates and constructs an identifier.
            pub fn new(value: impl Into<String>) -> Result<Self, InvalidIdentifier> {
                let value = value.into();
                if value.trim().is_empty() {
                    Err(InvalidIdentifier { kind: $kind })
                } else {
                    Ok(Self(value))
                }
            }

            /// Returns the identifier text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = InvalidIdentifier;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

string_identifier!(
    /// Stable identity of one Lam actor.
    ActorId,
    "actor identifier"
);
string_identifier!(
    /// Stable host-defined identity of one registered model configuration.
    ModelId,
    "model identifier"
);
string_identifier!(
    /// Stable identity of one admitted mailbox message.
    MessageId,
    "message identifier"
);
string_identifier!(
    /// Stable identity of one actor activation.
    RunId,
    "run identifier"
);
string_identifier!(
    /// Optional host-defined identity for a user principal.
    PrincipalId,
    "principal identifier"
);
string_identifier!(
    /// Identity of a trusted host-side component.
    ComponentId,
    "component identifier"
);
string_identifier!(
    /// Namespaced identity of a payload codec.
    CodecId,
    "codec identifier"
);

/// Non-secret identity recorded whenever an actor selects a model.
///
/// Authentication, endpoints, clients, and other executable configuration
/// remain in the runtime registry. This descriptor exists so historical logs
/// remain intelligible and a reopened actor cannot silently bind the same
/// [`ModelId`] to a different model.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDescriptor {
    provider: String,
    model: String,
    codec: String,
}

impl ModelDescriptor {
    /// Constructs a validated, non-secret model description.
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        codec: impl Into<String>,
    ) -> Result<Self, InvalidIdentifier> {
        let provider = nonempty(provider.into(), "model provider descriptor")?;
        let model = nonempty(model.into(), "model name descriptor")?;
        let codec = nonempty(codec.into(), "model codec descriptor")?;
        Ok(Self {
            provider,
            model,
            codec,
        })
    }

    /// Returns the provider family label.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Returns the provider's model identifier.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns the context/wire codec family label.
    #[must_use]
    pub fn codec(&self) -> &str {
        &self.codec
    }

    pub(crate) fn validate(&self) -> Result<(), InvalidIdentifier> {
        nonempty(self.provider.clone(), "model provider descriptor")?;
        nonempty(self.model.clone(), "model name descriptor")?;
        nonempty(self.codec.clone(), "model codec descriptor")?;
        Ok(())
    }
}

/// The model currently selected by one actor journal.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
    /// Stable registry key.
    pub model_id: ModelId,
    /// Durable, non-secret registry description.
    pub descriptor: ModelDescriptor,
    /// Reasoning effort the actor runs with, when the host records one at
    /// selection time (e.g. a child spawned with a fixed effort).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl ModelSelection {
    /// Couples a registry identity with its durable descriptor.
    #[must_use]
    pub const fn new(model_id: ModelId, descriptor: ModelDescriptor) -> Self {
        Self {
            model_id,
            descriptor,
            effort: None,
        }
    }

    /// Couples a registry identity with its durable descriptor and the
    /// reasoning effort the actor runs with.
    #[must_use]
    pub const fn with_effort(
        model_id: ModelId,
        descriptor: ModelDescriptor,
        effort: String,
    ) -> Self {
        Self {
            model_id,
            descriptor,
            effort: Some(effort),
        }
    }
}

fn nonempty(value: String, kind: &'static str) -> Result<String, InvalidIdentifier> {
    if value.trim().is_empty() {
        Err(InvalidIdentifier { kind })
    } else {
        Ok(value)
    }
}

/// Actor-journal position after an append.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize,
)]
#[serde(transparent)]
pub struct Revision(u64);

impl Revision {
    /// The head of an empty or nonexistent journal.
    pub const ZERO: Self = Self(0);

    /// Constructs a revision from its numeric representation.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric revision.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advances by an event count, returning `None` on representation overflow.
    #[must_use]
    pub fn checked_advance(self, count: usize) -> Option<Self> {
        let count = u64::try_from(count).ok()?;
        self.0.checked_add(count).map(Self)
    }
}

/// Position in the logical model-visible context stream.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize,
)]
#[serde(transparent)]
pub struct ContextSequence(u64);

impl ContextSequence {
    /// The position before the first context entry.
    pub const ZERO: Self = Self(0);

    /// Constructs a context sequence.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric context sequence.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// Host-observed Unix time in milliseconds.
///
/// Timestamps are informational. Journal revisions remain authoritative for
/// ordering and correctness.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    /// Constructs a timestamp from Unix milliseconds.
    #[must_use]
    pub const fn from_unix_millis(value: i64) -> Self {
        Self(value)
    }

    /// Returns Unix milliseconds.
    #[must_use]
    pub const fn as_unix_millis(self) -> i64 {
        self.0
    }
}

/// Identifies the logical format of an encoded payload.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodecRef {
    /// Namespaced codec identifier.
    pub id: CodecId,
    /// Codec-specific representation version.
    pub version: u32,
}

impl CodecRef {
    /// Constructs a codec reference.
    #[must_use]
    pub const fn new(id: CodecId, version: u32) -> Self {
        Self { id, version }
    }
}

/// A single authoritative structured payload plus its codec.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EncodedPayload {
    /// Codec required to interpret the value.
    pub codec: CodecRef,
    /// Authoritative JSON value.
    pub value: Value,
}

impl EncodedPayload {
    /// Constructs an already encoded payload.
    #[must_use]
    pub const fn new(codec: CodecRef, value: Value) -> Self {
        Self { codec, value }
    }

    /// Encodes a Serde value using Lam's native JSON codec.
    pub fn lam_json<T: Serialize>(value: T) -> Result<Self, serde_json::Error> {
        Ok(Self {
            codec: CodecRef::new(
                CodecId::new("lam/json").expect("the built-in codec id is valid"),
                1,
            ),
            value: serde_json::to_value(value)?,
        })
    }

    /// Decodes this payload as a requested Serde type.
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.value.clone())
    }
}
