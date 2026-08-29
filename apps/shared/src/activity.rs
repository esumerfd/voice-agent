//! Activity DTO family (D-01/D-03/D-05/D-06): the shape `orchestratord`'s
//! activity registry produces and every client (`orchestrator-cli`,
//! `orchestrator-tui`) renders. One `ActivityEvent` = one `invoke_workflow`
//! call (`run_id`, D-05) -- never collapsed by `workflow_id`. `log` carries
//! only synthesized lifecycle transitions (D-06/D-07): no `Service` trait
//! change backs this, and no handler ever streams live progress into it.
//!
//! Time is represented as `u64` epoch-milliseconds throughout
//! (`started_at_ms`, `at_ms`) rather than pulling in `chrono`/`time` --
//! avoids a new dependency for a value this crate only ever serializes and
//! compares, never formats for display (a client can format at render time
//! if needed).

use serde::{Deserialize, Serialize};

/// Where an activity currently stands (D-01), mapped 1:1 to the ROADMAP's
/// locked status-color spec: `NotStarted` = gray, `Running` = white,
/// `Success` = green, `Failure` = red. `#[serde(rename_all = "lowercase")]`
/// mirrors `client.rs`'s `ParameterType`/`InvokeStatus` convention so every
/// wire enum in this crate serializes the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityStatus {
    NotStarted,
    Running,
    Success,
    Failure,
}

/// A synthesized lifecycle transition (D-06): observed by the registry as it
/// watches an existing `invoke_workflow` call, never emitted by a `Service`
/// handler. `Invoked` fires before dispatch; `Running` only ever appears for
/// an async-mode invocation (D-08/D-09 ack window); `Completed`/`Failed` are
/// the two possible terminal transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityPhase {
    Invoked,
    Running,
    Completed,
    Failed,
}

/// One entry in an `ActivityEvent`'s detail log (D-06) -- one lifecycle
/// transition an activity has gone through, and, since 260816-ems, one tab
/// in a TUI client's per-activity detail view. `detail` carries the
/// terminal transition's result/error payload (or is `None` for the
/// non-terminal `Invoked`/`Running` phases).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityLogEvent {
    pub phase: ActivityPhase,
    pub at_ms: u64,
    pub detail: Option<serde_json::Value>,
}

/// One tracked `invoke_workflow` call (D-05): `run_id` is the unit of
/// identity, never collapsed by `workflow_id` -- re-running the same
/// workflow twice produces two separate `ActivityEvent`s. `client_name`
/// carries the self-reported identity established by the `Hello` handshake
/// (D-03/D-04).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub run_id: String,
    pub workflow_id: String,
    pub client_name: String,
    pub status: ActivityStatus,
    pub started_at_ms: u64,
    pub log: Vec<ActivityLogEvent>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sample_event() -> ActivityEvent {
        ActivityEvent {
            run_id: "run-1".to_string(),
            workflow_id: "set_timer".to_string(),
            client_name: "orchestrator-tui".to_string(),
            status: ActivityStatus::Success,
            started_at_ms: 1_000,
            log: vec![
                ActivityLogEvent {
                    phase: ActivityPhase::Invoked,
                    at_ms: 1_000,
                    detail: None,
                },
                ActivityLogEvent {
                    phase: ActivityPhase::Completed,
                    at_ms: 1_050,
                    detail: Some(json!({"ok": true})),
                },
            ],
        }
    }

    #[test]
    fn activity_status_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(ActivityStatus::NotStarted).unwrap(),
            json!("notstarted")
        );
        assert_eq!(
            serde_json::to_value(ActivityStatus::Running).unwrap(),
            json!("running")
        );
        assert_eq!(
            serde_json::to_value(ActivityStatus::Success).unwrap(),
            json!("success")
        );
        assert_eq!(
            serde_json::to_value(ActivityStatus::Failure).unwrap(),
            json!("failure")
        );
    }

    #[test]
    fn activity_phase_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(ActivityPhase::Invoked).unwrap(),
            json!("invoked")
        );
        assert_eq!(
            serde_json::to_value(ActivityPhase::Running).unwrap(),
            json!("running")
        );
        assert_eq!(
            serde_json::to_value(ActivityPhase::Completed).unwrap(),
            json!("completed")
        );
        assert_eq!(
            serde_json::to_value(ActivityPhase::Failed).unwrap(),
            json!("failed")
        );
    }

    #[test]
    fn activity_event_round_trips() {
        let event = sample_event();
        let before = serde_json::to_value(&event).expect("serialize before round-trip");
        let json_string = serde_json::to_string(&event).expect("to_string");
        let parsed: ActivityEvent = serde_json::from_str(&json_string).expect("from_str");
        let after = serde_json::to_value(&parsed).expect("serialize after round-trip");
        assert_eq!(before, after, "round-trip changed the JSON value");
        assert_eq!(parsed.run_id, "run-1");
        assert_eq!(parsed.workflow_id, "set_timer");
        assert_eq!(parsed.client_name, "orchestrator-tui");
        assert_eq!(parsed.status, ActivityStatus::Success);
        assert_eq!(parsed.started_at_ms, 1_000);
        assert_eq!(parsed.log.len(), 2);
    }
}
