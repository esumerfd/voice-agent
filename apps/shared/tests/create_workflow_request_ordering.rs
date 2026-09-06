//! Untagged-ordering guards for the Phase 8 `CreateWorkflowRequest.intent`/
//! `.triggers` fields (D-06/D-07, FMT-01).
//!
//! `envelope.rs`'s `RequestPayload` is `#[serde(untagged)]` (D-05): serde
//! tries each variant in declaration order and resolves to the first one
//! whose shape matches. D-05 forbids disturbing that variant ordering, and
//! the two new optional `CreateWorkflowRequest` fields are exactly the kind
//! of additive change that could -- a payload that now carries `intent`/
//! `triggers`, or a payload that carries neither, must still resolve to the
//! same `RequestPayload` variant it did before those fields existed. These
//! tests mirror the existing guard tests in `envelope.rs`
//! (`create_workflow_request_with_agent_block_still_resolves_as_create_workflow_payload`
//! / `create_workflow_request_with_no_agent_key_still_deserializes_with_no_agent_block`),
//! which are the exact precedent for this ordering-regression concern.

use serde_json::json;
use shared::envelope::{Envelope, RequestPayload};

/// A create-shaped payload carrying both `intent` and `triggers` still
/// resolves to `RequestPayload::CreateWorkflow` -- not greedily absorbed by
/// `DeleteWorkflow` or `ListWorkflows`.
#[test]
fn create_workflow_request_with_intent_and_triggers_still_resolves_as_create_workflow() {
    let raw = json!({
        "type": "req",
        "id": 60,
        "payload": {
            "id": "calendar_intent_wire",
            "name": "Calendar Intent Wire",
            "description": "Prompt body.",
            "script": null,
            "parameters": [],
            "mode": "create",
            "agent": null,
            "intent": "check today's calendar events",
            "triggers": ["cli", "voice"]
        }
    });
    let envelope: Envelope = serde_json::from_value(raw)
        .expect("intent/triggers-bearing create-workflow payload should deserialize");

    match envelope {
        Envelope::Req {
            payload: RequestPayload::CreateWorkflow(req),
            ..
        } => {
            assert_eq!(req.intent, Some("check today's calendar events".to_string()));
            assert_eq!(req.triggers, vec!["cli".to_string(), "voice".to_string()]);
        }
        other => panic!(
            "expected an intent/triggers-bearing CreateWorkflow-shaped payload to resolve to RequestPayload::CreateWorkflow, got: {other:?}"
        ),
    }
}

/// A create-shaped payload with no `intent` key and no `triggers` key still
/// resolves to `RequestPayload::CreateWorkflow`, with an absent intent and
/// an empty trigger list -- backward compatibility with every existing wire
/// literal predating these two fields.
#[test]
fn create_workflow_request_with_no_intent_or_triggers_key_still_resolves_as_create_workflow() {
    let raw = json!({
        "type": "req",
        "id": 61,
        "payload": {
            "id": "note_no_schema_fields",
            "name": "Note No Schema Fields",
            "description": "Some prose.",
            "script": null,
            "parameters": []
        }
    });
    let envelope: Envelope = serde_json::from_value(raw)
        .expect("intent/triggers-less create-workflow payload should deserialize");

    match envelope {
        Envelope::Req {
            payload: RequestPayload::CreateWorkflow(req),
            ..
        } => {
            assert!(
                req.intent.is_none(),
                "expected no intent when the key is absent, got: {:?}",
                req.intent
            );
            assert!(
                req.triggers.is_empty(),
                "expected an empty triggers list when the key is absent, got: {:?}",
                req.triggers
            );
        }
        other => panic!(
            "expected an intent/triggers-less CreateWorkflow-shaped payload to resolve to RequestPayload::CreateWorkflow, got: {other:?}"
        ),
    }
}

/// A `{"id": "..."}`-only payload still resolves to
/// `RequestPayload::DeleteWorkflow` -- the variant most at risk from a
/// subset-of-fields ordering regression introduced by the two new optional
/// `CreateWorkflowRequest` fields.
#[test]
fn id_only_payload_still_resolves_as_delete_workflow() {
    let raw = json!({
        "type": "req",
        "id": 62,
        "payload": { "id": "some_workflow" }
    });
    let envelope: Envelope =
        serde_json::from_value(raw).expect("id-only payload should deserialize");

    match envelope {
        Envelope::Req {
            payload: RequestPayload::DeleteWorkflow(req),
            ..
        } => {
            assert_eq!(req.id, "some_workflow");
        }
        other => panic!(
            "expected an `{{\"id\": ...}}`-only payload to resolve to RequestPayload::DeleteWorkflow, got: {other:?}"
        ),
    }
}

/// An empty `{}` payload still resolves to `RequestPayload::ListWorkflows`
/// -- the other variant most at risk from a subset-of-fields ordering
/// regression introduced by the two new optional `CreateWorkflowRequest`
/// fields.
#[test]
fn empty_object_payload_still_resolves_as_list_workflows() {
    let raw = json!({
        "type": "req",
        "id": 63,
        "payload": {}
    });
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
