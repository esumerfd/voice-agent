//! `process.*` — a generic process-spawn `Service` implementation (SVC-02,
//! D-12). Spawns a configured external command via argv (`tokio::process::
//! Command`, never a shell), captures its stdout, and marshals it back
//! through the identical `invoke -> status -> Completed -> result()` flow
//! every `Service` implements. `ProcessService` never branches on workflow
//! id or command content — `command`/`args` are fixed once at construction
//! from the caller's config, never derived from `invoke()`'s input payload,
//! so no caller-supplied `run` parameter can ever reach process
//! construction (extending the prior plan's path-as-security-surface lesson
//! to "command as security surface", T-03-09).
//!
//! `status()` reports `RunStatus::Completed` immediately after `invoke()`
//! returns — the spawned process has already been awaited to completion
//! inside `invoke()`, mirroring the prior sync-complete-handler precedent
//! for a fast local operation; there is no `Running` window for this
//! generic handler.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

use crate::service::{RunHandle, RunStatus, Service, ServiceError};

/// Result of a completed spawn, keyed by `RunHandle` and consulted by both
/// `status()` and `result()`. Never exposed outside this module.
enum Outcome {
    /// The child exited 0 and its stdout was captured (parsed as JSON, or
    /// wrapped as a plain string if it did not parse).
    Success(Value),
    /// A spawn failure or non-zero exit, carrying the SPECIFIC failure
    /// reason -- this is the only path that threads the full detail string
    /// into `InvokeOutcome.error` (see `dispatch/mod.rs`'s `result()` ->
    /// `ServiceError{detail}` -> `InvokeOutcome.error` path).
    Failure(String),
}

/// Generic process-spawn `Service` impl (SVC-02, D-12). `command`/`args`
/// are fixed once at construction from the caller's config -- never derived
/// from `invoke()`'s input payload -- so no caller-supplied `run` parameter
/// can ever reach process construction. Never assembles `command`/`args`
/// into a shell command line: every invocation goes through `Command`'s own
/// per-argument builder methods (T-03-09).
pub struct ProcessService {
    command: PathBuf,
    args: Vec<String>,
    outcomes: Mutex<HashMap<RunHandle, Outcome>>,
    next_id: Mutex<u64>,
}

impl ProcessService {
    /// `command`/`args` are set once here, by the caller's config
    /// (`main.rs`'s `ORCHESTRATOR_CALENDAR_SCRIPT`-resolved path, or an
    /// equivalent future spawn-based workflow's config) -- never by a
    /// caller-supplied `run` argument.
    pub fn new(command: impl Into<PathBuf>, args: Vec<String>) -> Self {
        Self {
            command: command.into(),
            args,
            outcomes: Mutex::new(HashMap::new()),
            next_id: Mutex::new(0),
        }
    }
}

#[async_trait]
impl Service for ProcessService {
    async fn invoke(&self, _input: Value) -> Result<RunHandle, ServiceError> {
        // The caller-supplied payload is intentionally never inspected --
        // `command`/`args` are fixed at construction (T-03-09).
        let id = {
            let mut next_id = self
                .next_id
                .lock()
                .expect("ProcessService next_id mutex poisoned");
            let id = *next_id;
            *next_id += 1;
            format!("process-{id}")
        };
        let handle = RunHandle::new(id);

        // Each arg is passed to `Command`'s own argv builder individually
        // (never joined into one string and handed to a shell) -- a
        // caller-influenced value can never be reinterpreted as shell
        // syntax.
        let spawn_result = Command::new(&self.command)
            .args(&self.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await;

        let outcome = match spawn_result {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let value = serde_json::from_str(&stdout).unwrap_or(Value::String(stdout));
                Outcome::Success(value)
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                Outcome::Failure(format!(
                    "command `{}` exited with {}: {}",
                    self.command.display(),
                    output.status,
                    stderr
                ))
            }
            Err(err) => Outcome::Failure(format!(
                "failed to spawn command `{}`: {}",
                self.command.display(),
                err
            )),
        };

        self.outcomes
            .lock()
            .expect("ProcessService outcomes mutex poisoned")
            .insert(handle.clone(), outcome);

        Ok(handle)
    }

    async fn status(&self, handle: &RunHandle) -> Result<RunStatus, ServiceError> {
        let outcomes = self
            .outcomes
            .lock()
            .expect("ProcessService outcomes mutex poisoned");
        if outcomes.contains_key(handle) {
            // No Running window (mirrors the prior sync-complete-handler
            // precedent) -- the spawn has already been awaited inside
            // invoke().
            Ok(RunStatus::Completed)
        } else {
            Err(ServiceError {
                detail: "unknown process handle".to_string(),
            })
        }
    }

    async fn cancel(&self, _handle: &RunHandle) -> Result<(), ServiceError> {
        // No-op, matching every other handler in this codebase.
        Ok(())
    }

    async fn result(&self, handle: &RunHandle) -> Result<Option<Value>, ServiceError> {
        let outcomes = self
            .outcomes
            .lock()
            .expect("ProcessService outcomes mutex poisoned");
        match outcomes.get(handle) {
            Some(Outcome::Success(value)) => Ok(Some(value.clone())),
            Some(Outcome::Failure(detail)) => Err(ServiceError {
                detail: detail.clone(),
            }),
            None => Err(ServiceError {
                detail: "unknown process handle".to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use serde_json::json;
    use tempfile::TempDir;

    use crate::service::{RunStatus, Service};

    use super::ProcessService;

    /// Writes `contents` to `dir/name` and marks it executable (mode 0755),
    /// mirroring `timers.rs`'s sibling test fixtures.
    fn write_executable_script(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).expect("failed to write fixture script");
        let mut perms = fs::metadata(&path)
            .expect("failed to read fixture script metadata")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).expect("failed to set fixture script executable bit");
        path
    }

    #[tokio::test]
    async fn json_stdout_round_trips_through_result() {
        let dir = TempDir::new().expect("failed to create tempdir");
        let script = write_executable_script(
            dir.path(),
            "emit_json.sh",
            "#!/usr/bin/env bash\nset -euo pipefail\necho '{\"date\": \"2026-01-01\"}'\n",
        );

        let service = ProcessService::new(script, Vec::new());
        let handle = service
            .invoke(json!({}))
            .await
            .expect("invoke should succeed for a script that exits 0");

        let output = service
            .result(&handle)
            .await
            .expect("result should succeed for a completed run")
            .expect("result should produce Some output for a successful run");

        assert_eq!(
            output,
            json!({"date": "2026-01-01"}),
            "expected the script's JSON stdout to round-trip exactly, got: {output:?}"
        );
    }

    #[tokio::test]
    async fn status_is_completed_immediately_after_invoke() {
        let dir = TempDir::new().expect("failed to create tempdir");
        let script = write_executable_script(
            dir.path(),
            "emit_json.sh",
            "#!/usr/bin/env bash\nset -euo pipefail\necho '{\"ok\": true}'\n",
        );

        let service = ProcessService::new(script, Vec::new());
        let handle = service
            .invoke(json!({}))
            .await
            .expect("invoke should succeed for a script that exits 0");

        assert_eq!(
            service.status(&handle).await,
            Ok(RunStatus::Completed),
            "expected Completed immediately after invoke -- no Running window for this handler"
        );
    }

    #[tokio::test]
    async fn missing_command_is_captured_not_surfaced_immediately() {
        let service = ProcessService::new("/nonexistent/path/to/binary-that-does-not-exist", Vec::new());

        let handle = service
            .invoke(json!({}))
            .await
            .expect("invoke should succeed even when the configured command cannot be spawned");

        let result = service.result(&handle).await;
        assert!(
            result.is_err(),
            "expected result() to surface the spawn failure as an Err, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn non_zero_exit_surfaces_as_error_naming_the_exit_status() {
        let dir = TempDir::new().expect("failed to create tempdir");
        let script = write_executable_script(
            dir.path(),
            "fail.sh",
            "#!/usr/bin/env bash\nexit 3\n",
        );

        let service = ProcessService::new(script, Vec::new());
        let handle = service
            .invoke(json!({}))
            .await
            .expect("invoke should succeed even when the script exits non-zero");

        let error = service
            .result(&handle)
            .await
            .expect_err("expected result() to return Err for a non-zero exit");

        assert!(
            error.detail.contains('3'),
            "expected the error detail to name the exit status (3), got: {error:?}"
        );
    }

    #[tokio::test]
    async fn metacharacter_laden_arg_arrives_at_child_literally() {
        let dir = TempDir::new().expect("failed to create tempdir");
        let script = write_executable_script(
            dir.path(),
            "echo_arg.sh",
            "#!/usr/bin/env bash\nset -euo pipefail\nprintf '%s' \"$1\" > \"$(dirname \"$0\")/received_arg.txt\"\necho '{\"ok\": true}'\n",
        );
        let marker = dir.path().join("should-not-exist");
        let metachar_arg = format!("; touch {} &&", marker.display());

        let service = ProcessService::new(script, vec![metachar_arg.clone()]);
        let handle = service
            .invoke(json!({}))
            .await
            .expect("invoke should succeed");
        service
            .result(&handle)
            .await
            .expect("result should succeed for a completed run");

        assert!(
            !marker.exists(),
            "expected the metacharacter-laden arg to never be shell-interpreted (marker file should not exist)"
        );

        let received = fs::read_to_string(dir.path().join("received_arg.txt"))
            .expect("failed to read the fixture's recorded argv element");
        assert_eq!(
            received, metachar_arg,
            "expected the arg to arrive at the child literally as one argv element"
        );
    }
}
