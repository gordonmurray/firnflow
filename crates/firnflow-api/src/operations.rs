//! In-memory registry for background API operations (issue #34).
//!
//! The async endpoints (`warmup`, `index`, `fts-index`, `scalar-index`,
//! `compact`) accept work, return `202 Accepted`, and run it in a
//! `tokio::spawn`. This registry gives each of those a first-class,
//! pollable handle: an opaque `operation_id` returned in the 202, and a
//! `GET /operations/{id}` status endpoint backed by the records here.
//!
//! Records are ephemeral — process-local, never persisted — and bounded:
//! only the most recent [`DEFAULT_MAX_COMPLETED`] completed operations
//! are kept, while running operations are never evicted. The public
//! shape is deliberately small so a future durable or clustered job
//! store could replace the backing without changing the API.
//!
//! ## Lifecycle
//!
//! V1 is `Running -> Succeeded | Failed`. There is no `Queued` because
//! the spawn model has no real queue (an op would sit "queued" for
//! microseconds), and no `Cancelled` because there is no cancellation
//! API. Both can be added later without a breaking change.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use serde::Serialize;
use uuid::Uuid;

/// Default cap on retained completed (succeeded / failed) operations.
/// Running operations are never counted against this.
pub const DEFAULT_MAX_COMPLETED: usize = 256;

/// The kind of background work an operation tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    /// Cache warmup (`POST /ns/{ns}/warmup`).
    Warmup,
    /// IVF_PQ vector index build (`POST /ns/{ns}/index`).
    Index,
    /// BM25 full-text index build (`POST /ns/{ns}/fts-index`).
    FtsIndex,
    /// BTree scalar index build (`POST /ns/{ns}/scalar-index`).
    ScalarIndex,
    /// Compaction (`POST /ns/{ns}/compact`).
    Compact,
}

/// Lifecycle state of a tracked operation. See the module docs for why
/// `queued` and `cancelled` are intentionally absent in v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationStatus {
    /// The background task is in flight.
    Running,
    /// The work completed successfully.
    Succeeded,
    /// The work returned an error (see [`OperationRecord::error`]).
    Failed,
}

/// A snapshot of a tracked operation, returned by the status endpoint.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OperationRecord {
    /// Opaque, time-sortable identifier. Clients must not parse it.
    pub operation_id: String,
    /// What kind of work this operation is.
    pub kind: OperationKind,
    /// Namespace the work targets.
    pub namespace: String,
    /// Current lifecycle state.
    pub status: OperationStatus,
    /// Milliseconds since the Unix epoch when the work began.
    pub started_at_ms: u64,
    /// Milliseconds since the Unix epoch when it reached a terminal
    /// state; `null` while still running.
    pub finished_at_ms: Option<u64>,
    /// Concise, client-facing failure message; `null` unless failed.
    /// Detailed diagnostics stay in the server logs.
    pub error: Option<String>,
}

/// In-memory, bounded registry of background operations.
pub struct OperationRegistry {
    records: DashMap<String, OperationRecord>,
    /// Completion order of terminal operations, used to evict the
    /// oldest once `max_completed` is exceeded.
    completed: Mutex<VecDeque<String>>,
    max_completed: usize,
}

impl OperationRegistry {
    /// Registry with the default completed-operation cap.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_COMPLETED)
    }

    /// Registry with a custom completed-operation cap (tests use a small
    /// cap to exercise eviction without thousands of inserts).
    pub fn with_capacity(max_completed: usize) -> Self {
        Self {
            records: DashMap::new(),
            completed: Mutex::new(VecDeque::new()),
            max_completed: max_completed.max(1),
        }
    }

    /// Register a new operation as `running` and return its opaque id.
    pub fn start(&self, kind: OperationKind, namespace: String) -> String {
        let operation_id = format!("op_{}", Uuid::now_v7());
        let record = OperationRecord {
            operation_id: operation_id.clone(),
            kind,
            namespace,
            status: OperationStatus::Running,
            started_at_ms: now_ms(),
            finished_at_ms: None,
            error: None,
        };
        self.records.insert(operation_id.clone(), record);
        operation_id
    }

    /// Mark an operation succeeded.
    pub fn succeed(&self, operation_id: &str) {
        self.finish(operation_id, OperationStatus::Succeeded, None);
    }

    /// Mark an operation failed with a concise client-facing message.
    pub fn fail(&self, operation_id: &str, error: impl Into<String>) {
        self.finish(operation_id, OperationStatus::Failed, Some(error.into()));
    }

    fn finish(&self, operation_id: &str, status: OperationStatus, error: Option<String>) {
        {
            let Some(mut rec) = self.records.get_mut(operation_id) else {
                // Already evicted or never registered — nothing to do.
                return;
            };
            rec.status = status;
            rec.finished_at_ms = Some(now_ms());
            rec.error = error;
        }
        // Record the completion and evict the oldest terminal records
        // beyond the cap. Running operations never enter this queue, so
        // they are never evicted.
        let mut order = self
            .completed
            .lock()
            .expect("operation registry mutex poisoned");
        order.push_back(operation_id.to_string());
        while order.len() > self.max_completed {
            if let Some(old) = order.pop_front() {
                self.records.remove(&old);
            }
        }
    }

    /// Snapshot of an operation, or `None` if the id is unknown or its
    /// record has been evicted.
    pub fn get(&self, operation_id: &str) -> Option<OperationRecord> {
        self.records.get(operation_id).map(|r| r.clone())
    }
}

impl Default for OperationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_then_succeed_and_fail() {
        let reg = OperationRegistry::new();

        let ok = reg.start(OperationKind::ScalarIndex, "n1".into());
        assert!(ok.starts_with("op_"), "ids are prefixed and opaque");
        let rec = reg.get(&ok).unwrap();
        assert_eq!(rec.kind, OperationKind::ScalarIndex);
        assert_eq!(rec.namespace, "n1");
        assert_eq!(rec.status, OperationStatus::Running);
        assert!(rec.finished_at_ms.is_none());
        assert!(rec.error.is_none());

        reg.succeed(&ok);
        let rec = reg.get(&ok).unwrap();
        assert_eq!(rec.status, OperationStatus::Succeeded);
        assert!(rec.finished_at_ms.is_some());
        assert!(rec.error.is_none());

        let bad = reg.start(OperationKind::Index, "n2".into());
        reg.fail(&bad, "index training failed");
        let rec = reg.get(&bad).unwrap();
        assert_eq!(rec.status, OperationStatus::Failed);
        assert_eq!(rec.error.as_deref(), Some("index training failed"));
    }

    #[test]
    fn unknown_id_is_none() {
        let reg = OperationRegistry::new();
        assert!(reg.get("op_nonexistent").is_none());
    }

    #[test]
    fn ids_are_unique() {
        let reg = OperationRegistry::new();
        let a = reg.start(OperationKind::Compact, "n".into());
        let b = reg.start(OperationKind::Compact, "n".into());
        assert_ne!(a, b);
    }

    #[test]
    fn completed_ops_are_evicted_beyond_cap() {
        let reg = OperationRegistry::with_capacity(2);
        let a = reg.start(OperationKind::Warmup, "n".into());
        let b = reg.start(OperationKind::Warmup, "n".into());
        let c = reg.start(OperationKind::Warmup, "n".into());
        reg.succeed(&a);
        reg.succeed(&b);
        reg.succeed(&c);
        // Oldest completed (a) is evicted; the two newest survive.
        assert!(
            reg.get(&a).is_none(),
            "oldest completed op should be evicted"
        );
        assert!(reg.get(&b).is_some());
        assert!(reg.get(&c).is_some());
    }

    #[test]
    fn running_ops_are_never_evicted() {
        let reg = OperationRegistry::with_capacity(1);
        let running = reg.start(OperationKind::Index, "n".into());
        let a = reg.start(OperationKind::Warmup, "n".into());
        let b = reg.start(OperationKind::Warmup, "n".into());
        reg.succeed(&a);
        reg.succeed(&b);
        // The cap (1) evicts completed `a`, but the still-running op is
        // untouched even though it is older.
        assert!(
            reg.get(&running).is_some(),
            "running op must not be evicted"
        );
        assert!(reg.get(&a).is_none());
        assert!(reg.get(&b).is_some());
    }
}
