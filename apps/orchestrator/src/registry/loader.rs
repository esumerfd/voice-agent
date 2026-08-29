//! Real registry read path (WF-01/02/03): scan the directory for `*.md`
//! workflow definitions, split frontmatter from body with gray_matter,
//! deserialize the raw frontmatter YAML text with the pinned maintained
//! YAML crate (`yaml_serde`), validate required fields + the closed
//! parameter-type enum, and collect per-file errors (skip-and-warn, D-07)
//! rather than aborting the scan.
//!
//! Raw deserialization structs use all-`Option` fields so a missing
//! required field is named by `validate_definition`, not by a serde
//! hard-fail (D-08). No `triggers`/`intent` fields are declared (D-05).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use gray_matter::engine::YAML;
use gray_matter::{Matter, ParsedEntity};
use serde::Deserialize;
use walkdir::WalkDir;

use crate::definition::{
    AgentSpec, ParameterSpec, ParameterType, ServiceMode, ServiceSpec, WorkflowDefinition,
};
use crate::error::LoadError;

use super::Registry;

/// Raw parameter frontmatter — all fields `Option` so a missing/invalid
/// value is named by `validate_definition`, not a serde hard-fail.
#[derive(Debug, Deserialize)]
struct RawParameter {
    #[serde(rename = "type")]
    type_: Option<String>,
    description: Option<String>,
    required: Option<bool>,
}

/// Raw service frontmatter (Service Contract handler binding, D-08).
/// `mode` is a raw `Option<String>` (not `Option<ServiceMode>`) so an
/// unrecognized value is named by `validate_definition` as
/// `LoadError::UnknownServiceMode`, not a serde hard-fail (D-10).
/// `command`/`args` (quick task 260720-s7i) are plain optional serde fields
/// -- no rename needed, and deliberately NOT validated for
/// existence/executability here (mirrors `ProcessService`'s
/// fail-at-spawn-time precedent).
#[derive(Debug, Deserialize)]
struct RawService {
    #[serde(rename = "type")]
    type_: Option<String>,
    handler: Option<String>,
    #[serde(rename = "mode")]
    mode: Option<String>,
    command: Option<String>,
    args: Option<Vec<String>>,
    /// AI-agent binding (06-01, SVC-03). `Option<RawAgent>` (not a bare
    /// struct) so a `service.agent:` key with an explicit YAML `null` value
    /// deserializes as absent (`None`), matching every other optional block
    /// in this file -- an author declaring no options writes `agent: {}`
    /// instead (deserializes to `Some(RawAgent{ files: None, .. })`).
    agent: Option<RawAgent>,
}

/// Raw AI-agent frontmatter (06-01, SVC-03). All fields `Option` so a
/// missing value resolves to `AgentSpec::default()`'s corresponding default
/// -- there is no "invalid" value shape to reject here (unlike
/// `parameters.*.type`'s closed enum).
#[derive(Debug, Deserialize)]
struct RawAgent {
    files: Option<Vec<String>>,
    model: Option<String>,
    max_budget_usd: Option<f64>,
    timeout_secs: Option<u64>,
}

/// Raw top-level frontmatter. Deliberately excludes `triggers`/`intent`
/// (D-05) — Phase 1 parses/validates only the M1 fields.
#[derive(Debug, Deserialize)]
struct RawFrontmatter {
    id: Option<String>,
    name: Option<String>,
    parameters: Option<HashMap<String, RawParameter>>,
    service: Option<RawService>,
}

impl Registry {
    /// Load from a directory; returns `(registry, errors)` — D-07 skip-and-warn.
    ///
    /// Scans `scan_dir` for `*.md` files (any depth, sorted by file name for
    /// deterministic duplicate-id resolution), attempts to load each one
    /// independently, and partitions the results: successes populate the
    /// registry, failures are collected into the error vec without
    /// aborting the scan. A duplicate id is reported as
    /// `LoadError::DuplicateId` and the first-loaded definition wins (D-09).
    pub fn load(scan_dir: impl AsRef<Path>) -> (Self, Vec<LoadError>) {
        let mut definitions: HashMap<String, WorkflowDefinition> = HashMap::new();
        let mut errors: Vec<LoadError> = Vec::new();

        // Walk-level errors (nonexistent scan dir, unreadable subdirectory)
        // are collected as `LoadError::Io` rather than dropped — a missing
        // workflows directory must be distinguishable from "no workflows
        // defined" (D-07). Collected into a Vec so the mutable borrow of
        // `errors` ends before the load loop below pushes per-file errors.
        let entries: Vec<walkdir::DirEntry> = WalkDir::new(scan_dir.as_ref())
            .sort_by_file_name()
            .into_iter()
            .filter_map(|e| match e {
                Ok(entry) => Some(entry),
                Err(err) => {
                    errors.push(LoadError::Io {
                        path: err
                            .path()
                            .map(Path::to_path_buf)
                            .unwrap_or_else(|| scan_dir.as_ref().to_path_buf()),
                        detail: err.to_string(),
                    });
                    None
                }
            })
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("md"))
            // 06-05 (SVC-03): a `.md` file whose immediate parent directory
            // is literally named `agent` is AI-agent reference/prose
            // material staged into a scratch directory via
            // `service.agent.files` -- never itself a workflow definition.
            // It has no YAML frontmatter, so without this exclusion the
            // scan below would report it as a broken workflow
            // (`LoadError::NoFrontmatter`) on every load. This mirrors why
            // `workflows/scripts/*.sh` companion files never collide with
            // this scan (there, a different extension; here, a reserved
            // directory name) and closes the exact "mistakenly
            // discoverable via `orchestrator list`" failure mode quick
            // task 260807-s3i already fixed once for `get_time.md`.
            .filter(|e| {
                e.path().parent().and_then(|p| p.file_name()).and_then(|n| n.to_str())
                    != Some("agent")
            })
            .collect();

        for entry in entries {
            match load_one(entry.path()) {
                Ok(def) => {
                    if definitions.contains_key(&def.id) {
                        errors.push(LoadError::DuplicateId {
                            path: entry.path().to_path_buf(),
                            id: def.id,
                        });
                    } else {
                        definitions.insert(def.id.clone(), def);
                    }
                }
                Err(err) => errors.push(err),
            }
        }

        (Registry { definitions }, errors)
    }
}

/// Load and validate a single workflow definition file.
fn load_one(path: &Path) -> Result<WorkflowDefinition, LoadError> {
    let (raw, description) = parse_file(path)?;
    validate_definition(raw, description, path)
}

/// Split frontmatter from body via gray_matter, then deserialize the raw
/// frontmatter YAML text with the pinned maintained YAML crate.
fn parse_file(path: &Path) -> Result<(RawFrontmatter, String), LoadError> {
    let content = fs::read_to_string(path).map_err(|e| LoadError::Io {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;

    let matter: Matter<YAML> = Matter::new();
    let entity: ParsedEntity = matter.parse(&content).map_err(|e| LoadError::ParseError {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;

    if entity.matter.trim().is_empty() {
        return Err(LoadError::NoFrontmatter {
            path: path.to_path_buf(),
        });
    }

    let raw: RawFrontmatter =
        yaml_serde::from_str(&entity.matter).map_err(|e| LoadError::ParseError {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;

    // Body prose is the description (D-05) — trimmed to avoid leading/
    // trailing newlines from the markdown body (Pitfall 4).
    let description = entity.content.trim().to_string();

    Ok((raw, description))
}

/// A required string field is "missing" when absent, empty, or
/// whitespace-only — an empty `id`/`name`/`service.handler` is
/// unaddressable/un-dispatchable and must be rejected, not registered.
fn required_string(value: Option<String>, field: &str, path: &Path) -> Result<String, LoadError> {
    match value {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(LoadError::MissingField {
            path: path.to_path_buf(),
            field: field.to_string(),
        }),
    }
}

/// Validate required fields + the closed parameter-type enum, naming the
/// offending field/value on failure (D-06/D-08). `parameters` defaults to
/// an empty map when absent or `{}` (Pitfall 3).
fn validate_definition(
    raw: RawFrontmatter,
    description: String,
    path: &Path,
) -> Result<WorkflowDefinition, LoadError> {
    let path_buf: PathBuf = path.to_path_buf();

    let id = required_string(raw.id, "id", path)?;

    let name = required_string(raw.name, "name", path)?;

    let raw_service = raw.service.ok_or_else(|| LoadError::MissingField {
        path: path_buf.clone(),
        field: "service.handler".to_string(),
    })?;

    // Absent, empty, or whitespace-only resolves to the shared run-now
    // fallback (D-10-style non-breaking default) rather than a MissingField
    // error -- a free-form handler key has no "bad value", only
    // "unspecified", unlike `mode`'s closed enum below.
    let handler = match raw_service.handler {
        Some(s) if !s.trim().is_empty() => s,
        _ => crate::definition::DEFAULT_HANDLER.to_string(),
    };

    // Absent or `sync` resolves to Sync (D-10 non-breaking default);
    // any value other than sync/async is a clean LoadError, never a
    // silent default to Async and never a panic (T-04-06).
    let mode = match raw_service.mode.as_deref() {
        Some("sync") | None => ServiceMode::Sync,
        Some("async") => ServiceMode::Async,
        Some(other) => {
            return Err(LoadError::UnknownServiceMode {
                path: path_buf.clone(),
                value: other.to_string(),
            })
        }
    };

    let mut parameters: HashMap<String, ParameterSpec> = HashMap::new();
    for (param_name, raw_param) in raw.parameters.unwrap_or_default() {
        let type_value = raw_param.type_.ok_or_else(|| LoadError::MissingField {
            path: path_buf.clone(),
            field: format!("parameters.{param_name}.type"),
        })?;

        let type_ = match type_value.as_str() {
            "string" => ParameterType::String,
            "int" => ParameterType::Int,
            "bool" => ParameterType::Bool,
            other => {
                return Err(LoadError::UnknownParameterType {
                    path: path_buf.clone(),
                    value: other.to_string(),
                })
            }
        };

        parameters.insert(
            param_name,
            ParameterSpec {
                type_,
                description: raw_param.description,
                required: raw_param.required.unwrap_or(false),
            },
        );
    }

    // Optional server-side script binding (quick task 260720-s7i): parsed
    // from trusted frontmatter only, never validated for
    // existence/executability at load time (mirrors ProcessService's
    // fail-at-spawn-time precedent -- a missing script surfaces later as a
    // runtime ServiceError, never a LoadError here). A relative command is
    // anchored to the directory containing this workflow file (`path`,
    // already in scope) rather than left relative to whatever CWD the
    // daemon process happens to be started from (quick task 260806-sp5) --
    // an absolute command is stored exactly as declared, never re-joined.
    // Still no filesystem call is made here, so the fail-at-spawn-time
    // precedent above is preserved unchanged.
    let command = raw_service.command.map(|c| {
        let raw = PathBuf::from(c);
        if raw.is_absolute() {
            raw
        } else {
            path.parent().unwrap_or_else(|| Path::new("")).join(raw)
        }
    });
    let args = raw_service.args.unwrap_or_default();

    // A workflow binds to exactly one handler shape (06-01, SVC-03) --
    // reject before any further mapping so neither `command` nor `agent`
    // partially applies.
    if command.is_some() && raw_service.agent.is_some() {
        return Err(LoadError::ConflictingServiceBinding { path: path_buf });
    }

    // Optional AI-agent binding (06-01, SVC-03, D-03): parsed from trusted
    // frontmatter only, mirroring `command`'s anchoring rule EXACTLY for
    // every declared `files` entry -- an absolute entry is stored verbatim;
    // a relative entry is joined onto this workflow file's own directory
    // (`path.parent()`), never the daemon's CWD. No filesystem call is made
    // here (same fail-at-use-time precedent as `command`).
    let agent: Option<AgentSpec> = raw_service.agent.map(|raw_agent| {
        let files = raw_agent
            .files
            .unwrap_or_default()
            .into_iter()
            .map(|f| {
                let raw = PathBuf::from(f);
                if raw.is_absolute() {
                    raw
                } else {
                    path.parent().unwrap_or_else(|| Path::new("")).join(raw)
                }
            })
            .collect();
        AgentSpec {
            files,
            model: raw_agent.model,
            max_budget_usd: raw_agent.max_budget_usd,
            timeout_secs: raw_agent.timeout_secs,
        }
    });

    Ok(WorkflowDefinition {
        id,
        name,
        description,
        parameters,
        service: ServiceSpec {
            type_: raw_service.type_,
            handler,
            mode,
            command,
            args,
            agent,
        },
        source_path: path_buf,
    })
}
