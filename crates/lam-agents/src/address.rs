use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

/// Canonical, hierarchical address of one actor in an [`crate::AgentSystem`].
///
/// Addresses are absolute Unix-style paths such as `/root` or
/// `/root/researcher`. They are permanent identities, not reusable aliases.
#[derive(Clone, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ActorAddress(String);

impl ActorAddress {
    /// Validates an absolute, canonical actor address.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidActorAddress> {
        let value = value.into();
        if !value.starts_with('/') {
            return Err(invalid("actor address must be absolute"));
        }
        if value == "/" {
            return Err(invalid("actor address must contain at least one name"));
        }
        if value.ends_with('/') {
            return Err(invalid("actor address must not end with `/`"));
        }
        for segment in value[1..].split('/') {
            validate_segment(segment)?;
        }
        Ok(Self(value))
    }

    /// Returns the canonical address text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the final path segment.
    #[must_use]
    pub fn name(&self) -> &str {
        self.0
            .rsplit('/')
            .next()
            .expect("validated addresses contain one segment")
    }

    /// Returns the parent address, or `None` for a top-level actor.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        let separator = self.0.rfind('/').expect("validated addresses are absolute");
        (separator > 0).then(|| Self(self.0[..separator].to_owned()))
    }

    /// Derives a canonical child address from one name segment.
    pub fn child(&self, name: impl AsRef<str>) -> Result<Self, InvalidActorAddress> {
        let name = name.as_ref();
        validate_segment(name)?;
        Ok(Self(format!("{}/{name}", self.0)))
    }

    pub(crate) fn is_direct_child_of(&self, parent: &Self) -> bool {
        self.parent().as_ref() == Some(parent)
    }
}

impl fmt::Display for ActorAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ActorAddress {
    type Err = InvalidActorAddress;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for ActorAddress {
    type Error = InvalidActorAddress;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for ActorAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// An actor address or child-name segment was not canonical.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct InvalidActorAddress {
    message: String,
}

fn validate_segment(segment: &str) -> Result<(), InvalidActorAddress> {
    if segment.is_empty() {
        return Err(invalid("actor address contains an empty name"));
    }
    if segment == "." || segment == ".." {
        return Err(invalid("actor names must not be `.` or `..`"));
    }
    if segment.contains('/') {
        return Err(invalid("actor names must not contain `/`"));
    }
    if segment.trim() != segment {
        return Err(invalid(
            "actor names must not have leading or trailing whitespace",
        ));
    }
    if segment.chars().any(char::is_control) {
        return Err(invalid("actor names must not contain control characters"));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> InvalidActorAddress {
    InvalidActorAddress {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::ActorAddress;

    #[test]
    fn validates_and_navigates_canonical_addresses() {
        let root = ActorAddress::new("/root").unwrap();
        let child = root.child("researcher").unwrap();
        let grandchild = child.child("reader.v2").unwrap();

        assert_eq!(child.as_str(), "/root/researcher");
        assert_eq!(child.name(), "researcher");
        assert_eq!(child.parent(), Some(root.clone()));
        assert!(child.is_direct_child_of(&root));
        assert_eq!(grandchild.parent(), Some(child));
        assert_eq!(root.parent(), None);
    }

    #[test]
    fn rejects_ambiguous_or_relative_addresses() {
        for invalid in [
            "root",
            "/",
            "/root/",
            "/root//child",
            "/root/./child",
            "/root/../child",
            "/root/ child",
        ] {
            assert!(ActorAddress::new(invalid).is_err(), "accepted {invalid:?}");
        }

        assert!(
            ActorAddress::new("/root")
                .unwrap()
                .child("nested/name")
                .is_err()
        );
    }
}
