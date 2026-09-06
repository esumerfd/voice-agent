---
id: countdown
name: Countdown
intent: count down out loud from three to go as a spoken cue to start something right now
parameters:
service:
  type: action
  handler: action.immediate
  command: scripts/countdown.sh
---
Counts down for the given duration and announces when time is up.
