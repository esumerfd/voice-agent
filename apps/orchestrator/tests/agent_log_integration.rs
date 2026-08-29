//! Durable AI-agent run log integration tests (06-03, SVC-03, G-07,
//! ROADMAP Criterion 4). Covers Task 1 (`agent_log::store` append/replay/
//! prune/`prompt_digest`), Task 2 (`agent_log::replay_runs` folding +
//! restart-recovery rewrite + handle-counter seeding), and Task 3
//! (`AiAgentService` log-before-memory wiring + `from_store`
//! reconstruction). Every test that spawns a subprocess uses
//! `tests/fixtures/fake_claude.sh` -- never the real `claude` binary, no
//! network access, no API cost.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tempfile::TempDir;
use tokio::sync::Mutex;

use orchestrator::agent_log;
use orchestrator::agent_log::store::{self, AgentRunPhase, AgentRunRecord};
use orchestrator::handlers::ai_agent::AiAgentService;
use orchestrator::runtime::local::LocalRuntime;
use orchestrator::{RunHandle, RunStatus, Service};

/// Serializes every Task 3 test in this file: each one spawns the
/// `fake_claude.sh` fixture as a real subprocess, mirroring the
/// `SPAWN_GUARD` convention already used in `runtime/local.rs` and
/// `ai_agent_integration.rs` to avoid cross-test env-var/process races.
static SPAWN_GUARD: Mutex<()> = Mutex::const_new(());

/// Absolute path to the `fake_claude.sh` fixture double.
fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_claude.sh")
}

/// A real, always-existing directory used as `workflow_dir` for tests that
/// declare no `files`.
fn workflow_dir_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A fully-populated `AgentRunRecord` template for store-level tests, with
/// `run_id`/`phase` as the only varying inputs -- every other field is a
/// fixed, deterministic value so equality assertions don't depend on wall
/// clock time.
fn sample_record(run_id: &str, phase: AgentRunPhase) -> AgentRunRecord {
    AgentRunRecord {
        run_id: run_id.to_string(),
        workflow_id: "agent_ping".to_string(),
        phase,
        at_ms: 1_000,
        claude_version: Some("2.1.224".to_string()),
        model: Some("claude-sonnet-5".to_string()),
        max_budget_usd: Some(1.0),
        scratch_dir: Some("/tmp/agent-run-scratch".to_string()),
        staged_files: vec!["dep.txt".to_string()],
        prompt_digest: "abc123".to_string(),
        prompt_bytes: 42,
        params: Some(json!({"foo": "bar"})),
        envelope: Some(json!({"is_error": false})),
        detail: None,
    }
}

/// Returns the single rotation file `dir` is expected to contain --
/// panics if there isn't exactly one, since every test using this helper
/// has already appended exactly one day's worth of records.
fn only_rotation_file(dir: &Path) -> PathBuf {
    let entries: Vec<PathBuf> =
        fs::read_dir(dir).expect("read_dir").filter_map(|e| e.ok()).map(|e| e.path()).collect();
    assert_eq!(entries.len(), 1, "expected exactly one rotation file, found {entries:?}");
    entries.into_iter().next().unwrap()
}

/// Polls `status()` until it leaves `Running` or `deadline` passes,
/// mirroring `ai_agent_integration.rs`'s own poll-until-terminal shape.
async fn poll_until_terminal(service: &AiAgentService, handle: &RunHandle) -> RunStatus {
    let mut status = service.status(handle).await.expect("status should succeed");
    let deadline = Instant::now() + Duration::from_secs(2);
    while status == RunStatus::Running && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        status = service.status(handle).await.expect("status should succeed");
    }
    status
}

// ---------------------------------------------------------------------
// Task 1: `agent_log::store` — append/replay/prune/prompt_digest
// ---------------------------------------------------------------------

#[test]
fn append_then_replay_round_trips_every_field() {
    let dir = TempDir::new().expect("tempdir");
    let record = sample_record("agent-0", AgentRunPhase::Completed);
    store::append(dir.path(), &record).expect("append should succeed");

    let replayed = store::replay(dir.path(), 7).expect("replay should succeed");
    assert_eq!(replayed, vec![record]);
}

#[test]
fn append_creates_the_target_directory_when_it_does_not_exist() {
    let base = TempDir::new().expect("tempdir");
    let dir = base.path().join("nested/agent-runs");
    assert!(!dir.exists(), "precondition: the directory must not exist yet");

    let record = sample_record("agent-0", AgentRunPhase::Started);
    store::append(&dir, &record).expect("append should create the directory");
    assert!(dir.is_dir(), "expected append to have created the target directory");
}

#[test]
fn two_appends_for_the_same_run_id_replay_in_write_order_oldest_first() {
    let dir = TempDir::new().expect("tempdir");
    let started = sample_record("agent-0", AgentRunPhase::Started);
    let completed = sample_record("agent-0", AgentRunPhase::Completed);
    store::append(dir.path(), &started).expect("append should succeed");
    store::append(dir.path(), &completed).expect("append should succeed");

    let replayed = store::replay(dir.path(), 7).expect("replay should succeed");
    assert_eq!(replayed.len(), 2);
    assert_eq!(replayed[0].phase, AgentRunPhase::Started, "expected the Started record first");
    assert_eq!(replayed[1].phase, AgentRunPhase::Completed, "expected the Completed record second");
}

#[test]
fn a_truncated_final_line_is_skipped_and_the_valid_line_still_replays() {
    let dir = TempDir::new().expect("tempdir");
    let record = sample_record("agent-0", AgentRunPhase::Started);
    store::append(dir.path(), &record).expect("append should succeed");

    let path = only_rotation_file(dir.path());
    let mut file = OpenOptions::new().append(true).open(&path).expect("open for append");
    write!(file, "{{not valid json").expect("write a truncated trailing line");

    let replayed = store::replay(dir.path(), 7)
        .expect("replay should succeed despite the truncated trailing line");
    assert_eq!(
        replayed,
        vec![record],
        "expected the valid line to survive and the truncated line to be skipped, not the whole file discarded"
    );
}

#[test]
fn a_file_not_matching_this_stores_rotation_naming_is_ignored_by_replay_and_untouched_by_prune() {
    let dir = TempDir::new().expect("tempdir");
    fs::create_dir_all(dir.path()).expect("create dir");
    let stray = dir.path().join("notes.txt");
    fs::write(&stray, "not a rotation file").expect("write stray file");

    let replayed = store::replay(dir.path(), 7).expect("replay should succeed");
    assert!(replayed.is_empty(), "expected a non-rotation file to be ignored by replay");

    store::prune(dir.path(), 0).expect("prune should succeed");
    assert!(stray.exists(), "expected the non-rotation file to survive prune untouched");
}

#[test]
fn prune_deletes_a_file_outside_the_retention_window_and_keeps_one_inside() {
    let dir = TempDir::new().expect("tempdir");
    // Deliberately ancient date -- outside any reasonable retention window.
    let old_path = dir.path().join("agent-runs-2000-01-01.jsonl");
    fs::write(&old_path, "").expect("write old rotation file");

    // Today's rotation file, created via a real append so its name matches
    // whatever `day_bucket` computes for "now" without this test hard-coding
    // today's date.
    store::append(dir.path(), &sample_record("agent-0", AgentRunPhase::Started))
        .expect("append should create today's rotation file");

    store::prune(dir.path(), 7).expect("prune should succeed");

    assert!(!old_path.exists(), "expected the old rotation file to be pruned");
    let remaining: Vec<PathBuf> =
        fs::read_dir(dir.path()).expect("read_dir").filter_map(|e| e.ok()).map(|e| e.path()).collect();
    assert_eq!(remaining.len(), 1, "expected exactly today's file to remain, found {remaining:?}");
}

#[test]
fn replay_on_a_nonexistent_directory_returns_an_empty_vec_not_an_error() {
    let base = TempDir::new().expect("tempdir");
    let missing = base.path().join("does-not-exist");
    let replayed = store::replay(&missing, 7).expect("replay on a missing dir should not error");
    assert!(replayed.is_empty());
}

#[test]
fn prompt_digest_is_deterministic_and_sensitive_to_a_one_byte_difference() {
    let a = store::prompt_digest(b"hello world");
    let b = store::prompt_digest(b"hello world");
    let c = store::prompt_digest(b"hello worlD"); // last byte differs
    assert_eq!(a, b, "expected identical input to produce identical digests");
    assert_ne!(a, c, "expected a one-byte difference to change the digest");
}

// ---------------------------------------------------------------------
// Task 2: `agent_log::replay_runs` — folding, restart recovery, minting
// ---------------------------------------------------------------------

#[test]
fn replaying_started_then_completed_yields_completed_with_the_envelope_surviving() {
    let dir = TempDir::new().expect("tempdir");
    let started = sample_record("agent-0", AgentRunPhase::Started);
    let mut completed = sample_record("agent-0", AgentRunPhase::Completed);
    completed.envelope = Some(json!({"is_error": false, "num_turns": 3}));
    store::append(dir.path(), &started).expect("append");
    store::append(dir.path(), &completed).expect("append");

    let (runs, _next_id) = agent_log::replay_runs(dir.path(), 7).expect("replay_runs should succeed");
    assert_eq!(runs.len(), 1);
    let run = runs.get("agent-0").expect("expected agent-0 in the replayed map");
    assert_eq!(run.phase, AgentRunPhase::Completed);
    assert_eq!(run.envelope, completed.envelope);
}

#[test]
fn replaying_started_then_failed_yields_failed_with_the_recorded_detail_intact() {
    let dir = TempDir::new().expect("tempdir");
    let started = sample_record("agent-0", AgentRunPhase::Started);
    let mut failed = sample_record("agent-0", AgentRunPhase::Failed);
    failed.detail = Some("boom: something broke".to_string());
    store::append(dir.path(), &started).expect("append");
    store::append(dir.path(), &failed).expect("append");

    let (runs, _next_id) = agent_log::replay_runs(dir.path(), 7).expect("replay_runs should succeed");
    let run = runs.get("agent-0").expect("expected agent-0 in the replayed map");
    assert_eq!(run.phase, AgentRunPhase::Failed);
    assert_eq!(run.detail.as_deref(), Some("boom: something broke"));
}

#[test]
fn replaying_only_started_the_in_flight_case_yields_interrupted_with_a_nonempty_detail() {
    let dir = TempDir::new().expect("tempdir");
    let started = sample_record("agent-0", AgentRunPhase::Started);
    store::append(dir.path(), &started).expect("append");

    let (runs, _next_id) = agent_log::replay_runs(dir.path(), 7).expect("replay_runs should succeed");
    let run = runs.get("agent-0").expect("expected agent-0 in the replayed map");
    assert_eq!(run.phase, AgentRunPhase::Interrupted, "a still-Started run must be rewritten to Interrupted");
    assert!(
        run.detail.as_ref().is_some_and(|d| !d.is_empty()),
        "expected a non-empty detail explaining the daemon restart, got: {:?}",
        run.detail
    );
}

#[test]
fn two_different_runs_replay_into_two_independent_entries() {
    let dir = TempDir::new().expect("tempdir");
    store::append(dir.path(), &sample_record("agent-0", AgentRunPhase::Completed)).expect("append");
    store::append(dir.path(), &sample_record("agent-1", AgentRunPhase::Failed)).expect("append");

    let (runs, _next_id) = agent_log::replay_runs(dir.path(), 7).expect("replay_runs should succeed");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs.get("agent-0").map(|r| r.phase), Some(AgentRunPhase::Completed));
    assert_eq!(runs.get("agent-1").map(|r| r.phase), Some(AgentRunPhase::Failed));
}

#[test]
fn replaying_a_directory_with_no_files_yields_an_empty_map() {
    let dir = TempDir::new().expect("tempdir");
    let (runs, next_id) = agent_log::replay_runs(dir.path(), 7).expect("replay_runs should succeed");
    assert!(runs.is_empty());
    assert_eq!(next_id, 0);
}

#[test]
fn replaying_a_file_whose_last_line_is_truncated_still_reconstructs_every_complete_earlier_transition() {
    let dir = TempDir::new().expect("tempdir");
    store::append(dir.path(), &sample_record("agent-0", AgentRunPhase::Started)).expect("append");
    store::append(dir.path(), &sample_record("agent-0", AgentRunPhase::Completed)).expect("append");

    let path = only_rotation_file(dir.path());
    let mut file = OpenOptions::new().append(true).open(&path).expect("open for append");
    write!(file, "{{not valid json").expect("write a truncated trailing line");

    let (runs, _next_id) = agent_log::replay_runs(dir.path(), 7)
        .expect("replay_runs should succeed despite the truncated trailing line");
    let run = runs.get("agent-0").expect("expected agent-0 reconstructed despite the truncated line");
    assert_eq!(run.phase, AgentRunPhase::Completed);
}

#[test]
fn next_id_after_replay_is_one_greater_than_the_highest_numeric_suffix_seen() {
    let dir = TempDir::new().expect("tempdir");
    store::append(dir.path(), &sample_record("agent-0", AgentRunPhase::Completed)).expect("append");
    store::append(dir.path(), &sample_record("agent-3", AgentRunPhase::Completed)).expect("append");
    store::append(dir.path(), &sample_record("agent-1", AgentRunPhase::Completed)).expect("append");

    let (_runs, next_id) = agent_log::replay_runs(dir.path(), 7).expect("replay_runs should succeed");
    assert_eq!(next_id, 4, "expected one past the highest replayed suffix (3)");
}

// ---------------------------------------------------------------------
// Task 3: `AiAgentService` — log-before-memory wiring + `from_store`
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_completed_run_leaves_started_and_completed_records_on_disk_and_the_map_is_a_subset() {
    let _guard = SPAWN_GUARD.lock().await;
    let log_dir = TempDir::new().expect("tempdir");
    let scratch_root = TempDir::new().expect("tempdir");
    let runtime = Arc::new(LocalRuntime::new(fixture_path(), "test-key", scratch_root.path()).await);
    let service = AiAgentService::from_store(runtime, log_dir.path(), 7)
        .expect("from_store on a fresh empty directory should succeed");

    let handle = service
        .invoke(json!({
            "__agent": {
                "workflow_id": "agent_ping",
                "body": "Reply with exactly the word: PONG",
                "workflow_dir": workflow_dir_fixture().to_string_lossy(),
                "files": [],
            },
            "params": {}
        }))
        .await
        .expect("invoke should succeed");

    let status = poll_until_terminal(&service, &handle).await;
    assert_eq!(status, RunStatus::Completed);

    let records = store::replay(log_dir.path(), 7).expect("replay should succeed");
    let run_records: Vec<&AgentRunRecord> = records.iter().filter(|r| r.run_id == "agent-0").collect();
    assert!(
        run_records.iter().any(|r| r.phase == AgentRunPhase::Started),
        "expected a Started record on disk, got: {run_records:?}"
    );
    assert!(
        run_records.iter().any(|r| r.phase == AgentRunPhase::Completed),
        "expected a Completed record on disk, got: {run_records:?}"
    );
    assert!(run_records.len() >= 2);

    // Subset check (G-07): every run_id the in-memory map can answer for
    // must also be replayable from disk -- the strongest externally
    // observable proxy for "the map is a strict subset of durable truth",
    // since `states` itself is private to the crate.
    let (replayed, _next_id) = agent_log::replay_runs(log_dir.path(), 7).expect("replay_runs should succeed");
    assert!(replayed.contains_key("agent-0"));
    assert_eq!(service.status(&handle).await, Ok(RunStatus::Completed));
}

#[tokio::test]
async fn from_store_reconstructs_status_and_result_for_a_completed_run_without_spawning_a_child() {
    let log_dir = TempDir::new().expect("tempdir");
    let started = sample_record("agent-0", AgentRunPhase::Started);
    let mut completed = sample_record("agent-0", AgentRunPhase::Completed);
    completed.envelope = Some(json!({"is_error": false, "result_marker": "from-disk"}));
    store::append(log_dir.path(), &started).expect("append");
    store::append(log_dir.path(), &completed).expect("append");

    // A runtime pointing at a nonexistent binary -- if `from_store` ever
    // spawned a child during reconstruction, this would either panic or
    // surface a Failed state instead of the pre-recorded Completed state,
    // since `status()`/`result()` are never called through `invoke()` here.
    let unreachable_scratch = TempDir::new().expect("tempdir");
    let runtime = Arc::new(LocalRuntime::new(
        "/nonexistent/path/should-never-be-spawned",
        "test-key",
        unreachable_scratch.path(),
    )
    .await);
    let service = AiAgentService::from_store(runtime, log_dir.path(), 7)
        .expect("from_store should reconstruct from a directory with only a completed run");

    let handle = RunHandle::new("agent-0");
    assert_eq!(service.status(&handle).await, Ok(RunStatus::Completed));
    let output = service
        .result(&handle)
        .await
        .expect("result should succeed for a reconstructed completed run")
        .expect("result should produce Some output");
    assert_eq!(output.get("result_marker").and_then(|v| v.as_str()), Some("from-disk"));
}

#[tokio::test]
async fn from_store_on_a_directory_with_only_a_started_record_answers_failed_naming_the_interruption() {
    let log_dir = TempDir::new().expect("tempdir");
    store::append(log_dir.path(), &sample_record("agent-0", AgentRunPhase::Started)).expect("append");

    let unreachable_scratch = TempDir::new().expect("tempdir");
    let runtime = Arc::new(LocalRuntime::new(
        "/nonexistent/path/should-never-be-spawned",
        "test-key",
        unreachable_scratch.path(),
    )
    .await);
    let service = AiAgentService::from_store(runtime, log_dir.path(), 7)
        .expect("from_store should reconstruct from a directory with only a Started run");

    let handle = RunHandle::new("agent-0");
    assert_eq!(service.status(&handle).await, Ok(RunStatus::Failed));
    let err = service.result(&handle).await.expect_err("expected Err for an interrupted run");
    assert!(
        err.detail.to_lowercase().contains("interrupt"),
        "expected the error detail to name the interruption, got: {:?}",
        err.detail
    );
}

#[tokio::test]
async fn a_handle_minted_after_from_store_never_collides_with_replayed_history() {
    let _guard = SPAWN_GUARD.lock().await;
    let log_dir = TempDir::new().expect("tempdir");
    store::append(log_dir.path(), &sample_record("agent-0", AgentRunPhase::Completed)).expect("append");
    store::append(log_dir.path(), &sample_record("agent-3", AgentRunPhase::Completed)).expect("append");

    let scratch_root = TempDir::new().expect("tempdir");
    let runtime = Arc::new(LocalRuntime::new(fixture_path(), "test-key", scratch_root.path()).await);
    let service =
        AiAgentService::from_store(runtime, log_dir.path(), 7).expect("from_store should reconstruct");

    let handle = service
        .invoke(json!({
            "__agent": {
                "workflow_id": "agent_ping",
                "body": "hi",
                "workflow_dir": workflow_dir_fixture().to_string_lossy(),
                "files": [],
            },
            "params": {}
        }))
        .await
        .expect("invoke should succeed");
    poll_until_terminal(&service, &handle).await;

    // The minted handle must be agent-4 -- one past the highest replayed
    // suffix (3) -- never colliding with agent-0 or agent-3 already on disk.
    assert_eq!(handle, RunHandle::new("agent-4"));
}

#[tokio::test]
async fn the_written_log_never_contains_the_api_key_and_prompt_bytes_exceeds_any_single_string_field() {
    let _guard = SPAWN_GUARD.lock().await;
    let log_dir = TempDir::new().expect("tempdir");
    let scratch_root = TempDir::new().expect("tempdir");
    let injected_key = "sk-test-super-secret-DO-NOT-LEAK-12345";
    let runtime = Arc::new(LocalRuntime::new(fixture_path(), injected_key, scratch_root.path()).await);
    let service = AiAgentService::from_store(runtime, log_dir.path(), 7)
        .expect("from_store on a fresh directory should succeed");

    // A deliberately long body -- guarantees `prompt_bytes` clearly exceeds
    // every other single string field on the record (workflow_id,
    // prompt_digest, scratch_dir, detail) regardless of how long the OS's
    // own temp-directory path happens to be on this machine.
    let long_body = "Reply with exactly the word: PONG. ".repeat(10);
    let handle = service
        .invoke(json!({
            "__agent": {
                "workflow_id": "agent_ping",
                "body": long_body,
                "workflow_dir": workflow_dir_fixture().to_string_lossy(),
                "files": [],
            },
            "params": {}
        }))
        .await
        .expect("invoke should succeed");
    let status = poll_until_terminal(&service, &handle).await;
    assert_eq!(status, RunStatus::Completed);

    let path = only_rotation_file(log_dir.path());
    let raw = fs::read(&path).expect("read log file");
    let raw_str = String::from_utf8_lossy(&raw);
    assert!(
        !raw_str.contains(injected_key),
        "the durable log must never contain the ANTHROPIC_API_KEY value"
    );

    let records = store::replay(log_dir.path(), 7).expect("replay should succeed");
    let completed = records
        .iter()
        .find(|r| r.run_id == "agent-0" && r.phase == AgentRunPhase::Completed)
        .expect("expected a Completed record for agent-0");
    let longest_string_field = [
        completed.workflow_id.len(),
        completed.prompt_digest.len(),
        completed.scratch_dir.as_ref().map_or(0, |s| s.len()),
        completed.detail.as_ref().map_or(0, |s| s.len()),
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    assert!(
        completed.prompt_bytes > longest_string_field,
        "expected prompt_bytes ({}) to exceed the longest single string field ({}) -- the prompt itself must never be inlined as some other field",
        completed.prompt_bytes,
        longest_string_field
    );
}
