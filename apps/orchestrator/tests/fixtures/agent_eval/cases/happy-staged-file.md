---
id: happy-staged-file
name: "Eval Case: Happy Staged File"
service:
  handler: agent.claude
  agent:
    files: ["agent/input.md"]
---
Read the staged input.md file and write a faithful summary of it to
summary.md.
