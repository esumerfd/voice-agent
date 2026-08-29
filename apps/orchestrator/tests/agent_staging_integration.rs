//! Adversarial staging-containment tests for `LocalRuntime::prepare`
//! (06-02, SVC-03, D-01/D-03; 06-AI-SPEC.md rubric E-03, guardrail G-03).
//! `prepare()` is the only thing standing between an unattended
//! `bypassPermissions` run and an unwanted side effect (06-AI-SPEC.md
//! Section 1b) -- every case below proves a declared dependency either
//! lands at its correct scratch-relative destination or is refused BEFORE
//! any content is copied. No test in this file spawns the real `claude`
//! binary (fixture-only, Tier A per 06-AI-SPEC.md Section 5); the one test
//! that DOES exercise the full handler stack asserts the fixture's
//! argv-capture file was never created, proving no child was spawned for a
//! refused run.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use orchestrator::error::RuntimeError;
use orchestrator::handlers::ai_agent::{AiAgentService, AGENT_HANDLER_KEY};
use orchestrator::runtime::local::{LocalRuntime, SCRATCH_PREFIX, SCRATCH_RETAIN_HOURS};
use orchestrator::runtime::AgentRuntime;
use orchestrator::{dispatch, Registry, RunStatus, Service};
use tempfile::TempDir;

/// Absolute path to the `fake_claude.sh` fixture double. `LocalRuntime::new`
/// always requires SOME binary path; the staging tests in this file never
/// reach `run()`/spawn a child, but the full-handler-level test at the
/// bottom does exercise this fixture to prove a refused run never spawns it.
fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_claude.sh")
}

/// Builds a `LocalRuntime` scoped to a fresh `scratch_root`.
async fn runtime_with_scratch_root(scratch_root: &Path) -> LocalRuntime {
    LocalRuntime::new(fixture_path(), "test-key", scratch_root).await
}

/// Counts every regular file under `dir` (recursively) -- used to assert a
/// refused entry left nothing staged.
fn count_files(dir: &Path) -> usize {
    let mut count = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                count += count_files(&path);
            } else {
                count += 1;
            }
        }
    }
    count
}

/// Writes `contents` to `dir/filename`, creating any intermediate
/// directories the fixture requests -- mirrors `registry_integration.rs`'s
/// fixture-writing convention.
fn write_workflow(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    fs::write(path, contents).expect("failed to write fixture");
}

/// One row of the adversarial staging table (plan Task 1 `<behavior>`).
/// `setup` receives the fresh `workflow_dir` and a sibling `root` scratch
/// area for fixtures that must live OUTSIDE the workflow directory (e.g. the
/// traversal/symlink-escape cases), and returns the absolute `files` entry
/// to declare -- exactly the shape `AgentSpec.files` already carries
/// (loader-anchored, absolute, never validated to exist at load time).
struct Case {
    name: &'static str,
    setup: fn(workflow_dir: &Path, root: &Path) -> PathBuf,
    expected: Expected,
}

enum Expected {
    /// Staging succeeds; the file lands at `dest` (relative to the prepared
    /// scratch directory) with exactly `content`.
    Accepted { dest: &'static str, content: &'static [u8] },
    /// Staging is refused because the SOURCE or DESTINATION resolves
    /// outside its allowed directory.
    PathEscape,
    /// Staging is refused because the declared entry could not be staged
    /// (missing file).
    Staging,
}

fn simple_reference(workflow_dir: &Path, _root: &Path) -> PathBuf {
    let path = workflow_dir.join("reference.md");
    fs::write(&path, b"hello world").expect("failed to write reference.md fixture");
    path
}

fn nested_relative(workflow_dir: &Path, _root: &Path) -> PathBuf {
    let dir = workflow_dir.join("data");
    fs::create_dir_all(&dir).expect("failed to create data dir");
    let path = dir.join("notes.json");
    fs::write(&path, b"{}").expect("failed to write notes.json fixture");
    path
}

fn relative_traversal(workflow_dir: &Path, root: &Path) -> PathBuf {
    let outside_dir = root.join("outside");
    fs::create_dir_all(&outside_dir).expect("failed to create outside dir");
    fs::write(outside_dir.join("id_rsa"), b"private-key-material").expect("failed to write secret fixture");
    // Mirrors how `registry/loader.rs` anchors a relative `../../../.ssh/id_rsa`-style
    // declared entry onto the workflow directory BEFORE `prepare()` ever sees it.
    workflow_dir.join("../outside/id_rsa")
}

fn absolute_outside(_workflow_dir: &Path, _root: &Path) -> PathBuf {
    // A real, always-present absolute path outside any workflow directory --
    // the canonical "declared absolute path outside the workflow directory"
    // adversarial case named explicitly in the plan's `<behavior>`.
    PathBuf::from("/etc/hosts")
}

fn inner_dot_dot_stays_inside(workflow_dir: &Path, _root: &Path) -> PathBuf {
    let sub_dir = workflow_dir.join("sub");
    fs::create_dir_all(&sub_dir).expect("failed to create sub dir");
    fs::write(workflow_dir.join("reference.md"), b"inner").expect("failed to write reference.md fixture");
    // `sub/../reference.md` -- resolves inside the workflow dir despite the
    // `..` component; must be accepted, not refused.
    sub_dir.join("../reference.md")
}

fn missing_file(workflow_dir: &Path, _root: &Path) -> PathBuf {
    workflow_dir.join("does-not-exist.md")
}

fn table_cases() -> Vec<Case> {
    vec![
        Case {
            name: "reference.md declared at <wf>/w.md lands at <scratch>/reference.md byte-identical",
            setup: simple_reference,
            expected: Expected::Accepted {
                dest: "reference.md",
                content: b"hello world",
            },
        },
        Case {
            name: "data/notes.json lands at <scratch>/data/notes.json with intermediate dir created",
            setup: nested_relative,
            expected: Expected::Accepted {
                dest: "data/notes.json",
                content: b"{}",
            },
        },
        Case {
            name: "relative traversal escaping the workflow directory is refused",
            setup: relative_traversal,
            expected: Expected::PathEscape,
        },
        Case {
            name: "declared absolute path outside the workflow directory is refused",
            setup: absolute_outside,
            expected: Expected::PathEscape,
        },
        Case {
            name: "an inner .. component that still resolves inside the workflow directory is accepted",
            setup: inner_dot_dot_stays_inside,
            expected: Expected::Accepted {
                dest: "reference.md",
                content: b"inner",
            },
        },
        Case {
            name: "a missing declared file is refused, never a silent skip",
            setup: missing_file,
            expected: Expected::Staging,
        },
    ]
}

#[cfg(unix)]
fn symlink_inside_pointing_outside(workflow_dir: &Path, root: &Path) -> PathBuf {
    use std::os::unix::fs::symlink;

    let outside_dir = root.join("outside");
    fs::create_dir_all(&outside_dir).expect("failed to create outside dir");
    let secret = outside_dir.join("secret.txt");
    fs::write(&secret, b"private").expect("failed to write secret fixture");

    let link = workflow_dir.join("link.txt");
    symlink(&secret, &link).expect("failed to create symlink");
    link
}

#[cfg(unix)]
fn symlinked_parent_resolving_outside(workflow_dir: &Path, root: &Path) -> PathBuf {
    use std::os::unix::fs::symlink;

    let outside_dir = root.join("outside");
    fs::create_dir_all(&outside_dir).expect("failed to create outside dir");
    fs::write(outside_dir.join("payload.txt"), b"private").expect("failed to write payload fixture");

    let linked_parent = workflow_dir.join("linked");
    symlink(&outside_dir, &linked_parent).expect("failed to create symlinked parent");
    linked_parent.join("payload.txt")
}

#[cfg(unix)]
fn unix_only_cases() -> Vec<Case> {
    vec![
        Case {
            name: "a symlink inside the workflow directory pointing at a file outside it is refused",
            setup: symlink_inside_pointing_outside,
            expected: Expected::PathEscape,
        },
        Case {
            name: "a declared entry under a symlinked parent directory resolving outside is refused",
            setup: symlinked_parent_resolving_outside,
            expected: Expected::PathEscape,
        },
    ]
}

/// Drives the whole adversarial table through `LocalRuntime::prepare` --
/// adding a new case is one row in `table_cases()`/`unix_only_cases()`.
/// Each row gets a FRESH `workflow_dir`/`scratch_root` pair so cases never
/// interfere with each other.
#[tokio::test]
async fn adversarial_path_table() {
    let mut cases = table_cases();
    #[cfg(unix)]
    cases.extend(unix_only_cases());

    for case in cases {
        let root = TempDir::new().expect("failed to create tempdir");
        let workflow_dir = root.path().join("wf");
        fs::create_dir_all(&workflow_dir).expect("failed to create workflow dir");
        let scratch_root = TempDir::new().expect("failed to create scratch tempdir");
        let runtime = runtime_with_scratch_root(scratch_root.path()).await;

        let entry = (case.setup)(&workflow_dir, root.path());

        let result = runtime
            .prepare("run-1", &workflow_dir, std::slice::from_ref(&entry))
            .await;

        match case.expected {
            Expected::Accepted { dest, content } => {
                let prepared = result.unwrap_or_else(|e| {
                    panic!("case {:?}: expected prepare() to succeed, got: {e:?}", case.name)
                });
                let staged = prepared.working_dir.join(dest);
                assert!(
                    staged.is_file(),
                    "case {:?}: expected {staged:?} to exist",
                    case.name
                );
                assert_eq!(
                    fs::read(&staged).unwrap_or_else(|e| panic!(
                        "case {:?}: failed to read staged file: {e}",
                        case.name
                    )),
                    content,
                    "case {:?}: expected byte-identical staged content",
                    case.name
                );
            }
            Expected::PathEscape => {
                assert!(
                    matches!(result, Err(RuntimeError::PathEscape { .. })),
                    "case {:?}: expected RuntimeError::PathEscape, got: {result:?}",
                    case.name
                );
                if let Err(RuntimeError::PathEscape { path, .. }) = &result {
                    assert_eq!(
                        path, &entry,
                        "case {:?}: expected the error to name the offending declared entry",
                        case.name
                    );
                }
                assert_eq!(
                    count_files(scratch_root.path()),
                    0,
                    "case {:?}: expected no file staged into scratch_root for a refused entry",
                    case.name
                );
            }
            Expected::Staging => {
                assert!(
                    matches!(result, Err(RuntimeError::Staging { .. })),
                    "case {:?}: expected RuntimeError::Staging, got: {result:?}",
                    case.name
                );
                if let Err(RuntimeError::Staging { path, .. }) = &result {
                    assert_eq!(
                        path, &entry,
                        "case {:?}: expected the error to name the missing path",
                        case.name
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn empty_files_list_stages_nothing_and_succeeds() {
    let workflow_dir = TempDir::new().expect("failed to create tempdir");
    let scratch_root = TempDir::new().expect("failed to create scratch tempdir");
    let runtime = runtime_with_scratch_root(scratch_root.path()).await;

    let prepared = runtime
        .prepare("run-1", workflow_dir.path(), &[])
        .await
        .expect("prepare should succeed for an empty files list");

    assert_eq!(
        count_files(&prepared.working_dir),
        0,
        "expected nothing staged for an empty files list"
    );
}

/// E-03/G-03: refusal happens BEFORE any child process is spawned. Drives
/// the full stack (`Registry::load` -> `dispatch()` -> `AiAgentService` ->
/// `LocalRuntime::prepare`) with a workflow declaring a traversal entry, and
/// asserts the fixture's argv-capture file was never created -- proving
/// `claude` (the fixture double standing in for it) was never spawned for a
/// refused run.
#[tokio::test]
async fn refusal_happens_before_any_child_process_is_spawned() {
    let workspace_root = TempDir::new().expect("failed to create tempdir");
    let workflows_dir = workspace_root.path().join("workflows");
    fs::create_dir_all(&workflows_dir).expect("failed to create workflows dir");
    let outside_dir = workspace_root.path().join("outside");
    fs::create_dir_all(&outside_dir).expect("failed to create outside dir");
    fs::write(outside_dir.join("secret.txt"), b"private").expect("failed to write secret fixture");

    write_workflow(
        &workflows_dir,
        "leaky.md",
        r#"---
id: leaky
name: Leaky Workflow
service:
  handler: agent.claude
  agent:
    files: ["../outside/secret.txt"]
---
Reply with exactly the word: PONG
"#,
    );

    let (registry, errors) = Registry::load(&workflows_dir);
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let definition = registry
        .lookup("leaky")
        .expect("lookup(\"leaky\") should return the loaded workflow")
        .clone();

    let scratch_root = TempDir::new().expect("failed to create scratch tempdir");
    let argv_file = scratch_root.path().join("argv_capture.txt");
    std::env::set_var("FAKE_CLAUDE_ARGV_FILE", &argv_file);

    let runtime = Arc::new(LocalRuntime::new(fixture_path(), "test-key", scratch_root.path()).await);
    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(AGENT_HANDLER_KEY.to_string(), Box::new(AiAgentService::new(runtime)));

    let outcome = dispatch(&definition, serde_json::Map::new(), &handlers)
        .await
        .expect("dispatch() itself must not error -- a refused staging surfaces as RunStatus::Failed");

    std::env::remove_var("FAKE_CLAUDE_ARGV_FILE");

    assert_eq!(
        outcome.status,
        RunStatus::Failed,
        "expected a refused staging entry to surface RunStatus::Failed, got: {outcome:?}"
    );
    assert!(
        !argv_file.exists(),
        "expected the fixture's argv-capture file to never be created -- no child should be spawned for a refused run"
    );
}

// --- 06-02 Task 2: scratch-directory retention policy (AI-SPEC Section 7) ---

/// Sets `path`'s modification time to `when` -- used to simulate an old
/// scratch directory without waiting real wall-clock time. Opening a
/// directory read-only and calling `set_modified` on the resulting `File`
/// is the standard-library-only way to do this on Unix (no `filetime`
/// dependency needed).
#[cfg(unix)]
fn set_mtime(path: &Path, when: SystemTime) {
    let file = fs::OpenOptions::new()
        .read(true)
        .open(path)
        .expect("failed to open path to set its mtime");
    file.set_modified(when).expect("failed to set mtime");
}

#[tokio::test]
async fn cleanup_of_a_failed_run_leaves_the_scratch_directory_on_disk_untouched() {
    let workflow_dir = TempDir::new().expect("failed to create tempdir");
    let scratch_root = TempDir::new().expect("failed to create scratch tempdir");
    let runtime = runtime_with_scratch_root(scratch_root.path()).await;

    let prepared = runtime
        .prepare("run-1", workflow_dir.path(), &[])
        .await
        .expect("prepare should succeed");

    runtime
        .cleanup(&prepared, true)
        .await
        .expect("cleanup should succeed for a failed run");

    assert!(
        prepared.working_dir.is_dir(),
        "expected the scratch directory to remain on disk after cleanup(failed: true)"
    );
}

#[tokio::test]
async fn cleanup_of_a_clean_run_also_leaves_the_scratch_directory_on_disk() {
    let workflow_dir = TempDir::new().expect("failed to create tempdir");
    let scratch_root = TempDir::new().expect("failed to create scratch tempdir");
    let runtime = runtime_with_scratch_root(scratch_root.path()).await;

    let prepared = runtime
        .prepare("run-1", workflow_dir.path(), &[])
        .await
        .expect("prepare should succeed");

    runtime
        .cleanup(&prepared, false)
        .await
        .expect("cleanup should succeed for a clean run");

    assert!(
        prepared.working_dir.is_dir(),
        "expected a clean run's scratch directory to be retained at completion time, not deleted"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn prune_scratch_deletes_a_directory_older_than_retain_hours_carrying_the_prefix() {
    let scratch_root = TempDir::new().expect("failed to create scratch tempdir");
    let old_dir = scratch_root.path().join(format!("{SCRATCH_PREFIX}old"));
    fs::create_dir_all(&old_dir).expect("failed to create old scratch dir");
    set_mtime(
        &old_dir,
        SystemTime::now() - Duration::from_secs((SCRATCH_RETAIN_HOURS + 1) * 3600),
    );

    LocalRuntime::prune_scratch(scratch_root.path(), SCRATCH_RETAIN_HOURS)
        .expect("prune_scratch should succeed");

    assert!(
        !old_dir.exists(),
        "expected a directory older than retain_hours carrying SCRATCH_PREFIX to be deleted"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn prune_scratch_leaves_a_directory_younger_than_retain_hours_untouched() {
    let scratch_root = TempDir::new().expect("failed to create scratch tempdir");
    let young_dir = scratch_root.path().join(format!("{SCRATCH_PREFIX}young"));
    fs::create_dir_all(&young_dir).expect("failed to create young scratch dir");
    set_mtime(
        &young_dir,
        SystemTime::now() - Duration::from_secs((SCRATCH_RETAIN_HOURS - 1) * 3600),
    );

    LocalRuntime::prune_scratch(scratch_root.path(), SCRATCH_RETAIN_HOURS)
        .expect("prune_scratch should succeed");

    assert!(
        young_dir.is_dir(),
        "expected a directory younger than retain_hours to be left untouched"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn prune_scratch_leaves_an_old_directory_without_the_runtime_prefix_untouched() {
    let scratch_root = TempDir::new().expect("failed to create scratch tempdir");
    let unrelated_dir = scratch_root.path().join("not-ours-old");
    fs::create_dir_all(&unrelated_dir).expect("failed to create unrelated dir");
    set_mtime(
        &unrelated_dir,
        SystemTime::now() - Duration::from_secs((SCRATCH_RETAIN_HOURS + 100) * 3600),
    );

    LocalRuntime::prune_scratch(scratch_root.path(), SCRATCH_RETAIN_HOURS)
        .expect("prune_scratch should succeed");

    assert!(
        unrelated_dir.is_dir(),
        "expected an old directory NOT carrying SCRATCH_PREFIX to be left untouched, even though it is old"
    );
}

#[tokio::test]
async fn prune_scratch_on_a_nonexistent_root_returns_ok() {
    let root = TempDir::new().expect("failed to create tempdir");
    let nonexistent = root.path().join("does-not-exist");

    let result = LocalRuntime::prune_scratch(&nonexistent, SCRATCH_RETAIN_HOURS);

    assert!(
        result.is_ok(),
        "expected prune_scratch on a nonexistent root to return Ok(()), got: {result:?}"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn prune_scratch_never_follows_a_symlinked_entry_out_of_root() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().expect("failed to create tempdir");
    let scratch_root = root.path().join("scratch");
    fs::create_dir_all(&scratch_root).expect("failed to create scratch root");
    let external_dir = root.path().join("external-target");
    fs::create_dir_all(&external_dir).expect("failed to create external target dir");
    fs::write(external_dir.join("keep-me.txt"), b"still here").expect("failed to write marker file");

    // A symlink INSIDE scratch_root, carrying the runtime's own prefix,
    // pointing OUTSIDE scratch_root at `external_dir`. `symlink_metadata`
    // reports this entry's own type as a symlink (never a directory), so
    // `prune_scratch` must skip it outright regardless of age -- it must
    // never dereference the symlink to inspect (let alone delete) whatever
    // it points at.
    let link = scratch_root.join(format!("{SCRATCH_PREFIX}escape-link"));
    symlink(&external_dir, &link).expect("failed to create symlink");

    LocalRuntime::prune_scratch(&scratch_root, SCRATCH_RETAIN_HOURS).expect("prune_scratch should succeed");

    assert!(
        external_dir.join("keep-me.txt").is_file(),
        "expected the external target directory (reached only via a symlink inside root) to still exist after prune_scratch"
    );
}
