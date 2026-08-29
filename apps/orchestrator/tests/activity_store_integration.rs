//! Integration tests for the on-disk JSONL activity rotation store (D-12).
//! Covers restart recovery, 7-day pruning, and torn-write resilience
//! (RESEARCH Pitfall 6) -- the store must never fail a whole file's replay
//! because of one malformed/truncated line.

use std::fs;
use std::io::Write as _;

use orchestrator::activity::store::{self, ActivityRecord};
use shared::ActivityPhase;
use tempfile::tempdir;

fn sample_record(run_id: &str, phase: ActivityPhase, at_ms: u64) -> ActivityRecord {
    ActivityRecord {
        run_id: run_id.to_string(),
        workflow_id: "set_timer".to_string(),
        client_name: "orchestrator-tui".to_string(),
        phase,
        at_ms,
        detail: None,
    }
}

/// A fresh `replay` over a tempfile dir (simulating a daemon restart, since
/// nothing but the on-disk directory carries over) reproduces every
/// appended record, in order.
#[test]
fn restart_recovery() {
    let dir = tempdir().expect("tempdir");
    store::append(dir.path(), &sample_record("run-1", ActivityPhase::Invoked, 1_000))
        .expect("append invoked");
    store::append(dir.path(), &sample_record("run-1", ActivityPhase::Completed, 1_050))
        .expect("append completed");

    let records = store::replay(dir.path(), 7).expect("replay after restart");

    assert_eq!(records.len(), 2, "both appended records must survive replay");
    assert_eq!(records[0].run_id, "run-1");
    assert_eq!(records[0].phase, ActivityPhase::Invoked);
    assert_eq!(records[1].phase, ActivityPhase::Completed);
}

/// A rotation file older than the retention window is deleted; a file
/// created via a real `append` call (today's real UTC date) is retained.
#[test]
fn prune() {
    let dir = tempdir().expect("tempdir");

    store::append(dir.path(), &sample_record("run-1", ActivityPhase::Invoked, 1_000))
        .expect("append recent record");
    let recent_path = fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .next()
        .expect("today's rotation file should exist after append");

    let old_path = dir.path().join("activities-2020-01-01.jsonl");
    fs::write(&old_path, "{}\n").expect("write old file");

    store::prune(dir.path(), 7).expect("prune");

    assert!(!old_path.exists(), "file older than 7 days should be pruned");
    assert!(recent_path.exists(), "file within 7 days should be retained");
}

/// A truncated/partial JSON line appended after two well-formed lines does
/// not prevent replay of the preceding valid records (Pitfall 6).
#[test]
fn torn_write_recovery() {
    let dir = tempdir().expect("tempdir");
    store::append(dir.path(), &sample_record("run-1", ActivityPhase::Invoked, 1_000))
        .expect("append valid line 1");
    store::append(dir.path(), &sample_record("run-1", ActivityPhase::Completed, 1_050))
        .expect("append valid line 2");

    let today_path = fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .next()
        .expect("today's rotation file should exist after append");

    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&today_path)
        .expect("open today's file for torn-write simulation");
    // Deliberately truncated -- no closing brace, no trailing newline.
    write!(file, "{{\"run_id\":\"run-2\",\"phase\":\"invoked\"").expect("write partial line");

    let records = store::replay(dir.path(), 7).expect("replay tolerates torn write");

    assert_eq!(
        records.len(),
        2,
        "both well-formed lines must replay despite the trailing torn write"
    );
    assert_eq!(records[0].run_id, "run-1");
    assert_eq!(records[1].run_id, "run-1");
}
