//! In-memory workflow registry (WF-02): enumerate/lookup surface backed by
//! `Registry::load`, implemented in `loader.rs` (scan -> parse -> validate
//! -> skip-and-warn collect, D-07).

use std::collections::HashMap;

use crate::definition::{WorkflowDefinition, WorkflowSummary};

mod loader;
pub mod writer;

/// Wraps the loaded definitions, keyed by workflow id.
pub struct Registry {
    definitions: HashMap<String, WorkflowDefinition>,
}

impl Registry {
    /// List all loaded workflows (id, name, description), deterministically
    /// sorted by id (D-09; closes Phase 1 review finding IN-02 — every
    /// consumer of `enumerate()` now sees a stable order, not HashMap order).
    pub fn enumerate(&self) -> Vec<WorkflowSummary> {
        let mut summaries: Vec<WorkflowSummary> = self
            .definitions
            .values()
            .map(|d| WorkflowSummary {
                id: d.id.clone(),
                name: d.name.clone(),
                description: d.description.clone(),
            })
            .collect();
        summaries.sort_by(|a, b| a.id.cmp(&b.id));
        summaries
    }

    /// Look up a workflow by id — `None` if not found.
    pub fn lookup(&self, id: &str) -> Option<&WorkflowDefinition> {
        self.definitions.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{ServiceMode, ServiceSpec};

    fn stub_definition(id: &str, name: &str) -> WorkflowDefinition {
        WorkflowDefinition {
            id: id.to_string(),
            name: name.to_string(),
            description: String::new(),
            parameters: HashMap::new(),
            service: ServiceSpec {
                type_: None,
                handler: "demo.handler".to_string(),
                mode: ServiceMode::Sync,
                command: None,
                args: Vec::new(),
                agent: None,
            },
            source_path: std::path::PathBuf::from("stub.md"),
        }
    }

    #[test]
    fn enumerate_returns_workflows_sorted_by_id() {
        let mut definitions = HashMap::new();
        for (id, name) in [("zebra", "Zebra"), ("apple", "Apple"), ("mango", "Mango")] {
            definitions.insert(id.to_string(), stub_definition(id, name));
        }
        let registry = Registry { definitions };

        let ids: Vec<String> = registry.enumerate().into_iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            vec!["apple".to_string(), "mango".to_string(), "zebra".to_string()],
            "expected enumerate() to return workflows sorted ascending by id, got: {ids:?}"
        );
    }
}
