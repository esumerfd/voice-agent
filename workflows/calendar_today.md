---
id: calendar_today
name: Calendar Today
parameters: {}
service:
  type: integration
  handler: process.calendar_today
---
Spawns a configured external script and returns today's date as JSON,
proving the Integration/API service type (SVC-02) through the same
Service Contract path as `set_timer`'s Action/script type.

The spawned script is configured via the `ORCHESTRATOR_CALENDAR_SCRIPT`
environment variable (default: `workflows/scripts/calendar_today.sh`) —
never a caller-facing `run` flag (D-12). Point it at a different script to
change what this workflow runs.
