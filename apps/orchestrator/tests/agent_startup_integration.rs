//! AI-agent daemon startup preflight tests (06-05, SVC-03, 06-AI-SPEC.md
//! guardrail G-02). Every test in this file targets the pure
//! `orchestrator::startup::preflight_agent` function directly -- none of
//! them starts a daemon, spawns a `claude`/`fake_claude.sh` child process,
//! or touches the network. `preflight_agent` lives in the library (not
//! inline in `bin/orchestratord.rs`) specifically so this file -- which
//! links only the `orchestrator` library crate, never the `orchestratord`
//! binary crate -- can exercise it.

use std::fs;
use std::path::Path;
use std::sync::Mutex;

use orchestrator::startup::preflight_agent;

/// Serializes every test in this file that mutates the process-global
/// `PATH` environment variable. `std::env::set_var`/`remove_var` mutate
/// whole-process state, and `cargo test` runs `#[test]` functions in this
/// binary concurrently by default -- mirrors the `SPAWN_GUARD` convention
/// already established in `runtime/local.rs` and `agent_log_integration.rs`
/// for a different process-global (`FAKE_CLAUDE_*` env vars), applied here
/// to `PATH`.
static PATH_GUARD: Mutex<()> = Mutex::new(());

/// Writes an executable shell script fixture at `path` (mode 0o755) so a
/// test has a real "resolvable, executable" binary to point
/// `preflight_agent` at, without depending on any binary already installed
/// on the host machine.
fn write_executable(path: &Path) {
    fs::write(path, b"#!/bin/sh\nexit 0\n").expect("failed to write fixture binary");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).expect("failed to stat fixture binary").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("failed to chmod fixture binary");
}

#[test]
fn absolute_executable_path_and_non_empty_key_resolves_to_the_same_absolute_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("claude");
    write_executable(&bin);

    let resolved = preflight_agent(&bin, Some("sk-real-key"))
        .expect("expected an absolute, executable path with a non-empty key to succeed");

    assert_eq!(resolved, bin, "expected the absolute input path to be returned as-is");
    assert!(resolved.is_absolute(), "expected the resolved path to be absolute");
}

#[test]
fn present_but_empty_key_is_refused_with_a_message_distinct_from_a_missing_binary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("claude");
    write_executable(&bin);

    let empty_key_err = preflight_agent(&bin, Some("   "))
        .expect_err("expected a present-but-whitespace-only key to be refused");

    let missing_binary_err =
        preflight_agent(&dir.path().join("does-not-exist"), Some("sk-real-key"))
            .expect_err("expected a nonexistent binary path to be refused");

    assert_ne!(
        empty_key_err, missing_binary_err,
        "expected the empty-key refusal message to differ from the missing-binary refusal message"
    );
    assert!(
        empty_key_err.contains("ANTHROPIC_API_KEY"),
        "expected the empty-key message to name the ANTHROPIC_API_KEY variable, got: {empty_key_err}"
    );
    assert!(
        empty_key_err.to_lowercase().contains("empty"),
        "expected the empty-key message to call out emptiness distinctly from an unset key, got: {empty_key_err}"
    );
}

#[test]
fn unset_key_is_refused_naming_the_environment_variable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("claude");
    write_executable(&bin);

    let err = preflight_agent(&bin, None).expect_err("expected an unset key to be refused");

    assert!(
        err.contains("ANTHROPIC_API_KEY"),
        "expected the refusal message to name the ANTHROPIC_API_KEY variable, got: {err}"
    );
}

#[test]
fn nonexistent_path_is_refused_naming_the_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist-claude");

    let err = preflight_agent(&missing, Some("sk-real-key"))
        .expect_err("expected a nonexistent path to be refused");

    assert!(
        err.contains("does-not-exist-claude"),
        "expected the refusal message to name the missing path, got: {err}"
    );
}

#[test]
fn existing_but_non_executable_path_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("claude");
    fs::write(&bin, b"not executable").expect("failed to write non-executable fixture");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(&bin).expect("failed to stat fixture binary").permissions();
    perms.set_mode(0o644);
    fs::set_permissions(&bin, perms).expect("failed to chmod fixture binary");

    let err = preflight_agent(&bin, Some("sk-real-key"))
        .expect_err("expected a non-executable path to be refused");
    assert!(!err.is_empty(), "expected a non-empty refusal message");
}

#[test]
fn bare_name_resolves_against_path_and_returns_an_absolute_path() {
    let _guard = PATH_GUARD.lock().expect("PATH_GUARD poisoned");
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("claude");
    write_executable(&bin);

    let original_path = std::env::var_os("PATH");
    std::env::set_var("PATH", dir.path());

    let result = preflight_agent(Path::new("claude"), Some("sk-real-key"));

    match original_path {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }

    let resolved = result.expect("expected a bare name present on PATH to resolve");
    assert!(resolved.is_absolute(), "expected the PATH-resolved path to be absolute");
    assert_eq!(
        resolved.canonicalize().expect("canonicalize resolved path"),
        bin.canonicalize().expect("canonicalize expected fixture path"),
        "expected the PATH-resolved path to point at the fixture binary"
    );
}

#[test]
fn bare_name_not_on_path_is_refused_rather_than_passed_through() {
    let _guard = PATH_GUARD.lock().expect("PATH_GUARD poisoned");
    let dir = tempfile::tempdir().expect("tempdir");

    let original_path = std::env::var_os("PATH");
    std::env::set_var("PATH", dir.path());

    let result = preflight_agent(Path::new("claude-does-not-exist-anywhere"), Some("sk-real-key"));

    match original_path {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }

    assert!(
        result.is_err(),
        "expected an unresolvable bare name to be refused rather than passed through as-is"
    );
}

#[test]
fn resolved_paths_are_always_absolute_for_both_absolute_input_and_path_resolution() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("claude");
    write_executable(&bin);

    let from_absolute_input =
        preflight_agent(&bin, Some("sk-real-key")).expect("absolute input should resolve");
    assert!(from_absolute_input.is_absolute());

    let _guard = PATH_GUARD.lock().expect("PATH_GUARD poisoned");
    let original_path = std::env::var_os("PATH");
    std::env::set_var("PATH", dir.path());
    let from_path = preflight_agent(Path::new("claude"), Some("sk-real-key"));
    match original_path {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }
    assert!(from_path.expect("PATH resolution should succeed").is_absolute());
}
