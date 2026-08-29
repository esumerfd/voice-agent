---
id: happy-typed-result
name: "Eval Case: Happy Typed Result"
service:
  handler: agent.claude
  agent: {}
---
Return a result whose text parses as JSON matching the agreed schema (see
06-AI-SPEC.md Section 4b.1's --json-schema-constrained result pattern).
