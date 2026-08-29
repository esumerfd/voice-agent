# Voice Agent

An open-source, local-first, CPU-only voice agent that launches arbitrary
workflows — each workflow is itself an agent performing a specific task.
The current milestone delivers the CLI orchestrator core: list and run
workflows by name. Voice capture (Whisper/Piper), the local LLM (Ollama),
and the Claudette avatar are planned but not yet wired.

## Prerequisites

- A stable Rust toolchain installed via [rustup](https://rustup.rs)
  (`cargo`, `rustc`) — Rust edition 2021. No specific version is pinned.
- `bash` — required by the `calendar_today` and `countdown` workflows, both
  of which spawn an external shell script.

## Project layout

- `apps/orchestrator` — library crate; the orchestrator/service core
  (workflow registry, dispatch, Service Contract).
- `apps/orchestrator-cli` — binary crate; produces the `orchestrator` CLI
- `apps/orchestrator-tui` — UI to monitor activities and their results.

## Workflows

A workflow is a defined activity. Defaults to a workflow directory but configurable with ORCHESTRATOR_WORKFLOW_HOME

- `workflows/` — declarative Markdown+YAML workflow definitions.

## Build

Four independent crates, no shared workspace root — `make build` runs these in order:

```bash
make build
```

## Test

```bash
make test
```

## Run — list workflows

```bash
make run
# or directly:
cargo run --manifest-path apps/orchestrator-cli/Cargo.toml -- list
```

After building, you can also run the compiled binary directly:

```bash
./apps/orchestrator-cli/target/debug/orchestrator list
```

## Run — launch a workflow

```bash
cargo run --manifest-path apps/orchestrator-cli/Cargo.toml -- run <workflow_id> [--flag value ...]
```

Flags are dynamic per workflow — they map exactly to the parameter names
declared in that workflow's definition file (`--<parameter_name>`, no
kebab-casing). Examples using the workflows shipped in this repo:

```bash
# Starts a local countdown timer; requires duration_minutes.
cargo run --manifest-path apps/orchestrator-cli/Cargo.toml -- run set_timer --duration_minutes 5

# Spawns a configured script and returns today's date as JSON.
cargo run --manifest-path apps/orchestrator-cli/Cargo.toml -- run calendar_today

# Spawns a configured script that counts down and announces when time is up.
cargo run --manifest-path apps/orchestrator-cli/Cargo.toml -- run countdown

# Runs the claude CLI unattended over a staged reference file and returns
# its summary. Requires ANTHROPIC_API_KEY set on the DAEMON's environment
# (see "AI-agent workflows" below) -- the CLI itself needs no key.
cargo run --manifest-path apps/orchestrator-cli/Cargo.toml -- run ai_summarize --focus "the Service Contract"
```

## Run — create a workflow

```bash
cargo run --manifest-path apps/orchestrator-cli/Cargo.toml -- workflow create <id> <source> [--display-name "Text"] [--description "Text"] [--param name:type:required ...] [--script | --markdown]
```

`<source>` is resolved locally by the CLI: an existing local file's contents
are read; anything else is sent as the literal content. The CLI never writes
to the filesystem itself — the resolved content is sent over the wire, and
the file lands in the **daemon's** `ORCHESTRATOR_WORKFLOWS_DIR`, runnable
immediately with `orchestrator run <id>` and no daemon restart.

```bash
# Creates a script-backed workflow with a required int parameter.
cargo run --manifest-path apps/orchestrator-cli/Cargo.toml -- \
  workflow create hello /path/to/hello.sh --param dur:int:required
```

## Run — delete a workflow

```bash
cargo run --manifest-path apps/orchestrator-cli/Cargo.toml -- workflow delete <id>
```

Removes `<id>`'s `.md` (and, for a script-backed workflow, its companion
script) from the **daemon's own** `ORCHESTRATOR_WORKFLOWS_DIR` over the
wire, the same as `workflow create` -- the CLI never touches the
filesystem itself. Deleting an id the daemon can't resolve is a nonzero
exit with a JSON error naming the id (not idempotent). The removal takes
effect on the next request with no daemon restart; an already-running
invocation of the deleted workflow is unaffected and runs to completion.

```bash
cargo run --manifest-path apps/orchestrator-cli/Cargo.toml -- workflow delete hello
```

## Run — tail the daemon log

```bash
orchestrator log
orchestrator log -n 50
orchestrator log -f
```

Needs no running daemon -- it reads the daemon's log file directly and
never opens a WebSocket connection, so it works even when `orchestratord`
has crashed or was never started. `orchestrator log` prints the last 20
lines by default (`--lines`/`-n` overrides the count); `orchestrator log
-f`/`--follow` additionally streams newly appended lines like `tail -f`
until interrupted. With no `--file`, it auto-discovers the log path
(`/tmp/orchestratord.dev.log`, then `/tmp/orchestratord.err.log`); `--file
<path>` overrides discovery entirely.

## AI-agent workflows

A fourth service type, `agent.claude` (SVC-03), runs a workflow's Markdown
body as a prompt through the `claude` CLI, unattended and non-interactively,
inside an isolated per-run scratch directory with a hard wall-clock ceiling
and a hard dollar budget. `workflows/ai_summarize.md` is the first real
example — it stages a reference document into its scratch directory and
asks Claude to summarize it.

Declare it in a workflow's frontmatter under `service.agent`:

```yaml
service:
  type: agent
  handler: agent.claude
  mode: async  # runs may take multiple seconds to minutes; always async
  agent:
    files:            # optional: paths relative to this workflow's own
      - agent/ref.md   # directory, staged into the run's scratch directory
    model: claude-sonnet-5   # optional: overrides the CLI's own default
    max_budget_usd: 0.50     # optional; default 1.00
    timeout_secs: 600        # optional; default 600 (10 minutes)
```

The Markdown body below the frontmatter is sent to Claude as the prompt —
write it as instructions to an unattended agent (it has no one to ask a
clarifying question).

The daemon (`orchestratord`, never the CLI) needs three things before it
will register this handler:

| Flag | Environment variable | Default | Purpose |
|---|---|---|---|
| `--claude-bin` | `ORCHESTRATOR_CLAUDE_BIN` | `claude` | Path to the `claude` binary. A bare name is resolved against `PATH` once at startup; production/service deployments should set this to an **absolute path** (`which claude`'s output) — a background service does not inherit an interactive shell's `PATH`. |
| `--agent-runs-dir` | `ORCHESTRATOR_AGENT_RUNS_DIR` | `./data/agent-runs` | Directory the durable per-run JSONL log is written into (retained 7 days). |
| `--agent-scratch-dir` | `ORCHESTRATOR_AGENT_SCRATCH_DIR` | `./data/agent-scratch` | Parent directory each run's isolated scratch directory is allocated inside (clean runs retained 24h; failed/flagged runs retained indefinitely until pruned). |

`ANTHROPIC_API_KEY` must also be set on the **daemon's** environment — a
present-but-empty value is refused just as loudly as an unset one, since it
fails almost instantly with a login-shaped error that is easy to
misdiagnose as a bad install. If any of these preconditions fail, the
daemon prints one `ERROR` line at startup naming exactly which one, and
starts normally without the `agent.claude` handler — every other workflow
still works.

Each run is recorded to `ORCHESTRATOR_AGENT_RUNS_DIR` (start/terminal
transitions, cost, token usage, the `claude --version` that ran it) and its
scratch directory is left on disk for inspection per the retention policy
above — nothing here is CWD-relative; every path is explicit.

## Configuration

| Variable | Purpose | Default |
|---|---|---|
| `ORCHESTRATOR_WORKFLOWS_DIR` | Directory scanned for workflow `.md` files. Also settable via the `--workflows-dir` flag. | `./workflows` |
| `ORCHESTRATOR_CALENDAR_SCRIPT` | External script spawned by the `calendar_today` workflow. | `workflows/scripts/calendar_today.sh` |
| `ORCHESTRATOR_CLAUDE_BIN` | Path to the `claude` binary the AI-agent handler spawns. Also settable via `--claude-bin`. | `claude` |
| `ORCHESTRATOR_AGENT_RUNS_DIR` | Directory the AI-agent handler's durable run log is written into. Also settable via `--agent-runs-dir`. | `./data/agent-runs` |
| `ORCHESTRATOR_AGENT_SCRATCH_DIR` | Parent directory for the AI-agent handler's per-run scratch directories. Also settable via `--agent-scratch-dir`. | `./data/agent-scratch` |
| `ANTHROPIC_API_KEY` | API key provisioned into the AI-agent handler's spawned `claude` process. Must be non-empty. | — (required for `agent.claude`) |

## Where to look next

- [`.planning/PROJECT.md`](.planning/PROJECT.md) — project context and scope.
- [`design-overview.md`](design-overview.md) — architecture.
- [`CLAUDE.md`](CLAUDE.md) / [`.claude/CLAUDE.md`](.claude/CLAUDE.md) —
  project constraints and goals.
- [`wiki/`](wiki/) — knowledge base (start at `wiki/hot.md`).
