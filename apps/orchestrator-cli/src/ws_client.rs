//! WS-backed `OrchestratorClient` implementation (D-01/D-06): the CLI-side
//! counterpart to `orchestrator::server::serve` (built in 04-04). Once
//! `main.rs` constructs one of these, it is the ONLY path `orchestrator-cli`
//! uses to reach the workflow registry -- there is no in-process fallback
//! (D-06 full cutover; see `main.rs` for the D-04 connect-failure handling).
//!
//! Each `list_workflows`/`describe_workflow`/`invoke_workflow` call sends an
//! `Envelope::Req` with a fresh, monotonically increasing `id`, suspends on a
//! `oneshot::Receiver`, and is resolved by a single read-loop task (spawned
//! once, in `connect`) that owns the WS read half and demultiplexes every
//! incoming frame: a `Res` resolves the matching pending `oneshot` by `id`;
//! an `Event` (reserved, D-03) is forwarded to an unbounded events channel
//! instead -- this Phase 4 CLI does not act on events yet, but the channel
//! exists so the read loop's forward never blocks regardless of whether
//! anything drains it.

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
    CreateWorkflowRequest, CreateWorkflowResponse, DeleteWorkflowRequest, DeleteWorkflowResponse,
    DescribeWorkflowRequest, DescribeWorkflowResponse, Envelope, InvokeStatus,
    InvokeWorkflowRequest, InvokeWorkflowResponse, ListWorkflowsRequest, ListWorkflowsResponse,
    OrchestratorClient, RequestPayload, ResponsePayload, WorkflowCreator, WorkflowDeleter,
};

/// The write half of the one WS connection this client owns.
type WsWrite = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

/// A WS-backed `OrchestratorClient` (D-01/D-06): every call round-trips over
/// a loopback WS connection to `orchestratord` using the D-03 envelope.
pub struct WsOrchestratorClient {
    write: Arc<AsyncMutex<WsWrite>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ResponsePayload>>>>,
    next_id: AtomicU64,
    /// Reserved `Event` frames (D-03) forwarded here by the read loop rather
    /// than treated as a pending `Res`. Nothing in Phase 4's CLI consumes
    /// this yet -- the channel is unbounded so the read loop's forward never
    /// blocks regardless of whether anything drains it.
    #[allow(dead_code)]
    events: Arc<AsyncMutex<mpsc::UnboundedReceiver<(u64, ResponsePayload)>>>,
}

impl WsOrchestratorClient {
    /// Connects to `addr` (e.g. `ws://127.0.0.1:47100`) and spawns the one
    /// read-loop task that owns the read half for the lifetime of this
    /// client. Returns `Err` if the connection cannot be established --
    /// `main.rs` surfaces that as the D-04 "start orchestratord" message,
    /// never auto-spawning the daemon itself.
    pub async fn connect(addr: &str) -> Result<Self, tokio_tungstenite::tungstenite::Error> {
        let (ws_stream, _response) = tokio_tungstenite::connect_async(addr).await?;
        let (write, mut read) = ws_stream.split();
        let write = Arc::new(AsyncMutex::new(write));
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ResponsePayload>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (events_tx, events_rx) = mpsc::unbounded_channel();

        let loop_pending = Arc::clone(&pending);
        tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                let Ok(msg) = msg else {
                    break; // connection error -- the loop ends; any still-pending calls hang until dropped
                };
                let Ok(text) = msg.to_text() else {
                    continue; // non-text frame -- never fatal, just ignored (T-04-07)
                };
                let Ok(envelope) = serde_json::from_str::<Envelope>(text) else {
                    continue; // malformed frame from the daemon -- ignored, never a panic (T-04-07)
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
                    Envelope::Event { id, payload } => {
                        let _ = events_tx.send((id, payload)); // unbounded -- never blocks the read loop
                    }
                    Envelope::Req { .. } => {
                        // orchestratord never sends a Req; ignore defensively (T-04-07).
                    }
                    // Placeholder keep-green arms for the two Phase 5
                    // envelope variants (05-01): orchestrator-cli never
                    // sends Hello (that's a client-side send concern, not a
                    // read-loop one) and never receives one; it also never
                    // renders activities. Ignore both defensively, mirroring
                    // the Req no-op arm above -- the real orchestrator-tui
                    // client (05-04) is the one that actually consumes these.
                    Envelope::Hello { .. } => {}
                    Envelope::Activity { .. } => {}
                }
            }
        });

        Ok(Self {
            write,
            pending,
            next_id: AtomicU64::new(0),
            events: Arc::new(AsyncMutex::new(events_rx)),
        })
    }

    /// Sends `payload` as a fresh `Envelope::Req`, suspends on a `oneshot`,
    /// and returns the correlated `Res` payload once the read loop resolves
    /// it. On any send failure this returns a synthesized failure payload
    /// rather than panicking (T-04-07) -- callers downcast the expected
    /// variant and treat an unexpected one as their own failure/empty shape.
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
/// panic, T-04-07).
fn failure_payload(detail: &str) -> ResponsePayload {
    ResponsePayload::InvokeWorkflow(InvokeWorkflowResponse {
        status: InvokeStatus::Failed,
        output: None,
        error: Some(detail.to_string()),
        run_id: None,
    })
}

#[async_trait]
impl OrchestratorClient for WsOrchestratorClient {
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

#[async_trait]
impl WorkflowCreator for WsOrchestratorClient {
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

#[async_trait]
impl WorkflowDeleter for WsOrchestratorClient {
    async fn delete_workflow(&self, req: DeleteWorkflowRequest) -> DeleteWorkflowResponse {
        match self.call(RequestPayload::DeleteWorkflow(req)).await {
            ResponsePayload::DeleteWorkflow(resp) => resp,
            other => DeleteWorkflowResponse {
                deleted: false,
                workflow_path: None,
                script_path: None,
                error: Some(format!(
                    "unexpected response payload from orchestratord: {other:?}"
                )),
            },
        }
    }
}
