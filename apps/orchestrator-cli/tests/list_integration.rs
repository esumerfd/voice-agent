//! Integration test for `orchestrator list` (CLI-01): the CLI must print a
//! row for every loaded workflow, driven through the lib's list entry
//! point — never re-implementing directory scanning in the CLI, and never
//! panicking on load errors (D-07).
//!
//! Migrated for the D-06 full cutover (04-05): `list.rs` now takes
//! `&dyn OrchestratorClient` instead of constructing its own
//! `InProcessOrchestrator`, and the compiled CLI binary no longer takes a
//! workflow-directory flag -- every end-to-end test here starts an
//! in-process WS test daemon (D-07, never a spawned `orchestratord`
//! subprocess) over a tempdir fixture and points the compiled CLI at it via
//! `--port`. `list.rs`
//! is still included directly from the binary crate's source (there is no
//! `[lib]` target for `orchestrator-cli`) so the direct-call test exercises
//! the exact function `main.rs` calls, with output captured into an
//! in-memory buffer instead of racing real process stdout.

#[path = "../src/list.rs"]
mod list;
#[path = "../src/ws_client.rs"]
mod ws_client;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use tempfile::TempDir;
use tokio::net::TcpListener;

use orchestrator::{InProcessOrchestrator, Service};

/// Writes `contents` to `dir/filename`, creating any intermediate
/// directories the fixture requests. Mirrors
/// apps/orchestrator/tests/registry_integration.rs.
fn write_workflow(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    fs::write(path, contents).expect("failed to write fixture");
}

/// Binds an ephemeral loopback WS server in-process (D-07 — never spawns an
/// `orchestratord` subprocess) wrapping an `InProcessOrchestrator` built from
/// `workflows_dir`, and returns the bound port for the compiled CLI binary
/// to connect to via `--port`.
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

#[tokio::test]
async fn list_prints_a_row_for_each_workflow() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "alpha.md",
        r#"---
id: alpha
name: Alpha Workflow
parameters: {}
service:
  type: action
  handler: demo.alpha
---
The first fixture workflow.
"#,
    );
    write_workflow(
        dir.path(),
        "beta.md",
        r#"---
id: beta
name: Beta Workflow
parameters: {}
service:
  type: action
  handler: demo.beta
---
The second fixture workflow.
"#,
    );

    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect the test WsOrchestratorClient to the in-process daemon");

    let mut buf = Vec::new();
    list::run(&client, &mut buf)
        .await
        .expect("list::run should not error on valid fixtures");
    let output = String::from_utf8(buf).expect("captured output was not valid UTF-8");

    assert!(
        output.contains("alpha"),
        "expected stdout to contain workflow id `alpha`, got: {output:?}"
    );
    assert!(
        output.contains("beta"),
        "expected stdout to contain workflow id `beta`, got: {output:?}"
    );
}

/// Runs the built `orchestrator` binary with the given CLI args and extra
/// environment variables on a blocking-pool thread -- keeping this async
/// test's own tokio runtime free to keep polling the in-process test
/// daemon task concurrently -- returning captured stdout/stderr. Exercises
/// real clap parsing and the real process exit code end-to-end against a
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

#[tokio::test]
async fn list_output_is_sorted_by_id() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "zebra.md",
        r#"---
id: zebra
name: Zebra Workflow
parameters: {}
service:
  type: action
  handler: demo.zebra
---
Zebra fixture.
"#,
    );
    write_workflow(
        dir.path(),
        "apple.md",
        r#"---
id: apple
name: Apple Workflow
parameters: {}
service:
  type: action
  handler: demo.apple
---
Apple fixture.
"#,
    );
    write_workflow(
        dir.path(),
        "mango.md",
        r#"---
id: mango
name: Mango Workflow
parameters: {}
service:
  type: action
  handler: demo.mango
---
Mango fixture.
"#,
    );

    let port = start_test_daemon(dir.path()).await;
    let port_str = port.to_string();

    let first_run = run_orchestrator(&["--port", &port_str, "list"], &[]).await;
    let stdout = String::from_utf8(first_run.stdout).expect("stdout was not valid UTF-8");

    let apple_pos = stdout.find("apple").expect("expected `apple` row in stdout");
    let mango_pos = stdout.find("mango").expect("expected `mango` row in stdout");
    let zebra_pos = stdout.find("zebra").expect("expected `zebra` row in stdout");
    assert!(
        apple_pos < mango_pos && mango_pos < zebra_pos,
        "expected ascending id order (apple, mango, zebra), got stdout: {stdout:?}"
    );

    let second_run = run_orchestrator(&["--port", &port_str, "list"], &[]).await;
    assert_eq!(
        stdout.as_bytes(),
        second_run.stdout.as_slice(),
        "expected byte-identical stdout across repeated runs (no HashMap-order flake)"
    );
}

#[tokio::test]
async fn list_columns_are_aligned() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "a.md",
        r#"---
id: a
name: Short
parameters: {}
service:
  type: action
  handler: demo.a
---
Short id fixture.
"#,
    );
    write_workflow(
        dir.path(),
        "bbbbb.md",
        r#"---
id: bbbbb
name: Long Name Workflow
parameters: {}
service:
  type: action
  handler: demo.bbbbb
---
Long id fixture.
"#,
    );

    let port = start_test_daemon(dir.path()).await;
    let port_str = port.to_string();

    let run = run_orchestrator(&["--port", &port_str, "list"], &[]).await;
    let stdout = String::from_utf8(run.stdout).expect("stdout was not valid UTF-8");
    let mut lines = stdout.lines();

    let header = lines.next().expect("expected a header line");
    assert!(header.contains("ID"), "expected header to contain `ID`, got: {header:?}");
    assert!(header.contains("NAME"), "expected header to contain `NAME`, got: {header:?}");
    assert!(
        header.contains("DESCRIPTION"),
        "expected header to contain `DESCRIPTION`, got: {header:?}"
    );
    let name_col = header.find("NAME").expect("expected a NAME column in the header");

    let mut found_short = false;
    let mut found_long = false;
    for line in lines {
        if let Some(pos) = line.find("Short") {
            assert_eq!(
                pos, name_col,
                "expected NAME column for the `a` row to start at offset {name_col}, got line: {line:?}"
            );
            found_short = true;
        } else if let Some(pos) = line.find("Long Name Workflow") {
            assert_eq!(
                pos, name_col,
                "expected NAME column for the `bbbbb` row to start at offset {name_col}, got line: {line:?}"
            );
            found_long = true;
        }
    }
    assert!(
        found_short && found_long,
        "expected both data rows in stdout: {stdout:?}"
    );
}

#[tokio::test]
async fn broken_file_warns_on_stderr_and_valid_rows_still_print() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "good.md",
        r#"---
id: good
name: Good Workflow
parameters: {}
service:
  type: action
  handler: demo.good
---
A valid fixture.
"#,
    );
    write_workflow(
        dir.path(),
        "bad.md",
        r#"---
name: Missing Id Workflow
service:
  type: action
  handler: demo.bad
---
Missing the id field.
"#,
    );

    let port = start_test_daemon(dir.path()).await;
    let port_str = port.to_string();

    let run = run_orchestrator(&["--port", &port_str, "list"], &[]).await;
    let stdout = String::from_utf8(run.stdout).expect("stdout was not valid UTF-8");
    let stderr = String::from_utf8(run.stderr).expect("stderr was not valid UTF-8");

    assert!(
        stdout.contains("good"),
        "expected the valid workflow `good` to still print on stdout, got: {stdout:?}"
    );

    let warning_lines: Vec<&str> = stderr.lines().filter(|l| l.starts_with("warning:")).collect();
    assert_eq!(
        warning_lines.len(),
        1,
        "expected exactly one warning line on stderr, got: {stderr:?}"
    );
    assert!(
        warning_lines[0].contains("bad.md"),
        "expected the warning to name the broken file `bad.md`, got: {}",
        warning_lines[0]
    );
    assert!(
        warning_lines[0].contains("id"),
        "expected the warning to name the missing field `id`, got: {}",
        warning_lines[0]
    );
}

#[tokio::test]
async fn port_flag_selects_which_daemon_the_cli_talks_to() {
    let dir_a = TempDir::new().expect("failed to create tempdir A");
    write_workflow(
        dir_a.path(),
        "from_daemon_a.md",
        r#"---
id: from_daemon_a
name: From Daemon A Workflow
parameters: {}
service:
  type: action
  handler: demo.from_daemon_a
---
Served by daemon A.
"#,
    );
    let dir_b = TempDir::new().expect("failed to create tempdir B");
    write_workflow(
        dir_b.path(),
        "from_daemon_b.md",
        r#"---
id: from_daemon_b
name: From Daemon B Workflow
parameters: {}
service:
  type: action
  handler: demo.from_daemon_b
---
Served by daemon B.
"#,
    );

    let port_a = start_test_daemon(dir_a.path()).await;
    let port_b = start_test_daemon(dir_b.path()).await;
    let port_a_str = port_a.to_string();
    let port_b_str = port_b.to_string();

    let run_a = run_orchestrator(&["--port", &port_a_str, "list"], &[]).await;
    let stdout_a = String::from_utf8(run_a.stdout).expect("stdout was not valid UTF-8");
    assert!(
        stdout_a.contains("from_daemon_a"),
        "expected --port to select daemon A's registry, got: {stdout_a:?}"
    );
    assert!(
        !stdout_a.contains("from_daemon_b"),
        "expected daemon B's workflow to be absent when --port selects daemon A, got: {stdout_a:?}"
    );

    let run_b = run_orchestrator(&["--port", &port_b_str, "list"], &[]).await;
    let stdout_b = String::from_utf8(run_b.stdout).expect("stdout was not valid UTF-8");
    assert!(
        stdout_b.contains("from_daemon_b"),
        "expected --port to select daemon B's registry, got: {stdout_b:?}"
    );
}

/// Proves the whitespace-collapse half of the fix (D-1): an agent-type
/// workflow's Markdown body -- multiple hard-wrapped paragraphs separated by
/// blank lines, mirroring `workflows/ai_summarize.md` -- must render as
/// exactly one row, not one line per physical line of the body. Today's
/// verbatim `summary.description` write reproduces `ai_summarize`'s real
/// 20+ line spill; asserting a 2-line total (header + 1 row) is the RED
/// signal.
#[tokio::test]
async fn multi_paragraph_description_collapses_to_one_row() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "sumfix.md",
        r#"---
id: sumfix
name: Summarize Fixture
parameters: {}
service:
  type: action
  handler: demo.sumfix
---
FIRSTWORDXYZ is the opening word of this multi-paragraph agent workflow body,
modeled on ai_summarize.md's structure with several hard-wrapped paragraphs
separated by blank lines to prove the whitespace collapse handles paragraph
breaks correctly.

This middle paragraph exists purely to add length between the opening marker
and the ending marker, pushing the whole collapsed description well past two
hundred characters so the 80-column truncation logic actually engages for
real instead of trivially fitting.

ENDMARKERXYZ appears only in this final paragraph and must never appear in
the rendered single-line row because the row is truncated long before this
point is reached.
"#,
    );

    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect the test WsOrchestratorClient to the in-process daemon");

    let mut buf = Vec::new();
    list::run(&client, &mut buf)
        .await
        .expect("list::run should not error on valid fixtures");
    let output = String::from_utf8(buf).expect("captured output was not valid UTF-8");

    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "expected exactly one header line plus one row for a single multi-paragraph workflow, got: {output:?}"
    );

    assert!(
        output.contains("FIRSTWORDXYZ"),
        "expected the row to preview the first paragraph's opening word, got: {output:?}"
    );
    assert!(
        !output.contains("ENDMARKERXYZ"),
        "expected the final paragraph's marker word to be truncated away, got: {output:?}"
    );

    let row = lines[1];
    assert!(
        row.chars().count() <= 80,
        "expected the row to be at most 80 characters wide, got {} chars: {row:?}",
        row.chars().count()
    );
}

/// Proves the width-cap half of the fix (D-2..D-6): a single-line but far
/// over-budget description must be cut to fit 80 columns and end in a
/// single ellipsis character, without panicking on the multi-byte em dash
/// sitting near the expected cut point (D-6 -- char-based, not byte-based,
/// truncation).
#[tokio::test]
async fn over_budget_description_is_truncated_with_an_ellipsis() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "bfix.md",
        r#"---
id: bfix
name: B Fixture
parameters: {}
service:
  type: action
  handler: demo.bfix
---
This description contains many ordinary words before reaching an em dash — positioned deliberately close to where the eighty column truncation boundary is expected to fall, so multi-byte character handling is exercised near the cut rather than somewhere in the middle of the text that would never trigger a boundary edge case worth testing.
"#,
    );

    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect the test WsOrchestratorClient to the in-process daemon");

    let mut buf = Vec::new();
    list::run(&client, &mut buf)
        .await
        .expect("list::run should not error on valid fixtures");
    let output = String::from_utf8(buf).expect("captured output was not valid UTF-8");

    let lines: Vec<&str> = output.lines().collect();
    let row = lines.get(1).expect("expected a data row in stdout");

    assert!(
        row.chars().count() <= 80,
        "expected the row to be at most 80 characters wide, got {} chars: {row:?}",
        row.chars().count()
    );
    assert!(
        row.ends_with('…'),
        "expected the truncated row to end with the ellipsis character, got: {row:?}"
    );
    assert!(
        row.contains("This description contains many ordinary words"),
        "expected the row to preserve the description's opening words, got: {row:?}"
    );
}

/// Regression guard (the fits-budget pass-through case, D-5): a description
/// that already fits its column's budget must cross through byte-identical
/// -- no ellipsis, no dropped characters, no rewritten internal spacing.
/// Modeled on `workflows/countdown.md`'s short one-sentence shape. Unlike
/// the two tests above, this one must PASS against today's unmodified
/// `list.rs` too -- it is the guard proving the fix does not over-reach.
#[tokio::test]
async fn description_that_fits_is_rendered_unchanged() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "cd.md",
        r#"---
id: cd
name: CD
parameters: {}
service:
  type: action
  handler: demo.cd
---
Counts down and announces when time is up.
"#,
    );
    write_workflow(
        dir.path(),
        "tm.md",
        r#"---
id: tm
name: TM
parameters: {}
service:
  type: action
  handler: demo.tm
---
A second short fixture workflow.
"#,
    );

    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect the test WsOrchestratorClient to the in-process daemon");

    let mut buf = Vec::new();
    list::run(&client, &mut buf)
        .await
        .expect("list::run should not error on valid fixtures");
    let output = String::from_utf8(buf).expect("captured output was not valid UTF-8");

    assert!(
        output.contains("Counts down and announces when time is up."),
        "expected the short description to appear verbatim in stdout, got: {output:?}"
    );
    assert!(
        !output.contains('…'),
        "expected no ellipsis anywhere in stdout when descriptions fit, got: {output:?}"
    );

    let row = output
        .lines()
        .find(|line| line.contains("Counts down"))
        .expect("expected to find the cd row in stdout");
    assert!(
        row.ends_with("Counts down and announces when time is up."),
        "expected the row to end with the description's exact final characters, got: {row:?}"
    );
}
