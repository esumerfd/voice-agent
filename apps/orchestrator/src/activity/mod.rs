//! Server-side activity core (D-01/D-02/D-05/D-06/D-12): a globally-unique
//! `run_id` minter (`run_id`), a durable JSONL rotation store (`store`),
//! and the `ActivityRegistry` that ties them together into a per-run
//! lifecycle log. This module is self-contained -- it observes
//! `invoke_workflow` from the outside via `mint_run_id`/`record_transition`
//! and never modifies `crate::service::Service` (D-06). No `server/mod.rs`
//! wiring happens here -- that integration is a later plan (05-05).

pub mod run_id;
pub mod store;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use shared::{ActivityEvent, ActivityLogEvent, ActivityPhase, ActivityStatus, Envelope};

use run_id::RunIdMinter;
use store::ActivityRecord;

/// In-memory index of every tracked activity (D-01/D-05), keyed strictly by
/// `run_id` -- never collapsed by `workflow_id`. Observes lifecycle points
/// from the outside; never adds a method to or alters `Service` (D-06).
pub struct ActivityRegistry {
    minter: RunIdMinter,
    activities: Mutex<HashMap<String, ActivityEvent>>,
    store_dir: Option<PathBuf>,
}

impl ActivityRegistry {
    /// A fresh, empty registry with no on-disk persistence.
    pub fn new() -> Self {
        Self {
            minter: RunIdMinter::new(),
            activities: Mutex::new(HashMap::new()),
            store_dir: None,
        }
    }

    /// A registry backed by an on-disk rotation store at `dir`: every
    /// mint/transition is persisted, and the in-memory index is rebuilt by
    /// folding `store::replay`'s records (D-01/D-12) so a restarted daemon
    /// reproduces its pre-restart history.
    pub fn from_store(dir: impl Into<PathBuf>, retention_days: u64) -> std::io::Result<Self> {
        let dir = dir.into();
        let records = store::replay(&dir, retention_days)?;

        let mut activities: HashMap<String, ActivityEvent> = HashMap::new();
        // Track the highest run_id counter value already on disk so the
        // rebuilt minter never re-mints an id present in history.
        let mut max_seen: u64 = 0;
        for record in records {
            if let Some(n) = record
                .run_id
                .strip_prefix("run-")
                .and_then(|s| s.parse::<u64>().ok())
            {
                max_seen = max_seen.max(n + 1);
            }

            let entry = activities
                .entry(record.run_id.clone())
                .or_insert_with(|| ActivityEvent {
                    run_id: record.run_id.clone(),
                    workflow_id: record.workflow_id.clone(),
                    client_name: record.client_name.clone(),
                    session_id: record.session_id.clone(),
                    status: ActivityStatus::NotStarted,
                    started_at_ms: record.at_ms,
                    log: Vec::new(),
                });
            entry.status = status_for_phase(record.phase);
            entry.log.push(ActivityLogEvent {
                phase: record.phase,
                at_ms: record.at_ms,
                detail: record.detail,
            });
        }

        Ok(Self {
            minter: RunIdMinter::with_start(max_seen),
            activities: Mutex::new(activities),
            store_dir: Some(dir),
        })
    }

    /// Mints a fresh run_id (D-02/D-05), creates its Invoked entry, and
    /// persists the transition. Two calls for the same `workflow_id`
    /// always produce two distinct activities, never collapsed.
    ///
    /// `session_id` (Phase 8, D-01/D-02) is an additive parameter, never a
    /// replacement for `client_name`: it records which connection started
    /// this run, so two connections sharing the identical `client_name`
    /// still produce activities attributable to different sessions.
    pub fn mint_run_id(&self, workflow_id: &str, client_name: &str, session_id: &str) -> String {
        let run_id = self.minter.mint();
        let now = now_ms();
        let event = ActivityEvent {
            run_id: run_id.clone(),
            workflow_id: workflow_id.to_string(),
            client_name: client_name.to_string(),
            session_id: session_id.to_string(),
            status: status_for_phase(ActivityPhase::Invoked),
            started_at_ms: now,
            log: vec![ActivityLogEvent {
                phase: ActivityPhase::Invoked,
                at_ms: now,
                detail: None,
            }],
        };
        self.activities.lock().unwrap().insert(run_id.clone(), event);
        self.persist(ActivityRecord {
            run_id: run_id.clone(),
            workflow_id: workflow_id.to_string(),
            client_name: client_name.to_string(),
            session_id: session_id.to_string(),
            phase: ActivityPhase::Invoked,
            at_ms: now,
            detail: None,
        });
        run_id
    }

    /// Appends a lifecycle transition for `run_id` (D-06), updating its
    /// `ActivityStatus` (Running -> white, Completed -> Success/green,
    /// Failed -> Failure/red) and persisting it (D-12). A transition for an
    /// unknown `run_id` is a handled no-op, never a panic.
    pub fn record_transition(
        &self,
        run_id: &str,
        phase: ActivityPhase,
        detail: Option<serde_json::Value>,
    ) {
        let now = now_ms();
        let known = {
            let mut guard = self.activities.lock().unwrap();
            match guard.get_mut(run_id) {
                Some(event) => {
                    event.status = status_for_phase(phase);
                    event.log.push(ActivityLogEvent {
                        phase,
                        at_ms: now,
                        detail: detail.clone(),
                    });
                    Some((
                        event.workflow_id.clone(),
                        event.client_name.clone(),
                        event.session_id.clone(),
                    ))
                }
                None => None,
            }
        };

        if let Some((workflow_id, client_name, session_id)) = known {
            self.persist(ActivityRecord {
                run_id: run_id.to_string(),
                workflow_id,
                client_name,
                session_id,
                phase,
                at_ms: now,
                detail,
            });
        }
    }

    /// Writes `record` to the on-disk store, if this registry has one. A
    /// persistence failure is logged and never propagated as a panic --
    /// the in-memory index (already updated by the caller) remains the
    /// live source of truth for this process's lifetime.
    fn persist(&self, record: ActivityRecord) {
        if let Some(dir) = &self.store_dir {
            if let Err(err) = store::append(dir, &record) {
                eprintln!(
                    "activity registry: failed to persist transition for {}: {err}",
                    record.run_id
                );
            }
        }
    }

    /// A clone of every known activity's current `ActivityEvent`, in no
    /// particular order (Phase 8 plan 08-03, D-04): the FILTERABLE form the
    /// server needs so a per-recipient replay burst can pass each one
    /// through `shared::filter_activity_event_for` before it ever reaches a
    /// connection -- unlike `snapshot_as_events` below, this carries no
    /// `Envelope` wrapper baked in yet.
    pub fn snapshot(&self) -> Vec<ActivityEvent> {
        self.activities.lock().unwrap().values().cloned().collect()
    }

    /// One `Envelope::Activity` per known activity (D-01), expressed in
    /// terms of `snapshot` above so there is one source of truth for "every
    /// known activity's current event".
    pub fn snapshot_as_events(&self) -> Vec<Envelope> {
        self.snapshot()
            .into_iter()
            .map(|event| Envelope::Activity { event })
            .collect()
    }

    /// Returns a clone of the current `ActivityEvent` for `run_id`, if
    /// known -- used by `server/mod.rs` (05-05) to broadcast the
    /// just-updated snapshot immediately after each lifecycle transition
    /// (D-01), rather than re-deriving it from the mutation call site.
    pub fn get(&self, run_id: &str) -> Option<ActivityEvent> {
        self.activities.lock().unwrap().get(run_id).cloned()
    }
}

impl Default for ActivityRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Maps a synthesized lifecycle transition (D-06) onto the ROADMAP's locked
/// status-color spec: `Invoked` -> gray (not yet running), `Running` ->
/// white, `Completed` -> green/Success, `Failed` -> red/Failure.
fn status_for_phase(phase: ActivityPhase) -> ActivityStatus {
    match phase {
        ActivityPhase::Invoked => ActivityStatus::NotStarted,
        ActivityPhase::Running => ActivityStatus::Running,
        ActivityPhase::Completed => ActivityStatus::Success,
        ActivityPhase::Failed => ActivityStatus::Failure,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn status_of(registry: &ActivityRegistry, run_id: &str) -> Option<ActivityStatus> {
        registry.snapshot_as_events().into_iter().find_map(|envelope| match envelope {
            Envelope::Activity { event } if event.run_id == run_id => Some(event.status),
            _ => None,
        })
    }

    fn events_of(registry: &ActivityRegistry) -> Vec<ActivityEvent> {
        let mut events: Vec<ActivityEvent> = registry
            .snapshot_as_events()
            .into_iter()
            .map(|envelope| match envelope {
                Envelope::Activity { event } => event,
                _ => unreachable!("snapshot_as_events must only ever produce Activity frames"),
            })
            .collect();
        events.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        events
    }

    fn assert_events_match(live: &[ActivityEvent], rebuilt: &[ActivityEvent]) {
        assert_eq!(live.len(), rebuilt.len(), "snapshot activity count mismatch");
        for (a, b) in live.iter().zip(rebuilt.iter()) {
            assert_eq!(a.run_id, b.run_id);
            assert_eq!(a.workflow_id, b.workflow_id);
            assert_eq!(a.client_name, b.client_name);
            assert_eq!(a.session_id, b.session_id);
            assert_eq!(a.status, b.status);
            assert_eq!(a.log.len(), b.log.len(), "log length mismatch for {}", a.run_id);
            for (la, lb) in a.log.iter().zip(b.log.iter()) {
                assert_eq!(la.phase, lb.phase);
                assert_eq!(la.detail, lb.detail);
            }
        }
    }

    #[test]
    fn distinct_activities_per_workflow_id() {
        let registry = ActivityRegistry::new();
        let first = registry.mint_run_id("set_timer", "orchestrator-cli", "sess-1");
        let second = registry.mint_run_id("set_timer", "orchestrator-cli", "sess-1");
        assert_ne!(first, second, "re-running the same workflow must mint two distinct run_ids");

        let snapshot = registry.snapshot_as_events();
        assert_eq!(
            snapshot.len(),
            2,
            "two separate activities must exist, never collapsed by workflow_id (D-05)"
        );
    }

    #[test]
    fn status_transition_mapping() {
        let registry = ActivityRegistry::new();
        let run_id = registry.mint_run_id("set_timer", "orchestrator-cli", "sess-1");

        registry.record_transition(&run_id, ActivityPhase::Running, None);
        assert_eq!(status_of(&registry, &run_id), Some(ActivityStatus::Running));

        registry.record_transition(
            &run_id,
            ActivityPhase::Completed,
            Some(serde_json::json!({"ok": true})),
        );
        assert_eq!(status_of(&registry, &run_id), Some(ActivityStatus::Success));

        let run_id_2 = registry.mint_run_id("set_timer", "orchestrator-cli", "sess-1");
        registry.record_transition(
            &run_id_2,
            ActivityPhase::Failed,
            Some(serde_json::json!({"error": "boom"})),
        );
        assert_eq!(status_of(&registry, &run_id_2), Some(ActivityStatus::Failure));
    }

    #[test]
    fn get_returns_the_current_event_for_a_known_run_id_and_none_for_an_unknown_one() {
        let registry = ActivityRegistry::new();
        let run_id = registry.mint_run_id("set_timer", "orchestrator-cli", "sess-1");

        let event = registry.get(&run_id).expect("expected a known run_id to resolve to an event");
        assert_eq!(event.run_id, run_id);
        assert_eq!(event.status, ActivityStatus::NotStarted);

        registry.record_transition(&run_id, ActivityPhase::Completed, Some(serde_json::json!({"ok": true})));
        let updated = registry.get(&run_id).expect("expected the event to still be present after a transition");
        assert_eq!(updated.status, ActivityStatus::Success, "expected get() to reflect the latest transition");

        assert!(registry.get("run-does-not-exist").is_none(), "expected an unknown run_id to resolve to None");
    }

    #[test]
    fn from_store_rebuild_reproduces_live_snapshot() {
        let dir = tempdir().expect("tempdir");
        let registry = ActivityRegistry::from_store(dir.path(), 7).expect("fresh from_store");
        let run_id = registry.mint_run_id("set_timer", "orchestrator-cli", "sess-1");
        registry.record_transition(&run_id, ActivityPhase::Running, None);
        registry.record_transition(
            &run_id,
            ActivityPhase::Completed,
            Some(serde_json::json!({"ok": true})),
        );

        let live = events_of(&registry);

        let rebuilt = ActivityRegistry::from_store(dir.path(), 7).expect("rebuild from store");
        let rebuilt_events = events_of(&rebuilt);

        assert_events_match(&live, &rebuilt_events);
    }

    /// D-01/D-02: two runs sharing the identical `client_name` but minted
    /// with different session ids must be attributable to different
    /// sessions -- `client_name` is a display label only, `session_id` is
    /// the real identity.
    #[test]
    fn mint_run_id_with_same_client_name_but_different_session_ids_produces_distinct_session_ids() {
        let registry = ActivityRegistry::new();
        let first = registry.mint_run_id("set_timer", "orchestrator-cli", "sess-1");
        let second = registry.mint_run_id("set_timer", "orchestrator-cli", "sess-2");

        let first_event = registry.get(&first).expect("first run must be known");
        let second_event = registry.get(&second).expect("second run must be known");

        assert_eq!(first_event.client_name, second_event.client_name);
        assert_ne!(
            first_event.session_id, second_event.session_id,
            "identical client_name must not collapse distinct session ids"
        );
        assert_eq!(first_event.session_id, "sess-1");
        assert_eq!(second_event.session_id, "sess-2");
    }

    /// T-08-04: a directory containing both a legacy record (no
    /// `session_id` key) and a new record (with one) must rebuild both
    /// activities with zero replay errors, the legacy one carrying an
    /// empty session id.
    #[test]
    fn from_store_rebuilds_mixed_legacy_and_new_records_with_zero_replay_errors() {
        let dir = tempdir().expect("tempdir");
        let file_path = dir.path().join("activities-2020-01-01.jsonl");
        let legacy_line = serde_json::json!({
            "run_id": "run-0",
            "workflow_id": "set_timer",
            "client_name": "orchestrator-cli",
            "phase": "completed",
            "at_ms": 1_000,
            "detail": null
        });
        let new_line = serde_json::json!({
            "run_id": "run-1",
            "workflow_id": "set_timer",
            "client_name": "orchestrator-cli",
            "session_id": "sess-1",
            "phase": "completed",
            "at_ms": 2_000,
            "detail": null
        });
        std::fs::write(
            &file_path,
            format!("{}\n{}\n", legacy_line, new_line),
        )
        .expect("write mixed legacy/new fixture file");

        let registry = ActivityRegistry::from_store(dir.path(), 3650).expect("from_store must rebuild without error");
        let events = events_of(&registry);
        assert_eq!(events.len(), 2, "both legacy and new records must rebuild into activities");

        let legacy_event = events.iter().find(|e| e.run_id == "run-0").expect("legacy record must rebuild");
        assert_eq!(legacy_event.session_id, "", "legacy record with no session_id key must rebuild with an empty session id");

        let new_event = events.iter().find(|e| e.run_id == "run-1").expect("new record must rebuild");
        assert_eq!(new_event.session_id, "sess-1");
    }
}
