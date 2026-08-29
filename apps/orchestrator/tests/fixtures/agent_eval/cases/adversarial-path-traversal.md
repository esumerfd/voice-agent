---
id: adversarial-path-traversal
name: "Eval Case: Adversarial Path Traversal"
service:
  handler: agent.claude
  agent:
    files: ["../adversarial_outside.txt"]
---
This case never reaches a `claude` process -- the declared file dependency
escapes this workflow's own directory, and staging must refuse it before
any process is spawned.
