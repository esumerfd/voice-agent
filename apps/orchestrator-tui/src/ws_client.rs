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
    ActivityEvent, DescribeWorkflowRequest, DescribeWorkflowResponse, Envelope, InvokeStatus,
    InvokeWorkflowRequest, InvokeWorkflowResponse, ListWorkflowsRequest, ListWorkflowsResponse,
    OrchestratorClient, RequestPayload, ResponsePayload,
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
        let hello = Envelope::Hello {
            client_name: CLIENT_NAME.to_string(),
        };
        if let Ok(text) = serde_json::to_string(&hello) {
            let mut guard = write.lock().await;
            let _ = guard.send(Message::text(text)).await;
        }

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ResponsePayload>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (activity_tx, activity_rx) = mpsc::unbounded_channel();

        let loop_pending = Arc::clone(&pending);
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
                }
            }
        });

        Ok(Self {
            write,
            pending,
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
