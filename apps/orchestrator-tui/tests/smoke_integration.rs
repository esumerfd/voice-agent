//! Entrypoint integration test for `orchestrator-tui` (05-06 Task 2).
//!
//! 05-02's original "prints a startup line and exits 0" skeleton behavior
//! is superseded here: `main.rs` now parses `--port` and attempts a real WS
//! connection to `orchestratord`. Its only entrypoint-level behavior worth
//! pinning without a live daemon is the D-04 connect-or-fail path -- Task
//! 1's `ws_client_integration.rs` already proves the transport itself
//! (Hello-first send, Activity forwarding) against a real in-test server.

use std::net::TcpListener as StdTcpListener;
use std::process::Command;

/// Binds an ephemeral loopback port and immediately drops the listener,
/// freeing a port number that was very likely unused a moment ago (mirrors
/// `orchestrator-cli/tests/ws_client_integration.rs`'s
/// `ws_client_connect_to_a_dead_port_returns_err` precedent).
fn reserve_free_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0")
        .expect("failed to bind an ephemeral port to reserve one");
    listener
        .local_addr()
        .expect("failed to read the bound local_addr")
        .port()
    // `listener` drops here, freeing the port.
}

#[test]
fn connect_failure_prints_actionable_message_and_exits_nonzero() {
    let port = reserve_free_port();

    let output = Command::new(env!("CARGO_BIN_EXE_orchestrator-tui"))
        .args(["--port", &port.to_string()])
        .output()
        .expect("failed to run orchestrator-tui binary");

    assert!(
        !output.status.success(),
        "expected a nonzero exit code when orchestratord is unreachable, got: {:?}",
        output.status.code()
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot reach orchestratord"),
        "expected an actionable connect-failure message, got: {stderr:?}"
    );
    assert!(
        stderr.contains(&format!("orchestratord --port {port}")),
        "expected the message to name the orchestratord start command with the requested port, got: {stderr:?}"
    );
}
