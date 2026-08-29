//! `workflow create`/`workflow edit` subcommand entry point (quick task
//! 260807-shx; edit mode added quick task 260812-qpn D-4; agent authoring
//! added quick task 260813-rm5). One module serves both modes via a `mode`
//! parameter on `run` rather than a near-duplicate `edit.rs`: this file
//! holds ~120 lines of pure helpers (`parse_params`, `resolve_source`,
//! `classify_as_script`, `derive_display_name`, `agent_config_from_flags`)
//! that edit needs verbatim, and a separate `edit.rs` would either
//! duplicate them or reach across modules with `super::create::…`, which is
//! fragile under this crate's `#[path]` test-inclusion trick (D-DISC-02 --
//! `orchestrator-cli` has no `[lib]` target, so test files include
//! `../src/*.rs` directly).
//!
//! Mirrors `run.rs`'s conventions: generic over `Write` for stdout so tests
//! capture output deterministically, takes `&dyn shared::WorkflowCreator`
//! (never a concrete client type and never a `crate::ws_client` reference --
//! that is what keeps the `#[path]` test-inclusion trick compiling, D-DISC-02),
//! renders both success and failure as JSON to `out`, and never panics on
//! caller input.
//!
//! Known, accepted consequence of the locked source-resolution rule
//! (D-DISC-04): `orchestrator workflow create demo "my-script.sh"` produces a
//! **markdown** workflow whose body is the literal text `my-script.sh`
//! whenever no file named `my-script.sh` exists in the CLI's CWD --
//! `--script` is the escape hatch. Same rule applies verbatim to edit (D-1).
//! This module owns the workflow write path in all three shapes (script-
//! backed action, markdown-body action, Claude-backed agent) and both
//! modes, and it still takes only `&dyn WorkflowCreator` and never a
//! `crate::ws_client` reference (D-DISC-02, unchanged by 260813-rm5).

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use serde_json::json;

use shared::{
    AgentConfig, CreateWorkflowRequest, ParameterDescriptor, ParameterType, WorkflowCreator,
    WorkflowWriteMode,
};

/// Raw `--agent`/`--agent-file`/`--timeout-secs`/`--max-budget-usd` flag
/// values (quick task 260813-rm5), converted to `Option<shared::AgentConfig>`
/// by `agent_config_from_flags` before `run` builds the request. A plain
/// struct (not four separate `run` parameters, D-2 in the plan's Task 2
/// action) -- `run` is already at the argument-count lint threshold.
#[derive(Debug, Clone, Default)]
pub struct AgentFlags {
    pub agent: bool,
    pub agent_file: Vec<String>,
    pub timeout_secs: Option<u64>,
    pub max_budget_usd: Option<f64>,
}

/// Converts raw agent flag values into `Option<AgentConfig>` (D-1/D-5).
/// Performs NO filesystem access of any kind (D-3) -- `agent_file` entries
/// cross the wire exactly as declared, verbatim, in order. Never panics --
/// every rejection returns a distinct, descriptive error string naming the
/// offending flag, mirroring `parse_params`'s convention. This is defense
/// in depth behind clap's own `requires = "agent"` relationship on each
/// option-carrying flag -- reachable only when `create::run` is called
/// directly (bypassing the CLI parser), as the direct-call tests do.
pub fn agent_config_from_flags(flags: &AgentFlags) -> Result<Option<AgentConfig>, String> {
    if flags.agent {
        return Ok(Some(AgentConfig {
            files: flags.agent_file.clone(),
            timeout_secs: flags.timeout_secs,
            max_budget_usd: flags.max_budget_usd,
        }));
    }
    if !flags.agent_file.is_empty() {
        return Err("--agent-file requires --agent".to_string());
    }
    if flags.timeout_secs.is_some() {
        return Err("--timeout-secs requires --agent".to_string());
    }
    if flags.max_budget_usd.is_some() {
        return Err("--max-budget-usd requires --agent".to_string());
    }
    Ok(None)
}

/// Runs the `workflow create`/`workflow edit` subcommand: resolves `source`
/// locally, classifies it as script-vs-markdown-vs-agent, parses `--param`
/// specs, sends the resolved CONTENT (never a client-local path) over
/// `WorkflowCreator` with `mode` set on the request, and renders
/// success/error JSON to `out`. Returns the process exit code (0 = success,
/// nonzero = any failure). Success rendering is mode-aware (D-3): create
/// reports `created: true`, edit reports `updated: true` -- the daemon-side
/// `CreateWorkflowResponse.created` field itself is unchanged and means "the
/// write succeeded" for either mode.
///
/// `agent_flags` short-circuits classification entirely when `--agent` is
/// present (quick task 260813-rm5, D-1): `classify_as_script`'s shebang/
/// extension sniffing is never consulted for an agent request -- no
/// heuristic can distinguish "a prompt for Claude" from "prose describing a
/// workflow", and guessing wrong would silently spend money on an
/// unattended `claude -p` run. `<source>` resolves identically to the
/// markdown path (D-2): `resolve_source`'s content, or `description` when
/// given, becomes the prompt body.
#[allow(clippy::too_many_arguments)]
pub async fn run<W: Write>(
    client: &dyn WorkflowCreator,
    mode: WorkflowWriteMode,
    id: &str,
    source: &str,
    display_name: Option<&str>,
    description: Option<&str>,
    param_specs: &[String],
    force_script: bool,
    force_markdown: bool,
    agent_flags: &AgentFlags,
    out: &mut W,
) -> std::io::Result<i32> {
    let parameters = match parse_params(param_specs) {
        Ok(parameters) => parameters,
        Err(message) => return render_error(out, message),
    };

    let agent = match agent_config_from_flags(agent_flags) {
        Ok(agent) => agent,
        Err(message) => return render_error(out, message),
    };

    let content = resolve_source(source);

    let name = display_name
        .map(|s| s.to_string())
        .unwrap_or_else(|| derive_display_name(id));

    let (body_description, script) = if agent.is_some() {
        // D-1: an agent request short-circuits classification -- the body
        // is the resolved content (or the description override), exactly
        // like the markdown path, and no script is ever sent.
        (description.map(|s| s.to_string()).unwrap_or(content), None)
    } else {
        let is_script = classify_as_script(&content, source, force_script, force_markdown);
        if is_script {
            (description.map(|s| s.to_string()).unwrap_or_default(), Some(content))
        } else {
            (description.map(|s| s.to_string()).unwrap_or(content), None)
        }
    };

    let req = CreateWorkflowRequest {
        id: id.to_string(),
        name,
        description: body_description,
        script,
        parameters,
        mode,
        agent,
    };

    let resp = client.create_workflow(req).await;

    if resp.created {
        let success_key = match mode {
            WorkflowWriteMode::Create => "created",
            WorkflowWriteMode::Edit => "updated",
        };
        let mut fields = serde_json::Map::new();
        fields.insert(success_key.to_string(), json!(true));
        fields.insert("workflow_path".to_string(), json!(resp.workflow_path));
        fields.insert("script_path".to_string(), json!(resp.script_path));
        render_success(out, serde_json::Value::Object(fields))
    } else {
        render_error(out, resp.error.unwrap_or_else(|| "unknown error".to_string()))
    }
}

/// Parses one `--param <name>:<type>[:<required>]` spec (D-DISC-05).
/// `<type>` is one of `string`, `int`, `bool`; `<required>` is one of
/// `required`, `optional`, `true`, `false` (absent means optional). Never
/// panics -- every rejection returns a distinct, descriptive error string.
pub fn parse_param(spec: &str) -> Result<ParameterDescriptor, String> {
    let parts: Vec<&str> = spec.split(':').collect();
    if parts.len() != 2 && parts.len() != 3 {
        return Err(format!(
            "invalid --param `{spec}`: expected `name:type` or `name:type:required`"
        ));
    }

    let name = parts[0];
    if name.is_empty() {
        return Err(format!("invalid --param `{spec}`: parameter name must not be empty"));
    }

    let type_ = match parts[1] {
        "string" => ParameterType::String,
        "int" => ParameterType::Int,
        "bool" => ParameterType::Bool,
        other => {
            return Err(format!(
                "invalid --param `{spec}`: unknown type `{other}` (expected string, int, or bool)"
            ))
        }
    };

    let required = match parts.get(2).copied() {
        None => false,
        Some("required") | Some("true") => true,
        Some("optional") | Some("false") => false,
        Some(other) => {
            return Err(format!(
                "invalid --param `{spec}`: unknown required token `{other}` (expected required, optional, true, or false)"
            ))
        }
    };

    Ok(ParameterDescriptor {
        name: name.to_string(),
        type_,
        required,
    })
}

/// Parses every `--param` spec and rejects a duplicate parameter name
/// (D-DISC-05) after all individual specs have parsed successfully.
pub fn parse_params(specs: &[String]) -> Result<Vec<ParameterDescriptor>, String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut parameters = Vec::with_capacity(specs.len());
    for spec in specs {
        let descriptor = parse_param(spec)?;
        if !seen.insert(descriptor.name.clone()) {
            return Err(format!("duplicate --param name `{}`", descriptor.name));
        }
        parameters.push(descriptor);
    }
    Ok(parameters)
}

/// Resolves `<source>` locally (D-DISC-04, generalizing CONTEXT.md's D#1 --
/// the daemon does the write, so it needs content, never a client-local
/// path): if `source` names an existing local file, its contents are read;
/// otherwise the literal `source` string itself is the content.
pub fn resolve_source(source: &str) -> String {
    let path = Path::new(source);
    if path.is_file() {
        std::fs::read_to_string(path).unwrap_or_else(|_| source.to_string())
    } else {
        source.to_string()
    }
}

/// Script-vs-markdown classification (D-DISC-04), CLI-side, resolved before
/// sending -- the daemon never re-guesses. Order of precedence: an explicit
/// `--script`/`--markdown` flag wins outright; else a shebang in the
/// resolved content forces script; else a `.sh`/`.bash`/`.zsh`/`.py`
/// extension on an existing local `source` file forces script; else
/// markdown.
pub fn classify_as_script(content: &str, source: &str, force_script: bool, force_markdown: bool) -> bool {
    if force_script {
        return true;
    }
    if force_markdown {
        return false;
    }
    if content.as_bytes().starts_with(b"#!") {
        return true;
    }
    let path = Path::new(source);
    if path.is_file() {
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if matches!(ext, "sh" | "bash" | "zsh" | "py") {
                return true;
            }
        }
    }
    false
}

/// Default frontmatter `name:` derivation (D-DISC-08): `my_workflow` ->
/// `My Workflow` -- split on `_`/`-`, uppercase each initial.
pub fn derive_display_name(id: &str) -> String {
    id.split(['_', '-'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_success<W: Write>(out: &mut W, value: serde_json::Value) -> std::io::Result<i32> {
    writeln!(out, "{}", pretty_json(&value))?;
    Ok(0)
}

fn render_error<W: Write>(out: &mut W, message: String) -> std::io::Result<i32> {
    writeln!(out, "{}", pretty_json(&json!({ "error": message })))?;
    Ok(1)
}

fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| format!("{value:?}"))
}
