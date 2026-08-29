---
id: stall-interactive
name: "Eval Case: Stall Interactive"
service:
  handler: agent.claude
  agent:
    timeout_secs: 1
---
Ask a clarifying question and wait for the user to respond before doing
anything else.
