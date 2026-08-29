//! Integration test for `orchestrator run` (CLI-02/CLI-03): a user runs a
//! named workflow with dynamic parameter flags and sees its result printed
//! as pretty JSON, exit 0; every failure prints a single JSON `error` object
//! on stdout with a nonzero exit and no crash (CLI-03, D-08/D-09/D-10/D-11).
//!
//! Migrated for the D-06 full cutover (04-05): the compiled `orchestrator`
//! binary no longer takes a workflow-directory flag or runs anything
//! in-process -- every end-to-end test here starts an in-process WS test
//! daemon (D-07, never a spawned `orchestratord` subprocess) over a tempdir
//! fixture and
//! points the compiled CLI at it via `--port`. `run.rs` and `ws_client.rs`
//! are included directly from the binary crate's source (mirrors the prior
//! `#[path]` convention) so the zero-param/bool-flag coercion/describe-sort
//! tests can drive `run::run` in-process against a test daemon wired with a
//! test-only `EchoService` stub, while the true end-to-end happy-path and
//! CLI-03 error-surface tests spawn the real built binary
//! (`run_orchestrator`) against a test daemon carrying the real
//! `timers.start`/`process.calendar_today` handlers.

#[path = "../src/run.rs"]
mod run;
#[path = "../src/ws_client.rs"]
mod ws_client;

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpListener;

use orchestrator::handlers::process::ProcessService;
use orchestrator::handlers::timers::TimerService;
use orchestrator::{
    DescribeWorkflowRequest, InProcessOrchestrator, OrchestratorClient, RunHandle, RunStatus,
    Service, ServiceError,
};

/// Writes `contents` to `dir/filename`, creating any intermediate
/// directories the fixture requests. Mirrors
/// apps/orchestrator-cli/tests/list_integration.rs.
fn write_workflow(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    fs::write(path, contents).expect("failed to write fixture");
}

/// Writes `contents` to `dir/filename` and marks it executable (mode 0755).
/// Mirrors `write_workflow` but for spawn-target fixture scripts
/// (ProcessService's `calendar_today` proof, SVC-02/D-12).
fn write_executable_fixture(dir: &Path, filename: &str, contents: &str) -> std::path::PathBuf {
    write_workflow(dir, filename, contents);
    let path = dir.join(filename);
    let mut perms = fs::metadata(&path)
        .expect("failed to read fixture script metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).expect("failed to set fixture script executable bit");
    path
}

/// Binds an ephemeral loopback WS server in-process (D-07 — never spawns an
/// `orchestratord` subprocess) wrapping an `InProcessOrchestrator` built from
/// `workflows_dir`/`handlers`, and returns the bound port for the compiled
/// CLI binary to connect to via `--port`.
async fn start_test_daemon(
    workflows_dir: &Path,
    handlers: HashMap<String, Box<dyn Service>>,
) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port");
    let port = listener
        .local_addr()
        .expect("failed to read the bound local_addr")
        .port();
    let orchestrator = Arc::new(InProcessOrchestrator::with_handlers(workflows_dir, handlers));
    let activity_registry = Arc::new(orchestrator::activity::ActivityRegistry::new());
    tokio::spawn(orchestrator::server::serve(listener, orchestrator, activity_registry));
    port
}

/// Runs the built `orchestrator` binary with the given CLI args and extra
/// environment variables on a blocking-pool thread -- keeping this async
/// test's own tokio runtime free to keep polling the in-process test
/// daemon task concurrently -- returning captured stdout/stderr. Exercises
/// the real clap parsing and the real process exit code (D-10) against a
/// test daemon over `--port` (D-06 -- the CLI no longer takes a
/// workflow-directory flag).
async fn run_orchestrator(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let envs: Vec<(String, String)> = envs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_orchestrator"));
        cmd.args(&args);
        for (key, value) in &envs {
            cmd.env(key, value);
        }
        cmd.output().expect("failed to run the orchestrator binary")
    })
    .await
    .expect("run_orchestrator's spawn_blocking task panicked")
}

/// Test-only `Service` double that immediately completes and echoes its
/// input payload back as `result()` output. Used only by the in-process
/// `run::run` tests below (zero-param dispatch, bool presence-flag
/// coercion) — never registered against the real `orchestratord` daemon,
/// which only ever wires real production handlers.
struct EchoService {
    outputs: Mutex<HashMap<RunHandle, Value>>,
    next_id: Mutex<u64>,
}

impl EchoService {
    fn new() -> Self {
        Self {
            outputs: Mutex::new(HashMap::new()),
            next_id: Mutex::new(0),
        }
    }
}

#[async_trait]
impl Service for EchoService {
    async fn invoke(&self, input: Value) -> Result<RunHandle, ServiceError> {
        let id = {
            let mut next_id = self.next_id.lock().expect("EchoService next_id mutex poisoned");
            let id = *next_id;
            *next_id += 1;
            format!("echo-{id}")
        };
        let handle = RunHandle::new(id);
        self.outputs
            .lock()
            .expect("EchoService outputs mutex poisoned")
            .insert(handle.clone(), input);
        Ok(handle)
    }

    async fn status(&self, _handle: &RunHandle) -> Result<RunStatus, ServiceError> {
        Ok(RunStatus::Completed)
    }

    async fn cancel(&self, _handle: &RunHandle) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn result(&self, handle: &RunHandle) -> Result<Option<Value>, ServiceError> {
        Ok(self
            .outputs
            .lock()
            .expect("EchoService outputs mutex poisoned")
            .get(handle)
            .cloned())
    }
}

/// Starts a test daemon wired with a `test.echo` `EchoService` and connects
/// a `WsOrchestratorClient` to it -- the WS-transport equivalent of the
/// prior in-process `InProcessOrchestrator::with_handlers` construction.
async fn echo_client(workflows_dir: &Path) -> ws_client::WsOrchestratorClient {
    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert("test.echo".to_string(), Box::new(EchoService::new()));
    let port = start_test_daemon(workflows_dir, handlers).await;
    ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect the test WsOrchestratorClient to the in-process daemon")
}

#[tokio::test]
async fn runs_zero_param_workflow_with_empty_payload_and_prints_result() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "no_params.md",
        r#"---
id: no_params
name: No Params Workflow
parameters: {}
service:
  type: action
  handler: test.echo
---
Zero-parameter fixture.
"#,
    );

    let client = echo_client(dir.path()).await;
    let mut buf = Vec::new();
    let code = run::run(&client, "no_params", &[], &mut buf)
        .await
        .expect("run::run should not error on a valid zero-param workflow");
    let output = String::from_utf8(buf).expect("captured output was not valid UTF-8");

    assert_eq!(code, 0, "expected exit code 0, got stdout: {output:?}");
    let parsed: Value = serde_json::from_str(&output).expect("expected valid JSON output");
    assert_eq!(
        parsed,
        json!({}),
        "expected the echoed empty payload as output, got: {parsed:?}"
    );
}

#[tokio::test]
async fn bool_flag_present_coerces_to_true_absent_coerces_to_false() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "notify.md",
        r#"---
id: notify
name: Notify Workflow
parameters:
  notify:
    type: bool
    required: false
service:
  type: action
  handler: test.echo
---
Bool-parameter fixture.
"#,
    );

    let client = echo_client(dir.path()).await;

    let mut buf_present = Vec::new();
    let code_present = run::run(&client, "notify", &["--notify".to_string()], &mut buf_present)
        .await
        .expect("run::run should not error with the bool flag present");
    let output_present =
        String::from_utf8(buf_present).expect("captured output was not valid UTF-8");
    assert_eq!(code_present, 0, "expected exit 0, got: {output_present:?}");
    let parsed_present: Value =
        serde_json::from_str(&output_present).expect("expected valid JSON output");
    assert_eq!(
        parsed_present.get("notify"),
        Some(&json!(true)),
        "expected `notify` to coerce to true when the flag is present, got: {parsed_present:?}"
    );

    let mut buf_absent = Vec::new();
    let code_absent = run::run(&client, "notify", &[], &mut buf_absent)
        .await
        .expect("run::run should not error with the bool flag absent");
    let output_absent = String::from_utf8(buf_absent).expect("captured output was not valid UTF-8");
    assert_eq!(code_absent, 0, "expected exit 0, got: {output_absent:?}");
    let parsed_absent: Value =
        serde_json::from_str(&output_absent).expect("expected valid JSON output");
    assert_eq!(
        parsed_absent.get("notify"),
        Some(&json!(false)),
        "expected `notify` to coerce to false when the flag is absent, got: {parsed_absent:?}"
    );
}

#[tokio::test]
async fn describe_workflow_returns_parameters_sorted_by_name() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "multi_param.md",
        r#"---
id: multi_param
name: Multi Param Workflow
parameters:
  zeta:
    type: string
    required: false
  alpha:
    type: int
    required: true
  mid:
    type: bool
    required: false
service:
  type: action
  handler: test.echo
---
Multi-parameter fixture.
"#,
    );

    let client = echo_client(dir.path()).await;
    let response = client
        .describe_workflow(DescribeWorkflowRequest {
            workflow_id: "multi_param".to_string(),
        })
        .await;

    assert!(response.found, "expected `multi_param` to be found");
    let names: Vec<String> = response.parameters.iter().map(|p| p.name.clone()).collect();
    assert_eq!(
        names,
        vec!["alpha".to_string(), "mid".to_string(), "zeta".to_string()],
        "expected parameters sorted ascending by name, got: {names:?}"
    );
}

#[tokio::test]
async fn describe_workflow_reports_not_found_for_unknown_id() {
    let dir = TempDir::new().expect("failed to create tempdir");

    let client = echo_client(dir.path()).await;
    let response = client
        .describe_workflow(DescribeWorkflowRequest {
            workflow_id: "nonexistent".to_string(),
        })
        .await;

    assert!(
        !response.found,
        "expected `found: false` for an unknown workflow id"
    );
    assert!(
        response.parameters.is_empty(),
        "expected no parameters for an unknown workflow id"
    );
}

#[tokio::test]
async fn runs_set_timer_end_to_end_and_prints_pretty_json_result() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "set_timer.md",
        r#"---
id: set_timer
name: Set Timer
parameters:
  duration_minutes:
    type: int
    description: how many minutes the timer should run for
    required: true
service:
  type: action
  handler: timers.start
---
Sets a local countdown timer.
"#,
    );

    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "timers.start".to_string(),
        Box::new(TimerService::with_unit(Duration::from_millis(50))),
    );
    let port = start_test_daemon(dir.path(), handlers).await;
    let port_str = port.to_string();

    let output = run_orchestrator(
        &["--port", &port_str, "run", "set_timer", "--duration_minutes", "0"],
        &[],
    )
    .await;

    assert!(
        output.status.success(),
        "expected exit 0, got status: {:?}, stdout: {:?}, stderr: {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout was not valid UTF-8");
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("expected valid JSON stdout, got {stdout:?}: {e}"));
    assert!(
        parsed.is_object(),
        "expected a pretty JSON object on stdout, got: {parsed:?}"
    );
    assert!(
        parsed.get("error").is_none(),
        "expected no `error` key on a successful run, got: {parsed:?}"
    );
}

// --- CLI-03 error-surface tests (Task 2/3) ---

#[tokio::test]
async fn missing_required_param_prints_json_error_and_exits_nonzero() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "set_timer.md",
        r#"---
id: set_timer
name: Set Timer
parameters:
  duration_minutes:
    type: int
    required: true
service:
  type: action
  handler: timers.start
---
Sets a local countdown timer.
"#,
    );

    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "timers.start".to_string(),
        Box::new(TimerService::with_unit(Duration::from_millis(50))),
    );
    let port = start_test_daemon(dir.path(), handlers).await;
    let port_str = port.to_string();

    let output = run_orchestrator(&["--port", &port_str, "run", "set_timer"], &[]).await;

    assert!(
        !output.status.success(),
        "expected nonzero exit for a missing required parameter"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout was not valid UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr was not valid UTF-8");
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("expected valid JSON stdout, got {stdout:?}: {e}"));
    let error_msg = parsed
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("expected an `error` key on stdout, got: {parsed:?}"));
    assert!(
        error_msg.contains("duration_minutes"),
        "expected the error to name the missing param, got: {error_msg:?}"
    );
    assert!(
        !stdout.contains("panic") && !stderr.contains("panic"),
        "expected no panic text, got stdout: {stdout:?}, stderr: {stderr:?}"
    );
}

#[tokio::test]
async fn unknown_flag_is_rejected_naming_the_flag_and_expected_parameters() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "set_timer.md",
        r#"---
id: set_timer
name: Set Timer
parameters:
  duration_minutes:
    type: int
    required: true
service:
  type: action
  handler: timers.start
---
Sets a local countdown timer.
"#,
    );

    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "timers.start".to_string(),
        Box::new(TimerService::with_unit(Duration::from_millis(50))),
    );
    let port = start_test_daemon(dir.path(), handlers).await;
    let port_str = port.to_string();

    let first_run = run_orchestrator(
        &["--port", &port_str, "run", "set_timer", "--bogus", "5"],
        &[],
    )
    .await;

    assert!(
        !first_run.status.success(),
        "expected nonzero exit for an unknown flag"
    );
    let stdout = String::from_utf8(first_run.stdout).expect("stdout was not valid UTF-8");
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("expected valid JSON stdout, got {stdout:?}: {e}"));
    let error_msg = parsed
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("expected an `error` key on stdout, got: {parsed:?}"));
    assert!(
        error_msg.contains("--bogus"),
        "expected the error to name the offending flag `--bogus`, got: {error_msg:?}"
    );
    assert!(
        error_msg.contains("expected parameters:"),
        "expected the error to contain `expected parameters:`, got: {error_msg:?}"
    );
    assert!(
        error_msg.contains("duration_minutes (int, required)"),
        "expected the error to list `duration_minutes (int, required)`, got: {error_msg:?}"
    );

    let second_run = run_orchestrator(
        &["--port", &port_str, "run", "set_timer", "--bogus", "5"],
        &[],
    )
    .await;
    let second_stdout = String::from_utf8(second_run.stdout).expect("stdout was not valid UTF-8");
    assert_eq!(
        stdout, second_stdout,
        "expected byte-identical stdout across repeated runs (no HashMap-order flake)"
    );
}

#[tokio::test]
async fn bad_type_value_prints_json_error_and_exits_nonzero() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "set_timer.md",
        r#"---
id: set_timer
name: Set Timer
parameters:
  duration_minutes:
    type: int
    required: true
service:
  type: action
  handler: timers.start
---
Sets a local countdown timer.
"#,
    );

    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "timers.start".to_string(),
        Box::new(TimerService::with_unit(Duration::from_millis(50))),
    );
    let port = start_test_daemon(dir.path(), handlers).await;
    let port_str = port.to_string();

    let output = run_orchestrator(
        &[
            "--port",
            &port_str,
            "run",
            "set_timer",
            "--duration_minutes",
            "notanumber",
        ],
        &[],
    )
    .await;

    assert!(
        !output.status.success(),
        "expected nonzero exit for a bad type value"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout was not valid UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr was not valid UTF-8");
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("expected valid JSON stdout, got {stdout:?}: {e}"));
    assert!(
        parsed.get("error").is_some(),
        "expected an `error` key on stdout, got: {parsed:?}"
    );
    assert!(
        !stdout.contains("panic") && !stderr.contains("panic"),
        "expected no panic text, got stdout: {stdout:?}, stderr: {stderr:?}"
    );
}

#[tokio::test]
async fn unknown_workflow_prints_json_error_and_exits_nonzero() {
    let dir = TempDir::new().expect("failed to create tempdir");

    let handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    let port = start_test_daemon(dir.path(), handlers).await;
    let port_str = port.to_string();

    let output = run_orchestrator(&["--port", &port_str, "run", "nonexistent_workflow"], &[]).await;

    assert!(
        !output.status.success(),
        "expected nonzero exit for an unknown workflow"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout was not valid UTF-8");
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("expected valid JSON stdout, got {stdout:?}: {e}"));
    let error_msg = parsed
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("expected an `error` key on stdout, got: {parsed:?}"));
    assert!(
        error_msg.contains("nonexistent_workflow"),
        "expected the error to name the unknown workflow id, got: {error_msg:?}"
    );
}

#[tokio::test]
async fn handler_failure_prints_json_error_and_exits_nonzero() {
    // duration_minutes rejects negative values at the handler level
    // (TimerService requires a valid i64; a wildly negative value combined
    // with the handler's own validation still surfaces as a clean JSON
    // error rather than a crash). We drive this through a workflow whose
    // declared parameter type doesn't match what the handler expects at
    // all: a required duration_minutes of type `string` (a value the CLI
    // dutifully coerces to a JSON string), which the handler then rejects
    // as non-integer, proving the handler-failure -> JSON error path.
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "set_timer.md",
        r#"---
id: set_timer
name: Set Timer
parameters:
  duration_minutes:
    type: string
    required: true
service:
  type: action
  handler: timers.start
---
Sets a local countdown timer.
"#,
    );

    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "timers.start".to_string(),
        Box::new(TimerService::with_unit(Duration::from_millis(50))),
    );
    let port = start_test_daemon(dir.path(), handlers).await;
    let port_str = port.to_string();

    let output = run_orchestrator(
        &[
            "--port",
            &port_str,
            "run",
            "set_timer",
            "--duration_minutes",
            "five",
        ],
        &[],
    )
    .await;

    assert!(
        !output.status.success(),
        "expected nonzero exit for a handler failure"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout was not valid UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr was not valid UTF-8");
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("expected valid JSON stdout, got {stdout:?}: {e}"));
    assert!(
        parsed.get("error").is_some(),
        "expected an `error` key on stdout, got: {parsed:?}"
    );
    assert!(
        !stdout.contains("panic") && !stderr.contains("panic"),
        "expected no panic text, got stdout: {stdout:?}, stderr: {stderr:?}"
    );
}

// --- SVC-02 process-spawn proof: calendar_today (Task 2/3, D-12) ---

const CALENDAR_TODAY_WORKFLOW: &str = r#"---
id: calendar_today
name: Calendar Today
parameters: {}
service:
  type: integration
  handler: process.calendar_today
---
Spawns a configured external script and returns today's date as JSON.
"#;

#[tokio::test]
async fn run_calendar_today_prints_todays_date_shaped_json_exit_zero() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "calendar_today.md", CALENDAR_TODAY_WORKFLOW);

    // Exercise the actually-committed script, not a stand-in fixture.
    let script_path = format!(
        "{}/../../workflows/scripts/calendar_today.sh",
        env!("CARGO_MANIFEST_DIR")
    );

    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "process.calendar_today".to_string(),
        Box::new(ProcessService::new(script_path, Vec::new())),
    );
    let port = start_test_daemon(dir.path(), handlers).await;
    let port_str = port.to_string();

    let output = run_orchestrator(&["--port", &port_str, "run", "calendar_today"], &[]).await;

    assert!(
        output.status.success(),
        "expected exit 0, got status: {:?}, stdout: {:?}, stderr: {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout was not valid UTF-8");
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("expected valid JSON stdout, got {stdout:?}: {e}"));
    let date = parsed
        .get("date")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("expected a `date` field, got: {parsed:?}"));

    // Assert the shape (YYYY-MM-DD), never a fixed literal date, so this
    // stays valid on any day the suite runs.
    let segments: Vec<&str> = date.split('-').collect();
    assert_eq!(
        segments.len(),
        3,
        "expected `date` shaped YYYY-MM-DD (3 dash-separated segments), got: {date:?}"
    );
    assert_eq!(segments[0].len(), 4, "expected a 4-digit year, got: {date:?}");
    assert_eq!(segments[1].len(), 2, "expected a 2-digit month, got: {date:?}");
    assert_eq!(segments[2].len(), 2, "expected a 2-digit day, got: {date:?}");
}

#[tokio::test]
async fn run_calendar_today_with_failing_script_prints_json_error_and_exits_nonzero() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "calendar_today.md", CALENDAR_TODAY_WORKFLOW);
    let script_path = write_executable_fixture(
        dir.path(),
        "failing_calendar.sh",
        "#!/usr/bin/env bash\nexit 7\n",
    );

    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "process.calendar_today".to_string(),
        Box::new(ProcessService::new(script_path, Vec::new())),
    );
    let port = start_test_daemon(dir.path(), handlers).await;
    let port_str = port.to_string();

    let output = run_orchestrator(&["--port", &port_str, "run", "calendar_today"], &[]).await;

    assert!(
        !output.status.success(),
        "expected nonzero exit for a failing configured script"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout was not valid UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr was not valid UTF-8");
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("expected valid JSON stdout, got {stdout:?}: {e}"));
    assert!(
        parsed.get("error").is_some(),
        "expected a single `error` key on stdout, got: {parsed:?}"
    );
    assert!(
        !stdout.contains("panic") && !stderr.contains("panic"),
        "expected no panic text, got stdout: {stdout:?}, stderr: {stderr:?}"
    );
}

#[tokio::test]
async fn set_timer_and_calendar_today_both_succeed_through_the_same_binary() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "set_timer.md",
        r#"---
id: set_timer
name: Set Timer
parameters:
  duration_minutes:
    type: int
    required: true
service:
  type: action
  handler: timers.start
---
Sets a local countdown timer.
"#,
    );
    write_workflow(dir.path(), "calendar_today.md", CALENDAR_TODAY_WORKFLOW);
    let script_path = write_executable_fixture(
        dir.path(),
        "ok_calendar.sh",
        "#!/usr/bin/env bash\nset -euo pipefail\necho '{\"date\": \"2026-01-01\"}'\n",
    );

    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "timers.start".to_string(),
        Box::new(TimerService::with_unit(Duration::from_millis(50))),
    );
    handlers.insert(
        "process.calendar_today".to_string(),
        Box::new(ProcessService::new(script_path, Vec::new())),
    );
    let port = start_test_daemon(dir.path(), handlers).await;
    let port_str = port.to_string();

    let timer_output = run_orchestrator(
        &["--port", &port_str, "run", "set_timer", "--duration_minutes", "0"],
        &[],
    )
    .await;
    assert!(
        timer_output.status.success(),
        "expected set_timer (Action/script type) to succeed, got status: {:?}, stdout: {:?}",
        timer_output.status,
        String::from_utf8_lossy(&timer_output.stdout)
    );

    let calendar_output =
        run_orchestrator(&["--port", &port_str, "run", "calendar_today"], &[]).await;
    assert!(
        calendar_output.status.success(),
        "expected calendar_today (Integration/API type) to succeed through the identical binary, got status: {:?}, stdout: {:?}",
        calendar_output.status,
        String::from_utf8_lossy(&calendar_output.stdout)
    );
}

// --- D-04: no daemon running -> clear nonzero-exit error, no auto-spawn ---

#[tokio::test]
async fn no_daemon_running_exits_nonzero_naming_orchestratord() {
    // Reserve a very-likely-free port by binding then dropping the listener
    // immediately -- nothing is listening on it afterward.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port to reserve a free one");
    let port = listener
        .local_addr()
        .expect("failed to read the bound local_addr")
        .port();
    drop(listener);
    let port_str = port.to_string();

    let output = run_orchestrator(&["--port", &port_str, "list"], &[]).await;

    assert!(
        !output.status.success(),
        "expected a nonzero exit when no orchestratord is listening"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout was not valid UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr was not valid UTF-8");
    assert!(
        stderr.contains("orchestratord"),
        "expected the stderr message to name orchestratord, got: {stderr:?}"
    );
    assert!(
        !stdout.contains("panic") && !stderr.contains("panic"),
        "expected no panic text, got stdout: {stdout:?}, stderr: {stderr:?}"
    );
}
