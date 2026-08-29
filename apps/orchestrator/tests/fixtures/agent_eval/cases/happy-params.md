---
id: happy-params
name: "Eval Case: Happy Params"
parameters:
  output_file:
    type: string
    description: "Filename the agent should write its output to"
    required: true
service:
  handler: agent.claude
  agent: {}
---
Write the output file named by the output_file parameter, containing a
short confirmation message.
