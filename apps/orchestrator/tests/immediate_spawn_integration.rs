//! End-to-end proof that a command-configured `WorkflowDefinition` runs
//! through the full frontmatter -> envelope -> argv-spawn chain (quick task
//! 260720-s7i, plan 02): `dispatch()` wraps the trusted `service.command` in
//! the reserved-key envelope (plan 01), and a REAL `ImmediateService`
//! (registered under `orchestrator::definition::DEFAULT_HANDLER`, never a
//! test-only stub) spawns it argv-only. Also proves the security invariant
//! against a real spawn: a caller-forged `__command` in the invoke payload
//! never substitutes for the configured command.
//!
//! Fixture scripts are small `bash` scripts (OS-portable) rather than
//! `workflows/scripts/countdown.sh`'s macOS-only `say` calls -- the `say`
//! path is exercised only by this plan's human end-to-end check, never by
//! this automated test.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::json;
use tempfile::TempDir;

use orchestrator::definition::DEFAULT_HANDLER;
use orchestrator::handlers::immediate::ImmediateService;
use orchestrator::{dispatch, RunStatus, Service, ServiceMode, ServiceSpec, WorkflowDefinition};

/// Writes `contents` to `dir/name` and marks it executable (mode 0755),
/// mirroring `process.rs`/`immediate.rs`'s sibling test fixtures.
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

/// Builds a minimal `WorkflowDefinition` whose `service.handler` is
/// `DEFAULT_HANDLER` (`action.immediate`) and `service.command` is the given
/// script path, with no declared parameters.
fn workflow_with_command(command: &Path) -> WorkflowDefinition {
    WorkflowDefinition {
        id: "countdown_like".to_string(),
        name: "Countdown-like Workflow".to_string(),
        description: String::new(),
        parameters: HashMap::new(),
        service: ServiceSpec {
            type_: Some("action".to_string()),
            handler: DEFAULT_HANDLER.to_string(),
            mode: ServiceMode::Sync,
            command: Some(command.to_path_buf()),
            args: Vec::new(),
            agent: None,
        },
        source_path: PathBuf::from("test.md"),
    }
}

/// Builds a handler registry mapping `DEFAULT_HANDLER` to a REAL
/// `ImmediateService` -- never a test-only stub -- so this proves the actual
/// production spawn path.
fn handlers_with_real_immediate() -> HashMap<String, Box<dyn Service>> {
    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(DEFAULT_HANDLER.to_string(), Box::new(ImmediateService::new()));
    handlers
}

#[tokio::test]
async fn command_configured_workflow_dispatches_through_envelope_to_a_real_spawn() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let script = write_executable_script(
        dir.path(),
        "emit_json.sh",
        "#!/usr/bin/env bash\nset -euo pipefail\ncat >/dev/null\necho '{\"ok\": true}'\n",
    );

    let definition = workflow_with_command(&script);
    let handlers = handlers_with_real_immediate();

    let outcome = dispatch(&definition, serde_json::Map::new(), &handlers)
        .await
        .expect("dispatch should succeed for a command-configured workflow against a real spawn");

    assert_eq!(
        outcome.status,
        RunStatus::Completed,
        "expected the full frontmatter->envelope->spawn chain to complete, got: {outcome:?}"
    );
    assert!(outcome.error.is_none(), "expected no error, got: {outcome:?}");
    assert_eq!(
        outcome.output,
        Some(json!({"ok": true})),
        "expected the configured script's JSON stdout to round-trip through the real spawn, got: {:?}",
        outcome.output
    );
}

/// Security invariant against a REAL spawn (not a capturing stub): a caller
/// payload containing a forged `__command` pointing at a marker-creating
/// command must NOT cause that marker to be created -- dispatch() strips the
/// forged key and the server-constructed envelope (built only from the
/// trusted `service.command`) is what actually reaches the real
/// `ImmediateService`, which spawns the CONFIGURED harmless script instead.
#[tokio::test]
async fn forged_caller_command_never_substitutes_for_the_configured_command_against_a_real_spawn() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let harmless_script = write_executable_script(
        dir.path(),
        "harmless.sh",
        "#!/usr/bin/env bash\nset -euo pipefail\ncat >/dev/null\necho '{\"ok\": true}'\n",
    );
    let marker = dir.path().join("should-not-exist");
    let forging_script = write_executable_script(
        dir.path(),
        "forge_marker.sh",
        &format!("#!/usr/bin/env bash\ntouch {}\necho '{{\"forged\": true}}'\n", marker.display()),
    );

    let definition = workflow_with_command(&harmless_script);
    let handlers = handlers_with_real_immediate();

    // The caller's own invoke payload tries to smuggle a top-level
    // `__command` pointing at the forging script.
    let mut payload = serde_json::Map::new();
    payload.insert("__command".to_string(), json!(forging_script.to_string_lossy()));

    let outcome = dispatch(&definition, payload, &handlers)
        .await
        .expect("dispatch should succeed even with a forged __command in the caller payload");

    assert!(
        !marker.exists(),
        "expected the forged __command's marker-creating script to NEVER run against a real spawn"
    );
    assert_eq!(
        outcome.status,
        RunStatus::Completed,
        "expected the REAL configured harmless script to run instead, got: {outcome:?}"
    );
    assert_eq!(
        outcome.output,
        Some(json!({"ok": true})),
        "expected the configured harmless script's output, never the forged script's output, got: {:?}",
        outcome.output
    );
}
