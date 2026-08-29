---
id: fail-agent-error
name: "Eval Case: Fail Agent Error"
service:
  handler: agent.claude
  agent: {}
---
Attempt a task that the agent itself reports as failed (is_error:true)
rather than completing it.
