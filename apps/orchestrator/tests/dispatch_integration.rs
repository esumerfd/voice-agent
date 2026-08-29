//! Integration tests for the Service Contract dispatch path (ORCH-01,
//! ORCH-02, D-08): a JSON payload is validated against the workflow's
//! `ParameterSpec` BEFORE any handler runs, a resolved handler is invoked
//! through the async `Service` trait, and the result is assembled into a
//! structured `InvokeOutcome` envelope — independent of which concrete
//! `Service` ran. `StubService` below is the ONLY `Service` implementation
//! in the whole crate; it must never appear under `apps/orchestrator/src/`
//! (D-08). None of these tests assert a panic — the dispatch path must
//! reject invalid input cleanly, never crash.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use orchestrator::definition::AgentSpec;
use orchestrator::handlers::timers::TimerService;
use orchestrator::{
    dispatch, DispatchError, ParameterSpec, ParameterType, RunHandle, RunStatus, Service,
    ServiceError, ServiceMode, ServiceSpec, ValidationError, WorkflowDefinition,
    ENVELOPE_AGENT_KEY, ENVELOPE_ARGS_KEY, ENVELOPE_COMMAND_KEY,
};

/// Test-only echo stub (D-08) — this is the ONLY `Service` implementation
/// in the crate, and it lives here under `tests/`, never under `src/`.
/// Tracks how many times `invoke` was called so validation-rejection tests
/// can assert the handler was never reached.
struct StubService {
    invoke_count: Arc<AtomicUsize>,
}

impl StubService {
    fn new(invoke_count: Arc<AtomicUsize>) -> Self {
        Self { invoke_count }
    }
}

#[async_trait]
impl Service for StubService {
    async fn invoke(&self, _input: serde_json::Value) -> Result<RunHandle, ServiceError> {
        self.invoke_count.fetch_add(1, Ordering::SeqCst);
        Ok(RunHandle::new("stub-run-1"))
    }

    async fn status(&self, _handle: &RunHandle) -> Result<RunStatus, ServiceError> {
        Ok(RunStatus::Completed)
    }

    async fn cancel(&self, _handle: &RunHandle) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn result(&self, _handle: &RunHandle) -> Result<Option<serde_json::Value>, ServiceError> {
        Ok(Some(json!({"produced_by": "stub-handler"})))
    }
}

/// Test-only stub (D-08) that reports `RunStatus::Running` for its first
/// `running_polls` calls to `status()`, then `RunStatus::Completed` forever
/// after — proves `dispatch()`'s poll loop actually loops rather than
/// checking status exactly once. Uses the same `Arc<AtomicUsize>` counting
/// technique as `StubService::invoke_count`.
struct RunningThenCompletedStub {
    running_polls: usize,
    status_calls: Arc<AtomicUsize>,
}

impl RunningThenCompletedStub {
    fn new(running_polls: usize, status_calls: Arc<AtomicUsize>) -> Self {
        Self {
            running_polls,
            status_calls,
        }
    }
}

#[async_trait]
impl Service for RunningThenCompletedStub {
    async fn invoke(&self, _input: serde_json::Value) -> Result<RunHandle, ServiceError> {
        Ok(RunHandle::new("running-stub-run-1"))
    }

    async fn status(&self, _handle: &RunHandle) -> Result<RunStatus, ServiceError> {
        let call_number = self.status_calls.fetch_add(1, Ordering::SeqCst);
        if call_number < self.running_polls {
            Ok(RunStatus::Running)
        } else {
            Ok(RunStatus::Completed)
        }
    }

    async fn cancel(&self, _handle: &RunHandle) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn result(&self, _handle: &RunHandle) -> Result<Option<serde_json::Value>, ServiceError> {
        Ok(Some(json!({"produced_by": "running-then-completed-stub"})))
    }
}

/// Builds a handler registry mapping `"demo.handler"` to the given
/// `RunningThenCompletedStub`.
fn handlers_with_running_stub(stub: RunningThenCompletedStub) -> HashMap<String, Box<dyn Service>> {
    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert("demo.handler".to_string(), Box::new(stub));
    handlers
}

/// Test-only capturing stub (D-08) proving dispatch()'s envelope contract
/// (quick task 260720-s7i): records the exact `Value` handed to `invoke()`
/// into a shared `Arc<Mutex<Option<Value>>>` so a test can assert on the
/// received shape after `dispatch()` returns. Status/result mirror
/// `StubService`'s fixed Completed/Ok(Some(..)) shape.
struct CapturingService {
    captured: Arc<Mutex<Option<Value>>>,
}

impl CapturingService {
    fn new(captured: Arc<Mutex<Option<Value>>>) -> Self {
        Self { captured }
    }
}

#[async_trait]
impl Service for CapturingService {
    async fn invoke(&self, input: serde_json::Value) -> Result<RunHandle, ServiceError> {
        *self.captured.lock().expect("captured mutex poisoned") = Some(input);
        Ok(RunHandle::new("capturing-run-1"))
    }

    async fn status(&self, _handle: &RunHandle) -> Result<RunStatus, ServiceError> {
        Ok(RunStatus::Completed)
    }

    async fn cancel(&self, _handle: &RunHandle) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn result(&self, _handle: &RunHandle) -> Result<Option<serde_json::Value>, ServiceError> {
        Ok(Some(json!({"produced_by": "capturing-handler"})))
    }
}

/// Builds a handler registry mapping `"demo.handler"` to the given
/// `CapturingService`.
fn handlers_with_capturing_stub(stub: CapturingService) -> HashMap<String, Box<dyn Service>> {
    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert("demo.handler".to_string(), Box::new(stub));
    handlers
}

/// Builds a minimal `WorkflowDefinition` whose `service.handler` is
/// `"demo.handler"`, with the given `service.command`/`args` (quick task
/// 260720-s7i) and no declared parameters.
fn workflow_with_command(command: Option<&str>, args: Vec<&str>) -> WorkflowDefinition {
    WorkflowDefinition {
        id: "demo_command".to_string(),
        name: "Demo Command Workflow".to_string(),
        description: String::new(),
        parameters: HashMap::new(),
        service: ServiceSpec {
            type_: Some("action".to_string()),
            handler: "demo.handler".to_string(),
            mode: ServiceMode::Sync,
            command: command.map(std::path::PathBuf::from),
            args: args.into_iter().map(|s| s.to_string()).collect(),
            agent: None,
        },
        source_path: std::path::PathBuf::from("test.md"),
    }
}

/// Builds a minimal `WorkflowDefinition` whose `service.handler` is
/// `"demo.handler"`, with the given parameter spec.
fn workflow_with_params(parameters: HashMap<String, ParameterSpec>) -> WorkflowDefinition {
    WorkflowDefinition {
        id: "demo".to_string(),
        name: "Demo Workflow".to_string(),
        description: String::new(),
        parameters,
        service: ServiceSpec {
            type_: Some("action".to_string()),
            handler: "demo.handler".to_string(),
            mode: ServiceMode::Sync,
            command: None,
            args: Vec::new(),
            agent: None,
        },
        source_path: std::path::PathBuf::from("test.md"),
    }
}

/// Builds a handler registry mapping `"demo.handler"` to the given stub.
fn handlers_with_stub(stub: StubService) -> HashMap<String, Box<dyn Service>> {
    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert("demo.handler".to_string(), Box::new(stub));
    handlers
}

#[tokio::test]
async fn stub_invoke_returns_handle_status_completed_cancel_noop() {
    let stub = StubService::new(Arc::new(AtomicUsize::new(0)));

    let handle = stub
        .invoke(json!({"anything": "goes"}))
        .await
        .expect("stub invoke should succeed");

    assert_eq!(
        stub.status(&handle).await,
        Ok(RunStatus::Completed),
        "stub status should report Completed"
    );
    assert_eq!(
        stub.cancel(&handle).await,
        Ok(()),
        "stub cancel should be a no-op success"
    );
}

#[tokio::test]
async fn missing_required_param_is_rejected_before_dispatch() {
    let mut parameters = HashMap::new();
    parameters.insert(
        "amount".to_string(),
        ParameterSpec {
            type_: ParameterType::Int,
            description: None,
            required: true,
        },
    );
    let definition = workflow_with_params(parameters);

    let invoke_count = Arc::new(AtomicUsize::new(0));
    let handlers = handlers_with_stub(StubService::new(invoke_count.clone()));

    // Payload omits the required "amount" parameter entirely.
    let payload = serde_json::Map::new();

    let result = dispatch(&definition, payload, &handlers).await;

    match result {
        Err(DispatchError::Validation(ValidationError::MissingRequired { param })) => {
            assert_eq!(param, "amount", "expected the error to name the missing param");
        }
        other => panic!("expected MissingRequired validation error, got: {other:?}"),
    }
    assert_eq!(
        invoke_count.load(Ordering::SeqCst),
        0,
        "stub invoke must never be reached when the payload fails validation"
    );
}

#[tokio::test]
async fn float_for_int_param_is_rejected_as_wrong_type() {
    let mut parameters = HashMap::new();
    parameters.insert(
        "minutes".to_string(),
        ParameterSpec {
            type_: ParameterType::Int,
            description: None,
            required: true,
        },
    );
    let definition = workflow_with_params(parameters);

    let invoke_count = Arc::new(AtomicUsize::new(0));
    let handlers = handlers_with_stub(StubService::new(invoke_count.clone()));

    let mut payload = serde_json::Map::new();
    payload.insert("minutes".to_string(), json!(5.5));

    let result = dispatch(&definition, payload, &handlers).await;

    match result {
        Err(DispatchError::Validation(ValidationError::WrongType { param, expected })) => {
            assert_eq!(param, "minutes", "expected the error to name the wrong-type param");
            assert_eq!(expected, ParameterType::Int, "expected Int as the declared type");
        }
        other => panic!("expected WrongType validation error, got: {other:?}"),
    }
    assert_eq!(
        invoke_count.load(Ordering::SeqCst),
        0,
        "stub invoke must never be reached when the payload fails validation (5.5 for an int param)"
    );
}

#[tokio::test]
async fn completed_invocation_returns_structured_envelope() {
    let mut parameters = HashMap::new();
    parameters.insert(
        "note".to_string(),
        ParameterSpec {
            type_: ParameterType::String,
            description: None,
            required: false,
        },
    );
    let definition = workflow_with_params(parameters);

    let invoke_count = Arc::new(AtomicUsize::new(0));
    let handlers = handlers_with_stub(StubService::new(invoke_count.clone()));

    let mut payload = serde_json::Map::new();
    payload.insert("note".to_string(), json!("INPUT_ECHO_SENTINEL"));
    let payload_clone = payload.clone();

    let outcome = dispatch(&definition, payload, &handlers)
        .await
        .expect("dispatch should succeed for a valid payload");

    assert_eq!(outcome.status, RunStatus::Completed, "expected status Completed");
    assert_eq!(
        outcome.output,
        Some(json!({"produced_by": "stub-handler"})),
        "expected the handler's own produced output, not an echo of the input"
    );
    assert_ne!(
        outcome.output,
        Some(Value::Object(payload_clone)),
        "output must be the handler's own result, never an echo of the input"
    );
    assert!(outcome.error.is_none(), "expected no error on a successful invocation");
    assert_eq!(
        invoke_count.load(Ordering::SeqCst),
        1,
        "expected the handler to be invoked exactly once for a valid payload"
    );
}

#[tokio::test]
async fn running_state_is_polled_until_completed() {
    let definition = workflow_with_params(HashMap::new());

    let status_calls = Arc::new(AtomicUsize::new(0));
    let handlers = handlers_with_running_stub(RunningThenCompletedStub::new(3, status_calls.clone()));

    let payload = serde_json::Map::new();

    let outcome = dispatch(&definition, payload, &handlers)
        .await
        .expect("dispatch should succeed once the stub reports Completed");

    assert_eq!(
        outcome.status,
        RunStatus::Completed,
        "expected the poll loop to terminate on Completed"
    );
    assert_eq!(
        outcome.output,
        Some(json!({"produced_by": "running-then-completed-stub"})),
        "expected the handler's own result() output after Completed"
    );
    assert!(outcome.error.is_none(), "expected no error once Completed is reached");
    assert!(
        status_calls.load(Ordering::SeqCst) > 1,
        "expected status() to be polled more than once (observed Running before Completed)"
    );
}

/// Builds a `WorkflowDefinition` matching `workflows/set_timer.md`'s
/// declared shape: a required `duration_minutes` int parameter, handler
/// `timers.start`.
fn workflow_set_timer() -> WorkflowDefinition {
    let mut parameters = HashMap::new();
    parameters.insert(
        "duration_minutes".to_string(),
        ParameterSpec {
            type_: ParameterType::Int,
            description: None,
            required: true,
        },
    );
    WorkflowDefinition {
        id: "set_timer".to_string(),
        name: "Set Timer".to_string(),
        description: String::new(),
        parameters,
        service: ServiceSpec {
            type_: Some("action".to_string()),
            handler: "timers.start".to_string(),
            mode: ServiceMode::Sync,
            command: None,
            args: Vec::new(),
            agent: None,
        },
        source_path: std::path::PathBuf::from("test.md"),
    }
}

#[tokio::test]
async fn set_timer_runs_through_dispatch_poll_loop_to_completed() {
    let definition = workflow_set_timer();

    // Sub-second injected unit (Pitfall 4): deadline lands ~150ms out, which
    // requires more than one 100ms POLL_INTERVAL sleep in dispatch()'s poll
    // loop to elapse -- proving the loop actually observed Running before
    // Completed, without a multi-minute real wait.
    let timer = TimerService::with_unit(std::time::Duration::from_millis(50));
    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert("timers.start".to_string(), Box::new(timer));

    let mut payload = serde_json::Map::new();
    payload.insert("duration_minutes".to_string(), json!(3));

    let started = std::time::Instant::now();
    let outcome = dispatch(&definition, payload, &handlers)
        .await
        .expect("dispatch should succeed once the timer completes");
    let elapsed = started.elapsed();

    assert_eq!(outcome.status, RunStatus::Completed, "expected the timer to complete");
    assert!(
        outcome.output.as_ref().is_some_and(|v| v.is_object()),
        "expected structured JSON object output (D-01), got: {:?}",
        outcome.output
    );
    assert!(outcome.error.is_none(), "expected no error once the timer completes");
    assert!(
        elapsed >= std::time::Duration::from_millis(100),
        "expected dispatch() to poll more than once (elapsed {elapsed:?} suggests only a single immediate check)"
    );
}

#[tokio::test]
async fn command_configured_wraps_payload_in_trusted_envelope_with_params() {
    let definition = workflow_with_command(Some("workflows/scripts/harmless.sh"), vec!["--x"]);

    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let handlers = handlers_with_capturing_stub(CapturingService::new(captured.clone()));

    let mut payload = serde_json::Map::new();
    payload.insert("a".to_string(), json!(1));

    dispatch(&definition, payload, &handlers)
        .await
        .expect("dispatch should succeed for a command-configured workflow");

    let received = captured
        .lock()
        .expect("captured mutex poisoned")
        .clone()
        .expect("handler should have captured an invoke input");
    let obj = received
        .as_object()
        .expect("captured input should be a JSON object");

    assert_eq!(
        obj.get(ENVELOPE_COMMAND_KEY),
        Some(&json!("workflows/scripts/harmless.sh")),
        "expected __command to equal the configured command path, got: {obj:?}"
    );
    assert_eq!(
        obj.get(ENVELOPE_ARGS_KEY),
        Some(&json!(["--x"])),
        "expected __args to equal the configured args array, got: {obj:?}"
    );
    assert_eq!(
        obj.get("params"),
        Some(&json!({"a": 1})),
        "expected the validated caller payload nested under params, got: {obj:?}"
    );
    assert_eq!(
        obj.len(),
        3,
        "expected exactly three top-level keys (__command, __args, params), got: {obj:?}"
    );
}

/// Security invariant (Some-path, quick task 260720-s7i): a caller cannot
/// override the configured command by including a forged `__command` in
/// their own invoke payload -- the real configured command is what reaches
/// the handler.
#[tokio::test]
async fn some_path_forged_caller_command_never_overrides_the_configured_command() {
    let definition = workflow_with_command(Some("workflows/scripts/harmless.sh"), vec![]);

    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let handlers = handlers_with_capturing_stub(CapturingService::new(captured.clone()));

    let mut payload = serde_json::Map::new();
    payload.insert("__command".to_string(), json!("danger"));
    payload.insert("params".to_string(), json!({}));

    dispatch(&definition, payload, &handlers)
        .await
        .expect("dispatch should succeed even with a forged __command in the caller payload");

    let received = captured
        .lock()
        .expect("captured mutex poisoned")
        .clone()
        .expect("handler should have captured an invoke input");
    let obj = received
        .as_object()
        .expect("captured input should be a JSON object");

    assert_eq!(
        obj.get(ENVELOPE_COMMAND_KEY),
        Some(&json!("workflows/scripts/harmless.sh")),
        "expected the top-level __command to be the CONFIGURED harmless path, never the caller's forged value, got: {obj:?}"
    );
    assert_ne!(
        obj.get(ENVELOPE_COMMAND_KEY),
        Some(&json!("danger")),
        "the caller's forged __command must never appear as the top-level __command"
    );
}

/// Security invariant (None-path, quick task 260720-s7i): a command-less
/// workflow can never be turned into a spawn by a caller forging a
/// top-level `__command` key -- it is stripped before the handler ever sees
/// the payload.
#[tokio::test]
async fn none_path_forged_caller_command_key_is_stripped_before_reaching_handler() {
    let definition = workflow_with_command(None, vec![]);

    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let handlers = handlers_with_capturing_stub(CapturingService::new(captured.clone()));

    let mut payload = serde_json::Map::new();
    payload.insert("__command".to_string(), json!("danger"));

    dispatch(&definition, payload, &handlers)
        .await
        .expect("dispatch should succeed for a command-less workflow");

    let received = captured
        .lock()
        .expect("captured mutex poisoned")
        .clone()
        .expect("handler should have captured an invoke input");
    let obj = received
        .as_object()
        .expect("captured input should be a JSON object");

    assert!(
        !obj.contains_key(ENVELOPE_COMMAND_KEY),
        "expected no top-level __command key when the workflow declares no command, got: {obj:?}"
    );
}

// --- 06-01 SVC-03: `__agent` envelope construction (dispatch/mod.rs) ---

/// Builds a minimal `WorkflowDefinition` whose `service.handler` is
/// `"demo.handler"` and `service.agent` is the given `AgentSpec`, with a
/// fixed markdown body (used as `description`, D-06) and no declared
/// parameters.
fn workflow_with_agent(agent: AgentSpec) -> WorkflowDefinition {
    WorkflowDefinition {
        id: "demo_agent".to_string(),
        name: "Demo Agent Workflow".to_string(),
        description: "Reply with exactly the word: PONG".to_string(),
        parameters: HashMap::new(),
        service: ServiceSpec {
            type_: Some("action".to_string()),
            handler: "demo.handler".to_string(),
            mode: ServiceMode::Sync,
            command: None,
            args: Vec::new(),
            agent: Some(agent),
        },
        source_path: std::path::PathBuf::from("test.md"),
    }
}

#[tokio::test]
async fn agent_definition_builds_envelope_with_workflow_id_body_files_params() {
    let definition = workflow_with_agent(AgentSpec {
        files: vec![std::path::PathBuf::from("/tmp/reference.md")],
        model: Some("claude-sonnet-5".to_string()),
        max_budget_usd: Some(0.5),
        timeout_secs: Some(30),
    });

    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let handlers = handlers_with_capturing_stub(CapturingService::new(captured.clone()));

    let mut payload = serde_json::Map::new();
    payload.insert("note".to_string(), json!("caller-supplied"));

    dispatch(&definition, payload, &handlers)
        .await
        .expect("dispatch should succeed for an agent-bound workflow");

    let received = captured
        .lock()
        .expect("captured mutex poisoned")
        .clone()
        .expect("handler should have captured an invoke input");
    let obj = received
        .as_object()
        .expect("captured input should be a JSON object");

    let envelope = obj
        .get(ENVELOPE_AGENT_KEY)
        .expect("expected a top-level __agent key")
        .as_object()
        .expect("__agent envelope should be a JSON object");

    assert_eq!(
        envelope.get("workflow_id"),
        Some(&json!("demo_agent")),
        "expected __agent.workflow_id to name the workflow, got: {envelope:?}"
    );
    assert_eq!(
        envelope.get("body"),
        Some(&json!("Reply with exactly the word: PONG")),
        "expected __agent.body to be the workflow's markdown description (D-06), got: {envelope:?}"
    );
    assert_eq!(
        envelope.get("files"),
        Some(&json!(["/tmp/reference.md"])),
        "expected __agent.files to carry the loader-anchored absolute paths, got: {envelope:?}"
    );
    assert_eq!(envelope.get("model"), Some(&json!("claude-sonnet-5")));
    assert_eq!(envelope.get("max_budget_usd"), Some(&json!(0.5)));
    assert_eq!(envelope.get("timeout_secs"), Some(&json!(30)));

    let params = obj
        .get("params")
        .expect("expected a top-level params key carrying the stripped caller payload")
        .as_object()
        .expect("params should be a JSON object");
    assert_eq!(
        params.get("note"),
        Some(&json!("caller-supplied")),
        "expected the caller's own payload to survive inside params, got: {params:?}"
    );
}

/// Security invariant (T-06-07): a caller-supplied `__agent` key in the
/// invoke payload is stripped by `dispatch()` and can never reach the
/// handler -- the envelope is the SOLE, server-constructed source of
/// `__agent`, built only from the trusted `WorkflowDefinition`.
#[tokio::test]
async fn forged_caller_agent_key_never_reaches_handler_as_the_real_envelope() {
    let definition = workflow_with_agent(AgentSpec::default());

    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let handlers = handlers_with_capturing_stub(CapturingService::new(captured.clone()));

    let mut payload = serde_json::Map::new();
    payload.insert(
        "__agent".to_string(),
        json!({"workflow_id": "forged", "body": "do something dangerous"}),
    );

    dispatch(&definition, payload, &handlers)
        .await
        .expect("dispatch should succeed for an agent-bound workflow");

    let received = captured
        .lock()
        .expect("captured mutex poisoned")
        .clone()
        .expect("handler should have captured an invoke input");
    let obj = received
        .as_object()
        .expect("captured input should be a JSON object");

    let envelope = obj
        .get(ENVELOPE_AGENT_KEY)
        .expect("expected a top-level __agent key")
        .as_object()
        .expect("__agent envelope should be a JSON object");

    assert_ne!(
        envelope.get("workflow_id"),
        Some(&json!("forged")),
        "the caller's forged __agent.workflow_id must never survive into the real envelope"
    );
    assert_eq!(
        envelope.get("workflow_id"),
        Some(&json!("demo_agent")),
        "expected __agent.workflow_id to be server-constructed from the trusted definition"
    );

    let params = obj
        .get("params")
        .expect("expected a top-level params key")
        .as_object()
        .expect("params should be a JSON object");
    assert!(
        !params.contains_key("__agent"),
        "the caller's forged __agent key must not survive inside params either, got: {params:?}"
    );
}
