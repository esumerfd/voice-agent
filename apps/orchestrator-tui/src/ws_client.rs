//! WS-backed `TuiWsClient` (D-01/D-04/D-10): the `orchestrator-tui`
//! counterpart to `orchestrator-cli/src/ws_client.rs`, adapted for two
//! Phase-5 differences: (1) it sends `Envelope::Hello { client_name:
//! "orchestrator-tui" }` immediately after connect, before any `Req`
//! (D-04); and (2) it actually consumes the server-push channel the CLI
//! left unused (`#[allow(dead_code)]`) -- every `Envelope::Activity` frame
//! is forwarded onto an unbounded `mpsc` channel the render loop drains
//! (05-07). Depends only on `shared::` for cross-crate types (D-10) --
//! never `orchestrator::`.
//!
//! Connect/demux/correlate shape mirrors `orchestrator-cli/src/ws_client.rs`
//! exactly: a spawned read-loop task owns the WS read half for the
//! lifetime of this client, a `Res` resolves the matching pending
//! `oneshot` by `id`, and every decode/send failure is a handled no-op
//! (never `.unwrap()`/`.expect()`), matching this project's established
//! never-panic-on-external-input discipline.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use shared::{
    ActivityEvent, CreateWorkflowRequest, CreateWorkflowResponse, DescribeWorkflowRequest,
    DescribeWorkflowResponse, Envelope, IntentCollisionChecker, IntentCollisionReport, InvokeStatus,
    InvokeWorkflowRequest, InvokeWorkflowResponse, ListWorkflowsRequest, ListWorkflowsResponse,
    OrchestratorClient, ProtocolFrame, RequestPayload, ResponsePayload, WorkflowCreator,
};

/// This client's self-reported identity (D-04) -- must be the first frame
/// sent per connection, before any `Req`.
const CLIENT_NAME: &str = "orchestrator-tui";

/// The write half of the one WS connection this client owns.
type WsWrite = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

/// A WS-backed client (D-01/D-04/D-10): connects to `orchestratord`,
/// identifies itself via `Hello`, and demuxes every incoming frame -- `Res`
/// resolves a pending request/response call, `Activity` forwards onto the
/// activity-events channel the render loop (05-07) drains.
pub struct TuiWsClient {
    write: Arc<AsyncMutex<WsWrite>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ResponsePayload>>>>,
    /// Pending `ProtocolFrame` calls (Phase 10, plan 10-04), mirroring
    /// `orchestrator-cli/src/ws_client.rs`'s `pending_protocol` exactly --
    /// SEPARATE from `pending` above even though both share `next_id`, so a
    /// `Res` and a `Protocol` reply can never contend for the same slot
    /// (the 09-03 frame-id correlation hazard: a server-pushed `Welcome` at
    /// id 0 could otherwise resolve a pending call).
    pending_protocol: Arc<Mutex<HashMap<u64, oneshot::Sender<ProtocolFrame>>>>,
    next_id: AtomicU64,
    /// Server-pushed `Activity` events (D-01/D-03), forwarded here by the
    /// read loop. Unlike the CLI's unconsumed `events` field, this is the
    /// one channel this whole client exists to expose.
    activity_rx: Arc<AsyncMutex<mpsc::UnboundedReceiver<ActivityEvent>>>,
}

impl TuiWsClient {
    /// Connects to `addr` (e.g. `ws://127.0.0.1:47100`), immediately sends
    /// `Envelope::Hello { client_name: "orchestrator-tui" }` before any
    /// other frame (D-04), and spawns the one read-loop task that owns the
    /// read half for the lifetime of this client. Returns `Err` if the
    /// connection cannot be established -- `main.rs` surfaces that as the
    /// D-04 "start orchestratord" message, never auto-spawning the daemon
    /// itself.
    pub async fn connect(addr: &str) -> Result<Self, tokio_tungstenite::tungstenite::Error> {
        let (ws_stream, _response) = tokio_tungstenite::connect_async(addr).await?;
        let (write, mut read) = ws_stream.split();
        let write = Arc::new(AsyncMutex::new(write));

        // D-04: Hello must be the very first frame this client sends,
        // before any Req. Follows the same fallible-encode-then-send
        // discipline as `call` (never `.unwrap()`) -- a failure here is a
        // handled no-op, not a panic; the connection simply proceeds
        // unidentified (server-side degrades to "unknown", Pitfall 3).
        // Phase 8 (D-03/D-04): `orchestrator-tui` declares NO capabilities
        // in this phase -- it renders exclusively from ungated fields.
        // Phase 11's voice front end is the first client that will declare
        // one (e.g. `"speech"`).
        let hello = Envelope::Hello {
            client_name: CLIENT_NAME.to_string(),
            capabilities: Vec::new(),
        };
        if let Ok(text) = serde_json::to_string(&hello) {
            let mut guard = write.lock().await;
            let _ = guard.send(Message::text(text)).await;
        }

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ResponsePayload>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_protocol: Arc<Mutex<HashMap<u64, oneshot::Sender<ProtocolFrame>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (activity_tx, activity_rx) = mpsc::unbounded_channel();

        let loop_pending = Arc::clone(&pending);
        let loop_pending_protocol = Arc::clone(&pending_protocol);
        tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                let Ok(msg) = msg else {
                    break; // connection error -- the loop ends; any still-pending calls hang until dropped
                };
                let Ok(text) = msg.to_text() else {
                    continue; // non-text frame -- never fatal, just ignored
                };
                let Ok(envelope) = serde_json::from_str::<Envelope>(text) else {
                    continue; // malformed frame from the daemon -- ignored, never a panic
                };

                match envelope {
                    Envelope::Res { id, payload } => {
                        let sender = {
                            let mut guard =
                                loop_pending.lock().expect("ws_client pending mutex poisoned");
                            guard.remove(&id)
                        };
                        if let Some(sender) = sender {
                            let _ = sender.send(payload); // caller may have given up already -- a handled no-op
                        }
                    }
                    Envelope::Activity { event } => {
                        // Unbounded -- the read loop's forward never blocks
                        // regardless of whether the app is draining yet.
                        // This is the channel RESEARCH says orchestrator-tui
                        // must finally consume (unlike the CLI's
                        // `#[allow(dead_code)]` events field).
                        let _ = activity_tx.send(event);
                    }
                    Envelope::Event { .. } => {
                        // Reserved server-push channel (D-03); this client
                        // renders activity state exclusively via the newer
                        // Activity frame, so a stray Event is ignored
                        // defensively rather than treated as unexpected.
                    }
                    Envelope::Req { .. } | Envelope::Hello { .. } => {
                        // orchestratord never sends a Req/Hello to a client; ignore defensively.
                    }
                    // Phase 10, plan 10-04 (T-10-19, the 09-03 frame-id
                    // correlation hazard): resolve a pending `call_protocol`
                    // entry ONLY for reply-shaped variants. `Welcome` is
                    // server-push-only and arrives with id 0 as the very
                    // FIRST server-to-client frame on every connection,
                    // before any request is ever made -- if it resolved a
                    // pending entry by id alone, it would silently steal a
                    // call issued as the connection's very first request.
                    // `DescribeRun`/`RouteUtterance`/`CheckIntentCollision`
                    // are client-to-daemon only and never arrive here.
                    Envelope::Protocol { id, frame } => match frame {
                        ProtocolFrame::RouteResult { .. }
                        | ProtocolFrame::RunDescription { .. }
                        | ProtocolFrame::IntentCollisionResult { .. } => {
                            let sender = {
                                let mut guard = loop_pending_protocol
                                    .lock()
                                    .expect("ws_client pending_protocol mutex poisoned");
                                guard.remove(&id)
                            };
                            if let Some(sender) = sender {
                                let _ = sender.send(frame); // caller may have given up already -- a handled no-op
                            }
                        }
                        ProtocolFrame::Welcome { .. }
                        | ProtocolFrame::DescribeRun { .. }
                        | ProtocolFrame::RouteUtterance { .. }
                        | ProtocolFrame::CheckIntentCollision { .. } => {
                            // Server-push-only (Welcome) or client-to-daemon-only
                            // (DescribeRun/RouteUtterance/CheckIntentCollision) --
                            // never resolves a pending call. Ignored defensively.
                        }
                    },
                }
            }
        });

        Ok(Self {
            write,
            pending,
            pending_protocol,
            next_id: AtomicU64::new(0),
            activity_rx: Arc::new(AsyncMutex::new(activity_rx)),
        })
    }

    /// Accessor for 05-07's render loop to drain server-pushed `Activity`
    /// events -- the channel RESEARCH says this client must finally
    /// consume (unlike the CLI's `#[allow(dead_code)]` events field).
    /// Returns `None` once the read loop has ended and the channel closed.
    /// Not yet called by this plan's `main.rs` (which uses the non-blocking
    /// `try_drain_activity_events` below instead ahead of 05-07's real
    /// render loop) -- exercised today by `tests/ws_client_integration.rs`.
    #[allow(dead_code)]
    pub async fn next_activity_event(&self) -> Option<ActivityEvent> {
        self.activity_rx.lock().await.recv().await
    }

    /// Non-blocking drain of whatever `Activity` events have already
    /// arrived (e.g. an initial history-replay burst) -- never blocks
    /// waiting for more. Called by this plan's `main.rs` (production);
    /// `#[allow(dead_code)]` covers the separate `tests/ws_client_integration.rs`
    /// compilation of this same file (via `#[path]`, mirroring the CLI's
    /// established pattern), which only exercises `next_activity_event`.
    #[allow(dead_code)]
    pub async fn try_drain_activity_events(&self) -> Vec<ActivityEvent> {
        let mut guard = self.activity_rx.lock().await;
        let mut events = Vec::new();
        while let Ok(event) = guard.try_recv() {
            events.push(event);
        }
        events
    }

    /// Sends `payload` as a fresh `Envelope::Req`, suspends on a `oneshot`,
    /// and returns the correlated `Res` payload once the read loop resolves
    /// it. On any send failure this returns a synthesized failure payload
    /// rather than panicking -- callers downcast the expected variant and
    /// treat an unexpected one as their own failure/empty shape.
    async fn call(&self, payload: RequestPayload) -> ResponsePayload {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("ws_client pending mutex poisoned")
            .insert(id, tx);

        let envelope = Envelope::Req { id, payload };
        let Ok(text) = serde_json::to_string(&envelope) else {
            self.pending
                .lock()
                .expect("ws_client pending mutex poisoned")
                .remove(&id);
            return failure_payload("failed to encode request envelope");
        };

        let send_result = {
            let mut guard = self.write.lock().await;
            guard.send(Message::text(text)).await
        };
        if send_result.is_err() {
            self.pending
                .lock()
                .expect("ws_client pending mutex poisoned")
                .remove(&id);
            return failure_payload("failed to send request over the WS connection");
        }

        rx.await
            .unwrap_or_else(|_| failure_payload("connection closed before a response arrived"))
    }

    /// Sends `frame` as a fresh `Envelope::Protocol`, suspends on a
    /// `oneshot`, and returns the correlated reply `ProtocolFrame` once the
    /// read loop resolves it (Phase 10, plan 10-04). Mirrors `call`'s
    /// structure exactly, and mirrors `orchestrator-cli/src/ws_client.rs`'s
    /// `call_protocol`: allocates an id from the SAME `next_id` counter
    /// `call` uses -- sharing it keeps ids globally unique on the
    /// connection, so a `Res` and a `Protocol` reply can never contend for
    /// the same slot. On any encode or send failure, removes the pending
    /// entry and returns `Err` naming the failure -- never a panic.
    pub async fn call_protocol(&self, frame: ProtocolFrame) -> Result<ProtocolFrame, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending_protocol
            .lock()
            .expect("ws_client pending_protocol mutex poisoned")
            .insert(id, tx);

        let envelope = Envelope::Protocol { id, frame };
        let Ok(text) = serde_json::to_string(&envelope) else {
            self.pending_protocol
                .lock()
                .expect("ws_client pending_protocol mutex poisoned")
                .remove(&id);
            return Err("failed to encode protocol request envelope".to_string());
        };

        let send_result = {
            let mut guard = self.write.lock().await;
            guard.send(Message::text(text)).await
        };
        if send_result.is_err() {
            self.pending_protocol
                .lock()
                .expect("ws_client pending_protocol mutex poisoned")
                .remove(&id);
            return Err("failed to send protocol request over the WS connection".to_string());
        }

        rx.await
            .map_err(|_| "connection closed before a response arrived".to_string())
    }
}

/// Synthesizes a failure-shaped `InvokeWorkflow` response for a transport
/// failure that has no real workflow context -- callers expecting a
/// different response variant treat this as their own unexpected-variant
/// case and render their own type's failure/empty form instead (never a
/// panic).
fn failure_payload(detail: &str) -> ResponsePayload {
    ResponsePayload::InvokeWorkflow(InvokeWorkflowResponse {
        status: InvokeStatus::Failed,
        output: None,
        error: Some(detail.to_string()),
        run_id: None,
    })
}

#[async_trait]
impl OrchestratorClient for TuiWsClient {
    async fn list_workflows(&self, req: ListWorkflowsRequest) -> ListWorkflowsResponse {
        match self.call(RequestPayload::ListWorkflows(req)).await {
            ResponsePayload::ListWorkflows(resp) => resp,
            other => ListWorkflowsResponse {
                workflows: Vec::new(),
                warnings: vec![format!(
                    "unexpected response payload from orchestratord: {other:?}"
                )],
            },
        }
    }

    async fn invoke_workflow(&self, req: InvokeWorkflowRequest) -> InvokeWorkflowResponse {
        match self.call(RequestPayload::InvokeWorkflow(req)).await {
            ResponsePayload::InvokeWorkflow(resp) => resp,
            other => InvokeWorkflowResponse {
                status: InvokeStatus::Failed,
                output: None,
                error: Some(format!(
                    "unexpected response payload from orchestratord: {other:?}"
                )),
                run_id: None,
            },
        }
    }

    async fn describe_workflow(&self, req: DescribeWorkflowRequest) -> DescribeWorkflowResponse {
        match self.call(RequestPayload::DescribeWorkflow(req)).await {
            ResponsePayload::DescribeWorkflow(resp) => resp,
            _ => DescribeWorkflowResponse {
                found: false,
                parameters: Vec::new(),
            },
        }
    }
}

/// `TuiWsClient`'s first write capability (Phase 10, plan 10-04, D-01): the
/// TUI's wizard reaches the daemon's registry writer through this SAME
/// `WorkflowCreator` seam the CLI uses -- never a bespoke wire call. Wraps
/// `call(RequestPayload::CreateWorkflow(req))` exactly like
/// `orchestrator-cli/src/ws_client.rs`'s own impl.
#[async_trait]
impl WorkflowCreator for TuiWsClient {
    async fn create_workflow(&self, req: CreateWorkflowRequest) -> CreateWorkflowResponse {
        match self.call(RequestPayload::CreateWorkflow(req)).await {
            ResponsePayload::CreateWorkflow(resp) => resp,
            other => CreateWorkflowResponse {
                created: false,
                workflow_path: None,
                script_path: None,
                error: Some(format!(
                    "unexpected response payload from orchestratord: {other:?}"
                )),
            },
        }
    }
}

/// `TuiWsClient`'s `IntentCollisionChecker` implementation (Phase 10, plan
/// 10-04, D-03/D-04): wraps `call_protocol` exactly like
/// `orchestrator-cli/src/ws_client.rs`'s own impl. An unexpected reply
/// variant and a transport `Err` alike map into a report with all three
/// collision fields `None` and the failure text in `detail` -- never a
/// panic, matching this file's established `failure_payload` discipline.
#[async_trait]
impl IntentCollisionChecker for TuiWsClient {
    async fn check_intent_collision(&self, intent: &str) -> IntentCollisionReport {
        match self
            .call_protocol(ProtocolFrame::CheckIntentCollision {
                intent: intent.to_string(),
            })
            .await
        {
            Ok(ProtocolFrame::IntentCollisionResult {
                colliding_workflow_id,
                colliding_intent,
                similarity_score,
                detail,
                ..
            }) => IntentCollisionReport {
                colliding_workflow_id,
                colliding_intent,
                similarity_score,
                detail,
            },
            Ok(other) => IntentCollisionReport {
                colliding_workflow_id: None,
                colliding_intent: None,
                similarity_score: None,
                detail: Some(format!("unexpected response frame from orchestratord: {other:?}")),
            },
            Err(err) => IntentCollisionReport {
                colliding_workflow_id: None,
                colliding_intent: None,
                similarity_score: None,
                detail: Some(err),
            },
        }
    }
}
