# Tutorial: the `orchestrator` CLI

The `orchestrator` binary is a thin client: it never runs a workflow itself.
Every command but one connects over a WebSocket to a running `orchestratord`
daemon and asks that daemon to list workflows, run one, or register a new
one; the exception is `log`, which reads the daemon's log file directly off
disk and needs no daemon connection at all. By the end of this tutorial you
will have installed and started the daemon, listed and run every workflow
shipped with this repo, authored a new workflow for each of the three
service shapes the daemon supports, tailed the daemon's log, and seen how a
single request flows from the CLI through to a result.

For a terse command/flag reference, see [`README.md`](../../../README.md).
For the deeper architecture behind what's described here, see
[`docs/design-overview.md`](../../../docs/design-overview.md).

## 1. Installation

### Prerequisites

- A stable Rust toolchain installed via [rustup](https://rustup.rs)
  (`cargo`, `rustc`) — Rust edition 2021, no specific version pinned.
- `bash` — the `calendar_today` and `countdown` workflows both spawn an
  external shell script.

### Clone, build, test

```bash
git clone <repo-url>
cd wk-voice-agent
make build
make test
```

`make test` is worth running once after a fresh clone: it exercises all four
products (`shared`, `orchestrator`, `orchestrator-cli`, `orchestrator-tui`)
and confirms you have a healthy tree before you touch anything else.

### Two ways to run the CLI

There is no root Cargo workspace in this repo (each product is its own
independent Cargo package), so there is no plain `cargo run -p
orchestrator-cli` from the repo root. Use one of these instead:

```bash
# Not yet installed on PATH -- run from inside the CLI's own package dir:
cd apps/orchestrator-cli && cargo run -- <args>

# Or, after `make build`, run the compiled binary directly from the repo root:
./apps/orchestrator-cli/target/debug/orchestrator <args>
```

Once installed (see below), the third way is simply `orchestrator <args>`
from anywhere.

### What `make install` does

```bash
make install
```

This builds `orchestratord` and registers it as a real OS-managed service:

- **macOS** — a `launchd` LaunchAgent, loaded via `launchctl load`. Check
  with `launchctl list | grep com.wk-voice-agent.orchestratord`.
- **Linux** — a user-level `systemd` unit (no root/`sudo` needed), enabled
  and started immediately. Its exact `systemctl` check command is printed
  by `make install` itself when it finishes, and is also visible directly
  in the root `Makefile`'s `install` target.

`make install` then also builds and copies both the `orchestrator` CLI and
the `orchestrator-tui` binaries into `$HOME/bin`, putting them on `PATH`.
`make uninstall` reverses all of it: it unregisters the service and removes
both binaries from `$HOME/bin`.

If your shell has already cached a different `orchestrator` earlier on
`PATH`, run `hash -r` (or open a new shell) so it picks up the newly
installed one.

### Starting the daemon

The CLI never starts the daemon for you (see "Connection contract" below),
so start it first:

```bash
make start    # builds orchestratord, then launches it in the background
make status   # confirms it's running
```

`make start`/`make status` (along with `make stop`/`make restart`) are
ad-hoc, pidfile-managed dev backgrounding — they do not survive logout or a
reboot. For a persistent daemon, use `make install` instead (next section).

### Connection contract

The CLI dials `ws://127.0.0.1:47100` by default (`--port` overrides this —
flag-only, there is no environment-variable fallback on the client side).
If it cannot reach a daemon there, it prints one line to **stderr** naming
the address and the fix, then exits 1. It never starts the daemon itself.

**Troubleshooting "cannot reach orchestratord":**

- Is the daemon actually running? `make status` (dev backgrounding) or the
  OS-service check commands above (persistent install).
- Did you pass a non-default `--port` to the CLI that doesn't match the
  port the daemon was started with?
- Did a previous `make stop` take down an OS-managed instance? Per `make
  help`'s own note, once `stop` takes down an OS-managed daemon it stays
  down until re-registered with `make install`.

## 2. Using the CLI

There are exactly six commands: `list`, `run`, `workflow create`,
`workflow edit`, `workflow delete`, and `log`. No others exist, and there is
no flag to toggle a different output shape — what each command prints is
fixed. This section walks the first five against the four workflows shipped
in `workflows/`, then covers `log` (the one command that needs no running
daemon at all) on its own:

| id | service.type | service.handler | parameters |
|---|---|---|---|
| `set_timer` | action | `timers.start` | `duration_minutes` int, required |
| `calendar_today` | integration | `process.calendar_today` | none |
| `countdown` | action | `action.immediate` | none |
| `ai_summarize` | agent | `agent.claude` | `focus` string, optional |

### `orchestrator list`

```bash
orchestrator list
```

Prints an aligned three-column table (`ID  NAME  DESCRIPTION`), rows sorted
by id, with the `DESCRIPTION` column filled from each workflow's Markdown
body (which is why `ai_summarize`'s row runs long — its body is the prompt
sent to Claude, not a one-line blurb). Captured against a live daemon,
column 1 alone:

```
ID              NAME
ai_summarize    AI Summarize
calendar_today  Calendar Today
countdown       Countdown
set_timer       Set Timer
```

If any workflow file the daemon scanned failed to load, that shows up as a
`warning: ...` line on **stderr** — stdout stays pipe-clean either way.

### `orchestrator run set_timer`

```bash
orchestrator run set_timer --duration_minutes 5
```

This is what `workflows/set_timer.md` declares:

```yaml
id: set_timer
name: Set Timer
parameters:
  duration_minutes:
    type: int
    description: how many minutes the timer should run for
    required: true
service:
  type: action
  handler: timers.start
```

There is no `mode` key here, so `set_timer` runs in the default sync mode.

This is the workflow to use for understanding the central idea behind
`run`: its flags are not fixed by the CLI. `run` first asks the daemon to
describe the named workflow's declared parameters, then parses your
trailing flags against exactly that list. Give it an unknown flag or a bad
value and it tells you what it actually expected — both captured live:

```bash
orchestrator run set_timer --minutes 5
# {"error": "unknown parameter flag `--minutes`; expected parameters: duration_minutes (int, required)"}

orchestrator run set_timer --duration_minutes five
# {"error": "flag `--duration_minutes` expects an integer value, got `five`; expected parameters: duration_minutes (int, required)"}
```

`set_timer` runs in the default **sync** mode, and `timers.start` really
does wait out the full duration before reporting `Completed` — there is no
shortcut. That means `run set_timer --duration_minutes 5` blocks the CLI
for a real 5 minutes before returning:

```json
{
  "timer_completed": true
}
```

If you want to see a result quickly while trying this yourself,
`--duration_minutes 1` blocks for one real minute instead of five.

### `orchestrator run calendar_today`

```bash
orchestrator run calendar_today
```

This is what `workflows/calendar_today.md` declares:

```yaml
id: calendar_today
name: Calendar Today
parameters: {}
service:
  type: integration
  handler: process.calendar_today
```

There is no `mode` key here either, so `calendar_today` also runs in the default sync mode.

No parameters. `calendar_today` is an integrated command: `process.calendar_today`
is a handler key registered in the daemon's own Rust source, and serving a
run means shelling out to an external script rather than doing the work in
process. The workflow file itself declares no target of its own — its
`service:` block above names only a `type` and a `handler` key, nothing
else. The daemon resolves the companion script's path once at startup from
the `ORCHESTRATOR_CALENDAR_SCRIPT` environment variable, defaulting to
`workflows/scripts/calendar_today.sh` when that variable is unset, and then
constructs the process handler with no extra arguments. Contrast this with
`countdown`, which names its own script in its own definition
(`command: scripts/countdown.sh` above) — see Section 3b for why the
project offers both shapes. Real captured output (the date will match
whatever day you actually run it):

```json
{
  "date": "2026-08-11"
}
```

### `orchestrator run countdown`

```bash
orchestrator run countdown
```

This is what `workflows/countdown.md` declares:

```yaml
id: countdown
name: Countdown
parameters:
service:
  type: action
  handler: action.immediate
  command: scripts/countdown.sh
```

There is no `mode` key here, so `countdown` runs in the default sync mode too.
Note that the `handler: action.immediate` key is present in the file even
though `action.immediate` is the default handler that an absent `handler:`
key would resolve to anyway.

Also no parameters. Unlike `calendar_today`, the script this workflow runs
is named by the workflow definition file itself, not by daemon
configuration — see Section 3 for the distinction between the two.
`countdown.sh` only speaks the countdown aloud (macOS `say`) and never
prints to stdout, so a real run's captured output is a single blank line —
not an error, just an empty successful result.

### `orchestrator run ai_summarize`

```bash
orchestrator run ai_summarize --focus "the Service Contract"
```

This is what `workflows/ai_summarize.md` declares:

```yaml
id: ai_summarize
name: AI Summarize
parameters:
  focus:
    type: string
    description: What aspect of the reference material to emphasize
    required: false
service:
  type: agent
  handler: agent.claude
  mode: async
  agent:
    files:
      - agent/ai_summarize_reference.md
    timeout_secs: 600
    max_budget_usd: 0.50
```

`ai_summarize` is the only one of the four declaring `mode: async` — the
other three have no `mode` key and default to sync.

This is the one async workflow shipped in the repo (`mode: async`). `run`
acks immediately with a `status`/`run_id` object and exits 0 — real
captured output:

```json
{
  "run_id": "run-14",
  "status": "started"
}
```

Exit 0 here means **accepted and dispatched**, not finished — this CLI
never prints the eventual result. Watch the outcome in `orchestrator-tui`
or in the agent run log under the daemon's configured
`ORCHESTRATOR_AGENT_RUNS_DIR`.

The most likely first-run failure for this one workflow specifically: if
the daemon's startup preflight for the Claude handler didn't pass (missing
`claude` binary, or an unset/empty `ANTHROPIC_API_KEY`), the `agent.claude`
handler is simply not registered, and only this workflow fails — the other
three keep working normally. The key belongs on the **daemon's**
environment, never passed to or read by the CLI.

### `orchestrator workflow create`

```bash
orchestrator workflow create <id> <source> \
  [--display-name "Text"] [--description "Text"] \
  [--param name:type[:required] ...] \
  [--script | --markdown | --agent] \
  [--agent-file <path> ...] [--timeout-secs <n>] [--max-budget-usd <n>]
```

- `<source>` is resolved **locally by the CLI**: if it names an existing
  file, that file's contents are read and sent; otherwise the string itself
  is sent as literal content. Either way, the CLI never writes anything to
  disk itself — the resolved content travels over the wire and lands in
  the **daemon's** own workflows directory.
- `--display-name` / `--description` override the default title-cased
  `<id>`-derived name and the default description.
- `--param` is repeatable: one `name:type[:required]` per declared
  parameter.
- `--script` / `--markdown` force the content's classification when the
  automatic shebang/extension sniffing would guess wrong; they conflict
  with each other.
- Creation refuses to overwrite an existing id.
- The new workflow is runnable immediately with `orchestrator run <id>` —
  no daemon restart required.

**`--agent`: authoring a Claude-backed workflow over the wire** (quick task
260813-rm5). Four things a reader cannot guess from the flag list alone:

- `--agent` is the **sole** way to reach a Claude-backed (`service.type:
  agent`) workflow over the wire, and it is **never inferred** from
  content — no heuristic can tell "a prompt for Claude" apart from "prose
  describing a workflow," and guessing wrong would silently spend money on
  an unattended `claude -p` run. `--agent` conflicts with both `--script`
  and `--markdown` — all three are explicit overrides of the same
  classification.
- With `--agent`, `<source>` resolves **exactly like the markdown path**:
  the resolved content (or `--description`, when given) becomes the prompt
  body sent to `claude -p`.
- `--agent-file` is repeatable and names a **DAEMON-side** path — the CLI
  never resolves, reads, or stats it. The referenced file must already
  exist in the daemon's own workflows directory (staged there separately,
  exactly like a hand-authored `.md`'s `files:` list).
- An agent workflow is always `mode: async` — there is no `--sync` escape
  hatch for this classification.
- `--timeout-secs` / `--max-budget-usd` are optional; omitting either omits
  that frontmatter key entirely, so the daemon's own documented runtime
  defaults apply — the CLI never bakes in a number of its own.

Worked example, proven against a live daemon — create a local prompt file,
then point `workflow create` at it with `--agent`, reusing the reference
file `ai_summarize.md` already stages in the daemon's workflows directory:

```bash
cat > /tmp/agent_prompt.md << 'EOF'
You are running non-interactively as part of an automated workflow. There is
no one available to answer a question, approve a plan, or clarify an
ambiguous instruction -- if something is unclear, make the most reasonable
assumption you can defend, note that assumption in your summary, and
proceed. Do not end your work by asking a question or offering to continue;
finish the task now.

Read the file `agent/ai_summarize_reference.md` in your current directory --
it is staged for you. Write one sentence to a new file named `result.md` in
your current directory confirming you read it.

When you finish, report only what you actually did and verified.
EOF

orchestrator workflow create demo_agent /tmp/agent_prompt.md \
  --agent \
  --agent-file agent/ai_summarize_reference.md \
  --timeout-secs 90 \
  --max-budget-usd 0.10 \
  --param focus:string:optional
```

Real captured output:

```json
{
  "created": true,
  "script_path": null,
  "workflow_path": "./workflows/demo_agent.md"
}
```

`script_path` is `null` — an agent create never writes a companion script.
The actual emitted frontmatter (captured live, `--agent-file`'s relative
path stored verbatim, `--max-budget-usd 0.10` round-tripping through YAML
as `0.1`):

```yaml
---
id: demo_agent
name: Demo Agent
parameters:
  focus:
    type: string
    required: false
service:
  type: agent
  handler: agent.claude
  mode: async
  agent:
    files:
    - agent/ai_summarize_reference.md
    timeout_secs: 90
    max_budget_usd: 0.1
---
```

Same `service:` key set, and the same `type`/`handler`/`mode` scalars, as
`workflows/ai_summarize.md` (Section 3c) — proven live by diffing the two
files' frontmatter after a real create.

First, create a real local script — `<source>` only classifies as a
script if it resolves to an actual file on the CLI's own disk:

```bash
cat > /tmp/hello.sh << 'EOF'
#!/usr/bin/env bash
echo "{\"greeting\": \"hello, $1\"}"
EOF
chmod +x /tmp/hello.sh
```

Then point `workflow create` at it:

```bash
orchestrator workflow create hello /tmp/hello.sh --param dur:int:required
```

Real captured output (paths are relative to the **daemon's** own
workflows directory, not the CLI's caller — that's why a daemon started
from the repo root writes these files into this repo's `workflows/`):

```json
{
  "created": true,
  "script_path": "./workflows/scripts/hello.sh",
  "workflow_path": "./workflows/hello.md"
}
```

**Pitfall — a `<source>` that isn't a real local file.** Point the same
command at a path that doesn't exist on the CLI's disk:

```bash
orchestrator workflow create hello_literal /path/to/hello.sh --param dur:int:required
```

This still exits `0` and still reports `"created": true` — nothing
about it looks like a failure. Real captured output:

```json
{
  "created": true,
  "script_path": null,
  "workflow_path": "./workflows/hello_literal.md"
}
```

Look at `script_path`: it's `null`. `resolve_source` looked for
`/path/to/hello.sh` on the CLI's own disk, found no such file, and sent
the literal string `/path/to/hello.sh` as the content instead. With no
shebang and no existing file to sniff an extension from, that content
gets classified as markdown, not a script — so what actually landed in
the daemon's workflows directory is a markdown workflow whose entire
body is the text `/path/to/hello.sh`, not a runnable script. This is
intentional, locked behavior (the CLI resolves `<source>` on its own
disk, never the daemon's), not a bug — if the source genuinely is a
script the CLI can't resolve locally, pass `--script` to force the
classification instead of relying on shebang/extension sniffing.

### `orchestrator workflow edit`

```bash
orchestrator workflow edit <id> <source> \
  [--display-name "Text"] [--description "Text"] \
  [--param name:type[:required] ...] \
  [--script | --markdown]
```

Same flags (including `--agent`/`--agent-file`/`--timeout-secs`/
`--max-budget-usd`), same defaulting semantics, same source-resolution rule
as `workflow create` — the only behavioral difference is the guard
direction. Three things a reader cannot guess from the flag list alone:

- **`<id>` must already exist.** This is the exact inverse of `create`'s
  guard: `edit` refuses when the id is unknown, `create` refuses when it
  isn't.
- **An edit is a full REPLACE, not a patch.** Omitting `--param` on an edit
  clears the parameter map, exactly as it would produce an empty map on a
  create — there is no merge with the previous definition.
- **The change is live immediately**, with no daemon restart — the exact
  same "no restart required" property `create` and `delete` already have,
  because the registry re-reads from disk on every request.

Reusing `hello` from the `workflow create` example above — edit its script
content and re-send the same `--param` (omitting it here would clear the
`dur` parameter, per the full-replace rule just stated):

```bash
cat > /tmp/hello.sh << 'EOF'
#!/usr/bin/env bash
echo "{\"greeting\": \"hi there, $1\"}"
EOF
chmod +x /tmp/hello.sh

orchestrator workflow edit hello /tmp/hello.sh --param dur:int:required
```

Real captured output:

```json
{
  "script_path": "./workflows/scripts/hello.sh",
  "updated": true,
  "workflow_path": "./workflows/hello.md"
}
```

The success key is `updated`, not `created` — the CLI renders the
mode-appropriate key even though the daemon's own wire response field
(`created: bool`, meaning "the write succeeded") is unchanged for either
mode. `workflows/scripts/hello.sh` on the daemon's disk now holds the new
script content.

Editing an id the daemon doesn't know about is a nonzero exit, error JSON on
stdout naming the id:

```bash
orchestrator workflow edit definitely_not_a_workflow "some prose"
```

```json
{
  "error": "workflow `definitely_not_a_workflow` not found at ./workflows/definitely_not_a_workflow.md (use `workflow create` to add a new definition)"
}
```

**One more thing worth stating explicitly:** editing a script-backed
workflow into a markdown-only one (by omitting `--script` on a plain-prose
`<source>`, or passing `--markdown`) leaves the old companion script file on
disk — the rewritten `.md` simply stops declaring `service.command`. Nothing
deletes it automatically; that is `workflow delete`'s job, not `edit`'s. The
same "orphan, don't delete" behavior extends to `--agent` in **either**
direction (quick task 260813-rm5, D-8): editing an action workflow into an
agent one with `--agent` simply stops declaring `service.command` (orphaning
any companion script exactly as above), and editing an agent workflow back
into an action one with `--script`/`--markdown` simply stops declaring
`service.agent`. Neither direction needs special-casing — `edit` is already
a full REPLACE built solely from the new request, so the rewritten `.md`
only ever declares what the new flags say. Proven live: creating an action
workflow, editing it with `--agent` (now agent-typed), and editing it again
with `--markdown` (now action-typed again) — `orchestrator list` and the
on-disk `.md` shape track every step, immediately, with no daemon restart.

### `orchestrator workflow delete`

```bash
orchestrator workflow delete <id>
```

Removes `<id>`'s `.md` (and, for a script-backed workflow, its companion
script) from the **daemon's own** workflows directory — over the wire, the
same as `workflow create`; the CLI never touches the filesystem itself.

```bash
orchestrator workflow delete hello
```

Real captured output shape on success:

```json
{
  "deleted": true,
  "script_path": "./workflows/scripts/hello.sh",
  "workflow_path": "./workflows/hello.md"
}
```

`script_path` is `null` for a markdown-only workflow, matching `create`'s
own shape.

Deleting an id the daemon can't resolve is a nonzero exit, error JSON on
stdout naming the id — delete is **not idempotent**, so a typo'd id is a
loud failure rather than a silent no-op:

```bash
orchestrator workflow delete definitely_not_a_workflow
```

```json
{
  "error": "workflow `definitely_not_a_workflow` not found"
}
```

Two guarantees worth stating explicitly, since neither is obvious from the
command alone:

- The deletion takes effect on the **very next request**, with no daemon
  restart — `orchestrator list` immediately stops showing the id, and
  `orchestrator run <id>` immediately reports it unknown.
- An **already-running** invocation of the just-deleted workflow is
  unaffected and runs to completion — delete removes the definition only,
  never a dispatched run.

### `orchestrator log`

```bash
orchestrator log
orchestrator log -n 50
orchestrator log -f
```

Unlike the other three commands, `log` needs **no running daemon** — it
reads a log file directly off disk and never opens a WebSocket connection,
so it is the one command that still works when `orchestratord` has crashed
or was never started. With no `--file`, it auto-discovers the log path by
trying, in order, the dev pidfile-backgrounded log first
(`/tmp/orchestratord.dev.log`, written by `make start`), then the
launchd/systemd stderr log (`/tmp/orchestratord.err.log`); `--file <path>`
overrides discovery entirely and is the escape hatch for a nonstandard
location. If neither candidate exists, it exits 1 and names every path it
tried.

`orchestrator log` (no flags) prints the last 20 lines and exits — a
one-shot tail, pipeable and scriptable, not a live stream. `--lines`/`-n`
overrides the line count:

```bash
orchestrator log -n 50
```

`--follow`/`-f` additionally streams newly appended lines after the
initial tail, like `tail -f`, until interrupted (Ctrl-C):

```bash
orchestrator log -f
```

### Output and exit-code conventions

This is genuinely surprising the first time you hit it, so it's worth
stating plainly: `run` renders **both** success and failure as JSON on
**stdout**, and signals failure only through a nonzero exit code — never
through which stream the output landed on. `list`, by contrast, keeps
stdout for its table and sends warnings to stderr. There is no flag to
change either convention; output shape is fixed per command.

## 3. Authoring a new workflow

A workflow is a Markdown file living in the daemon's workflows directory.
Its YAML frontmatter carries `id`, `name`, `parameters`, and a `service`
block; the Markdown body below the frontmatter is human-facing description
— except for the AI-agent type, where the body is the prompt sent to
Claude. The **filename stem is the id**: `hello.md` becomes workflow id
`hello`, which is exactly why a workflow created with `workflow create` is
runnable by that id immediately.

The `parameters` map is what determines the flags `run` accepts (closing
the loop with Section 2): each entry has a `type` of `string`, `int`, or
`bool`, an optional `description`, and a `required` flag.

```yaml
parameters:
  duration_minutes:
    type: int
    description: how many minutes the timer should run for
    required: true
```

**The routing rule, stated once, plainly:** `service.type` (`action` /
`integration` / `agent`) is a declarative label for humans reading the
file. Dispatch resolves `service.handler` and nothing else. Changing
`type:` alone changes nothing about what actually runs.

### 3a. Action / script — modeled on `countdown`

```yaml
service:
  type: action
  handler: action.immediate
  command: scripts/countdown.sh
```

`action.immediate` is the generic per-workflow script runner, and it is
also the **default handler**: a workflow whose `service.handler` is absent
or empty lands here anyway (`countdown`'s own definition omits `handler:`
entirely).

Two things a first-time author gets wrong:

- `command` resolves **relative to the workflow file's own directory**,
  not the daemon's working directory. `countdown.md` and its
  `scripts/countdown.sh` travel together.
- Parameters reach the script as **JSON on stdin**, not as `argv`. Read
  stdin in your script rather than looking for `$1`, `$2`, ....

The script must be executable (the execute bit set) — a spawn failure here
surfaces as a specific `failed to spawn command ...` error.

### 3b. Integration / API — modeled on `calendar_today`

```yaml
service:
  type: integration
  handler: process.calendar_today
```

This is the place to resolve the question Section 2 leaves open: both
`process.calendar_today` and `action.immediate` spawn an OS process, so the
underlying **mechanism is identical**. What differs is **who configures the
target command**:

- `action.immediate` is generic: any workflow file names its own
  `service.command`, resolved relative to that file's directory. Anyone
  authoring a workflow can point it wherever they like — no Rust changes
  needed.
- `process.calendar_today` is a **dedicated, named capability** the daemon
  binds at startup to one operator-configured script
  (`ORCHESTRATOR_CALENDAR_SCRIPT`). It is deliberately not caller-facing:
  the workflow supplies parameters, never the target command.

That is the shape an integration/API service takes in this daemon — a
capability the daemon owns, and a workflow merely names by its handler key.
Be honest with yourself about the cost: adding a genuinely new integration
of this shape means registering a new handler key in `orchestratord`'s
source, whereas an action/script workflow needs zero Rust changes. Reach
for `action.immediate` + `service.command` first; only add a dedicated
handler when you need a capability the daemon itself should own and gate
(credentials, a fixed target, an operator-controlled script path).

### 3c. AI-agent / Claude-backed — modeled on `ai_summarize`

An agent-type workflow no longer has to be hand-written (quick task
260813-rm5): `orchestrator workflow create <id> <source> --agent
[--agent-file <path> ...] [--timeout-secs <n>] [--max-budget-usd <n>]`
produces exactly the shape below over the wire — see Section 2's
`workflow create` subsection for the worked example and real captured
output. Hand-authoring the `.md` directly remains a supported, equally
valid path (for example, to set `service.agent.model`, which has no CLI
flag).

```yaml
service:
  type: agent
  handler: agent.claude
  mode: async  # always async -- a run may take seconds to minutes
  agent:
    files:
      - agent/ai_summarize_reference.md
    max_budget_usd: 0.50   # default 1.00 when omitted
    timeout_secs: 600      # default 600 (10 minutes) when omitted
```

The Markdown body becomes the prompt sent to `claude -p`, unattended — write
it for an agent with nobody to ask a clarifying question; state the
reasonable-assumption-and-proceed instruction explicitly, as
`ai_summarize.md`'s own body does.

`agent.files` paths are relative to the **workflow's own directory** and
are staged into a fresh, isolated per-run scratch directory before the
agent runs. This staging is also why a prose file living under a directory
literally named `agent` (like `workflows/agent/ai_summarize_reference.md`)
never itself shows up in `orchestrator list` as a broken workflow: the
registry loader deliberately excludes `.md` files whose immediate parent
directory is named `agent` from the workflow scan.

Daemon-side prerequisites, checked once at startup (never per-run): the
`claude` binary must resolve, the configured runs and scratch directories
must be usable, and `ANTHROPIC_API_KEY` must be set and non-empty on the
**daemon's** environment. If any precondition fails, the daemon prints one
`ERROR` line naming exactly which one and starts normally without this
handler — every other workflow keeps working; only `agent.claude`-handled
runs fail.

### Installing a new definition

Two ways to get a new workflow onto the daemon — both cover all three
service shapes (action/script, action/markdown, and agent):

1. Write the `.md` file directly into the daemon's `ORCHESTRATOR_WORKFLOWS_DIR`.
2. Send it over the wire with `orchestrator workflow create` (Section 2) —
   the CLI never writes to disk itself; the content lands in the daemon's
   own workflows directory. `--agent` reaches the agent shape the same way
   `--script`/`--markdown` reach the other two.

**Safety note:** a workflow definition can name a command the daemon will
execute (`action.immediate`'s `service.command`, or a prompt handed to an
unattended Claude session with `agent.claude`). Authoring a workflow
definition is therefore equivalent to granting code execution on the
daemon's host, as the daemon's user. Only add definitions you trust.

## 4. Architecture overview

This section stays intentionally short — see
[`docs/design-overview.md`](../../../docs/design-overview.md) for the full
three-plane model (Interaction Plane / Orchestration Plane / Execution
Plane) and
[`docs/user-guide-arch.md`](../../../docs/user-guide-arch.md) for a
concrete `calendar_today` process/thread trace.

**Runtime shape.** `orchestratord` is the daemon, and the *only* component
that owns the workflow registry, the handler map, and the activity store.
`orchestrator-cli` and `orchestrator-tui` are both thin WebSocket clients
holding no registry of their own — the CLI is short-lived (one connection,
one request, exit), the TUI is long-lived and renders live activity as it
streams in.

**The two stable seams.** Everything in this repo is built to keep exactly
two contracts stable, so everything behind either one is replaceable:

- **Interaction Protocol** — the WebSocket `Envelope` type shared via
  `apps/shared`. `orchestrator-cli` and `orchestrator-tui` both speak it;
  it's also the reason a voice client can arrive later as a third peer
  without touching any handler.
- **Service Contract** — the `Service` trait every handler implements
  (`invoke` / `status` / `result`). Adding a new capability means writing a
  new `Service` impl and registering it; dispatch itself never changes.

**Dispatch pipeline.** Every `run` request — regardless of which handler
eventually serves it — flows through the same six steps:

1. Validate the payload against the workflow's declared `parameters`;
   handlers never see invalid input.
2. Resolve `service.handler` in the handler map; an unresolved key is a
   clean error.
3. Build the handler's input from trusted definition data (the agent block
   when `service.agent` is present, otherwise the command/args when
   `service.command` is present) — never from the caller's raw request.
4. `Service::invoke`, then poll `Service::status` on a 100ms interval while
   it reports `Running`.
5. On `Completed`, call `Service::result`.
6. Assemble one `InvokeOutcome` envelope — **identical in shape** no matter
   which `Service` ran.

That last step is the point of the whole design: a timer, a spawned script,
a bound integration, and an unattended Claude session all resolve to the
same envelope shape on the way back to the caller.

**Registered handlers:**

| handler key | service.type | shipped example |
|---|---|---|
| `timers.start` | action | `set_timer` |
| `process.calendar_today` | integration | `calendar_today` |
| `action.immediate` | action | `countdown` |
| `agent.claude` | agent | `ai_summarize` |
