//! In-memory replay of durable AI-agent run transitions (06-03, SVC-03,
//! G-07, the locked restart-recovery bar). Mirrors `activity/mod.rs`'s
//! `ActivityRegistry::from_store` replay-and-fold shape, but scoped to the
//! handler's own `RunHandle -> phase` history (see `store.rs`'s module doc
//! for why this is a sibling store, never a shared one, per
//! 06-RESEARCH.md's "Pitfall 4").

pub mod store;

use std::collections::HashMap;
use std::path::Path;

pub use store::{AgentRunPhase, AgentRunRecord};

/// The retention window (days) this store shares with `activity/store.rs`'s
/// own window, so the two logs age out together.
pub const AGENT_LOG_RETENTION_DAYS: u64 = 7;

/// One run's folded final state after replay -- what
/// `AiAgentService::from_store` (Task 3) needs to rebuild its in-memory
/// `RunState` map.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayedRun {
    pub phase: AgentRunPhase,
    pub envelope: Option<serde_json::Value>,
    pub detail: Option<String>,
}

/// Replays every durable transition in `dir` (within `retention_days`),
/// folds them by `run_id` (last write wins for the phase), and applies the
/// locked restart-recovery bar: any run whose final replayed phase is still
/// `Started` is rewritten to `Interrupted` with a detail naming the daemon
/// restart.
///
/// This function does NOT track PIDs, probe liveness, or attempt to
/// re-attach to a still-running child process. A run left `Started` at
/// replay time is reported as honestly interrupted, never resurrected and
/// never left `Running` forever. That boundary is locked (06-03-PLAN.md
/// objective, 06-AI-SPEC.md's restart-recovery bar) and must not be
/// "improved" without an explicit decision.
///
/// The second return value is the next free handle counter: the highest
/// numeric suffix seen among replayed `agent-{n}` run ids, plus one (0 when
/// nothing replayed) -- mirrors `ActivityRegistry::from_store`'s minter
/// seeding so a restarted daemon never re-mints an id already on disk.
pub fn replay_runs(
    dir: &Path,
    retention_days: u64,
) -> std::io::Result<(HashMap<String, ReplayedRun>, u64)> {
    let records = store::replay(dir, retention_days)?;

    let mut runs: HashMap<String, ReplayedRun> = HashMap::new();
    let mut max_seen: u64 = 0;

    for record in records {
        if let Some(n) = record.run_id.strip_prefix("agent-").and_then(|s| s.parse::<u64>().ok())
        {
            max_seen = max_seen.max(n + 1);
        }

        // Last write wins: `store::replay` returns records oldest-first, so
        // a later `insert` for the same `run_id` overwrites the earlier
        // fold, exactly matching a real transition sequence.
        runs.insert(
            record.run_id.clone(),
            ReplayedRun {
                phase: record.phase,
                envelope: record.envelope,
                detail: record.detail,
            },
        );
    }

    for run in runs.values_mut() {
        if run.phase == AgentRunPhase::Started {
            run.phase = AgentRunPhase::Interrupted;
            run.detail =
                Some("interrupted: the daemon restarted while this run was still in flight".to_string());
        }
    }

    Ok((runs, max_seen))
}
