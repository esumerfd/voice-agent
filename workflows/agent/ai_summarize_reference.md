# wk-voice-agent Service Contract

This orchestrator launches arbitrary workflows through one uniform Service
Contract: a workflow declares an input schema (its `parameters:` block), an
invocation (`service.handler`), a lifecycle (`invoke` -> `status` -> optional
`cancel` -> `result`), and an output schema (whatever JSON the handler's
`result()` returns). Every workflow, regardless of what kind of work it does,
goes through this identical path.

There are four service types today. `action`/script workflows spawn a
configured external command and wait for it to exit (`process.calendar_today`
is the proven example). `action.immediate` is the default fallback handler
used when a workflow declares no explicit `service.handler`. `timers.start`
is an in-process action service. `agent.claude` -- this workflow's own
handler -- spawns the `claude` CLI as a one-shot, non-interactive subprocess
inside an isolated per-run scratch directory, with a hard wall-clock ceiling
and a hard dollar budget, and returns its parsed JSON result envelope.

The orchestrator itself never special-cases any one handler. `dispatch()`
resolves a workflow's declared handler purely by looking it up in a registry
map; nothing in the dispatch path, the WS server, or the CLI client ever
names `agent.claude` (or any other handler) as a literal string. This is
what lets a new service type be added later -- a remote worker, a container
runtime, a different LLM CLI -- without touching the seam every existing
workflow already depends on.
