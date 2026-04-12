//! Namespace identifiers and validation.

use serde::{Deserialize, Serialize};

use crate::FirnflowError;

const MAX_NAMESPACE_LEN: usize = 64;

/// A validated namespace identifier.
///
/// Namespace names are lowercase alphanumeric plus hyphens, at most 64
/// characters. Validation happens once at construction; thereafter the
/// value is guaranteed well-formed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NamespaceId(String);

impl NamespaceId {
    /// Parse and validate a namespace name.
    pub fn new(name: impl Into<String>) -> Result<Self, FirnflowError> {
        let name = name.into();
        if name.is_empty() || name.len() > MAX_NAMESPACE_LEN {
            return Err(FirnflowError::InvalidNamespace(name));
        }
        if !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(FirnflowError::InvalidNamespace(name));
        }
        Ok(Self(name))
    }

    /// Return the raw string form of this namespace id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NamespaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_names() {
        for name in ["acme", "acme-prod", "ns-2026", "a", "0"] {
            assert!(NamespaceId::new(name).is_ok(), "should accept {name}");
        }
    }

    #[test]
    fn rejects_empty() {
        assert!(NamespaceId::new("").is_err());
    }

    #[test]
    fn rejects_uppercase() {
        assert!(NamespaceId::new("Acme").is_err());
    }

    #[test]
    fn rejects_underscores() {
        assert!(NamespaceId::new("ns_underscore").is_err());
    }

    #[test]
    fn rejects_over_max_length() {
        assert!(NamespaceId::new("a".repeat(65)).is_err());
    }
}
