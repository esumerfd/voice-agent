//! CLI argument surface (CLI-01): `orchestrator list`/`orchestrator run`.
//!
//! `--port` is flag-only (D-02/D-06): deliberately NO env-var fallback,
//! unlike the now-removed flag this CLI used to carry for the workflow
//! definitions folder (the registry moved to `orchestratord`, D-06 -- the
//! daemon owns it via its own equivalent flag). The daemon's WS port must
//! never be silently overridden by an inherited environment variable on the
//! client side either.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "orchestrator")]
pub struct Cli {
    /// WS port of the running `orchestratord` daemon to connect to
    /// (flag-only, D-02/D-06 -- no environment-variable override).
    #[arg(long, default_value_t = shared::DEFAULT_PORT)]
    pub port: u16,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// List all available workflows
    List,
    /// Run a named workflow with dynamic parameter flags (CLI-02).
    ///
    /// Flags are not known at compile time — they depend on the named
    /// workflow's declared `ParameterSpec` (D-02) — so `raw_flags` captures
    /// every trailing token verbatim (including further `--`-prefixed
    /// tokens) for `run.rs` to parse and coerce.
    Run {
        workflow_id: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        raw_flags: Vec<String>,
    },
    /// Manage workflow definitions (quick task 260807-shx).
    Workflow {
        #[command(subcommand)]
        command: WorkflowCommands,
    },
    /// Tail the orchestratord daemon's log file (quick task 260812-qeg).
    ///
    /// Daemon-independent: works even with orchestratord not running --
    /// this never opens a WS connection. Default is one-shot, printing the
    /// last `--lines` lines and exiting (D-1); with no `--file`, the daemon
    /// log path is auto-discovered (D-4). `--follow`/`-f` additionally
    /// streams newly appended lines like `tail -f`, until interrupted.
    Log {
        /// Explicit log file path, overriding auto-discovery entirely.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Number of trailing lines to print.
        #[arg(long = "lines", short = 'n', default_value_t = 20)]
        lines: usize,
        /// Stream newly appended log lines after the initial tail, like
        /// `tail -f`, until interrupted.
        #[arg(long = "follow", short = 'f')]
        follow: bool,
    },
}

#[derive(Subcommand)]
pub enum WorkflowCommands {
    /// Create a new workflow `.md` (and, when `<source>` classifies as a
    /// script, a companion executable script) in the DAEMON's own
    /// workflows directory over the wire -- the CLI never writes to the
    /// filesystem directly (D-06/D-DISC-01). `<id>` is the workflow id AND
    /// the `.md` filename stem, so `orchestrator run <id>` works
    /// immediately after create (D-DISC-08).
    Create {
        id: String,
        source: String,
        /// Overrides the default title-cased `<id>`-derived display name.
        #[arg(long = "display-name")]
        display_name: Option<String>,
        /// Overrides the default description (markdown body prose, or
        /// empty for a script-backed workflow).
        #[arg(long)]
        description: Option<String>,
        /// Repeatable `name:type[:required]` parameter declaration
        /// (D-DISC-05).
        #[arg(long = "param")]
        param: Vec<String>,
        /// Force script classification regardless of shebang/extension
        /// sniffing (D-DISC-04).
        #[arg(long, conflicts_with_all = ["markdown", "agent"])]
        script: bool,
        /// Force markdown classification regardless of shebang/extension
        /// sniffing (D-DISC-04).
        #[arg(long, conflicts_with_all = ["script", "agent"])]
        markdown: bool,
        /// Produce a Claude-backed (`service.type: agent`) workflow instead
        /// of an action one (quick task 260813-rm5, D-1) -- the sole way to
        /// reach an agent-type workflow over the wire; never inferred from
        /// content. `<source>` resolves identically to the markdown path
        /// (D-2) and becomes the prompt sent to `claude -p`. Conflicts with
        /// `--script`/`--markdown` -- all three are explicit overrides of
        /// the same classification.
        #[arg(long, conflicts_with_all = ["script", "markdown"])]
        agent: bool,
        /// Repeatable declared agent file dependency (quick task 260813-rm5,
        /// D-3). Requires `--agent`. The path is DAEMON-side and is NEVER
        /// resolved, read, or stat'd by the CLI -- it crosses the wire
        /// verbatim and is anchored onto the workflow `.md`'s own directory
        /// by the daemon's registry loader, exactly like a hand-authored
        /// `.md`'s `files:` list.
        #[arg(long = "agent-file", requires = "agent")]
        agent_file: Vec<String>,
        /// Wall-clock ceiling in seconds for a single `claude -p` invocation
        /// (quick task 260813-rm5, D-4/D-5). Requires `--agent`. An agent
        /// workflow is always async. Omitting this flag omits the
        /// frontmatter key entirely, so the daemon's own documented default
        /// applies at runtime.
        #[arg(long = "timeout-secs", requires = "agent")]
        timeout_secs: Option<u64>,
        /// Hard dollar cap for a single `claude -p` invocation (quick task
        /// 260813-rm5, D-5). Requires `--agent`. Omitting this flag omits
        /// the frontmatter key entirely, so the daemon's own documented
        /// default applies at runtime.
        #[arg(long = "max-budget-usd", requires = "agent")]
        max_budget_usd: Option<f64>,
    },
    /// Remove an existing workflow `.md` (and, when the workflow is
    /// script-backed, its companion script) from the DAEMON's own
    /// workflows directory over the wire -- the CLI never touches the
    /// filesystem directly (D-06/D-DISC-01), mirroring `Create` exactly.
    /// An id the daemon cannot resolve is a nonzero-exit error naming the
    /// id (D-2) -- not idempotent, and no `--force`/`--if-exists` flag
    /// ships in this quick task.
    Delete { id: String },
    /// Overwrite an EXISTING workflow's `.md` (and, when `<source>`
    /// classifies as a script, its companion script) in the DAEMON's own
    /// workflows directory over the wire -- the CLI never writes to the
    /// filesystem directly (D-06/D-DISC-01), the exact inverse guard of
    /// `Create`: `<id>` MUST already exist, or the daemon refuses and names
    /// the id (quick task 260812-qpn D-2). An edit is a full REPLACE, not a
    /// patch -- omitting `--param` clears the parameter map exactly as it
    /// would on a create (D-1) -- and the change is visible immediately to
    /// `list`/`run` with no daemon restart (D-6).
    Edit {
        id: String,
        source: String,
        /// Overrides the default title-cased `<id>`-derived display name.
        #[arg(long = "display-name")]
        display_name: Option<String>,
        /// Overrides the default description (markdown body prose, or
        /// empty for a script-backed workflow).
        #[arg(long)]
        description: Option<String>,
        /// Repeatable `name:type[:required]` parameter declaration
        /// (D-DISC-05).
        #[arg(long = "param")]
        param: Vec<String>,
        /// Force script classification regardless of shebang/extension
        /// sniffing (D-DISC-04).
        #[arg(long, conflicts_with_all = ["markdown", "agent"])]
        script: bool,
        /// Force markdown classification regardless of shebang/extension
        /// sniffing (D-DISC-04).
        #[arg(long, conflicts_with_all = ["script", "agent"])]
        markdown: bool,
        /// Produce a Claude-backed (`service.type: agent`) workflow instead
        /// of an action one (quick task 260813-rm5, D-1/D-8) -- converts an
        /// existing action-type workflow to agent-type; edit is a full
        /// REPLACE, so this needs no special-casing beyond the request
        /// simply stopping to declare `service.command`. `<source>`
        /// resolves identically to the markdown path (D-2) and becomes the
        /// prompt sent to `claude -p`. Conflicts with `--script`/
        /// `--markdown`.
        #[arg(long, conflicts_with_all = ["script", "markdown"])]
        agent: bool,
        /// Repeatable declared agent file dependency (quick task 260813-rm5,
        /// D-3). Requires `--agent`. The path is DAEMON-side and is NEVER
        /// resolved, read, or stat'd by the CLI.
        #[arg(long = "agent-file", requires = "agent")]
        agent_file: Vec<String>,
        /// Wall-clock ceiling in seconds for a single `claude -p` invocation
        /// (quick task 260813-rm5, D-4/D-5). Requires `--agent`. Omitting
        /// this flag omits the frontmatter key entirely, so the daemon's
        /// own documented default applies at runtime.
        #[arg(long = "timeout-secs", requires = "agent")]
        timeout_secs: Option<u64>,
        /// Hard dollar cap for a single `claude -p` invocation (quick task
        /// 260813-rm5, D-5). Requires `--agent`. Omitting this flag omits
        /// the frontmatter key entirely, so the daemon's own documented
        /// default applies at runtime.
        #[arg(long = "max-budget-usd", requires = "agent")]
        max_budget_usd: Option<f64>,
    },
}
