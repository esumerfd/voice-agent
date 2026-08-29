//! Integration test for `orchestrator log`: tailing the orchestratord
//! daemon's log file must work with **no live daemon connection** (D-01/D-03
//! of the plan) -- this is the load-bearing property the whole command
//! exists for. `log.rs` is included directly from the binary crate's source
//! (mirrors `list_integration.rs`'s `#[path]` trick; there is no `[lib]`
//! target for `orchestrator-cli`) so the pure-function/unit-level tests
//! exercise the exact code `main.rs` calls, while the compiled-binary tests
//! prove the CLI truly never dials `WsOrchestratorClient::connect` before
//! handling `Commands::Log`.

#[path = "../src/log.rs"]
mod log;

use std::fs;
use std::io::{Read, Write as _};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use tempfile::TempDir;

// ---------------------------------------------------------------------
// tail_lines
// ---------------------------------------------------------------------

#[test]
fn tail_lines_yields_the_last_n_lines_with_no_trailing_empty_element() {
    let lines = log::tail_lines("a\nb\nc\n", 2);
    assert_eq!(lines, vec!["b", "c"]);
}

#[test]
fn tail_lines_with_n_greater_than_line_count_yields_every_line() {
    let lines = log::tail_lines("a\nb\nc\n", 10);
    assert_eq!(lines, vec!["a", "b", "c"]);
}

#[test]
fn tail_lines_of_empty_content_yields_nothing() {
    let lines = log::tail_lines("", 5);
    assert!(lines.is_empty(), "expected no lines, got: {lines:?}");
}

#[test]
fn tail_lines_with_n_zero_yields_nothing() {
    let lines = log::tail_lines("a\nb\nc\n", 0);
    assert!(lines.is_empty(), "expected no lines, got: {lines:?}");
}

// ---------------------------------------------------------------------
// resolve_log_path
// ---------------------------------------------------------------------

#[test]
fn resolve_log_path_with_explicit_existing_path_yields_that_path() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let existing = dir.path().join("daemon.log");
    fs::write(&existing, "hello\n").expect("failed to write fixture log");

    let resolved = log::resolve_log_path(Some(&existing), &[])
        .expect("expected Ok for an explicit path that exists");
    assert_eq!(resolved, existing);
}

#[test]
fn resolve_log_path_with_explicit_missing_path_errs_naming_it() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let missing = dir.path().join("does-not-exist.log");

    let err = log::resolve_log_path(Some(&missing), &[])
        .expect_err("expected Err for an explicit path that does not exist");
    assert!(
        err.contains(&missing.display().to_string()),
        "expected error to name the missing path {missing:?}, got: {err}"
    );
}

#[test]
fn resolve_log_path_with_no_explicit_path_falls_through_candidates_in_order() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let missing = dir.path().join("missing.log");
    let existing = dir.path().join("existing.log");
    fs::write(&existing, "hi\n").expect("failed to write fixture log");

    let candidates: Vec<PathBuf> = vec![missing.clone(), existing.clone()];
    let resolved =
        log::resolve_log_path(None, &candidates).expect("expected Ok -- the second candidate exists");
    assert_eq!(resolved, existing);
}

#[test]
fn resolve_log_path_with_no_existing_candidate_errs_naming_both() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let missing_a = dir.path().join("missing_a.log");
    let missing_b = dir.path().join("missing_b.log");

    let candidates: Vec<PathBuf> = vec![missing_a.clone(), missing_b.clone()];
    let err = log::resolve_log_path(None, &candidates)
        .expect_err("expected Err -- neither candidate exists");
    assert!(
        err.contains(&missing_a.display().to_string()),
        "expected error to name {missing_a:?}, got: {err}"
    );
    assert!(
        err.contains(&missing_b.display().to_string()),
        "expected error to name {missing_b:?}, got: {err}"
    );
}

// ---------------------------------------------------------------------
// log::run (in-process, buffer-captured)
// ---------------------------------------------------------------------

#[tokio::test]
async fn run_writes_the_tail_into_the_out_buffer_and_returns_zero() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let path = dir.path().join("daemon.log");
    fs::write(&path, "line1\nline2\nline3\n").expect("failed to write fixture log");

    let mut buf = Vec::new();
    let code = log::run(Some(&path), &[], 2, false, &mut buf)
        .await
        .expect("log::run should not error against a valid file");
    let output = String::from_utf8(buf).expect("captured output was not valid UTF-8");

    assert_eq!(code, 0);
    assert_eq!(output, "line2\nline3\n");
}

#[tokio::test]
async fn run_against_an_unresolvable_path_returns_nonzero_and_writes_nothing() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let missing = dir.path().join("does-not-exist.log");

    let mut buf = Vec::new();
    let code = log::run(Some(&missing), &[], 20, false, &mut buf)
        .await
        .expect("log::run should not error at the io::Result level even on resolution failure");

    assert_ne!(code, 0, "expected a nonzero exit code for an unresolvable path");
    assert!(buf.is_empty(), "expected nothing written to the out buffer, got: {buf:?}");
}

#[tokio::test]
async fn run_with_no_file_resolves_through_injected_candidates_in_d4_order() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let missing = dir.path().join("missing.log");
    let existing = dir.path().join("existing.log");
    fs::write(&existing, "one\ntwo\n").expect("failed to write fixture log");

    let candidates: Vec<PathBuf> = vec![missing, existing];
    let mut buf = Vec::new();
    let code = log::run(None, &candidates, 10, false, &mut buf)
        .await
        .expect("log::run should not error -- the second candidate exists");
    let output = String::from_utf8(buf).expect("captured output was not valid UTF-8");

    assert_eq!(code, 0);
    assert_eq!(output, "one\ntwo\n");
}

// ---------------------------------------------------------------------
// follow_from
// ---------------------------------------------------------------------

#[tokio::test]
async fn follow_from_bounded_poll_emits_only_bytes_appended_past_offset() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let path = dir.path().join("f.log");
    fs::write(&path, "before\n").expect("failed to write fixture log");
    let offset = fs::metadata(&path).expect("failed to stat fixture log").len();

    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("failed to open fixture log for append");
    write!(file, "after\n").expect("failed to append to fixture log");
    drop(file);

    let mut buf = Vec::new();
    log::follow_from(&path, offset, &mut buf, Duration::from_millis(1), Some(1))
        .await
        .expect("follow_from should not error");

    assert_eq!(buf, b"after\n");
}

#[tokio::test]
async fn follow_from_emits_nothing_and_terminates_cleanly_when_file_does_not_grow() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let path = dir.path().join("f.log");
    fs::write(&path, "static\n").expect("failed to write fixture log");
    let offset = fs::metadata(&path).expect("failed to stat fixture log").len();

    let mut buf = Vec::new();
    log::follow_from(&path, offset, &mut buf, Duration::from_millis(1), Some(2))
        .await
        .expect("follow_from should not error");

    assert!(buf.is_empty(), "expected nothing written, got: {buf:?}");
}

#[tokio::test]
async fn follow_from_emits_content_appended_while_the_loop_is_polling() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let path = dir.path().join("f.log");
    fs::write(&path, "start\n").expect("failed to write fixture log");
    let offset = fs::metadata(&path).expect("failed to stat fixture log").len();

    let append_path = path.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&append_path)
            .expect("failed to open fixture log for append");
        writeln!(file, "mid-loop").expect("failed to append mid-loop content");
    });

    let mut buf = Vec::new();
    log::follow_from(&path, offset, &mut buf, Duration::from_millis(10), Some(10))
        .await
        .expect("follow_from should not error");

    let output = String::from_utf8(buf).expect("captured output was not valid UTF-8");
    assert!(
        output.contains("mid-loop"),
        "expected content appended mid-loop to be captured, got: {output:?}"
    );
}

// ---------------------------------------------------------------------
// compiled binary -- proves the daemon-independence property end-to-end
// ---------------------------------------------------------------------

/// Runs the built `orchestrator` binary with the given args, on a
/// blocking-pool thread so this test's own tokio runtime stays free.
/// Mirrors `list_integration.rs::run_orchestrator`.
async fn run_orchestrator(args: &[&str]) -> Output {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_orchestrator"));
        cmd.args(&args);
        cmd.output().expect("failed to run the orchestrator binary")
    })
    .await
    .expect("run_orchestrator's spawn_blocking task panicked")
}

#[tokio::test]
async fn compiled_binary_prints_the_log_tail_and_exits_zero_with_no_daemon_running() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let path = dir.path().join("daemon.log");
    fs::write(&path, "startup ok\nrun_id=1 success=true\n").expect("failed to write fixture log");

    // A port nothing listens on -- proves the log path never dials
    // WsOrchestratorClient::connect.
    let run = run_orchestrator(&[
        "--port",
        "1", // reserved, nothing binds here
        "log",
        "--file",
        path.to_str().expect("tempdir path was not valid UTF-8"),
    ])
    .await;

    let stdout = String::from_utf8(run.stdout).expect("stdout was not valid UTF-8");
    assert!(
        run.status.success(),
        "expected exit 0 with no daemon running, got status {:?}, stdout: {stdout:?}, stderr: {:?}",
        run.status,
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        stdout.contains("run_id=1 success=true"),
        "expected stdout to contain the fixture's log content, got: {stdout:?}"
    );
}

#[tokio::test]
async fn compiled_binary_missing_file_exits_nonzero_and_names_the_path() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let missing = dir.path().join("does-not-exist.log");

    let run = run_orchestrator(&[
        "--port",
        "1",
        "log",
        "--file",
        missing.to_str().expect("tempdir path was not valid UTF-8"),
    ])
    .await;

    assert!(
        !run.status.success(),
        "expected a nonzero exit for a missing log file, got status {:?}",
        run.status
    );
    let stderr = String::from_utf8(run.stderr).expect("stderr was not valid UTF-8");
    assert!(
        stderr.contains(&missing.display().to_string()),
        "expected stderr to name the missing path {missing:?}, got: {stderr:?}"
    );
}

/// Spawns the compiled binary with `log --file <fixture> <flag>` (`flag` is
/// `--follow` or `-f`), lets it run briefly, then kills it. A process that
/// is still alive after the wait (had to be killed, not one that exited on
/// its own) proves the flag parsed and the follow loop actually started;
/// the captured stdout proves the one-shot tail phase still ran before the
/// follow loop began.
async fn assert_flag_starts_following(flag: &'static str) {
    let dir = TempDir::new().expect("failed to create tempdir");
    let path = dir.path().join("f.log");
    fs::write(&path, "hello\n").expect("failed to write fixture log");
    let path_str = path.to_str().expect("tempdir path was not valid UTF-8").to_string();

    let (still_running, stdout) = tokio::task::spawn_blocking(move || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_orchestrator"))
            .args(["--port", "1", "log", "--file", &path_str, flag])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn the orchestrator binary");

        std::thread::sleep(Duration::from_millis(200));
        let still_running = child.try_wait().expect("try_wait failed").is_none();
        child.kill().expect("failed to kill the child process");
        let _ = child.wait();

        let mut stdout = String::new();
        child
            .stdout
            .take()
            .expect("child stdout was not captured")
            .read_to_string(&mut stdout)
            .ok();
        (still_running, stdout)
    })
    .await
    .expect("assert_flag_starts_following's spawn_blocking task panicked");

    assert!(
        still_running,
        "expected the process (flag `{flag}`) to still be running (actively following) after 200ms"
    );
    assert!(
        stdout.contains("hello"),
        "expected the initial tail for flag `{flag}` in stdout, got: {stdout:?}"
    );
}

#[tokio::test]
async fn compiled_binary_accepts_the_follow_long_flag() {
    assert_flag_starts_following("--follow").await;
}

#[tokio::test]
async fn compiled_binary_accepts_the_follow_short_flag() {
    assert_flag_starts_following("-f").await;
}
