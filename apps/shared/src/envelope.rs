//! Shared wire envelope (D-03): every WebSocket message exchanged between
//! `orchestratord` and its clients (`orchestrator-cli`, `orchestrator-tui`)
//! follows `{type, ...}`, wrapping the existing `Serialize`/`Deserialize`
//! client types (`InvokeWorkflowRequest/Response`, `ListWorkflowsRequest/
//! Response`, `DescribeWorkflowRequest/Response`) by reference rather than
//! redefining them — this module defines only the framing, never a
//! duplicate payload shape. `Req`/`Res` correlate a client request to its
//! reply by `id`; `Event` is reserved now for later server-push (e.g.
//! run-completed notifications) so the wire protocol never needs a
//! redesign (D-03).
//!
//! Phase 5 extends this same tagged enum with two more frames rather than
//! inventing a second, envelope-less wire format (RESEARCH Pattern 5):
//! `Hello { client_name }` MUST be the first frame a client sends on a new
//! connection, before any `Req` (D-04) -- it carries no `id` because it
//! never correlates to a reply. `Activity { event }` is a server-push-only
//! frame (D-01/D-03) broadcasting/replaying an `ActivityEvent` snapshot; a
//! client must never send one.

use serde::{Deserialize, Serialize};

use crate::activity::ActivityEvent;
use crate::client::{
    CreateWorkflowRequest, CreateWorkflowResponse, DeleteWorkflowRequest, DeleteWorkflowResponse,
    DescribeWorkflowRequest, DescribeWorkflowResponse, InvokeWorkflowRequest,
    InvokeWorkflowResponse, ListWorkflowsRequest, ListWorkflowsResponse,
};

/// Default WS loopback port (D-01/D-02) — a fixed default the CLI can
/// override with `--port` (flag-only, deliberately no env var per D-02, in
/// contrast to `ORCHESTRATOR_WORKFLOWS_DIR`).
pub const DEFAULT_PORT: u16 = 47100;

/// The `{type, id, payload}` wire frame (D-03). Internally tagged on
/// `type` so the JSON literal is `{"type":"req"|"res"|"event", "id":...,
/// "payload":...}` — matches the OpenClaw reference protocol this design
/// is modeled on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Envelope {
    /// A client-to-daemon request; `id` correlates the eventual `Res`.
    Req { id: u64, payload: RequestPayload },
    /// The daemon's direct reply to a `Req` sharing the same `id`.
    Res { id: u64, payload: ResponsePayload },
    /// Reserved for later server-push (e.g. an async run's terminal
    /// result) — not emitted by any Phase 4 Plan 01 code, but part of the
    /// wire contract from the start so it never requires a redesign.
    Event { id: u64, payload: ResponsePayload },
    /// Client-to-daemon identify frame (D-04) — MUST be the first frame a
    /// client sends per connection, before any `Req`. Carries no `id`
    /// because it never correlates to a reply. `client_name` is a plain
    /// self-reported string (e.g. `"orchestrator-cli"` /
    /// `"orchestrator-tui"`) — no authentication or capability negotiation
    /// (Deferred).
    Hello { client_name: String },
    /// Daemon-to-client activity snapshot (D-01/D-03) — pushed as a replay
    /// burst to a newly-connected client and again on every live lifecycle
    /// transition thereafter. Server-push-only: a client must never send
    /// this frame.
    Activity { event: ActivityEvent },
}

/// The payload of an `Envelope::Req`. `#[serde(untagged)]` wraps the
/// existing client request types by reference (never redefined) — variant
/// order matters: `CreateWorkflow` requires `id`+`name`+`description` (the
/// most fields of any variant, so it goes FIRST — the most specific match
/// and therefore the safest, quick task 260807-shx D-DISC-03),
/// `DeleteWorkflow` requires only `id` and MUST sit immediately after
/// `CreateWorkflow` and before `InvokeWorkflow`/`DescribeWorkflow`/
/// `ListWorkflows` (quick task 260812-qp4 D-7): `DeleteWorkflow`'s single
/// field is a subset of `CreateWorkflow`'s fields, so it must never be
/// tried before the more specific `CreateWorkflow` match (or a create
/// request could misroute to delete); it must also never sink below
/// `ListWorkflows`'s empty `{}`, which would greedily absorb it first.
/// `InvokeWorkflow` requires `payload`, `DescribeWorkflow` requires
/// `workflow_id`, and `ListWorkflows` is `{}` so it MUST be last or it would
/// greedily match any object during untagged resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestPayload {
    CreateWorkflow(CreateWorkflowRequest),
    DeleteWorkflow(DeleteWorkflowRequest),
    InvokeWorkflow(InvokeWorkflowRequest),
    DescribeWorkflow(DescribeWorkflowRequest),
    ListWorkflows(ListWorkflowsRequest),
}

/// The payload of an `Envelope::Res`/`Envelope::Event`. `#[serde(untagged)]`
/// wraps the existing client response types by reference — ordered
/// `CreateWorkflow` first (its required `created: bool` cannot be produced
/// by, or absorb, any existing response shape, quick task 260807-shx
/// D-DISC-03), then `DeleteWorkflow` (has `deleted: bool`, quick task
/// 260812-qp4 D-7 — sits directly below `CreateWorkflow` and above every
/// other variant for the same subset-of-fields reasoning as
/// `RequestPayload`), then `InvokeWorkflow` (has `status`), `DescribeWorkflow`
/// (has `found`), `ListWorkflows` (has `workflows`) for the same
/// greedy-match reasoning as `RequestPayload`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsePayload {
    CreateWorkflow(CreateWorkflowResponse),
    DeleteWorkflow(DeleteWorkflowResponse),
    InvokeWorkflow(InvokeWorkflowResponse),
    DescribeWorkflow(DescribeWorkflowResponse),
    ListWorkflows(ListWorkflowsResponse),
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::client::{
        CreateWorkflowRequest, CreateWorkflowResponse, DeleteWorkflowRequest,
        DeleteWorkflowResponse, DescribeWorkflowRequest, DescribeWorkflowResponse,
        InvokeWorkflowRequest, InvokeWorkflowResponse, InvokeStatus,
        ListWorkflowsRequest, ListWorkflowsResponse, ParameterDescriptor, WorkflowWriteMode,
    };
    use crate::client::{ParameterType, WorkflowSummary};

    /// Round-trips `envelope` through `to_string`/`from_str` and asserts the
    /// re-parsed value is JSON-equal to the original (Envelope/RequestPayload/
    /// ResponsePayload don't derive `PartialEq` themselves — they wrap client
    /// types that don't either — so equality is asserted via `serde_json::Value`).
    fn assert_round_trips(envelope: &Envelope) {
        let before = serde_json::to_value(envelope).expect("serialize before round-trip");
        let json_string = serde_json::to_string(envelope).expect("to_string");
        let parsed: Envelope = serde_json::from_str(&json_string).expect("from_str");
        let after = serde_json::to_value(&parsed).expect("serialize after round-trip");
        assert_eq!(before, after, "round-trip changed the JSON value");
    }

    #[test]
    fn req_serializes_with_literal_type_req_and_matching_id() {
        let envelope = Envelope::Req {
            id: 7,
            payload: RequestPayload::InvokeWorkflow(InvokeWorkflowRequest {
                workflow_id: "set_timer".to_string(),
                payload: json!({"duration_minutes": 5}),
            }),
        };

        let value = serde_json::to_value(&envelope).expect("serialize");
        assert_eq!(value["type"], json!("req"), "expected type == \"req\", got: {value}");
        assert_eq!(value["id"], json!(7), "expected id == 7, got: {value}");
        assert_eq!(
            value["payload"]["workflow_id"],
            json!("set_timer"),
            "expected payload to carry the wrapped InvokeWorkflowRequest, got: {value}"
        );
    }

    #[test]
    fn res_serializes_with_literal_type_res() {
        let envelope = Envelope::Res {
            id: 3,
            payload: ResponsePayload::InvokeWorkflow(InvokeWorkflowResponse {
                status: InvokeStatus::Completed,
                output: Some(json!({"ok": true})),
                error: None,
                run_id: None,
            }),
        };

        let value = serde_json::to_value(&envelope).expect("serialize");
        assert_eq!(value["type"], json!("res"), "expected type == \"res\", got: {value}");
    }

    #[test]
    fn event_serializes_with_literal_type_event() {
        let envelope = Envelope::Event {
            id: 3,
            payload: ResponsePayload::InvokeWorkflow(InvokeWorkflowResponse {
                status: InvokeStatus::Completed,
                output: None,
                error: None,
                run_id: None,
            }),
        };

        let value = serde_json::to_value(&envelope).expect("serialize");
        assert_eq!(
            value["type"],
            json!("event"),
            "expected type == \"event\", got: {value}"
        );
    }

    #[test]
    fn req_round_trips_invoke_workflow_payload() {
        let envelope = Envelope::Req {
            id: 1,
            payload: RequestPayload::InvokeWorkflow(InvokeWorkflowRequest {
                workflow_id: "set_timer".to_string(),
                payload: json!({"duration_minutes": 5}),
            }),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn req_round_trips_describe_workflow_payload() {
        let envelope = Envelope::Req {
            id: 2,
            payload: RequestPayload::DescribeWorkflow(DescribeWorkflowRequest {
                workflow_id: "set_timer".to_string(),
            }),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn req_round_trips_list_workflows_payload() {
        let envelope = Envelope::Req {
            id: 3,
            payload: RequestPayload::ListWorkflows(ListWorkflowsRequest {}),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn res_round_trips_invoke_workflow_payload() {
        let envelope = Envelope::Res {
            id: 4,
            payload: ResponsePayload::InvokeWorkflow(InvokeWorkflowResponse {
                status: InvokeStatus::Completed,
                output: Some(json!({"ok": true})),
                error: None,
                run_id: None,
            }),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn res_round_trips_describe_workflow_payload() {
        let envelope = Envelope::Res {
            id: 5,
            payload: ResponsePayload::DescribeWorkflow(DescribeWorkflowResponse {
                found: true,
                parameters: vec![ParameterDescriptor {
                    name: "duration_minutes".to_string(),
                    type_: ParameterType::Int,
                    required: true,
                }],
            }),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn res_round_trips_list_workflows_payload() {
        let envelope = Envelope::Res {
            id: 6,
            payload: ResponsePayload::ListWorkflows(ListWorkflowsResponse {
                workflows: vec![WorkflowSummary {
                    id: "set_timer".to_string(),
                    name: "Set Timer".to_string(),
                    description: "Sets a timer".to_string(),
                }],
                warnings: Vec::new(),
            }),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn event_round_trips_invoke_workflow_payload() {
        let envelope = Envelope::Event {
            id: 7,
            payload: ResponsePayload::InvokeWorkflow(InvokeWorkflowResponse {
                status: InvokeStatus::Failed,
                output: None,
                error: Some("boom".to_string()),
                run_id: None,
            }),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn event_round_trips_describe_workflow_payload() {
        let envelope = Envelope::Event {
            id: 8,
            payload: ResponsePayload::DescribeWorkflow(DescribeWorkflowResponse {
                found: false,
                parameters: Vec::new(),
            }),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn event_round_trips_list_workflows_payload() {
        let envelope = Envelope::Event {
            id: 9,
            payload: ResponsePayload::ListWorkflows(ListWorkflowsResponse {
                workflows: Vec::new(),
                warnings: vec!["some.md: parse error".to_string()],
            }),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn req_round_trips_create_workflow_payload_markdown_only() {
        let envelope = Envelope::Req {
            id: 20,
            payload: RequestPayload::CreateWorkflow(CreateWorkflowRequest {
                id: "note_shx".to_string(),
                name: "Note Shx".to_string(),
                description: "Some prose describing a note.".to_string(),
                script: None,
                parameters: Vec::new(),
                mode: WorkflowWriteMode::Create,
                agent: None,
            }),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn req_round_trips_create_workflow_payload_with_script() {
        let envelope = Envelope::Req {
            id: 21,
            payload: RequestPayload::CreateWorkflow(CreateWorkflowRequest {
                id: "hello_shx".to_string(),
                name: "Hello Shx".to_string(),
                description: String::new(),
                script: Some("#!/bin/sh\necho hi\n".to_string()),
                parameters: vec![ParameterDescriptor {
                    name: "dur".to_string(),
                    type_: ParameterType::Int,
                    required: true,
                }],
                mode: WorkflowWriteMode::Create,
                agent: None,
            }),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn req_round_trips_create_workflow_payload_in_edit_mode() {
        let envelope = Envelope::Req {
            id: 34,
            payload: RequestPayload::CreateWorkflow(CreateWorkflowRequest {
                id: "hello_shx".to_string(),
                name: "Hello Shx Edited".to_string(),
                description: "Edited prose.".to_string(),
                script: None,
                parameters: Vec::new(),
                mode: WorkflowWriteMode::Edit,
                agent: None,
            }),
        };
        assert_round_trips(&envelope);
    }

    /// A serialized request whose JSON explicitly carries the edit mode
    /// still resolves to `RequestPayload::CreateWorkflow` -- untagged
    /// variant ordering (D-DISC-03) is unharmed by the new field (quick task
    /// 260812-qpn D-2).
    #[test]
    fn create_workflow_request_with_edit_mode_still_resolves_as_create_workflow_payload() {
        let raw = json!({
            "type": "req",
            "id": 35,
            "payload": {
                "id": "hello_shx",
                "name": "Hello Shx",
                "description": "Edited.",
                "script": null,
                "parameters": [],
                "mode": "edit"
            }
        });
        let envelope: Envelope = serde_json::from_value(raw)
            .expect("edit-mode create-workflow payload should deserialize");

        match envelope {
            Envelope::Req {
                payload: RequestPayload::CreateWorkflow(req),
                ..
            } => {
                assert_eq!(req.mode, WorkflowWriteMode::Edit);
            }
            other => panic!(
                "expected an edit-mode CreateWorkflow-shaped payload to resolve to RequestPayload::CreateWorkflow, got: {other:?}"
            ),
        }
    }

    /// A request JSON with no `mode` key at all deserializes to the create
    /// default (D-2 backward compatibility) -- an older client that has
    /// never heard of edit mode still resolves to create.
    #[test]
    fn create_workflow_request_with_no_mode_key_defaults_to_create() {
        let raw = json!({
            "type": "req",
            "id": 36,
            "payload": {
                "id": "note_shx",
                "name": "Note Shx",
                "description": "Some prose.",
                "script": null,
                "parameters": []
            }
        });
        let envelope: Envelope =
            serde_json::from_value(raw).expect("mode-less create-workflow payload should deserialize");

        match envelope {
            Envelope::Req {
                payload: RequestPayload::CreateWorkflow(req),
                ..
            } => {
                assert_eq!(req.mode, WorkflowWriteMode::Create);
            }
            other => panic!(
                "expected a mode-less CreateWorkflow-shaped payload to resolve to RequestPayload::CreateWorkflow, got: {other:?}"
            ),
        }
    }

    #[test]
    fn res_round_trips_create_workflow_payload() {
        let envelope = Envelope::Res {
            id: 22,
            payload: ResponsePayload::CreateWorkflow(CreateWorkflowResponse {
                created: true,
                workflow_path: Some("/tmp/workflows/hello_shx.md".to_string()),
                script_path: Some("/tmp/workflows/scripts/hello_shx.sh".to_string()),
                error: None,
            }),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn res_round_trips_create_workflow_failure_payload() {
        let envelope = Envelope::Res {
            id: 23,
            payload: ResponsePayload::CreateWorkflow(CreateWorkflowResponse {
                created: false,
                workflow_path: None,
                script_path: None,
                error: Some("invalid id".to_string()),
            }),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn req_round_trips_delete_workflow_payload() {
        let envelope = Envelope::Req {
            id: 30,
            payload: RequestPayload::DeleteWorkflow(DeleteWorkflowRequest {
                id: "hello_shx".to_string(),
            }),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn res_round_trips_delete_workflow_payload() {
        let envelope = Envelope::Res {
            id: 31,
            payload: ResponsePayload::DeleteWorkflow(DeleteWorkflowResponse {
                deleted: true,
                workflow_path: Some("/tmp/workflows/hello_shx.md".to_string()),
                script_path: Some("/tmp/workflows/scripts/hello_shx.sh".to_string()),
                error: None,
            }),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn delete_workflow_request_deserializes_as_delete_workflow_not_list() {
        let raw = json!({
            "type": "req",
            "id": 32,
            "payload": { "id": "hello_shx" }
        });
        let envelope: Envelope =
            serde_json::from_value(raw).expect("delete-workflow payload should deserialize");

        match envelope {
            Envelope::Req {
                payload: RequestPayload::DeleteWorkflow(req),
                ..
            } => {
                assert_eq!(req.id, "hello_shx");
            }
            other => panic!(
                "expected a DeleteWorkflow-shaped payload ({{\"id\": ...}}) to resolve to RequestPayload::DeleteWorkflow, got: {other:?}"
            ),
        }
    }

    #[test]
    fn create_workflow_request_still_deserializes_as_create_workflow_unaffected_by_delete() {
        let raw = json!({
            "type": "req",
            "id": 33,
            "payload": {
                "id": "note_shx",
                "name": "Note Shx",
                "description": "Some prose.",
                "script": null,
                "parameters": []
            }
        });
        let envelope: Envelope =
            serde_json::from_value(raw).expect("create-workflow payload should still deserialize");

        match envelope {
            Envelope::Req {
                payload: RequestPayload::CreateWorkflow(req),
                ..
            } => {
                assert_eq!(req.id, "note_shx");
            }
            other => panic!(
                "expected a CreateWorkflow-shaped payload to still resolve to RequestPayload::CreateWorkflow (D-7 ordering guard), got: {other:?}"
            ),
        }
    }

    #[test]
    fn create_workflow_request_deserializes_as_create_workflow_not_describe_or_list() {
        let raw = json!({
            "type": "req",
            "id": 24,
            "payload": {
                "id": "note_shx",
                "name": "Note Shx",
                "description": "Some prose.",
                "script": null,
                "parameters": []
            }
        });
        let envelope: Envelope =
            serde_json::from_value(raw).expect("create-workflow payload should deserialize");

        match envelope {
            Envelope::Req {
                payload: RequestPayload::CreateWorkflow(req),
                ..
            } => {
                assert_eq!(req.id, "note_shx");
            }
            other => panic!(
                "expected a CreateWorkflow-shaped payload to resolve to RequestPayload::CreateWorkflow, got: {other:?}"
            ),
        }
    }

    #[test]
    fn describe_workflow_request_still_deserializes_as_describe_workflow_unaffected_by_create() {
        let raw = json!({"type": "req", "id": 25, "payload": {"workflow_id": "set_timer"}});
        let envelope: Envelope =
            serde_json::from_value(raw).expect("describe-workflow payload should deserialize");

        match envelope {
            Envelope::Req {
                payload: RequestPayload::DescribeWorkflow(_),
                ..
            } => {}
            other => panic!(
                "expected a workflow_id-only payload to still resolve to RequestPayload::DescribeWorkflow, got: {other:?}"
            ),
        }
    }

    #[test]
    fn create_workflow_response_deserializes_as_create_workflow_not_invoke() {
        let raw = json!({
            "type": "res",
            "id": 26,
            "payload": {
                "created": true,
                "workflow_path": "/tmp/workflows/x.md",
                "script_path": null,
                "error": null
            }
        });
        let envelope: Envelope =
            serde_json::from_value(raw).expect("create-workflow response payload should deserialize");

        match envelope {
            Envelope::Res {
                payload: ResponsePayload::CreateWorkflow(resp),
                ..
            } => {
                assert!(resp.created);
            }
            other => panic!(
                "expected a CreateWorkflow-shaped response payload to resolve to ResponsePayload::CreateWorkflow, got: {other:?}"
            ),
        }
    }

    #[test]
    fn invoke_response_still_deserializes_as_invoke_workflow_unaffected_by_create() {
        let raw = json!({
            "type": "res",
            "id": 27,
            "payload": {"status": "Completed", "output": null, "error": null, "run_id": null}
        });
        let envelope: Envelope =
            serde_json::from_value(raw).expect("invoke-workflow response payload should deserialize");

        match envelope {
            Envelope::Res {
                payload: ResponsePayload::InvokeWorkflow(_),
                ..
            } => {}
            other => panic!(
                "expected an invoke-shaped response payload to still resolve to ResponsePayload::InvokeWorkflow, got: {other:?}"
            ),
        }
    }

    #[test]
    fn empty_object_payload_deserializes_as_list_workflows_not_invoke_or_describe() {
        let raw = json!({"type": "req", "id": 42, "payload": {}});
        let envelope: Envelope =
            serde_json::from_value(raw).expect("empty payload object should deserialize");

        match envelope {
            Envelope::Req {
                payload: RequestPayload::ListWorkflows(_),
                ..
            } => {}
            other => panic!(
                "expected an empty `{{}}` payload to resolve to RequestPayload::ListWorkflows, got: {other:?}"
            ),
        }
    }

    #[test]
    fn default_port_is_47100() {
        assert_eq!(DEFAULT_PORT, 47100);
    }

    /// A serialized create request whose JSON carries an agent object still
    /// resolves to `RequestPayload::CreateWorkflow` -- untagged variant
    /// ordering (D-DISC-03) is unharmed by the new field (quick task
    /// 260813-rm5, T-rm5-07).
    #[test]
    fn create_workflow_request_with_agent_block_still_resolves_as_create_workflow_payload() {
        let raw = json!({
            "type": "req",
            "id": 50,
            "payload": {
                "id": "agent_rm5",
                "name": "Agent Rm5",
                "description": "Prompt body.",
                "script": null,
                "parameters": [],
                "mode": "create",
                "agent": {
                    "files": ["agent/ref.md"],
                    "timeout_secs": 600,
                    "max_budget_usd": 0.5
                }
            }
        });
        let envelope: Envelope = serde_json::from_value(raw)
            .expect("agent-block create-workflow payload should deserialize");

        match envelope {
            Envelope::Req {
                payload: RequestPayload::CreateWorkflow(req),
                ..
            } => {
                let agent = req.agent.expect("expected Some(agent) to survive the round trip");
                assert_eq!(agent.files, vec!["agent/ref.md".to_string()]);
                assert_eq!(agent.timeout_secs, Some(600));
                assert_eq!(agent.max_budget_usd, Some(0.5));
            }
            other => panic!(
                "expected an agent-block CreateWorkflow-shaped payload to resolve to RequestPayload::CreateWorkflow, got: {other:?}"
            ),
        }
    }

    /// A create request JSON with no `agent` key at all still deserializes,
    /// with no agent block set -- backward compatibility with every
    /// existing wire literal (quick task 260813-rm5, D-2/T-rm5-07).
    #[test]
    fn create_workflow_request_with_no_agent_key_still_deserializes_with_no_agent_block() {
        let raw = json!({
            "type": "req",
            "id": 51,
            "payload": {
                "id": "note_rm5",
                "name": "Note Rm5",
                "description": "Some prose.",
                "script": null,
                "parameters": []
            }
        });
        let envelope: Envelope =
            serde_json::from_value(raw).expect("agent-less create-workflow payload should deserialize");

        match envelope {
            Envelope::Req {
                payload: RequestPayload::CreateWorkflow(req),
                ..
            } => {
                assert!(req.agent.is_none(), "expected no agent block when the key is absent");
            }
            other => panic!(
                "expected an agent-less CreateWorkflow-shaped payload to resolve to RequestPayload::CreateWorkflow, got: {other:?}"
            ),
        }
    }

    fn sample_activity_event() -> crate::activity::ActivityEvent {
        crate::activity::ActivityEvent {
            run_id: "run-1".to_string(),
            workflow_id: "set_timer".to_string(),
            client_name: "orchestrator-tui".to_string(),
            status: crate::activity::ActivityStatus::Running,
            started_at_ms: 1_000,
            log: vec![crate::activity::ActivityLogEvent {
                phase: crate::activity::ActivityPhase::Invoked,
                at_ms: 1_000,
                detail: None,
            }],
        }
    }

    #[test]
    fn hello_serializes_with_literal_type_hello() {
        let envelope = Envelope::Hello {
            client_name: "orchestrator-tui".to_string(),
        };

        let value = serde_json::to_value(&envelope).expect("serialize");
        assert_eq!(value["type"], json!("hello"), "expected type == \"hello\", got: {value}");
        assert_eq!(
            value["client_name"],
            json!("orchestrator-tui"),
            "expected client_name to carry the reported name, got: {value}"
        );
    }

    #[test]
    fn hello_round_trips() {
        let envelope = Envelope::Hello {
            client_name: "orchestrator-cli".to_string(),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn activity_serializes_with_literal_type_activity() {
        let envelope = Envelope::Activity {
            event: sample_activity_event(),
        };

        let value = serde_json::to_value(&envelope).expect("serialize");
        assert_eq!(
            value["type"],
            json!("activity"),
            "expected type == \"activity\", got: {value}"
        );
        assert_eq!(
            value["event"]["run_id"],
            json!("run-1"),
            "expected event to carry the wrapped ActivityEvent, got: {value}"
        );
    }

    #[test]
    fn activity_round_trips() {
        let envelope = Envelope::Activity {
            event: sample_activity_event(),
        };
        assert_round_trips(&envelope);
    }

    #[test]
    fn activity_event_round_trips() {
        let event = sample_activity_event();
        let before = serde_json::to_value(&event).expect("serialize before round-trip");
        let json_string = serde_json::to_string(&event).expect("to_string");
        let parsed: crate::activity::ActivityEvent =
            serde_json::from_str(&json_string).expect("from_str");
        let after = serde_json::to_value(&parsed).expect("serialize after round-trip");
        assert_eq!(before, after, "round-trip changed the JSON value");
    }
}
