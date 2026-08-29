//! `LocalRuntime` — the D-01/D-02 local-subprocess `AgentRuntime`
//! implementation (06-01, SVC-03). Spawns the configured `claude` binary
//! (or, in every test in this repo, `tests/fixtures/fake_claude.sh`) via
//! `tokio::process::Command`, scoped to a fresh `tempfile`-allocated
//! per-run scratch directory. `claude_bin` is a constructor parameter
//! (mirroring `ProcessService::new(command, args)`), so no test in this
//! crate ever spawns the real `claude` binary.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

use crate::error::RuntimeError;

use super::{AgentRuntime, PreparedRun, RuntimeOutcome, RuntimeRequest};

/// Prefix every per-run scratch directory is minted with (06-01), hoisted
/// into a constant here (06-02) so the creator (`prepare`) and the sweeper
/// (`prune_scratch`) can never drift apart -- mirrors this crate's existing
/// "one constant, two call sites" discipline (`definition::DEFAULT_HANDLER`).
pub const SCRATCH_PREFIX: &str = "agent-run-";

/// Minimum age (hours) a CLEAN run's scratch directory is retained before
/// `prune_scratch` is allowed to delete it (06-AI-SPEC.md Section 7's
/// sampling-strategy retention constraint). Resolves the cleanup question
/// 06-CONTEXT.md left to discretion: a flagged/failed run is retained
/// unconditionally (see `cleanup` below); a clean run survives at least
/// this long so the workflow author's own review of the post-run scratch
/// directory against the result narrative -- the only control that catches
/// a self-verification correlated failure (06-AI-SPEC.md Section 1b) --
/// still has evidence to look at. Immediate unconditional deletion at
/// run-completion time is explicitly ruled out.
pub const SCRATCH_RETAIN_HOURS: u64 = 24;

/// Local-subprocess `AgentRuntime` (D-01/D-02/D-05: only the simple,
/// one-shot `claude -p` runtime ships this phase). `scratch_root` is the
/// parent directory every per-run scratch directory is allocated inside
/// (via `tempfile`, never a hand-rolled naming scheme -- race-free unique
/// naming is `tempfile`'s job).
pub struct LocalRuntime {
    claude_bin: PathBuf,
    api_key: String,
    scratch_root: PathBuf,
    /// Cached once at construction (06-04 Task 3) -- see
    /// `capture_claude_version`'s doc comment.
    claude_version: Option<String>,
}

impl LocalRuntime {
    /// `claude_bin` is the injectable binary path (a test passes the
    /// `fake_claude.sh` fixture; production passes the daemon-resolved
    /// absolute `claude` path). `api_key` is provisioned into the child's
    /// environment as `ANTHROPIC_API_KEY`, never logged. `scratch_root` is
    /// the parent directory `prepare()` allocates per-run tempdirs inside.
    ///
    /// Runs `claude_bin --version` exactly once, here, and caches the
    /// result (06-04 Task 3) -- see `capture_claude_version`. Never fails
    /// and never hangs (CR-01, 06-REVIEW.md): a version-lookup failure --
    /// including a bounded-timeout expiry against a hung/corrupted binary
    /// -- degrades to `None`, never a constructor error and never an
    /// indefinite block. `main()` calls this BEFORE the daemon's TCP
    /// listener binds, so an unbounded wait here would make every handler
    /// unreachable, not just `agent.claude` -- see `capture_claude_version`'s
    /// own doc comment for the bounded-timeout mechanism.
    pub async fn new(
        claude_bin: impl Into<PathBuf>,
        api_key: impl Into<String>,
        scratch_root: impl Into<PathBuf>,
    ) -> Self {
        let claude_bin = claude_bin.into();
        let claude_version = capture_claude_version(&claude_bin).await;
        Self {
            claude_bin,
            api_key: api_key.into(),
            scratch_root: scratch_root.into(),
            claude_version,
        }
    }
}

/// Bounded wall-clock ceiling for the one-shot `claude_bin --version` probe
/// (CR-01, 06-REVIEW.md): a hung/corrupted `claude` binary must never block
/// `LocalRuntime::new` -- and therefore daemon startup, since `main()` calls
/// it before the TCP listener binds -- indefinitely. Mirrors the same
/// "never fatal, never hangs" discipline `LocalRuntime::run`'s own
/// `RuntimeRequest::timeout` ceiling already applies to a real run.
const CLAUDE_VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs `claude_bin --version` ONCE (never per invocation -- callers cache
/// the result), never fails, and never hangs (WR-03/CR-01, 06-REVIEW.md):
/// returns the trimmed stdout, or `None` on ANY failure -- binary missing/
/// non-executable, non-zero exit, non-UTF8 output, empty output, OR the
/// bounded `CLAUDE_VERSION_TIMEOUT` ceiling expiring against a hung/
/// corrupted binary. This is the drift tripwire 06-AI-SPEC.md F-05/Section 7
/// names: a future behaviour change in the real CLI is traceable to a
/// specific recorded version string rather than silently blamed on this
/// handler. A version-lookup failure is deliberately NOT fatal --
/// `LocalRuntime::new` never fails because of it -- and, as of CR-01, never
/// hangs because of it either: this doc comment's worst-case behavior claim
/// now matches the bounded-timeout implementation below, closing the gap
/// WR-03 flagged (the pre-CR-01 comment documented every graceful-failure
/// mode but never mentioned a binary that never returns at all).
///
/// Uses `tokio::process::Command` (never the blocking `std::process::
/// Command`) specifically so `tokio::time::timeout` below can actually
/// terminate a hung child: `kill_on_drop(true)` is what makes a dropped
/// timeout future actually kill the process it owns, exactly like
/// `LocalRuntime::run`'s own wall-clock ceiling. Thin wrapper over
/// `capture_claude_version_with_timeout` fixing the ceiling at
/// `CLAUDE_VERSION_TIMEOUT` -- the timeout is a separate parameter purely
/// so a test can inject a short ceiling instead of waiting out the real
/// 5s default, mirroring how `RuntimeRequest::timeout` is injected rather
/// than hard-coded in `LocalRuntime::run`'s own tests.
async fn capture_claude_version(claude_bin: &Path) -> Option<String> {
    capture_claude_version_with_timeout(claude_bin, CLAUDE_VERSION_TIMEOUT).await
}

/// See `capture_claude_version`. `timeout` is factored out as a parameter
/// (CR-01, 06-REVIEW.md) solely for test injection.
async fn capture_claude_version_with_timeout(claude_bin: &Path, timeout: Duration) -> Option<String> {
    let mut cmd = Command::new(claude_bin);
    cmd.arg("--version").stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);

    let child = cmd.spawn().ok()?;
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(_)) | Err(_) => return None,
    };

    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[async_trait]
impl AgentRuntime for LocalRuntime {
    async fn prepare(
        &self,
        run_id: &str,
        workflow_dir: &Path,
        files: &[PathBuf],
    ) -> Result<PreparedRun, RuntimeError> {
        std::fs::create_dir_all(&self.scratch_root).map_err(|e| RuntimeError::Io {
            path: self.scratch_root.clone(),
            detail: e.to_string(),
        })?;

        let dir = tempfile::Builder::new()
            .prefix(SCRATCH_PREFIX)
            .tempdir_in(&self.scratch_root)
            .map_err(|e| RuntimeError::Io {
                path: self.scratch_root.clone(),
                detail: e.to_string(),
            })?;

        // Retain the directory past the `TempDir` guard's drop -- the
        // caller (AiAgentService) owns cleanup via `AgentRuntime::cleanup`,
        // never an implicit drop.
        let working_dir = dir.keep();

        // D-01/D-03 staging (06-02): canonicalize the per-run scratch
        // directory and the workflow directory ONCE each -- canonicalize
        // resolves symlinks, which matters on macOS where the temporary
        // directory itself is typically reached through a symlink
        // (`/tmp` -> `/private/tmp`).
        let scratch_canon = std::fs::canonicalize(&working_dir).map_err(|e| RuntimeError::Staging {
            path: working_dir.clone(),
            detail: e.to_string(),
        })?;
        let wf_canon = std::fs::canonicalize(workflow_dir).map_err(|e| RuntimeError::Staging {
            path: workflow_dir.to_path_buf(),
            detail: e.to_string(),
        })?;

        for entry in files {
            stage_one(entry, &wf_canon, &scratch_canon)?;
        }

        Ok(PreparedRun {
            run_id: run_id.to_string(),
            working_dir,
        })
    }

    async fn run(
        &self,
        prepared: &PreparedRun,
        request: &RuntimeRequest,
    ) -> Result<RuntimeOutcome, RuntimeError> {
        // G-06: this CLI exposes no output-token cap, so a dollar ceiling is
        // the only hard stop a runaway agentic loop has. Resolved to a
        // concrete number BEFORE the argv build below, so the flag can be
        // appended unconditionally rather than behind an `if let Some`.
        let max_budget_usd = request
            .max_budget_usd
            .unwrap_or(crate::definition::DEFAULT_AGENT_MAX_BUDGET_USD);

        // Argv is built in ONE visible place, in a fixed order (06-04 Task
        // 1, E-04/G-04): every element here is either a fixed safety/cost
        // flag or a frontmatter-sourced, author-trusted knob. Caller data
        // (a workflow's runtime `params`) can reach the child through
        // exactly ONE slot -- the `-p` prompt argument, immediately below,
        // where it is already `serde_json`-serialized inside D-06's
        // delimited block by `handlers::ai_agent::compose_prompt`. No other
        // argv element is ever derived from caller input.
        let mut cmd = Command::new(&self.claude_bin);
        cmd.current_dir(&prepared.working_dir) // D-01/D-02: cwd IS the isolation boundary; no --cwd flag exists
            .env("ANTHROPIC_API_KEY", &self.api_key)
            .arg("-p")
            .arg(&request.prompt)
            // `--bare`: forces environment-variable-based authentication
            // and skips ambient hook/plugin/MCP/project-instruction
            // auto-discovery -- matches the locked authentication decision
            // and reinforces D-01's isolation intent for free.
            .arg("--bare")
            // `--output-format json`: the machine-readable envelope is the
            // SOLE channel `Service::result()` sources output from.
            .arg("--output-format")
            .arg("json")
            // `--permission-mode bypassPermissions`: an unattended child has
            // no terminal to answer a tool-approval prompt, so any other
            // mode risks a process that blocks forever.
            .arg("--permission-mode")
            .arg("bypassPermissions")
            // `--max-budget-usd`: ALWAYS present, unconditional -- never
            // behind `if let Some`. This CLI exposes no output-token cap, so
            // a dollar ceiling is the only hard stop a runaway agentic loop
            // has. Never omit it.
            .arg("--max-budget-usd")
            .arg(max_budget_usd.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // 06-04 Task 2 (G-01): makes the `tokio::time::timeout` below
            // actually terminate the child -- when the timeout future is
            // dropped, tokio kills the process it owns. Accepted residual:
            // this kills the direct child only, not a whole process group.
            // Killing the group would need a new dependency or unsafe
            // signal handling for a single-local-user system; the
            // scratch-directory boundary and the budget cap above remain in
            // force for any grandchild that outlives the parent -- recorded
            // as T-06-04's residual, not left implicit.
            .kill_on_drop(true);

        // Frontmatter-sourced, trusted-only knob (Section 4/4b.3) -- never
        // derived from caller `--param` input. Optional by design: no
        // `--model` element appears at all when frontmatter omits it,
        // letting the CLI's own default govern.
        if let Some(model) = &request.model {
            cmd.arg("--model").arg(model);
        }

        // 06-04 Task 2 (G-01): `claude` exposes no timeout flag and no turn
        // cap, and `dispatch()`'s poll loop is documented as unbounded, so
        // this handler must own the ceiling. `spawn()` + `tokio::time::
        // timeout` (never a bare `.output().await`) is what makes that
        // ceiling real: on the elapsed arm, the `child` future is dropped,
        // and `kill_on_drop` above is what actually terminates the process.
        let child = cmd.spawn().map_err(|e| RuntimeError::Spawn {
            binary: self.claude_bin.clone(),
            detail: e.to_string(),
        })?;

        let output = match tokio::time::timeout(request.timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                return Err(RuntimeError::Spawn {
                    binary: self.claude_bin.clone(),
                    detail: e.to_string(),
                })
            }
            Err(_elapsed) => {
                return Err(RuntimeError::Timeout {
                    seconds: request.timeout.as_secs(),
                })
            }
        };

        let raw_json: Value = serde_json::from_slice(&output.stdout).map_err(|e| {
            RuntimeError::Envelope {
                detail: format!(
                    "failed to parse claude stdout as JSON: {e} (stdout: {:?}, stderr: {:?})",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                ),
            }
        })?;

        // `is_error` is read from the parsed envelope, never from the exit
        // code alone (06-AI-SPEC.md Section 3: a well-formed workflow
        // failure inside the agent's own reasoning can surface as
        // `is_error:true` with exit 0). A MISSING `is_error` field (06-04
        // Task 3, G-08) is treated the same as unparseable JSON -- a
        // malformed envelope the model did not produce per contract -- so it
        // surfaces as `RuntimeError::Envelope` and lets the handler's
        // one-retry-then-fail policy govern it, rather than silently
        // defaulting to either outcome here.
        let is_error = raw_json.get("is_error").and_then(|v| v.as_bool()).ok_or_else(|| {
            RuntimeError::Envelope {
                detail: format!(
                    "envelope missing or non-boolean `is_error` field (stdout: {:?})",
                    String::from_utf8_lossy(&output.stdout),
                ),
            }
        })?;
        let result_text = raw_json
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        Ok(RuntimeOutcome {
            is_error,
            result_text,
            raw_json,
            claude_version: self.claude_version.clone(),
        })
    }

    async fn cleanup(&self, _prepared: &PreparedRun, _failed: bool) -> Result<(), RuntimeError> {
        // Documented no-op at run-completion time, for BOTH outcomes
        // (06-02 resolves 06-CONTEXT.md's open cleanup question): deletion
        // is deferred entirely to the age-based `prune_scratch` sweep
        // 06-05 runs at daemon startup, never performed here. A
        // failed/flagged run's evidence must stay inspectable
        // indefinitely until swept, and a clean run must survive
        // SCRATCH_RETAIN_HOURS so the workflow author's own
        // scratch-directory review -- the control that catches a
        // self-verification correlated failure (06-AI-SPEC.md Section
        // 1b) -- still has something to look at; immediate unconditional
        // deletion here would make every offline metric unauditable.
        // `failed` stays part of this method's signature (unused today)
        // so a future container/remote-VM runtime, or a later phase
        // adding unconditional retention tagging, can act on it without a
        // signature change.
        Ok(())
    }
}

impl LocalRuntime {
    /// Deletes every scratch directory directly under `root` whose
    /// modification time is older than `retain_hours` AND whose name
    /// carries `SCRATCH_PREFIX` -- the retention-policy sweep 06-05 runs
    /// once at daemon startup, mirroring how `activity::store::prune` runs
    /// before `ActivityRegistry::from_store`. An associated function (not
    /// a trait method -- `AgentRuntime`'s seam does not need it).
    ///
    /// Deletion therefore only ever targets a directory that is BOTH a
    /// real (non-symlink) direct child of the runtime's own configured
    /// scratch root AND minted by this runtime itself -- never a caller-
    /// or workflow-influenced path (T-06-10).
    ///
    /// A missing/unreadable `root` is treated as "nothing to prune", not
    /// an error, matching `activity::store::prune`'s missing-directory
    /// convention. An individual entry's removal error is ignored the
    /// same way -- one unremovable directory must never abort the sweep
    /// for every other entry.
    pub fn prune_scratch(root: &Path, retain_hours: u64) -> std::io::Result<()> {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(_) => return Ok(()),
        };

        let now = std::time::SystemTime::now();
        let retain = std::time::Duration::from_secs(retain_hours.saturating_mul(3600));

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();

            // `symlink_metadata` (never `metadata`, which follows a
            // symlink) is what refuses to follow a symlink out of `root`:
            // an entry that is itself a symlink is skipped outright,
            // without ever inspecting or touching its target.
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !meta.is_dir() {
                continue;
            }

            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with(SCRATCH_PREFIX) {
                continue;
            }

            let modified = match meta.modified() {
                Ok(m) => m,
                Err(_) => continue,
            };
            // A `modified` time in the future (clock skew) is treated as
            // fresh, never pruned -- `duration_since` returning `Err` is
            // exactly that case.
            if let Ok(age) = now.duration_since(modified) {
                if age > retain {
                    let _ = std::fs::remove_dir_all(&path);
                }
            }
        }

        Ok(())
    }
}

/// Stages one declared file dependency (D-03: satisfies D-01/D-03; the
/// containment check satisfies guardrail G-03). Both ends of the boundary
/// are checked: the SOURCE side stops a workflow author's declared entry
/// from reaching outside their own directory tree, the DESTINATION side
/// stops a crafted relative path (or a pre-existing symlink inside the
/// scratch tree) from writing outside the per-run scratch directory.
/// `Path::starts_with` (component-wise) is used for every containment
/// check -- never a string-prefix comparison, which would wrongly accept a
/// sibling directory whose name merely starts with the same characters
/// (e.g. `/tmp/agent-run-1-evil` "starting with" `/tmp/agent-run-1`).
fn stage_one(entry: &Path, wf_canon: &Path, scratch_canon: &Path) -> Result<(), RuntimeError> {
    // A canonicalize failure means the dependency doesn't exist (or isn't
    // reachable) -- fail loudly, naming the declared path, never silently
    // skip it.
    let src_canon = std::fs::canonicalize(entry).map_err(|e| RuntimeError::Staging {
        path: entry.to_path_buf(),
        detail: e.to_string(),
    })?;

    // Source-side containment: the declared entry must resolve (following
    // symlinks) inside the workflow file's own directory tree.
    if !src_canon.starts_with(wf_canon) {
        return Err(RuntimeError::PathEscape {
            path: entry.to_path_buf(),
            reason: "source resolves outside the workflow directory".to_string(),
        });
    }

    // Destination: the entry's path relative to the workflow directory,
    // joined onto the per-run scratch directory -- creating any
    // intermediate directories a nested entry (e.g. `data/notes.json`)
    // needs.
    let relative = src_canon
        .strip_prefix(wf_canon)
        .expect("src_canon.starts_with(wf_canon) was just checked above");
    let destination = scratch_canon.join(relative);
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RuntimeError::Staging {
            path: destination.clone(),
            detail: e.to_string(),
        })?;
    }
    std::fs::copy(&src_canon, &destination).map_err(|e| RuntimeError::Staging {
        path: entry.to_path_buf(),
        detail: e.to_string(),
    })?;

    // Belt-and-braces destination check: catches a pre-existing symlink
    // inside the scratch tree that would otherwise let the copy land
    // outside `scratch_canon` despite the source-side check having passed.
    // On failure the copied file is removed so a refused entry never
    // leaves a staged artifact behind.
    let dest_canon = std::fs::canonicalize(&destination).map_err(|e| RuntimeError::Staging {
        path: destination.clone(),
        detail: e.to_string(),
    })?;
    if !dest_canon.starts_with(scratch_canon) {
        let _ = std::fs::remove_file(&destination);
        return Err(RuntimeError::PathEscape {
            path: entry.to_path_buf(),
            reason: "destination resolves outside the scratch directory".to_string(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use tempfile::TempDir;
    use tokio::sync::Mutex;

    use super::{AgentRuntime, LocalRuntime, RuntimeRequest};

    /// Serializes every test in this module that spawns the fixture double.
    /// `std::env::set_var`/`remove_var` mutate whole-process state; without
    /// this guard, a concurrently-running `#[tokio::test]` in this same
    /// module could spawn a child that inherits another test's transiently
    /// set `FAKE_CLAUDE_*` env var, corrupting that test's fixture-file
    /// assertion (test-isolation hazard, not a production code issue --
    /// `LocalRuntime` itself never touches process-global env beyond the
    /// child's own `ANTHROPIC_API_KEY`).
    static SPAWN_GUARD: Mutex<()> = Mutex::const_new(());

    /// Absolute path to the `fake_claude.sh` fixture double, resolved
    /// relative to this crate's manifest directory so the test does not
    /// depend on the process's current working directory.
    fn fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_claude.sh")
    }

    /// A real, always-existing directory used as `workflow_dir` for tests
    /// in this module that declare no `files` -- `prepare()` canonicalizes
    /// `workflow_dir` unconditionally (06-02 staging), so it must resolve
    /// to a real directory even when the files list is empty. The adversarial
    /// staging behaviors themselves are exercised in
    /// `tests/agent_staging_integration.rs`, not here.
    fn workflow_dir_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[tokio::test]
    async fn prepare_allocates_a_fresh_directory_inside_scratch_root() {
        // SPAWN_GUARD: `LocalRuntime::new` now spawns the fixture for its
        // `--version` capture (CR-01, 06-REVIEW.md) -- the fixture's
        // `--version` branch reads FAKE_CLAUDE_VERSION_* env vars, so any
        // test constructing a `LocalRuntime` must be serialized against a
        // concurrently-running test that mutates those vars.
        let _guard = SPAWN_GUARD.lock().await;
        let scratch_root = TempDir::new().expect("failed to create tempdir");
        let runtime = LocalRuntime::new(fixture_path(), "test-key", scratch_root.path()).await;

        let prepared = runtime
            .prepare("run-1", &workflow_dir_fixture(), &[])
            .await
            .expect("prepare should succeed");

        assert!(
            prepared.working_dir.starts_with(scratch_root.path()),
            "expected the prepared working_dir ({:?}) to be inside scratch_root ({:?})",
            prepared.working_dir,
            scratch_root.path()
        );
        assert!(
            prepared.working_dir.is_dir(),
            "expected the prepared working_dir to actually exist on disk"
        );
    }

    #[tokio::test]
    async fn prepare_allocates_distinct_directories_for_concurrent_runs() {
        // SPAWN_GUARD: see the comment in
        // `prepare_allocates_a_fresh_directory_inside_scratch_root` above.
        let _guard = SPAWN_GUARD.lock().await;
        let scratch_root = TempDir::new().expect("failed to create tempdir");
        let runtime = LocalRuntime::new(fixture_path(), "test-key", scratch_root.path()).await;

        let first = runtime
            .prepare("run-1", &workflow_dir_fixture(), &[])
            .await
            .expect("prepare should succeed");
        let second = runtime
            .prepare("run-2", &workflow_dir_fixture(), &[])
            .await
            .expect("prepare should succeed");

        assert_ne!(
            first.working_dir, second.working_dir,
            "expected tempfile's race-free naming to allocate distinct directories"
        );
    }

    #[tokio::test]
    async fn run_spawns_the_fixture_and_parses_the_default_canned_envelope() {
        let _guard = SPAWN_GUARD.lock().await;
        let scratch_root = TempDir::new().expect("failed to create tempdir");
        let runtime = LocalRuntime::new(fixture_path(), "test-key", scratch_root.path()).await;
        let prepared = runtime
            .prepare("run-1", &workflow_dir_fixture(), &[])
            .await
            .expect("prepare should succeed");

        let outcome = runtime
            .run(
                &prepared,
                &RuntimeRequest {
                    prompt: "hi".to_string(),
                    model: None,
                    max_budget_usd: None,
                    timeout: Duration::from_secs(5),
                },
            )
            .await
            .expect("run should succeed against the fixture double");

        assert!(!outcome.is_error, "expected the default canned envelope's is_error:false");
        assert_eq!(outcome.result_text, "PONG");
    }

    #[tokio::test]
    async fn run_spawns_child_with_cwd_equal_to_the_prepared_scratch_directory() {
        let _guard = SPAWN_GUARD.lock().await;
        let scratch_root = TempDir::new().expect("failed to create tempdir");
        let runtime = LocalRuntime::new(fixture_path(), "test-key", scratch_root.path()).await;
        let prepared = runtime
            .prepare("run-1", &workflow_dir_fixture(), &[])
            .await
            .expect("prepare should succeed");

        let cwd_file = prepared.working_dir.join("cwd_capture.txt");
        std::env::set_var("FAKE_CLAUDE_CWD_FILE", &cwd_file);

        runtime
            .run(
                &prepared,
                &RuntimeRequest {
                    prompt: "hi".to_string(),
                    model: None,
                    max_budget_usd: None,
                    timeout: Duration::from_secs(5),
                },
            )
            .await
            .expect("run should succeed against the fixture double");

        std::env::remove_var("FAKE_CLAUDE_CWD_FILE");

        let recorded_cwd = std::fs::read_to_string(&cwd_file)
            .expect("fixture should have recorded its cwd")
            .trim()
            .to_string();
        let expected = prepared
            .working_dir
            .canonicalize()
            .expect("failed to canonicalize expected working_dir");
        let actual = PathBuf::from(&recorded_cwd)
            .canonicalize()
            .expect("failed to canonicalize recorded cwd");

        assert_eq!(
            actual, expected,
            "expected the spawned child's cwd to equal the per-run scratch directory, not the test process's own CWD"
        );
    }

    #[tokio::test]
    async fn run_maps_spawn_failure_to_runtime_error_spawn() {
        let _guard = SPAWN_GUARD.lock().await;
        let scratch_root = TempDir::new().expect("failed to create tempdir");
        let runtime = LocalRuntime::new(
            "/nonexistent/path/to/binary-that-does-not-exist",
            "test-key",
            scratch_root.path(),
        )
        .await;
        let prepared = runtime
            .prepare("run-1", &workflow_dir_fixture(), &[])
            .await
            .expect("prepare should succeed");

        let result = runtime
            .run(
                &prepared,
                &RuntimeRequest {
                    prompt: "hi".to_string(),
                    model: None,
                    max_budget_usd: None,
                    timeout: Duration::from_secs(5),
                },
            )
            .await;

        assert!(
            matches!(result, Err(crate::error::RuntimeError::Spawn { .. })),
            "expected a non-spawnable binary to surface RuntimeError::Spawn, got: {result:?}"
        );
    }

    // -----------------------------------------------------------------
    // 06-04 Task 2: wall-clock ceiling
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn run_kills_the_child_and_returns_timeout_when_it_exceeds_the_injected_ceiling() {
        let _guard = SPAWN_GUARD.lock().await;
        let scratch_root = TempDir::new().expect("failed to create tempdir");
        let runtime = LocalRuntime::new(fixture_path(), "test-key", scratch_root.path()).await;
        let prepared = runtime
            .prepare("run-1", &workflow_dir_fixture(), &[])
            .await
            .expect("prepare should succeed");

        // Written only after the fixture's FULL sleep elapses -- a merely
        // abandoned (not actually killed) child would still write it once
        // its original 2000ms sleep eventually finished.
        let marker = prepared.working_dir.join("post_sleep_marker.txt");
        std::env::set_var("FAKE_CLAUDE_SLEEP_MS", "2000");
        std::env::set_var("FAKE_CLAUDE_MARKER_FILE", &marker);

        let started = Instant::now();
        let result = runtime
            .run(
                &prepared,
                &RuntimeRequest {
                    prompt: "hi".to_string(),
                    model: None,
                    max_budget_usd: None,
                    timeout: Duration::from_millis(200),
                },
            )
            .await;
        let elapsed = started.elapsed();

        // Wait out the rest of the fixture's original 2000ms sleep window
        // before checking the marker -- proves absence, not just "hasn't
        // happened yet".
        let remaining = Duration::from_millis(2200).saturating_sub(elapsed);
        tokio::time::sleep(remaining).await;

        std::env::remove_var("FAKE_CLAUDE_SLEEP_MS");
        std::env::remove_var("FAKE_CLAUDE_MARKER_FILE");

        assert!(
            matches!(result, Err(crate::error::RuntimeError::Timeout { .. })),
            "expected RuntimeError::Timeout, got: {result:?}"
        );
        assert!(
            elapsed < Duration::from_millis(1000),
            "expected the run to fail near the 200ms ceiling, not the fixture's 2000ms sleep, took {elapsed:?}"
        );
        assert!(
            !marker.exists(),
            "expected the killed child to never reach the post-sleep marker write -- proves it was actually terminated, not merely abandoned"
        );
    }

    #[tokio::test]
    async fn run_returns_envelope_error_when_stdout_is_not_valid_json() {
        let _guard = SPAWN_GUARD.lock().await;
        let scratch_root = TempDir::new().expect("failed to create tempdir");
        let runtime = LocalRuntime::new(fixture_path(), "test-key", scratch_root.path()).await;
        let prepared = runtime
            .prepare("run-1", &workflow_dir_fixture(), &[])
            .await
            .expect("prepare should succeed");

        let response_file = scratch_root.path().join("response.txt");
        std::fs::write(&response_file, b"not json at all").expect("failed to write response fixture");
        std::env::set_var("FAKE_CLAUDE_RESPONSE_FILE", &response_file);

        let result = runtime
            .run(
                &prepared,
                &RuntimeRequest {
                    prompt: "hi".to_string(),
                    model: None,
                    max_budget_usd: None,
                    timeout: Duration::from_secs(5),
                },
            )
            .await;

        std::env::remove_var("FAKE_CLAUDE_RESPONSE_FILE");

        assert!(
            matches!(result, Err(crate::error::RuntimeError::Envelope { .. })),
            "expected invalid-JSON stdout to surface RuntimeError::Envelope, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn run_returns_envelope_error_when_is_error_field_is_missing() {
        let _guard = SPAWN_GUARD.lock().await;
        let scratch_root = TempDir::new().expect("failed to create tempdir");
        let runtime = LocalRuntime::new(fixture_path(), "test-key", scratch_root.path()).await;
        let prepared = runtime
            .prepare("run-1", &workflow_dir_fixture(), &[])
            .await
            .expect("prepare should succeed");

        let response_file = scratch_root.path().join("response.txt");
        std::fs::write(&response_file, br#"{"result":"no is_error field here"}"#)
            .expect("failed to write response fixture");
        std::env::set_var("FAKE_CLAUDE_RESPONSE_FILE", &response_file);

        let result = runtime
            .run(
                &prepared,
                &RuntimeRequest {
                    prompt: "hi".to_string(),
                    model: None,
                    max_budget_usd: None,
                    timeout: Duration::from_secs(5),
                },
            )
            .await;

        std::env::remove_var("FAKE_CLAUDE_RESPONSE_FILE");

        assert!(
            matches!(result, Err(crate::error::RuntimeError::Envelope { .. })),
            "expected a valid-JSON envelope missing `is_error` to surface RuntimeError::Envelope, got: {result:?}"
        );
    }

    // -----------------------------------------------------------------
    // 06-04 Task 3: cached `--version` capture
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn capture_claude_version_returns_none_for_a_nonexistent_binary() {
        assert_eq!(
            super::capture_claude_version(std::path::Path::new(
                "/nonexistent/path/to/binary-that-does-not-exist"
            ))
            .await,
            None
        );
    }

    #[tokio::test]
    async fn capture_claude_version_returns_the_trimmed_version_string_for_the_fixture() {
        // SPAWN_GUARD IS needed here (unlike before CR-01, 06-REVIEW.md):
        // the fixture's `--version` branch now also reads
        // FAKE_CLAUDE_VERSION_SLEEP_MS/FAKE_CLAUDE_VERSION_MARKER_FILE (see
        // fake_claude.sh), so a concurrently-running test that sets those
        // vars (e.g. the hung-binary test below) could otherwise leak into
        // this call.
        let _guard = SPAWN_GUARD.lock().await;
        let version = super::capture_claude_version(&fixture_path()).await;
        assert_eq!(version.as_deref(), Some("2.1.226 (Claude Code)"));
    }

    // -----------------------------------------------------------------
    // CR-01 (06-REVIEW.md): `capture_claude_version` must be bounded --
    // a hung/corrupted `--version` invocation must never block
    // `LocalRuntime::new` (and therefore daemon startup) indefinitely.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn capture_claude_version_returns_none_and_stays_bounded_when_the_binary_hangs() {
        let _guard = SPAWN_GUARD.lock().await;
        // FAKE_CLAUDE_VERSION_SLEEP_MS (2000ms) deliberately exceeds the
        // INJECTED timeout (200ms) below -- `capture_claude_version_with_timeout`
        // lets this test prove the bounded-timeout mechanism fires quickly
        // without waiting out the real 5s `CLAUDE_VERSION_TIMEOUT` default.
        let marker = std::env::temp_dir().join(format!(
            "fake-claude-version-marker-{}",
            std::process::id()
        ));
        std::env::set_var("FAKE_CLAUDE_VERSION_SLEEP_MS", "2000");
        std::env::set_var("FAKE_CLAUDE_VERSION_MARKER_FILE", &marker);
        let _ = std::fs::remove_file(&marker);

        let started = Instant::now();
        let version =
            super::capture_claude_version_with_timeout(&fixture_path(), Duration::from_millis(200)).await;
        let elapsed = started.elapsed();

        // Wait out the rest of the fixture's original 2000ms sleep window
        // before checking the marker -- proves the child was actually
        // killed, not merely abandoned (mirrors `run_kills_the_child_and_
        // returns_timeout_when_it_exceeds_the_injected_ceiling` above).
        let remaining = Duration::from_millis(2200).saturating_sub(elapsed);
        tokio::time::sleep(remaining).await;

        std::env::remove_var("FAKE_CLAUDE_VERSION_SLEEP_MS");
        std::env::remove_var("FAKE_CLAUDE_VERSION_MARKER_FILE");
        let marker_existed = marker.exists();
        let _ = std::fs::remove_file(&marker);

        assert_eq!(version, None, "expected a hung --version probe to degrade to None, not fatal");
        assert!(
            elapsed < Duration::from_millis(1000),
            "expected capture_claude_version_with_timeout to bound the hung binary well under its 2000ms sleep, took {elapsed:?}"
        );
        assert!(
            !marker_existed,
            "expected the killed --version child to never reach its post-sleep marker write -- proves it was actually terminated, not merely abandoned"
        );
    }

    #[tokio::test]
    async fn run_populates_claude_version_on_the_outcome_from_the_cached_value() {
        let _guard = SPAWN_GUARD.lock().await;
        let scratch_root = TempDir::new().expect("failed to create tempdir");
        let runtime = LocalRuntime::new(fixture_path(), "test-key", scratch_root.path()).await;
        let prepared = runtime
            .prepare("run-1", &workflow_dir_fixture(), &[])
            .await
            .expect("prepare should succeed");

        let outcome = runtime
            .run(
                &prepared,
                &RuntimeRequest {
                    prompt: "hi".to_string(),
                    model: None,
                    max_budget_usd: None,
                    timeout: Duration::from_secs(5),
                },
            )
            .await
            .expect("run should succeed against the fixture double");

        assert_eq!(
            outcome.claude_version.as_deref(),
            Some("2.1.226 (Claude Code)"),
            "expected the cached version captured at construction to be threaded onto the outcome"
        );
    }
}
