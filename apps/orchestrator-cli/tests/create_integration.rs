//! Integration tests for `orchestrator workflow create` (quick task
//! 260807-shx, Task 4). Mirrors `list_integration.rs`/`ws_client_integration.rs`'s
//! `#[path]` module-inclusion trick (no `[lib]` target for `orchestrator-cli`)
//! and their `start_test_daemon`/compiled-binary invocation shapes.

#[path = "../src/create.rs"]
mod create;
#[path = "../src/ws_client.rs"]
mod ws_client;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use shared::{ParameterType, WorkflowCreator};
use tempfile::TempDir;
use tokio::net::TcpListener;

use orchestrator::{InProcessOrchestrator, Service};

fn write_file(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    fs::write(path, contents).expect("failed to write fixture");
}

/// Binds an ephemeral loopback WS server in-process wrapping an
/// `InProcessOrchestrator` built from `workflows_dir` (mirrors
/// `list_integration.rs::start_test_daemon`).
async fn start_test_daemon(workflows_dir: &Path) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port");
    let port = listener
        .local_addr()
        .expect("failed to read the bound local_addr")
        .port();
    let handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    let orchestrator = Arc::new(InProcessOrchestrator::with_handlers(workflows_dir, handlers));
    let activity_registry = Arc::new(orchestrator::activity::ActivityRegistry::new());
    tokio::spawn(orchestrator::server::serve(listener, orchestrator, activity_registry));
    port
}

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

// ---- --param parsing ----

#[test]
fn param_dur_int_required_parses_correctly() {
    let parsed = create::parse_param("dur:int:required").expect("should parse");
    assert_eq!(parsed.name, "dur");
    assert_eq!(parsed.type_, ParameterType::Int);
    assert!(parsed.required);
}

#[test]
fn param_note_string_with_no_required_token_defaults_to_optional() {
    let parsed = create::parse_param("note:string").expect("should parse");
    assert_eq!(parsed.name, "note");
    assert_eq!(parsed.type_, ParameterType::String);
    assert!(!parsed.required);
}

#[test]
fn param_loud_bool_optional_parses_as_not_required() {
    let parsed = create::parse_param("loud:bool:optional").expect("should parse");
    assert_eq!(parsed.name, "loud");
    assert_eq!(parsed.type_, ParameterType::Bool);
    assert!(!parsed.required);
}

#[test]
fn param_true_false_tokens_map_to_required() {
    let required = create::parse_param("x:int:true").expect("should parse");
    assert!(required.required);
    let optional = create::parse_param("x:int:false").expect("should parse");
    assert!(!optional.required);
}

#[test]
fn param_rejects_one_field() {
    assert!(create::parse_param("dur").is_err());
}

#[test]
fn param_rejects_four_fields() {
    assert!(create::parse_param("dur:int:required:extra").is_err());
}

#[test]
fn param_rejects_empty_name() {
    assert!(create::parse_param(":int:required").is_err());
}

#[test]
fn param_rejects_unknown_type_token() {
    let err = create::parse_param("dur:float").expect_err("should reject unknown type");
    assert!(!err.is_empty());
}

#[test]
fn param_rejects_unknown_required_token() {
    let err = create::parse_param("dur:int:maybe").expect_err("should reject unknown required token");
    assert!(!err.is_empty());
}

#[test]
fn param_rejects_duplicate_name() {
    let specs = vec!["dur:int:required".to_string(), "dur:string".to_string()];
    let err = create::parse_params(&specs).expect_err("should reject duplicate param name");
    assert!(err.contains("dur"), "expected the error to name the duplicate param, got: {err}");
}

#[test]
fn param_distinct_error_strings_never_panic() {
    let cases = ["", "a", "a:b:c:d", ":int", "a:", "a:int:x"];
    let mut errors = std::collections::HashSet::new();
    for case in cases {
        if let Err(e) = create::parse_param(case) {
            errors.insert(e);
        }
    }
    assert!(errors.len() >= 2, "expected multiple distinct error messages, got: {errors:?}");
}

// ---- source resolution ----

#[test]
fn source_naming_an_existing_file_yields_that_files_contents() {
    let dir = TempDir::new().expect("tempdir");
    write_file(dir.path(), "script.sh", "#!/bin/sh\necho hi\n");
    let path = dir.path().join("script.sh").to_string_lossy().to_string();

    let content = create::resolve_source(&path);
    assert_eq!(content, "#!/bin/sh\necho hi\n");
}

#[test]
fn source_naming_no_existing_file_yields_the_literal_string() {
    let content = create::resolve_source("just some prose, not a real path");
    assert_eq!(content, "just some prose, not a real path");
}

// ---- classification ----

#[test]
fn shebang_content_classifies_as_script() {
    assert!(create::classify_as_script("#!/bin/sh\necho hi\n", "inline", false, false));
}

#[test]
fn sh_extension_file_classifies_as_script() {
    let dir = TempDir::new().expect("tempdir");
    write_file(dir.path(), "thing.sh", "echo hi\n");
    let path = dir.path().join("thing.sh").to_string_lossy().to_string();
    assert!(create::classify_as_script("echo hi\n", &path, false, false));
}

#[test]
fn plain_prose_classifies_as_markdown() {
    assert!(!create::classify_as_script("just some prose", "inline", false, false));
}

#[test]
fn markdown_flag_overrides_a_shebang() {
    assert!(!create::classify_as_script("#!/bin/sh\necho hi\n", "inline", false, true));
}

#[test]
fn script_flag_overrides_plain_prose() {
    assert!(create::classify_as_script("just some prose", "inline", true, false));
}

// ---- display name derivation ----

#[test]
fn default_display_name_title_cases_the_id() {
    assert_eq!(create::derive_display_name("my_workflow"), "My Workflow");
}

// ---- end-to-end (direct create::run against an in-process daemon) ----

#[tokio::test]
async fn direct_markdown_create_is_subsequently_listable() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect");
    let client: &dyn WorkflowCreator = &client;

    let mut buf = Vec::new();
    let code = create::run(
        client,
        shared::WorkflowWriteMode::Create,
        "note_shx",
        "Some prose describing a note.",
        None,
        None,
        &[],
        false,
        true,
        &create::AgentFlags::default(),
        &mut buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(code, 0, "expected exit 0, got output: {}", String::from_utf8_lossy(&buf));

    let (registry, errors) = orchestrator::Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    assert!(registry.lookup("note_shx").is_some(), "expected the new workflow to be listable");
}

#[tokio::test]
async fn direct_shebang_script_create_writes_executable_script_file() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect");
    let client: &dyn WorkflowCreator = &client;

    let mut buf = Vec::new();
    let code = create::run(
        client,
        shared::WorkflowWriteMode::Create,
        "hello_shx",
        "#!/bin/sh\necho hi\n",
        None,
        None,
        &[],
        false,
        false,
        &create::AgentFlags::default(),
        &mut buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(code, 0, "expected exit 0, got output: {}", String::from_utf8_lossy(&buf));

    let script_path = dir.path().join("scripts").join("hello_shx.sh");
    assert!(script_path.exists(), "expected the script file to exist at {script_path:?}");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&script_path).expect("metadata").permissions().mode();
        assert!(mode & 0o100 != 0, "expected the owner-execute bit set, got mode {mode:o}");
    }
}

#[tokio::test]
async fn direct_create_with_params_lands_in_the_parameters_map() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect");
    let client: &dyn WorkflowCreator = &client;

    let mut buf = Vec::new();
    let code = create::run(
        client,
        shared::WorkflowWriteMode::Create,
        "timed_shx",
        "prose body",
        None,
        None,
        &["dur:int:required".to_string()],
        false,
        true,
        &create::AgentFlags::default(),
        &mut buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(code, 0, "expected exit 0, got output: {}", String::from_utf8_lossy(&buf));

    let (registry, _errors) = orchestrator::Registry::load(dir.path());
    let def = registry.lookup("timed_shx").expect("expected the new workflow to resolve");
    let dur: &orchestrator::ParameterSpec = def.parameters.get("dur").expect("expected dur param");
    assert_eq!(dur.type_, ParameterType::Int);
    assert!(dur.required);
}

// ---- end-to-end through the compiled binary ----

#[tokio::test]
async fn compiled_binary_create_then_list_shows_the_new_id() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let port_str = port.to_string();

    let create_output = run_orchestrator(&[
        "--port",
        &port_str,
        "workflow",
        "create",
        "cli_shx",
        "Some prose for the compiled-binary test.",
        "--param",
        "dur:int:required",
    ])
    .await;
    assert!(
        create_output.status.success(),
        "expected exit 0, got status {:?}, stdout: {}, stderr: {}",
        create_output.status,
        String::from_utf8_lossy(&create_output.stdout),
        String::from_utf8_lossy(&create_output.stderr)
    );

    let list_output = run_orchestrator(&["--port", &port_str, "list"]).await;
    let stdout = String::from_utf8(list_output.stdout).expect("stdout was not valid UTF-8");
    assert!(stdout.contains("cli_shx"), "expected the new workflow id in list output, got: {stdout:?}");
}

#[tokio::test]
async fn compiled_binary_duplicate_create_exits_nonzero_with_error_naming_the_id() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let port_str = port.to_string();

    let first = run_orchestrator(&["--port", &port_str, "workflow", "create", "dup_cli_shx", "Some prose."]).await;
    assert!(first.status.success(), "expected the first create to succeed");

    let second = run_orchestrator(&["--port", &port_str, "workflow", "create", "dup_cli_shx", "Some prose."]).await;
    assert!(!second.status.success(), "expected a duplicate create to exit nonzero");
    let stdout = String::from_utf8(second.stdout).expect("stdout was not valid UTF-8");
    assert!(stdout.contains("dup_cli_shx"), "expected the error to mention the duplicate id, got: {stdout:?}");
}
