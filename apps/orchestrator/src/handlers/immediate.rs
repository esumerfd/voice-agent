//! `action.immediate` -- the generic "run now" fallback `Service` impl.
//! Detects `dispatch()`'s server-constructed envelope (quick task 260720-s7i,
//! plan 01) and branches two ways.
//!
//! Envelope present (`ENVELOPE_COMMAND_KEY` at the top level): spawns the
//! configured command via `tokio::process::Command` -- argv-only, never a
//! shell -- piping the envelope's `params` to the child's stdin as JSON,
//! awaiting completion, and storing a success/failure `Outcome` mirroring
//! `ProcessService` (parse-stdout-as-JSON-or-fallback-to-string on success;
//! a spawn/non-zero-exit failure carries the specific detail string). This
//! handler trusts the envelope because `dispatch()` (plan 01) is the SOLE
//! constructor/stripper of the reserved `__command`/`__args` namespace -- a
//! caller can never forge one on a command-less workflow, nor override a
//! configured one (T-s7i-04).
//!
//! Envelope absent: falls back to the original run-now behavior, echoing
//! the caller's payload back in a structured JSON object -- no process is
//! spawned.
//!
//! Registered under the `DEFAULT_HANDLER` constant (`definition/mod.rs`) so
//! a handler-less workflow (loader.rs's absent/empty `service.handler`
//! defaulting) resolves to it through the existing registry lookup +
//! dispatch poll loop with zero special-casing.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::service::{RunHandle, RunStatus, Service, ServiceError};
use crate::{ENVELOPE_ARGS_KEY, ENVELOPE_COMMAND_KEY};

/// Result of a completed invoke, keyed by `RunHandle` and consulted by both
/// `status()` and `result()`. Mirrors `ProcessService`'s `Outcome` shape.
/// Never exposed outside this module.
enum Outcome {
    /// Either the spawn branch's exit-0 child (stdout parsed as JSON, or
    /// wrapped as a plain string if it did not parse) or the echo branch's
    /// structured echo of the caller payload.
    Success(Value),
    /// A spawn failure or non-zero exit, carrying the SPECIFIC failure
    /// reason -- the only path threading a detail string into
    /// `InvokeOutcome.error` via `result()`'s `Err(ServiceError)`.
    Failure(String),
}

/// The run-now `Service` impl (SVC-02-style spawn ADDED to the original
/// minimal fallback). `invoke()` mints `immediate-{n}` via a
/// `Mutex`-guarded counter (mirroring `TimerService`'s `timer-{n}`), and the
/// run is `Completed` immediately after invoke -- there is no `Running`
/// window for either branch.
pub struct ImmediateService {
    outcomes: Mutex<HashMap<RunHandle, Outcome>>,
    next_id: Mutex<u64>,
}

impl ImmediateService {
    pub fn new() -> Self {
        Self {
            outcomes: Mutex::new(HashMap::new()),
            next_id: Mutex::new(0),
        }
    }
}

impl Default for ImmediateService {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Service for ImmediateService {
    async fn invoke(&self, input: serde_json::Value) -> Result<RunHandle, ServiceError> {
        let id = {
            let mut next_id = self
                .next_id
                .lock()
                .expect("ImmediateService next_id mutex poisoned");
            let id = *next_id;
            *next_id += 1;
            format!("immediate-{id}")
        };
        let handle = RunHandle::new(id);

        let input_obj = input.as_object();
        let command = input_obj
            .and_then(|o| o.get(ENVELOPE_COMMAND_KEY))
            .and_then(|v| v.as_str());

        let outcome = match command {
            Some(cmd) => {
                let args: Vec<String> = input_obj
                    .and_then(|o| o.get(ENVELOPE_ARGS_KEY))
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let params = input_obj
                    .and_then(|o| o.get("params"))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let params_bytes = serde_json::to_vec(&params).unwrap_or_else(|_| b"{}".to_vec());

                // Pre-spawn `[debug]` line: command/args rendered with
                // `{:?}` debug-quoting (same log-forging defense
                // `format_run_log_line` uses, T-s7i-05) -- a
                // newline-carrying author-controlled path can never forge
                // an extra log line.
                eprintln!("[debug] action.immediate: spawning command={cmd:?} args={args:?}");

                let started = Instant::now();
                // Each arg is passed to `Command`'s own argv builder
                // individually (never joined into one string and handed to
                // a shell) -- mirrors `ProcessService` (T-s7i-04).
                let spawn_result = Command::new(cmd)
                    .args(&args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn();

                match spawn_result {
                    Ok(mut child) => {
                        if let Some(mut stdin) = child.stdin.take() {
                            // A write failure (e.g. a child that never
                            // reads stdin) is non-fatal -- drop stdin and
                            // still await output (T-s7i-06, accepted risk).
                            let _ = stdin.write_all(&params_bytes).await;
                            drop(stdin); // signal EOF to the child
                        }

                        match child.wait_with_output().await {
                            Ok(output) => {
                                let elapsed_ms = started.elapsed().as_millis();
                                eprintln!(
                                    "[debug] action.immediate: exited command={cmd:?} status={} stdout_bytes={} stderr_bytes={} elapsed_ms={elapsed_ms}",
                                    output.status,
                                    output.stdout.len(),
                                    output.stderr.len()
                                );
                                if output.status.success() {
                                    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                                    let value =
                                        serde_json::from_str(&stdout).unwrap_or(Value::String(stdout));
                                    Outcome::Success(value)
                                } else {
                                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                                    Outcome::Failure(format!(
                                        "command `{cmd}` exited with {}: {stderr}",
                                        output.status
                                    ))
                                }
                            }
                            Err(err) => {
                                Outcome::Failure(format!("failed to await command `{cmd}`: {err}"))
                            }
                        }
                    }
                    Err(err) => Outcome::Failure(format!("failed to spawn command `{cmd}`: {err}")),
                }
            }
            None => {
                // No debug line for the echo branch -- nothing is spawned.
                // Structured JSON only (D-01) -- never a spoken-language
                // string.
                Outcome::Success(json!({"status": "completed", "params": input}))
            }
        };

        self.outcomes
            .lock()
            .expect("ImmediateService outcomes mutex poisoned")
            .insert(handle.clone(), outcome);

        Ok(handle)
    }

    async fn status(&self, handle: &RunHandle) -> Result<RunStatus, ServiceError> {
        let outcomes = self
            .outcomes
            .lock()
            .expect("ImmediateService outcomes mutex poisoned");
        if outcomes.contains_key(handle) {
            // No Running window for either branch -- the spawn (if any) has
            // already been awaited inside invoke().
            Ok(RunStatus::Completed)
        } else {
            Err(ServiceError {
                detail: "unknown immediate handle".to_string(),
            })
        }
    }

    async fn cancel(&self, _handle: &RunHandle) -> Result<(), ServiceError> {
        // No-op, matching every other handler in this codebase.
        Ok(())
    }

    async fn result(&self, handle: &RunHandle) -> Result<Option<serde_json::Value>, ServiceError> {
        let outcomes = self
            .outcomes
            .lock()
            .expect("ImmediateService outcomes mutex poisoned");
        match outcomes.get(handle) {
            Some(Outcome::Success(value)) => Ok(Some(value.clone())),
            Some(Outcome::Failure(detail)) => Err(ServiceError {
                detail: detail.clone(),
            }),
            None => Err(ServiceError {
                detail: "unknown immediate handle".to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use serde_json::{json, Value};
    use tempfile::TempDir;

    use crate::service::{RunStatus, Service};
    use crate::{ENVELOPE_ARGS_KEY, ENVELOPE_COMMAND_KEY};

    use super::ImmediateService;

    /// Writes `contents` to `dir/name` and marks it executable (mode 0755),
    /// mirroring `process.rs`'s sibling test fixture of the same shape.
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

    /// Builds an envelope `Value` BY HAND using the SAME reserved-key
    /// constants `dispatch` exports (re-exported from the crate root), never
    /// a hand-rolled string literal, so this test can never drift from the
    /// producer's spelling.
    fn envelope(command: &str, args: Vec<&str>, params: Value) -> Value {
        json!({
            ENVELOPE_COMMAND_KEY: command,
            ENVELOPE_ARGS_KEY: args,
            "params": params,
        })
    }

    #[tokio::test]
    async fn invoke_then_status_is_completed_immediately() {
        let service = ImmediateService::new();
        let handle = service
            .invoke(json!({}))
            .await
            .expect("invoke should always succeed");

        assert_eq!(
            service.status(&handle).await,
            Ok(RunStatus::Completed),
            "expected Completed immediately after invoke -- no Running window"
        );
    }

    #[tokio::test]
    async fn envelope_command_spawns_and_json_stdout_round_trips_through_result() {
        let dir = TempDir::new().expect("failed to create tempdir");
        let script = write_executable_script(
            dir.path(),
            "emit_json.sh",
            "#!/usr/bin/env bash\nset -euo pipefail\ncat >/dev/null\necho '{\"ok\": true}'\n",
        );

        let service = ImmediateService::new();
        let input = envelope(&script.to_string_lossy(), vec![], json!({}));
        let handle = service
            .invoke(input)
            .await
            .expect("invoke should succeed for a script that exits 0");

        let output = service
            .result(&handle)
            .await
            .expect("result should succeed for a completed run")
            .expect("result should produce Some output for a successful run");

        assert_eq!(
            output,
            json!({"ok": true}),
            "expected the script's JSON stdout to round-trip exactly, got: {output:?}"
        );
    }

    #[tokio::test]
    async fn envelope_command_status_is_completed_immediately_after_invoke() {
        let dir = TempDir::new().expect("failed to create tempdir");
        let script = write_executable_script(
            dir.path(),
            "emit_json.sh",
            "#!/usr/bin/env bash\nset -euo pipefail\ncat >/dev/null\necho '{\"ok\": true}'\n",
        );

        let service = ImmediateService::new();
        let input = envelope(&script.to_string_lossy(), vec![], json!({}));
        let handle = service
            .invoke(input)
            .await
            .expect("invoke should succeed for a script that exits 0");

        assert_eq!(
            service.status(&handle).await,
            Ok(RunStatus::Completed),
            "expected Completed immediately after invoke -- no Running window for the spawn branch either"
        );
    }

    #[tokio::test]
    async fn envelope_command_pointing_at_nonexistent_path_surfaces_as_error() {
        let service = ImmediateService::new();
        let input = envelope(
            "/nonexistent/path/to/binary-that-does-not-exist",
            vec![],
            json!({}),
        );
        let handle = service
            .invoke(input)
            .await
            .expect("invoke should succeed even when the configured command cannot be spawned");

        let result = service.result(&handle).await;
        assert!(
            result.is_err(),
            "expected result() to surface the spawn failure as an Err, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn envelope_command_non_zero_exit_surfaces_as_error_naming_the_exit_status() {
        let dir = TempDir::new().expect("failed to create tempdir");
        let script = write_executable_script(dir.path(), "fail.sh", "#!/usr/bin/env bash\ncat >/dev/null\nexit 3\n");

        let service = ImmediateService::new();
        let input = envelope(&script.to_string_lossy(), vec![], json!({}));
        let handle = service
            .invoke(input)
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
    async fn envelope_params_are_piped_to_child_stdin_as_json() {
        let dir = TempDir::new().expect("failed to create tempdir");
        let script = write_executable_script(
            dir.path(),
            "echo_stdin.sh",
            "#!/usr/bin/env bash\nset -euo pipefail\ncat > \"$(dirname \"$0\")/received_stdin.json\"\necho '{\"ok\": true}'\n",
        );

        let service = ImmediateService::new();
        let params = json!({"greeting": "hello"});
        let input = envelope(&script.to_string_lossy(), vec![], params.clone());
        let handle = service.invoke(input).await.expect("invoke should succeed");
        service
            .result(&handle)
            .await
            .expect("result should succeed for a completed run")
            .expect("result should produce Some output");

        let received_raw = fs::read_to_string(dir.path().join("received_stdin.json"))
            .expect("failed to read the fixture's recorded stdin");
        let received: Value = serde_json::from_str(received_raw.trim())
            .expect("expected the recorded stdin to be valid JSON");
        assert_eq!(
            received, params,
            "expected the child to receive the envelope's params JSON on stdin"
        );
    }

    #[tokio::test]
    async fn no_command_key_echoes_input_and_spawns_no_process() {
        let service = ImmediateService::new();
        let input = json!({"anything": "goes"});

        let handle = service
            .invoke(input.clone())
            .await
            .expect("invoke should always succeed");

        let output = service
            .result(&handle)
            .await
            .expect("result should succeed for a known handle")
            .expect("result should produce Some output for a known handle");

        assert!(
            output.is_object(),
            "expected a structured JSON object (D-01), never a spoken-language string; got: {output:?}"
        );
        assert_eq!(
            output,
            json!({"status": "completed", "params": input}),
            "expected the no-command branch to echo the caller payload under `params`, got: {output:?}"
        );
    }

    #[tokio::test]
    async fn two_invokes_yield_distinct_handles() {
        let service = ImmediateService::new();
        let first = service
            .invoke(json!({}))
            .await
            .expect("first invoke should succeed");
        let second = service
            .invoke(json!({}))
            .await
            .expect("second invoke should succeed");

        assert_ne!(first, second, "expected two invokes to mint distinct handles");
    }

    #[tokio::test]
    async fn status_of_unknown_handle_is_err() {
        let service = ImmediateService::new();
        let unknown = service
            .invoke(json!({}))
            .await
            .expect("invoke should succeed");
        // Invoke once to obtain a real handle shape, then query a handle
        // that was never issued by this service instance.
        let other = ImmediateService::new();

        let result = other.status(&unknown).await;
        assert!(result.is_err(), "expected status of an unknown handle to be Err");
    }

    #[tokio::test]
    async fn result_of_known_handle_is_structured_json_object() {
        let service = ImmediateService::new();
        let handle = service
            .invoke(json!({}))
            .await
            .expect("invoke should succeed");

        let output = service
            .result(&handle)
            .await
            .expect("result should succeed for a known handle")
            .expect("result should produce Some output for a known handle");

        assert!(
            output.is_object(),
            "expected a structured JSON object (D-01), never a spoken-language string; got: {output:?}"
        );
    }

    #[tokio::test]
    async fn result_of_unknown_handle_is_err() {
        let service = ImmediateService::new();
        let issued = service
            .invoke(json!({}))
            .await
            .expect("invoke should succeed");
        let other = ImmediateService::new();

        let result = other.result(&issued).await;
        assert!(result.is_err(), "expected result of an unknown handle to be Err");
    }

    #[tokio::test]
    async fn cancel_is_a_no_op_ok() {
        let service = ImmediateService::new();
        let handle = service
            .invoke(json!({}))
            .await
            .expect("invoke should succeed");

        assert_eq!(service.cancel(&handle).await, Ok(()));
    }
}
