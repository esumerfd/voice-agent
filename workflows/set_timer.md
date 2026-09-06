---
id: set_timer
name: Set Timer
intent: set a timer for a given number of minutes and notify me when the time is up
parameters:
  duration_minutes:
    type: int
    description: how many minutes the timer should run for
    required: true
service:
  type: action
  handler: timers.start
---
Sets a local countdown timer and speaks a confirmation when it expires.
