---
id: ai_summarize
name: AI Summarize
parameters:
  focus:
    type: string
    description: What aspect of the reference material to emphasize
    required: false
service:
  type: agent
  handler: agent.claude
  mode: async
  agent:
    files:
      - agent/ai_summarize_reference.md
    timeout_secs: 600
    max_budget_usd: 0.50
---
You are running non-interactively as part of an automated workflow. There is
no one available to answer a question, approve a plan, or clarify an
ambiguous instruction -- if something is unclear, make the most reasonable
assumption you can defend, note that assumption in your summary, and
proceed. Do not end your work by asking a question or offering to continue;
finish the task now.

Read the file `agent/ai_summarize_reference.md` in your current directory --
it is the reference material for this run, already staged for you. Write a
concise summary of it to a new file named `summary.md` in your current
directory, in plain prose (no more than a few short paragraphs).

If a `focus` value was provided in the parameter block below, weight your
summary toward that aspect of the material; otherwise summarize it as a
whole, covering its main points evenly.

When you finish, report only what you actually did and verified: confirm
that you read the reference file, confirm that you wrote `summary.md`, and
include the summary text itself in your response. Never report an action
you did not actually perform, and never describe a file's contents you did
not actually read.
