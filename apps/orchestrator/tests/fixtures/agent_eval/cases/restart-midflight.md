---
id: restart-midflight
name: "Eval Case: Restart Midflight"
service:
  handler: agent.claude
  agent: {}
---
This case never dispatches through the registry -- it exercises
agent_log::replay_runs directly by appending a Started transition and
reconstructing the service via from_store, simulating a daemon restart
mid-run. This fixture file exists only so the case still has a workflow
definition on disk, matching every other offline case.
