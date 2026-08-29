//! WS transport server (D-01/D-03): a thin envelope-decode ->
//! `InProcessOrchestrator` method call -> envelope-encode wrapper around the
//! existing dispatch/registry seam. Never re-implements validation/dispatch
//! (RESEARCH Pitfall 3) -- every request is routed straight to
//! `InProcessOrchestrator`'s existing trait methods. This module never binds
//! any address itself and contains no interface-binding literal of any kind
//! (all-interfaces or loopback) -- the caller (tests here, `orchestratord`'s
//! `main` in production, T-04-05) owns the bind decision and hands `serve`
//! an already-bound `TcpListener`.
//!
//! The one genuinely new piece of logic (RESEARCH Pattern 4): the
//! `InvokeWorkflow` request handler branches on the resolved workflow's
//! `service.mode` (D-10) -- `Sync` awaits `dispatch()` inline via the
//! existing `invoke_workflow` call and replies once with the terminal
//! result; `Async` replies immediately with a `Started` ack + `run_id`
//! (D-08/D-09), then `tokio::spawn`s the same `invoke_workflow` call and
//! delivers its terminal result later as an `Envelope::Event`. `dispatch()`
//! itself is never touched.
//!
//! Phase 5 (plan 05-04) turns this from a set of isolated per-connection
//! handlers into a fan-out hub (D-01/D-03/D-04): `handle_connection` now
//! reads exactly one frame before the general dispatch loop and requires it
//! be `Envelope::Hello` (Pitfall 3) -- a missing/malformed/oversized Hello
//! degrades leniently to a `"unknown"`-labeled but fully functional
//! connection, never fatal (Security Domain V5, T-05-07). Every connection
//! registers a per-connection `mpsc::UnboundedSender<Envelope>` in the new
//! `ConnectionRegistry` (`connection_registry.rs`), and the async-mode
//! terminal-result push now broadcasts to every registered connection
//! instead of writing only to the connection that started the run -- "every
//! client sees every activity" (D-01). `deregister` runs unconditionally on
//! every connection-exit path (Pitfall 2), never one call per `break` arm.
//!
//! Phase 5 (plan 05-05) wires the `ActivityRegistry` (05-03) into both
//! `InvokeWorkflow` arms: `mint_run_id`/`record_transition` replace the
//! prior per-connection envelope-id-derived scheme entirely (Pitfall 5) --
//! `run_id` is now server-owned and collision-free for both `Sync` and
//! `Async` (D-02). Every transition broadcasts an `Envelope::Activity`
//! snapshot to every connected client via `ConnectionRegistry::broadcast`
//! (D-01), and `handle_connection` replays the registry's current snapshot
//! to a newly-connecting client immediately after Hello, BEFORE that
//! connection is registered for live broadcast (RESEARCH Pattern 2).

mod connection_registry;

use std::sync::Arc;

use futures_util::lock::Mutex;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use connection_registry::ConnectionRegistry;

use crate::activity::ActivityRegistry;
use crate::client::InProcessOrchestrator;
use crate::definition::ServiceMode;
use crate::error::ServerError;
use shared::{
    ActivityPhase, Envelope, InvokeStatus, InvokeWorkflowResponse, OrchestratorClient,
    RequestPayload, ResponsePayload, WorkflowCreator, WorkflowDeleter,
};

/// The write half of one accepted WS connection, shared (via `Arc<Mutex<..>>`)
/// between the read loop's own reply path, this connection's own
/// registry-drain task, and any `tokio::spawn`'d async-mode event-push task
/// (RESEARCH Pattern 4's "both write to the same connection" requirement).
type WsWrite = SplitSink<WebSocketStream<TcpStream>, Message>;
type WsRead = SplitStream<WebSocketStream<TcpStream>>;

/// D-04's `client_name` is a short, plain self-reported string (e.g.
/// `"orchestrator-cli"` / `"orchestrator-tui"`). An over-long value is a
/// potential resource-exhaustion vector (T-05-07, ASVS V5) -- reject it
/// outright rather than storing/broadcasting an unbounded string; the
/// caller falls back to the same `"unknown"` label used for a
/// missing/malformed Hello.
const MAX_CLIENT_NAME_LEN: usize = 256;

/// The client-identity label used whenever the first frame on a connection
/// is missing, unparseable, not `Hello`, or an oversized `Hello` (Pitfall 3,
/// T-05-07) -- a deliberate lenient degrade, never fatal to the connection.
const UNKNOWN_CLIENT: &str = "unknown";

/// Accept loop (Pattern 2, D-07): bind once via the caller-supplied,
/// already-bound listener, spawn one task per connection so a single bad
/// client can never affect another (T-04-01). Owns the single
/// `ConnectionRegistry` shared by every connection this daemon instance
/// accepts (D-01); `activity_registry` (05-03) is caller-owned (the daemon
/// binary constructs it from its 7-day on-disk store, 05-05) and shared
/// read-only-by-reference across every connection.
pub async fn serve(
    listener: TcpListener,
    orchestrator: Arc<InProcessOrchestrator>,
    activity_registry: Arc<ActivityRegistry>,
) {
    let registry = ConnectionRegistry::default();
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            // A transient accept error affects this one attempt only -- the
            // daemon keeps serving (mirrors T-04-01's per-connection
            // isolation one level up).
            Err(_) => continue,
        };
        let orchestrator = Arc::clone(&orchestrator);
        let registry = registry.clone();
        let activity_registry = Arc::clone(&activity_registry);
        tokio::spawn(handle_connection(stream, orchestrator, registry, activity_registry));
    }
}

/// Owns one accepted connection end-to-end: WS handshake, Hello-first read,
/// activity-history replay burst, decode loop, per-request routing, and
/// unconditional registry cleanup on every exit path. A failure anywhere in
/// this function affects only this connection -- it is always run inside
/// its own `tokio::spawn`.
async fn handle_connection(
    stream: TcpStream,
    orchestrator: Arc<InProcessOrchestrator>,
    registry: ConnectionRegistry,
    activity_registry: Arc<ActivityRegistry>,
) {
    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(_) => return, // failed WS handshake -- this connection only
    };
    let (write, mut read) = ws_stream.split();
    let write: Arc<Mutex<WsWrite>> = Arc::new(Mutex::new(write));

    // Hello-first read (D-03/D-04, Pitfall 3): read exactly one frame
    // before the general dispatch loop. If it decoded to something other
    // than Hello, don't discard it -- process it as a normal request below,
    // just under the "unknown" client label.
    let (client_name, pending) = read_hello(&mut read).await;

    // Replay burst (D-01, RESEARCH Pattern 2): send the registry's current
    // activity snapshot to THIS connection's own write path before it is
    // registered for live broadcast below -- a newly-connecting client
    // always sees history before any live update, never the reverse.
    for event in activity_registry.snapshot_as_events() {
        send_frame(&write, &event).await;
    }

    let conn_id = connection_registry::next_conn_id();
    let (tx, mut rx) = mpsc::unbounded_channel::<Envelope>();
    registry.register(conn_id, client_name.clone(), tx).await;

    // Per-connection drain task (RESEARCH Pattern 1): forwards anything
    // broadcast to this connection into its own write half. A slow drain
    // here only grows this connection's own mpsc queue -- it never blocks
    // delivery to any other connection (Pitfall 1).
    let drain_write = Arc::clone(&write);
    let drain_task = tokio::spawn(async move {
        while let Some(env) = rx.recv().await {
            send_frame(&drain_write, &env).await;
        }
    });

    if let Some(envelope) = pending {
        dispatch_envelope(envelope, &orchestrator, &write, &registry, &activity_registry, &client_name).await;
    }

    while let Some(msg) = read.next().await {
        let msg = match msg {
            Ok(msg) => msg,
            Err(_) => break, // this connection is done; others are unaffected
        };

        if msg.is_close() {
            break;
        }

        let Ok(text) = msg.to_text() else {
            continue; // non-text/non-UTF8 frame -- not fatal, just ignored
        };

        if text.trim().is_empty() {
            let detail = ServerError::EmptyFrame.to_string();
            send_error_res(&write, 0, detail).await;
            continue;
        }

        let envelope: Envelope = match serde_json::from_str::<Envelope>(text) {
            Ok(envelope) => envelope,
            Err(err) => {
                // Never `.unwrap()` on frame decode (T-04-01/V5) -- a
                // malformed frame errors this connection's request only, the
                // read loop continues serving further frames.
                let detail = ServerError::MalformedFrame {
                    detail: err.to_string(),
                }
                .to_string();
                send_error_res(&write, 0, detail).await;
                continue;
            }
        };

        dispatch_envelope(envelope, &orchestrator, &write, &registry, &activity_registry, &client_name).await;
    }

    // Unconditional cleanup (Pitfall 2): every exit path above (`break` on
    // read error, `break` on close, or the loop simply ending when the
    // stream ends) funnels through this single post-loop cleanup -- never
    // one call per `break` arm, so the registry never leaks a dead entry.
    registry.deregister(conn_id).await;
    drain_task.abort();
}

/// Reads exactly one frame before the general dispatch loop and requires it
/// be `Envelope::Hello` (Pitfall 3). The returned `client_name` defaults to
/// `"unknown"` whenever the first frame is missing, unparseable, an
/// oversized Hello, or anything other than Hello -- a deliberate lenient
/// degrade (RESEARCH Open Question 1), never fatal to the connection. If
/// the first frame decoded successfully as something other than `Hello`, it
/// is returned so the caller can still process it as a normal request
/// instead of silently discarding it (this is what keeps every pre-Phase-5
/// test -- which never sends a Hello at all -- working unchanged).
async fn read_hello(read: &mut WsRead) -> (String, Option<Envelope>) {
    let Some(Ok(msg)) = read.next().await else {
        return (UNKNOWN_CLIENT.to_string(), None);
    };

    if msg.is_close() {
        return (UNKNOWN_CLIENT.to_string(), None);
    }

    let Ok(text) = msg.to_text() else {
        return (UNKNOWN_CLIENT.to_string(), None);
    };

    match serde_json::from_str::<Envelope>(text) {
        Ok(Envelope::Hello { client_name }) => match bound_client_name(client_name) {
            Some(name) => (name, None),
            None => (UNKNOWN_CLIENT.to_string(), None),
        },
        Ok(other) => (UNKNOWN_CLIENT.to_string(), Some(other)),
        Err(_) => (UNKNOWN_CLIENT.to_string(), None),
    }
}

/// Bounds a self-reported `client_name` to a sane maximum length (T-05-07,
/// ASVS V5). Rejecting (rather than truncating) an over-long name keeps the
/// "missing/oversized Hello is a handled no-op labeled unknown" contract
/// uniform across both failure shapes.
fn bound_client_name(client_name: String) -> Option<String> {
    if client_name.chars().count() > MAX_CLIENT_NAME_LEN {
        None
    } else {
        Some(client_name)
    }
}

/// Routes one decoded envelope after the Hello-first step -- shared by both
/// the pending-first-frame path (in `handle_connection`) and the general
/// read loop, so the two code paths can never drift apart.
async fn dispatch_envelope(
    envelope: Envelope,
    orchestrator: &Arc<InProcessOrchestrator>,
    write: &Arc<Mutex<WsWrite>>,
    registry: &ConnectionRegistry,
    activity_registry: &Arc<ActivityRegistry>,
    client_name: &str,
) {
    match envelope {
        Envelope::Req { id, payload } => {
            handle_request(id, payload, orchestrator, write, registry, activity_registry, client_name).await;
        }
        // A client is only ever expected to send `Req` frames; a stray
        // `Res`/`Event` from a client is a protocol misuse, not a fatal
        // condition -- answer with a graceful error, keep serving.
        Envelope::Res { id, .. } => {
            let detail = ServerError::UnexpectedFrameType {
                got: "res".to_string(),
            }
            .to_string();
            send_error_res(write, id, detail).await;
        }
        Envelope::Event { id, .. } => {
            let detail = ServerError::UnexpectedFrameType {
                got: "event".to_string(),
            }
            .to_string();
            send_error_res(write, id, detail).await;
        }
        // A Hello arriving anywhere other than the very first frame is a
        // protocol misuse (the handshake already happened, or was already
        // leniently skipped) -- graceful error, keep serving, same
        // treatment as the other stray-frame-type arms above.
        Envelope::Hello { .. } => {
            let detail = ServerError::UnexpectedFrameType {
                got: "hello".to_string(),
            }
            .to_string();
            send_error_res(write, 0, detail).await;
        }
        // A client must never send `Activity` (server-push-only,
        // D-01/D-03) -- same graceful-error, keep-serving treatment.
        Envelope::Activity { .. } => {
            let detail = ServerError::UnexpectedFrameType {
                got: "activity".to_string(),
            }
            .to_string();
            send_error_res(write, 0, detail).await;
        }
    }
}

/// Routes one decoded `Envelope::Req` by `RequestPayload` variant, calling
/// straight into `InProcessOrchestrator`'s existing trait methods (Pitfall
/// 3 -- never `Registry::load`/`validate_payload` directly here).
async fn handle_request(
    id: u64,
    payload: RequestPayload,
    orchestrator: &Arc<InProcessOrchestrator>,
    write: &Arc<Mutex<WsWrite>>,
    registry: &ConnectionRegistry,
    activity_registry: &Arc<ActivityRegistry>,
    client_name: &str,
) {
    match payload {
        // Quick task 260807-shx Task 3: a thin decode-call-encode wrapper
        // around `InProcessOrchestrator::create_workflow` (`WorkflowCreator`),
        // consistent with this module's stated discipline of never
        // re-implementing registry logic in the transport layer -- the
        // daemon's own `workflows_dir` is the sole write target, never
        // anything derived from the request. Not a run: no activity
        // transition, no per-run log line.
        RequestPayload::CreateWorkflow(req) => {
            let resp = orchestrator.create_workflow(req).await;
            send_res(write, id, ResponsePayload::CreateWorkflow(resp)).await;
        }
        // Quick task 260812-qp4: a thin decode-call-encode wrapper around
        // `InProcessOrchestrator::delete_workflow` (`WorkflowDeleter`),
        // mirroring the `CreateWorkflow` arm above exactly. Not a run: no
        // activity transition and no per-run log line, matching create.
        RequestPayload::DeleteWorkflow(req) => {
            let resp = orchestrator.delete_workflow(req).await;
            send_res(write, id, ResponsePayload::DeleteWorkflow(resp)).await;
        }
        RequestPayload::ListWorkflows(req) => {
            let resp = orchestrator.list_workflows(req).await;
            send_res(write, id, ResponsePayload::ListWorkflows(resp)).await;
        }
        RequestPayload::DescribeWorkflow(req) => {
            let resp = orchestrator.describe_workflow(req).await;
            send_res(write, id, ResponsePayload::DescribeWorkflow(resp)).await;
        }
        RequestPayload::InvokeWorkflow(req) => {
            // Captured before `req` is moved into `invoke_workflow` below in
            // both arms -- used only by the per-run structured log line
            // (quick task 260719-foq), never by dispatch/validation logic.
            let workflow_id = req.workflow_id.clone();

            // Resolve service.mode (D-10) via a plain Registry lookup --
            // never a re-implementation of validate_payload/dispatch
            // (Pitfall 3); `invoke_workflow` below is the only call that
            // ever actually dispatches.
            match orchestrator.resolve_service_mode(&req.workflow_id) {
                ServiceMode::Sync => {
                    // Sync (Pattern 4/D-02): mint a server-owned run_id and
                    // record+broadcast the Invoked transition BEFORE
                    // dispatch, matching the async arm below -- a sync run
                    // is tracked from the same moment an async one is, it
                    // just never visibly passes through Running (05-03's
                    // own documented decision).
                    let run_id = activity_registry.mint_run_id(&req.workflow_id, client_name);
                    broadcast_activity(registry, activity_registry, &run_id).await;

                    // Await dispatch() inline via the existing
                    // invoke_workflow call and reply once with the terminal
                    // result -- matches today's in-process behavior.
                    let mut resp = orchestrator.invoke_workflow(req).await;

                    let (phase, detail) = phase_and_detail_for(&resp);
                    activity_registry.record_transition(&run_id, phase, detail);
                    broadcast_activity(registry, activity_registry, &run_id).await;

                    // One structured stderr log line per run (quick task
                    // 260719-foq), reusing the timestamps just recorded
                    // above -- no second clock read.
                    emit_run_log(orchestrator, activity_registry, &workflow_id, &run_id, phase);

                    // D-02: sync invocations now carry a run_id too, not
                    // just async ones -- a sync run shows up as a tracked
                    // activity in the TUI.
                    resp.run_id = Some(run_id);
                    send_res(write, id, ResponsePayload::InvokeWorkflow(resp)).await;
                }
                ServiceMode::Async => {
                    // Async (Pattern 4/D-08/D-09): mint a server-owned
                    // run_id (D-02/D-05, replacing the collision-prone
                    // envelope-id-derived scheme, Pitfall 5), record+
                    // broadcast Invoked, then ack immediately with Started
                    // + the run_id, never blocking the read loop on
                    // dispatch()'s full poll loop.
                    let run_id = activity_registry.mint_run_id(&req.workflow_id, client_name);
                    broadcast_activity(registry, activity_registry, &run_id).await;

                    let ack = InvokeWorkflowResponse {
                        status: InvokeStatus::Started,
                        output: None,
                        error: None,
                        run_id: Some(run_id.clone()),
                    };
                    send_res(write, id, ResponsePayload::InvokeWorkflow(ack)).await;

                    let orchestrator = Arc::clone(orchestrator);
                    let registry = registry.clone();
                    let activity_registry = Arc::clone(activity_registry);
                    let workflow_id = workflow_id.clone();
                    tokio::spawn(async move {
                        // D-06: Running fires when the spawned task
                        // actually begins, distinct from the Invoked
                        // transition recorded above at request time.
                        activity_registry.record_transition(&run_id, ActivityPhase::Running, None);
                        broadcast_activity(&registry, &activity_registry, &run_id).await;

                        let resp = orchestrator.invoke_workflow(req).await;

                        let (phase, detail) = phase_and_detail_for(&resp);
                        activity_registry.record_transition(&run_id, phase, detail);
                        broadcast_activity(&registry, &activity_registry, &run_id).await;

                        // One structured stderr log line per run (quick task
                        // 260719-foq), reusing the timestamps just recorded
                        // above -- no second clock read.
                        emit_run_log(&orchestrator, &activity_registry, &workflow_id, &run_id, phase);

                        // D-01: broadcast the terminal result to every
                        // connected client, not just the connection that
                        // started this run -- "every client sees every
                        // activity". ConnectionRegistry::broadcast already
                        // treats a send to a disconnected client as a
                        // handled no-op (Pitfall 1), preserving the
                        // pre-existing "a run survives a disconnected
                        // client" guarantee (D-08/T-04-04).
                        broadcast_event(&registry, id, ResponsePayload::InvokeWorkflow(resp)).await;
                    });
                }
            }
        }
    }
}

/// Maps a terminal `InvokeWorkflowResponse` onto the `ActivityPhase`/detail
/// pair `ActivityRegistry::record_transition` expects (D-06): `Completed`
/// carries the structured output, `Failed` carries the error message. Never
/// called with a `Started` response (that status is only ever synthesized
/// by this module's own ack, never returned by `invoke_workflow`), but
/// degrades to a `Running` no-op rather than panicking if it ever were.
fn phase_and_detail_for(resp: &InvokeWorkflowResponse) -> (ActivityPhase, Option<serde_json::Value>) {
    match resp.status {
        InvokeStatus::Completed => (ActivityPhase::Completed, resp.output.clone()),
        InvokeStatus::Failed => (
            ActivityPhase::Failed,
            resp.error.clone().map(|error| serde_json::json!({ "error": error })),
        ),
        InvokeStatus::Started => (ActivityPhase::Running, None),
    }
}

/// Broadcasts the current `ActivityEvent` snapshot for `run_id` to every
/// connected client (D-01), if the registry still knows about it. A no-op
/// for an unknown `run_id` -- mirrors `ActivityRegistry::record_transition`'s
/// own never-panic-on-unknown-run_id discipline.
async fn broadcast_activity(registry: &ConnectionRegistry, activity_registry: &ActivityRegistry, run_id: &str) {
    if let Some(event) = activity_registry.get(run_id) {
        registry.broadcast(&Envelope::Activity { event }).await;
    }
}

/// Encodes and writes an `Envelope::Res`. A send failure (e.g. the peer
/// already disconnected) is a handled no-op, never a panic.
async fn send_res(write: &Arc<Mutex<WsWrite>>, id: u64, payload: ResponsePayload) {
    send_frame(write, &Envelope::Res { id, payload }).await;
}

/// Encodes and broadcasts an `Envelope::Event` to every connected client
/// (D-01) -- the async-mode terminal-result push. Broadcasting (rather than
/// writing only to the originating connection's write half) is what makes
/// "every client sees every activity" true; `ConnectionRegistry::broadcast`
/// already treats a send to a disconnected client as a handled no-op
/// (Pitfall 1), preserving the existing "a run survives a disconnected
/// client" guarantee (D-08/T-04-04).
async fn broadcast_event(registry: &ConnectionRegistry, id: u64, payload: ResponsePayload) {
    registry.broadcast(&Envelope::Event { id, payload }).await;
}

/// Builds a best-effort `InvokeWorkflowResponse`-shaped error `Res` for a
/// decode/routing failure that has no real request `id` to correlate with
/// (frame never decoded, or decoded to something other than a `Req`).
async fn send_error_res(write: &Arc<Mutex<WsWrite>>, id: u64, detail: String) {
    let resp = InvokeWorkflowResponse {
        status: InvokeStatus::Failed,
        output: None,
        error: Some(detail),
        run_id: None,
    };
    send_res(write, id, ResponsePayload::InvokeWorkflow(resp)).await;
}

/// Builds a single-line, key=value structured log record for one workflow
/// run (quick task 260719-foq): `run_id=`, `workflow=`, `success=`,
/// `start_ms=`, `end_ms=`, `source=`. `name`/`source` are rendered with
/// Rust debug (`{:?}`) quoting so spaces are quoted and any embedded
/// control character (including a newline) is escaped -- a crafted
/// workflow name can never forge an extra log line (T-foq-01). Never
/// includes a trailing newline (the `eprintln!` caller adds one).
fn format_run_log_line(name: &str, success: bool, start_ms: u64, end_ms: u64, source: &str, run_id: &str) -> String {
    format!(
        "orchestratord run_id={run_id} workflow={name:?} success={success} start_ms={start_ms} end_ms={end_ms} source={source:?}"
    )
}

/// Emits one structured stderr log line for a just-terminated run (quick
/// task 260719-foq), reusing the `ActivityEvent` timestamps already
/// recorded by `record_transition` -- never a second clock read. A no-op if
/// `activity_registry` no longer knows `run_id` (mirrors
/// `broadcast_activity`'s own never-panic-on-unknown-run_id discipline).
/// `orchestrator.workflow_log_info` resolves the name/source; an unknown
/// workflow id (only reachable for a failed sync run against a since-
/// removed/never-existed workflow) falls back to the raw `workflow_id` as
/// name and `-` as source rather than skipping the log line entirely.
fn emit_run_log(
    orchestrator: &InProcessOrchestrator,
    activity_registry: &ActivityRegistry,
    workflow_id: &str,
    run_id: &str,
    phase: ActivityPhase,
) {
    let Some(event) = activity_registry.get(run_id) else {
        return;
    };

    let start_ms = event.started_at_ms;
    let end_ms = event.log.last().map(|e| e.at_ms).unwrap_or(event.started_at_ms);
    let success = matches!(phase, ActivityPhase::Completed);

    let (name, source) = match orchestrator.workflow_log_info(workflow_id) {
        Some(info) => (info.name, info.source_path.display().to_string()),
        None => (workflow_id.to_string(), "-".to_string()),
    };

    eprintln!("{}", format_run_log_line(&name, success, start_ms, end_ms, &source, run_id));
}

/// The single write chokepoint: encode + write, treating both an encode
/// failure and a `Sink::send` failure as handled no-ops (never `.unwrap()`/
/// `.expect()`) -- required for T-04-01 (malformed frame isolation) and
/// T-04-04 (disconnected-client event push, D-08). Used directly by the
/// per-connection drain task (RESEARCH Pattern 1) in addition to
/// `send_res`/`broadcast_event` above.
async fn send_frame(write: &Arc<Mutex<WsWrite>>, envelope: &Envelope) {
    let Ok(text) = serde_json::to_string(envelope) else {
        return; // an internal encode failure must not panic this task
    };
    let mut guard = write.lock().await;
    let _ = guard.send(Message::text(text)).await;
}

#[cfg(test)]
mod format_run_log_tests {
    use super::format_run_log_line;

    #[test]
    fn completed_run_line_has_all_required_fields_and_no_embedded_newline() {
        let line = format_run_log_line("Set Timer", true, 1_000, 1_500, "workflows/set_timer.md", "run-1");

        assert!(!line.contains('\n'), "log line must be single-line, got: {line:?}");
        assert!(line.contains("success=true"), "expected success=true, got: {line}");
        assert!(line.contains("start_ms=1000"), "expected start_ms=1000, got: {line}");
        assert!(line.contains("end_ms=1500"), "expected end_ms=1500, got: {line}");
        assert!(line.contains("run_id=run-1"), "expected run_id=run-1, got: {line}");
        assert!(line.contains("Set Timer"), "expected the workflow name, got: {line}");
        assert!(line.contains("workflows/set_timer.md"), "expected the source path, got: {line}");
    }

    #[test]
    fn failed_run_line_has_success_false() {
        let line = format_run_log_line("Set Timer", false, 1_000, 1_200, "workflows/set_timer.md", "run-2");

        assert!(line.contains("success=false"), "expected success=false, got: {line}");
    }

    #[test]
    fn embedded_newline_in_name_is_escaped_not_emitted_as_a_real_line_break() {
        let line = format_run_log_line("evil\nname", true, 1_000, 1_100, "workflows/evil.md", "run-3");

        assert!(
            !line.contains('\n'),
            "a crafted workflow name with an embedded newline must never forge an extra log line (T-foq-01), got: {line:?}"
        );
        assert!(
            line.contains("\\n"),
            "expected the embedded newline to be escaped via debug quoting, got: {line:?}"
        );
    }
}
