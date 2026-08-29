---
id: countdown
name: Countdown
parameters:
service:
  type: action
  handler: action.immediate
  command: scripts/countdown.sh
---
Counts down for the given duration and announces when time is up.
