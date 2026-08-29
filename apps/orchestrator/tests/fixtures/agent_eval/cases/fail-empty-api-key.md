---
id: fail-empty-api-key
name: "Eval Case: Fail Empty Api Key"
service:
  handler: agent.claude
  agent: {}
---
Models an empty-credential invocation: the CLI reports an instant
is_error:true "Not logged in" result rather than a spawn or install
failure -- this must never be misreported as either of those.
