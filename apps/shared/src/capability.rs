//! Capability-gated result addressing (D-03/D-04): this module owns the
//! single, deliberately closed set of *gated field names* -- top-level keys
//! of a result object that are delivered only to a connection whose
//! declared `Hello.capabilities` includes the matching capability name.
//! Every fan-out point (the live `Activity` broadcast, the connect-time
//! replay burst, the `RunDescription` recovery reply, and the terminal
//! `Event`) filters through `filter_output_for`/`filter_activity_event_for`
//! so the addressing rule is written down exactly once, with no path
//! around it. Declaring a capability only ever WIDENS what a connection is
//! allowed to render for its OWN runs (D-03) -- it never selects a
//! recipient and never grants ownership of a run someone else started; a
//! bystander connection that never started a run still sees the shared
//! `Activity` lifecycle for it, only with gated field contents stripped
//! (D-04).

use crate::activity::ActivityEvent;

/// The one gated capability this phase defines. Phase 11's voice front end
/// is the first real producer (a workflow whose handler emits a `speech`
/// result field) and consumer (the first client to ever declare it on
/// `Hello`).
pub const CAPABILITY_SPEECH: &str = "speech";

/// The closed, deliberately tiny set of top-level result keys that are
/// gated (D-03/D-04): a key in this list is delivered only to a connection
/// declaring the matching capability name. Every OTHER top-level key is
/// delivered to every connection, gated or not.
pub const GATED_CAPABILITY_FIELDS: &[&str] = &[CAPABILITY_SPEECH];

/// Removes every top-level key of `output` that is in
/// `GATED_CAPABILITY_FIELDS` but not present in `declared` (D-03). A
/// non-object `output` (including `Value::Null`, an array, or a bare
/// scalar) is returned unchanged -- there is nothing to gate on a value
/// with no top-level keys.
pub fn filter_output_for(output: &serde_json::Value, declared: &[String]) -> serde_json::Value {
    let Some(obj) = output.as_object() else {
        return output.clone();
    };

    let mut filtered = obj.clone();
    for key in GATED_CAPABILITY_FIELDS {
        if !declared.iter().any(|d| d == key) {
            filtered.remove(*key);
        }
    }
    serde_json::Value::Object(filtered)
}

/// Filters an `ActivityEvent`'s log details through `filter_output_for`
/// (D-04): every field OTHER than `log[].detail` -- `run_id`, `workflow_id`,
/// `client_name`, `session_id`, `status`, `started_at_ms`, and each log
/// entry's `phase`/`at_ms` -- is left completely untouched, so a bystander
/// connection still sees that a run happened and how it ended; only the
/// gated content inside a `detail` payload is ever removed.
pub fn filter_activity_event_for(event: &ActivityEvent, declared: &[String]) -> ActivityEvent {
    let mut filtered = event.clone();
    for entry in filtered.log.iter_mut() {
        if let Some(detail) = &entry.detail {
            entry.detail = Some(filter_output_for(detail, declared));
        }
    }
    filtered
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::activity::{ActivityLogEvent, ActivityPhase, ActivityStatus};

    fn sample_event_with_detail(detail: Option<serde_json::Value>) -> ActivityEvent {
        ActivityEvent {
            run_id: "run-1".to_string(),
            workflow_id: "speak_it".to_string(),
            client_name: "voice-front-end".to_string(),
            session_id: "sess-1".to_string(),
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
                    detail,
                },
            ],
        }
    }

    #[test]
    fn gated_key_is_removed_for_an_empty_declared_list() {
        let output = json!({"text": "hello", "speech": {"audio": "base64..."}});
        let filtered = filter_output_for(&output, &[]);
        assert_eq!(filtered, json!({"text": "hello"}));
    }

    #[test]
    fn gated_key_is_kept_for_a_declared_list_containing_it() {
        let output = json!({"text": "hello", "speech": {"audio": "base64..."}});
        let declared = vec![CAPABILITY_SPEECH.to_string()];
        let filtered = filter_output_for(&output, &declared);
        assert_eq!(filtered, output, "expected the gated key to survive when declared");
    }

    #[test]
    fn ungated_key_survives_both_with_and_without_the_declaration() {
        let output = json!({"text": "hello"});
        assert_eq!(filter_output_for(&output, &[]), output);
        assert_eq!(
            filter_output_for(&output, &[CAPABILITY_SPEECH.to_string()]),
            output
        );
    }

    #[test]
    fn non_object_output_passes_through_unchanged() {
        assert_eq!(filter_output_for(&json!(null), &[]), json!(null));
        assert_eq!(filter_output_for(&json!([1, 2, 3]), &[]), json!([1, 2, 3]));
        assert_eq!(filter_output_for(&json!("plain string"), &[]), json!("plain string"));
        assert_eq!(filter_output_for(&json!(42), &[]), json!(42));
    }

    #[test]
    fn none_detail_stays_none() {
        let event = sample_event_with_detail(None);
        let filtered = filter_activity_event_for(&event, &[]);
        assert!(filtered.log[1].detail.is_none(), "expected a None detail to remain None");
    }

    #[test]
    fn filtered_events_log_length_and_phase_sequence_are_identical_to_the_original() {
        let event = sample_event_with_detail(Some(json!({"text": "hi", "speech": {"audio": "x"}})));
        let filtered = filter_activity_event_for(&event, &[]);

        assert_eq!(filtered.log.len(), event.log.len(), "expected the same number of log entries");
        let original_phases: Vec<ActivityPhase> = event.log.iter().map(|e| e.phase).collect();
        let filtered_phases: Vec<ActivityPhase> = filtered.log.iter().map(|e| e.phase).collect();
        assert_eq!(original_phases, filtered_phases, "expected the phase sequence to be untouched");

        // The gated field itself, however, must be gone from the terminal
        // detail while the ungated one survives.
        let detail = filtered.log[1].detail.as_ref().expect("expected Some(detail)");
        assert_eq!(detail, &json!({"text": "hi"}));
    }

    #[test]
    fn filter_activity_event_leaves_identity_and_status_fields_untouched() {
        let event = sample_event_with_detail(Some(json!({"speech": {"audio": "x"}})));
        let filtered = filter_activity_event_for(&event, &[]);

        assert_eq!(filtered.run_id, event.run_id);
        assert_eq!(filtered.workflow_id, event.workflow_id);
        assert_eq!(filtered.client_name, event.client_name);
        assert_eq!(filtered.session_id, event.session_id);
        assert_eq!(filtered.status, event.status);
        assert_eq!(filtered.started_at_ms, event.started_at_ms);
    }
}
