//! Fail-closed confirmation-tier classification (ROUT-04, D-01). The
//! load-bearing access-control mechanism for probabilistically-matched
//! destructive actions (ASVS V4) -- its default branch must be the safe
//! one. Pure, no I/O, no `async`, no float arithmetic anywhere in it: it
//! reads only the already-loaded, trusted `WorkflowDefinition.service`
//! fields (never a caller payload, never re-derived from a route score).

use crate::definition::WorkflowDefinition;

/// Whether a matched workflow may be dispatched immediately (`RouteFreely`)
/// or must be confirmed by the caller first (`ConfirmRequired`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmTier {
    RouteFreely,
    ConfirmRequired,
}

impl ConfirmTier {
    /// The single mapping from this enum to the `"route_freely"`/
    /// `"confirm_required"` strings the wire frame carries in plan 09-03 --
    /// the wire spelling has exactly one home.
    pub fn as_wire_str(&self) -> &'static str {
        match self {
            ConfirmTier::RouteFreely => "route_freely",
            ConfirmTier::ConfirmRequired => "confirm_required",
        }
    }
}

/// D-01: exactly the two `service.type` values that are ever freely
/// routable. Every other value -- absent, empty, unrecognised -- falls
/// through to `ConfirmRequired`.
const BENIGN_SERVICE_TYPES: [&str; 2] = ["action", "integration"];

/// Handler-name substrings that mark a workflow `ConfirmRequired`
/// regardless of its declared type. Multi-character, unambiguous substrings
/// only -- a two-character fragment would collide with innocent handler
/// names, and a substring scan cannot tell the difference. A false
/// positive here costs one unnecessary confirmation prompt; a false
/// negative costs an unconfirmed destructive action, so the asymmetry is
/// deliberate: never add a fragment shorter than these.
const DESTRUCTIVE_HANDLER_PATTERNS: [&str; 8] = [
    "delete", "destroy", "purge", "remove", "wipe", "truncate", "uninstall", "drop",
];

/// Classifies `def` as `RouteFreely` ONLY when its normalised
/// (trimmed + lowercased) `service.type_` is present AND a member of
/// [`BENIGN_SERVICE_TYPES`] AND its lowercased `service.handler` contains
/// none of [`DESTRUCTIVE_HANDLER_PATTERNS`]. Every other input -- absent,
/// empty, unrecognised, or a destructive handler on an otherwise-benign
/// type -- falls through to `ConfirmRequired`, the safe outcome.
///
/// Inverting this shape (an explicit list of dangerous types with a
/// permissive default) is forbidden: a workflow type nobody has thought of
/// yet must land on the guarded side, not the free one.
pub fn confirm_tier(def: &WorkflowDefinition) -> ConfirmTier {
    let is_benign_type = def
        .service
        .type_
        .as_deref()
        .map(|raw| raw.trim().to_lowercase())
        .is_some_and(|normalised| BENIGN_SERVICE_TYPES.contains(&normalised.as_str()));

    let handler_lower = def.service.handler.to_lowercase();
    let has_destructive_handler =
        DESTRUCTIVE_HANDLER_PATTERNS.iter().any(|pattern| handler_lower.contains(pattern));

    if is_benign_type && !has_destructive_handler {
        ConfirmTier::RouteFreely
    } else {
        ConfirmTier::ConfirmRequired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{ParameterSpec, ServiceMode, ServiceSpec};
    use crate::registry::Registry;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    /// Mirrors `registry/mod.rs`'s `stub_definition` literal-construction
    /// convention. Every `WorkflowDefinition`/`ServiceSpec` field read here
    /// is `pub` on a type this module already imports -- no privacy
    /// boundary crossed, unlike `Registry`'s own private `definitions` map.
    fn stub_definition(type_: Option<&str>, handler: &str) -> WorkflowDefinition {
        WorkflowDefinition {
            id: "stub".to_string(),
            name: "Stub".to_string(),
            description: String::new(),
            parameters: HashMap::<String, ParameterSpec>::new(),
            service: ServiceSpec {
                type_: type_.map(|s| s.to_string()),
                handler: handler.to_string(),
                mode: ServiceMode::Sync,
                command: None,
                args: Vec::new(),
                agent: None,
            },
            source_path: PathBuf::from("stub.md"),
            intent: String::new(),
            triggers: Vec::new(),
        }
    }

    #[test]
    fn agent_type_always_requires_confirmation() {
        assert_eq!(confirm_tier(&stub_definition(Some("agent"), "agent.claude")), ConfirmTier::ConfirmRequired);
    }

    #[test]
    fn action_type_with_a_benign_handler_routes_freely() {
        assert_eq!(confirm_tier(&stub_definition(Some("action"), "action.immediate")), ConfirmTier::RouteFreely);
    }

    #[test]
    fn integration_type_with_a_benign_handler_routes_freely() {
        assert_eq!(
            confirm_tier(&stub_definition(Some("integration"), "process.calendar_today")),
            ConfirmTier::RouteFreely
        );
    }

    #[test]
    fn an_absent_service_type_fails_closed() {
        assert_eq!(confirm_tier(&stub_definition(None, "demo.handler")), ConfirmTier::ConfirmRequired);
    }

    #[test]
    fn an_empty_or_whitespace_only_service_type_fails_closed() {
        assert_eq!(confirm_tier(&stub_definition(Some(""), "demo.handler")), ConfirmTier::ConfirmRequired);
        assert_eq!(confirm_tier(&stub_definition(Some("   "), "demo.handler")), ConfirmTier::ConfirmRequired);
    }

    #[test]
    fn an_unrecognised_service_type_fails_closed() {
        assert_eq!(
            confirm_tier(&stub_definition(Some("experimental"), "demo.handler")),
            ConfirmTier::ConfirmRequired
        );
    }

    #[test]
    fn service_type_comparison_is_case_and_whitespace_normalised() {
        assert_eq!(confirm_tier(&stub_definition(Some("ACTION"), "action.immediate")), ConfirmTier::RouteFreely);
        assert_eq!(
            confirm_tier(&stub_definition(Some(" action "), "action.immediate")),
            ConfirmTier::RouteFreely
        );
    }

    #[test]
    fn a_destructive_handler_overrides_an_otherwise_benign_type() {
        assert_eq!(
            confirm_tier(&stub_definition(Some("action"), "workflow.delete_thing")),
            ConfirmTier::ConfirmRequired
        );
    }

    #[test]
    fn as_wire_str_returns_the_exact_snake_case_strings() {
        assert_eq!(ConfirmTier::RouteFreely.as_wire_str(), "route_freely");
        assert_eq!(ConfirmTier::ConfirmRequired.as_wire_str(), "confirm_required");
    }

    /// Loads the four REAL shipped workflow files and asserts each
    /// classifies exactly as D-01 predicts, with zero edits to those files
    /// (this test only reads them). Resolution mirrors
    /// `registry_integration.rs`/`workflow_schema_integration.rs`'s
    /// existing `CARGO_MANIFEST_DIR`-relative pattern.
    #[test]
    fn the_four_real_workflow_files_classify_exactly_as_d_01_predicts() {
        let workflows_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../workflows");
        let (registry, errors) = Registry::load(&workflows_dir);
        assert!(errors.is_empty(), "expected the real workflows/ directory to load with zero errors, got: {errors:?}");

        let expected: &[(&str, ConfirmTier)] = &[
            ("ai_summarize", ConfirmTier::ConfirmRequired),
            ("calendar_today", ConfirmTier::RouteFreely),
            ("countdown", ConfirmTier::RouteFreely),
            ("set_timer", ConfirmTier::RouteFreely),
        ];
        for (id, expected_tier) in expected {
            let def = registry.lookup(id).unwrap_or_else(|| panic!("expected workflow `{id}` to load"));
            assert_eq!(
                confirm_tier(def),
                *expected_tier,
                "expected `{id}` to classify as {expected_tier:?}, got a different tier"
            );
        }
    }
}
