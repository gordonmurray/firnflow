//! Per-namespace generation counter.
//!
//! Each namespace owns an opaque `u64` counter. Reads that consult the
//! cache embed the current generation in the cache key; writes
//! atomically increment the counter, making all previously cached
//! entries for that namespace unreachable by key in O(1) time regardless
//! of how many queries exist.

use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;

use crate::NamespaceId;

/// Atomic per-namespace generation counter.
#[derive(Debug, Default)]
pub struct GenerationCounter {
    inner: DashMap<NamespaceId, AtomicU64>,
}

impl GenerationCounter {
    /// Create an empty counter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the current generation for a namespace.
    ///
    /// Namespaces that have never been written return `0`. The first
    /// call to [`bump`](Self::bump) produces `1`.
    pub fn current(&self, ns: &NamespaceId) -> u64 {
        self.inner
            .get(ns)
            .map(|g| g.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    /// Atomically increment the generation for a namespace and return
    /// the new value.
    pub fn bump(&self, ns: &NamespaceId) -> u64 {
        let entry = self.inner.entry(ns.clone()).or_default();
        entry.fetch_add(1, Ordering::AcqRel) + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(name: &str) -> NamespaceId {
        NamespaceId::new(name).unwrap()
    }

    #[test]
    fn starts_at_zero() {
        let c = GenerationCounter::new();
        assert_eq!(c.current(&ns("a")), 0);
    }

    #[test]
    fn bump_is_monotonic() {
        let c = GenerationCounter::new();
        let a = ns("a");
        assert_eq!(c.bump(&a), 1);
        assert_eq!(c.bump(&a), 2);
        assert_eq!(c.bump(&a), 3);
        assert_eq!(c.current(&a), 3);
    }

    #[test]
    fn namespaces_are_independent() {
        let c = GenerationCounter::new();
        let a = ns("a");
        let b = ns("b");
        c.bump(&a);
        c.bump(&a);
        c.bump(&b);
        assert_eq!(c.current(&a), 2);
        assert_eq!(c.current(&b), 1);
    }
}
