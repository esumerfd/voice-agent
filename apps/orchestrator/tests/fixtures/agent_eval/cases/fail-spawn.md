---
id: fail-spawn
name: "Eval Case: Fail Spawn"
service:
  handler: agent.claude
  agent: {}
---
This case never actually reaches a `claude` process -- the runtime under
test is deliberately configured with a missing/non-executable binary path
so the spawn itself fails before any envelope exists.
