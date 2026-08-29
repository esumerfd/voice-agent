//! Daemon-side workflow-creation write path (quick task 260807-shx, D-06:
//! the CLI never touches the in-process registry path directly -- this
//! module is where the daemon owns the actual filesystem write). Builds
//! frontmatter exclusively via `#[derive(Serialize)]` structs and
//! `yaml_serde::to_string` -- never by formatting untrusted values into a
//! YAML template (T-shx-02) -- so a crafted `name` can never inject sibling
//! frontmatter keys.
//!
//! Validates id/name/content sizes and collisions BEFORE any filesystem
//! call (T-shx-01/T-shx-04/T-shx-05); writes the script (when requested)
//! before the `.md` so a partially-created state never leaves a loadable
//! `.md` pointing at a missing script.
//!
//! This module owns three write shapes (quick task 260813-rm5 adds the
//! third): script-backed action, markdown-body action, and Claude-backed
//! agent. The emitted `service.agent` block must stay loadable by
//! `loader.rs`'s `RawAgent` -- both sides of that contract are proven by
//! `agent_workflow_write_integration.rs`'s load-back assertions.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use shared::{CreateWorkflowRequest, ParameterType, WorkflowWriteMode};

use super::Registry;
use crate::error::{CreateError, DeleteError};

pub const MAX_WORKFLOW_ID_LEN: usize = 64;
pub const MAX_WORKFLOW_NAME_LEN: usize = 200;
pub const MAX_CONTENT_LEN: usize = 1_048_576; // 1 MiB
pub const SCRIPTS_SUBDIR: &str = "scripts";
/// Quick task 260813-rm5, D-7: generous for real use (a workflow's agent
/// dependencies are typically a handful of reference files) while bounding
/// the per-run staging work a hostile request could otherwise inflate.
pub const MAX_AGENT_FILES: usize = 50;
/// Quick task 260813-rm5, D-7: generous for a real relative path while
/// bounding a single oversized entry.
pub const MAX_AGENT_FILE_LEN: usize = 512;

/// The daemon-side absolute paths written by a successful create.
#[derive(Debug)]
pub struct CreateOutcome {
    pub workflow_path: PathBuf,
    pub script_path: Option<PathBuf>,
}

#[derive(Serialize)]
struct FmParameter {
    #[serde(rename = "type")]
    type_: ParameterType,
    required: bool,
}

/// Quick task 260813-rm5: the agent frontmatter block, skipped-when-absent
/// field by field so an agent request that declares no options emits the
/// minimal `agent: {}` shape `loader.rs`'s `RawAgent` already documents as
/// valid (D-5) rather than baking in a CLI-side default.
#[derive(Serialize)]
struct FmAgent {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_budget_usd: Option<f64>,
}

#[derive(Serialize)]
struct FmService {
    #[serde(rename = "type")]
    type_: String,
    handler: String,
    /// Quick task 260813-rm5, D-4: an agent request always emits
    /// `Some(ServiceMode::Async)`; an action request emits `None` so the
    /// key stays absent exactly as it does today (unchanged, absent
    /// resolves to `Sync`). Typed as `Option<crate::definition::ServiceMode>`
    /// -- never a bare string -- so the enum's own lowercase serialization
    /// is what keeps the emitted token from drifting from the token
    /// `loader.rs` parses.
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<crate::definition::ServiceMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    /// Quick task 260813-rm5: `Some` for an agent request, `None` for an
    /// action request -- the same `Option`-as-discriminator shape the wire
    /// request itself uses.
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<FmAgent>,
}

#[derive(Serialize)]
struct Frontmatter {
    id: String,
    name: String,
    parameters: BTreeMap<String, FmParameter>,
    service: FmService,
}

/// Writes a workflow `.md` (and, when `req.script` is `Some`, a companion
/// executable script) into `workflows_dir` -- both write modes (quick task
/// 260812-qpn D-1/D-2). `req.mode` selects the guard: `Create` refuses when
/// `id` already exists (D-DISC-06, unchanged); `Edit` refuses when it does
/// NOT, then overwrites the existing definition in place (`fs::write`
/// already truncates, so the overwrite needs no new I/O code). Never panics
/// on any input reachable from the wire -- every failure path returns a
/// `CreateError` and leaves the directory unchanged.
pub fn create_workflow(
    workflows_dir: &Path,
    req: &CreateWorkflowRequest,
) -> Result<CreateOutcome, CreateError> {
    validate_id(&req.id)?;
    validate_name(&req.name)?;
    validate_content_len(&req.description, MAX_CONTENT_LEN)?;
    if let Some(script) = &req.script {
        validate_content_len(script, MAX_CONTENT_LEN)?;
    }
    // D-6: a workflow binds to exactly one handler shape -- refused before
    // any filesystem call, mirroring `loader.rs`'s load-time enforcement of
    // the same rule.
    if req.script.is_some() && req.agent.is_some() {
        return Err(CreateError::ConflictingServiceBinding { id: req.id.clone() });
    }
    if let Some(agent) = &req.agent {
        validate_agent_files(&agent.files)?;
    }

    fs::create_dir_all(workflows_dir).map_err(|e| CreateError::Io {
        path: workflows_dir.to_path_buf(),
        detail: e.to_string(),
    })?;

    match req.mode {
        WorkflowWriteMode::Create => check_collisions(workflows_dir, req)?,
        WorkflowWriteMode::Edit => check_exists(workflows_dir, req)?,
    }

    let md_path = workflows_dir.join(format!("{}.md", req.id));

    let script_path = if let Some(script_content) = &req.script {
        let scripts_dir = workflows_dir.join(SCRIPTS_SUBDIR);
        fs::create_dir_all(&scripts_dir).map_err(|e| CreateError::Io {
            path: scripts_dir.clone(),
            detail: e.to_string(),
        })?;
        let path = scripts_dir.join(format!("{}.sh", req.id));
        fs::write(&path, script_content).map_err(|e| CreateError::Io {
            path: path.clone(),
            detail: e.to_string(),
        })?;
        chmod_executable(&path)?;
        Some(path)
    } else {
        None
    };

    let command = req
        .script
        .as_ref()
        .map(|_| format!("{SCRIPTS_SUBDIR}/{}.sh", req.id));

    let mut parameters: BTreeMap<String, FmParameter> = BTreeMap::new();
    for param in &req.parameters {
        parameters.insert(
            param.name.clone(),
            FmParameter {
                type_: param.type_,
                required: param.required,
            },
        );
    }

    // Branch the service block on the agent field (quick task 260813-rm5):
    // with an agent block, emit the agent type/handler/async-mode/agent
    // shape and no command; with no agent block, emit exactly what this
    // module emitted before this quick task, with mode and agent absent.
    let service = if let Some(agent_config) = &req.agent {
        FmService {
            type_: "agent".to_string(),
            handler: crate::handlers::ai_agent::AGENT_HANDLER_KEY.to_string(),
            mode: Some(crate::definition::ServiceMode::Async),
            command: None,
            agent: Some(FmAgent {
                files: agent_config.files.clone(),
                timeout_secs: agent_config.timeout_secs,
                max_budget_usd: agent_config.max_budget_usd,
            }),
        }
    } else {
        FmService {
            type_: "action".to_string(),
            handler: crate::definition::DEFAULT_HANDLER.to_string(),
            mode: None,
            command,
            agent: None,
        }
    };

    let frontmatter = Frontmatter {
        id: req.id.clone(),
        name: req.name.clone(),
        parameters,
        service,
    };

    let file_contents = render_file(&frontmatter, &req.description, &md_path)?;

    fs::write(&md_path, file_contents).map_err(|e| CreateError::Io {
        path: md_path.clone(),
        detail: e.to_string(),
    })?;

    Ok(CreateOutcome {
        workflow_path: md_path,
        script_path,
    })
}

/// The daemon-side absolute paths removed by a successful delete (quick
/// task 260812-qp4).
#[derive(Debug)]
pub struct DeleteOutcome {
    pub workflow_path: PathBuf,
    /// `Some` when the deleted workflow declared a `service.command`
    /// companion script that was also removed (D-5). Always `None` in Task
    /// 1 -- Task 2 populates this.
    pub script_path: Option<PathBuf>,
}

/// Removes an existing workflow `.md` (and, when declared, its companion
/// `service.command` script) from `workflows_dir` (quick task 260812-qp4,
/// D-6: validate and resolve before any removal -- the exact inverse
/// discipline of `create_workflow`'s "validate before any filesystem
/// call"). Never panics on any input reachable from the wire -- every
/// failure path returns a `DeleteError` and leaves the directory unchanged.
///
/// Resolution: when `Registry::lookup(id)` hits, the `.md` is
/// `WorkflowDefinition::source_path` and the candidate script is
/// `service.command` (already resolved to an absolute path anchored to the
/// workflow file's own directory by `loader.rs` at load time -- never
/// re-joined here). `service.agent` bindings contribute no removal
/// candidate (D-5) -- declared file dependencies are never removed. When
/// `lookup(id)` misses, falls back per D-8 to `workflows_dir/<id>.md` when
/// that file exists on disk (an existing-but-unparseable `.md`, mirroring
/// `create_workflow`'s `AlreadyExists` fallback in the opposite direction),
/// with `workflows_dir/scripts/<id>.sh` as the candidate script when that
/// exists; if the fallback `.md` does not exist either, `NotFound`.
///
/// Containment (T-qp4-02): every candidate path that exists is
/// canonicalized and asserted to be a descendant of the canonicalized
/// `workflows_dir` before any removal -- the sole defence against a
/// hand-authored `.md`'s `service.command` steering a removal at a file
/// outside the daemon's own directory. The id itself is already covered by
/// `validate_id` plus the registry-mediated lookup (T-qp4-01).
///
/// Removal: `.md` first, then the script -- the exact inverse of
/// `create_workflow`'s write order, so no intermediate state ever leaves a
/// loadable `.md` pointing at a missing script (D-6).
pub fn delete_workflow(workflows_dir: &Path, id: &str) -> Result<DeleteOutcome, DeleteError> {
    validate_id(id).map_err(|_| DeleteError::NotFound { id: id.to_string() })?;

    let (registry, _errors) = Registry::load(workflows_dir);

    let (md_path, script_path_candidate) = match registry.lookup(id) {
        Some(definition) => (definition.source_path.clone(), definition.service.command.clone()),
        None => {
            let fallback_md = workflows_dir.join(format!("{id}.md"));
            if !fallback_md.exists() {
                return Err(DeleteError::NotFound { id: id.to_string() });
            }
            let fallback_script = workflows_dir.join(SCRIPTS_SUBDIR).join(format!("{id}.sh"));
            let script_candidate = fallback_script.exists().then_some(fallback_script);
            (fallback_md, script_candidate)
        }
    };

    assert_contained(workflows_dir, &md_path)?;
    if let Some(script_path) = &script_path_candidate {
        assert_contained(workflows_dir, script_path)?;
    }

    fs::remove_file(&md_path).map_err(|e| DeleteError::Io {
        path: md_path.clone(),
        detail: e.to_string(),
    })?;

    let removed_script_path = if let Some(script_path) = script_path_candidate {
        fs::remove_file(&script_path).map_err(|e| DeleteError::Io {
            path: script_path.clone(),
            detail: e.to_string(),
        })?;
        Some(script_path)
    } else {
        None
    };

    Ok(DeleteOutcome {
        workflow_path: md_path,
        script_path: removed_script_path,
    })
}

/// Canonicalizes `workflows_dir` and `candidate` (when `candidate` exists)
/// and asserts the candidate resolves as a descendant of the workflows dir
/// (T-qp4-02). A `candidate` that does not exist is a handled no-op here --
/// `fs::remove_file` surfaces its own `DeleteError::Io` later if it turns
/// out load-bearing; there is nothing to canonicalize for a path that isn't
/// there.
fn assert_contained(workflows_dir: &Path, candidate: &Path) -> Result<(), DeleteError> {
    if !candidate.exists() {
        return Ok(());
    }
    let canonical_dir = fs::canonicalize(workflows_dir).map_err(|e| DeleteError::Io {
        path: workflows_dir.to_path_buf(),
        detail: e.to_string(),
    })?;
    let canonical_candidate = fs::canonicalize(candidate).map_err(|e| DeleteError::Io {
        path: candidate.to_path_buf(),
        detail: e.to_string(),
    })?;
    if !canonical_candidate.starts_with(&canonical_dir) {
        return Err(DeleteError::PathEscape {
            path: candidate.to_path_buf(),
            reason: format!("outside workflows dir {}", canonical_dir.display()),
        });
    }
    Ok(())
}

/// Non-empty, at most `MAX_WORKFLOW_ID_LEN` characters, every character in
/// `[a-z0-9_-]`, and the first character alphanumeric -- rejects `/`, `\`,
/// `.`, `..`, absolute paths, and a leading `-` by construction (T-shx-01).
fn validate_id(id: &str) -> Result<(), CreateError> {
    if id.is_empty() {
        return Err(CreateError::InvalidId {
            id: id.to_string(),
            reason: "must not be empty".to_string(),
        });
    }
    if id.chars().count() > MAX_WORKFLOW_ID_LEN {
        return Err(CreateError::InvalidId {
            id: id.to_string(),
            reason: format!("must be at most {MAX_WORKFLOW_ID_LEN} characters"),
        });
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(CreateError::InvalidId {
            id: id.to_string(),
            reason: "must contain only lowercase letters, digits, `_`, and `-`".to_string(),
        });
    }
    let first = id.chars().next().expect("id is non-empty, checked above");
    if !first.is_ascii_alphanumeric() {
        return Err(CreateError::InvalidId {
            id: id.to_string(),
            reason: "must start with an alphanumeric character".to_string(),
        });
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), CreateError> {
    if name.trim().is_empty() {
        return Err(CreateError::InvalidName {
            reason: "must not be empty".to_string(),
        });
    }
    if name.chars().count() > MAX_WORKFLOW_NAME_LEN {
        return Err(CreateError::InvalidName {
            reason: format!("must be at most {MAX_WORKFLOW_NAME_LEN} characters"),
        });
    }
    Ok(())
}

fn validate_content_len(content: &str, max: usize) -> Result<(), CreateError> {
    let len = content.len();
    if len > max {
        return Err(CreateError::ContentTooLarge { len, max });
    }
    Ok(())
}

/// Quick task 260813-rm5, D-7: each `agent.files` entry must be non-empty,
/// free of NUL bytes, and within `MAX_AGENT_FILE_LEN`; the list itself must
/// be within `MAX_AGENT_FILES`. Deliberately NOT path-contained -- an entry
/// is a daemon-side path staged read-only into a per-run scratch dir, and
/// the wire already grants strictly more than that (a create request
/// supplies arbitrary script CONTENT chmod'd `0755` for `action.immediate`
/// to execute), so a containment check here would buy no privilege
/// reduction while breaking parity with a hand-authored `.md` (T-rm5-05,
/// accepted).
fn validate_agent_files(files: &[String]) -> Result<(), CreateError> {
    if files.len() > MAX_AGENT_FILES {
        return Err(CreateError::InvalidAgentFile {
            reason: format!(
                "agent.files declares {} entries, exceeding the {MAX_AGENT_FILES} limit",
                files.len()
            ),
        });
    }
    for entry in files {
        if entry.is_empty() {
            return Err(CreateError::InvalidAgentFile {
                reason: "agent.files entry must not be empty".to_string(),
            });
        }
        if entry.contains('\0') {
            return Err(CreateError::InvalidAgentFile {
                reason: "agent.files entry must not contain a NUL byte".to_string(),
            });
        }
        if entry.chars().count() > MAX_AGENT_FILE_LEN {
            return Err(CreateError::InvalidAgentFile {
                reason: format!(
                    "agent.files entry exceeds the {MAX_AGENT_FILE_LEN} character limit"
                ),
            });
        }
    }
    Ok(())
}

/// D-DISC-06: refuse, never overwrite. Collides when `Registry::load`
/// already resolves the id, when `<id>.md` exists on disk (even if
/// unparseable), or when a script was requested and `scripts/<id>.sh`
/// already exists.
fn check_collisions(workflows_dir: &Path, req: &CreateWorkflowRequest) -> Result<(), CreateError> {
    let (registry, _errors) = Registry::load(workflows_dir);
    let md_path = workflows_dir.join(format!("{}.md", req.id));

    if registry.lookup(&req.id).is_some() || md_path.exists() {
        return Err(CreateError::AlreadyExists {
            id: req.id.clone(),
            path: md_path,
        });
    }

    if req.script.is_some() {
        let script_path = workflows_dir.join(SCRIPTS_SUBDIR).join(format!("{}.sh", req.id));
        if script_path.exists() {
            return Err(CreateError::AlreadyExists {
                id: req.id.clone(),
                path: script_path,
            });
        }
    }

    Ok(())
}

/// Edit mode's inverse guard (quick task 260812-qpn, D-2/T-qpn-02): refuses
/// UNLESS `req.id` already resolves, either through `Registry::load` or as a
/// bare `<id>.md` on disk (mirrors `check_collisions`'s existing-but-
/// unparseable fallback, in the opposite direction) -- an edit can only ever
/// rewrite a path that already resolves as a workflow, never create a new
/// file anywhere.
fn check_exists(workflows_dir: &Path, req: &CreateWorkflowRequest) -> Result<(), CreateError> {
    let (registry, _errors) = Registry::load(workflows_dir);
    let md_path = workflows_dir.join(format!("{}.md", req.id));

    if registry.lookup(&req.id).is_some() || md_path.exists() {
        Ok(())
    } else {
        Err(CreateError::NotFound {
            id: req.id.clone(),
            path: md_path,
        })
    }
}

#[cfg(unix)]
fn chmod_executable(path: &Path) -> Result<(), CreateError> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = fs::metadata(path)
        .map_err(|e| CreateError::Io {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).map_err(|e| CreateError::Io {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })
}

#[cfg(not(unix))]
fn chmod_executable(_path: &Path) -> Result<(), CreateError> {
    Ok(())
}

/// Serializes `frontmatter` via `yaml_serde::to_string` (never by
/// formatting untrusted values into a template, T-shx-02) and wraps it in
/// `---` delimiters around the description body. Whether the serializer
/// itself emits a leading `---\n` is an implementation detail -- stripped
/// here if present so the final file never carries a doubled marker.
fn render_file(frontmatter: &Frontmatter, description: &str, path_for_errors: &Path) -> Result<String, CreateError> {
    let mut yaml = yaml_serde::to_string(frontmatter).map_err(|e| CreateError::Io {
        path: path_for_errors.to_path_buf(),
        detail: format!("failed to serialize frontmatter: {e}"),
    })?;

    if let Some(rest) = yaml.strip_prefix("---\n") {
        yaml = rest.to_string();
    }
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }

    Ok(format!("---\n{yaml}---\n{description}\n"))
}
