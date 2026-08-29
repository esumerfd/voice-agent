//! `timers.start` — the first real `Service` implementation under `src/`
//! (SVC-01). The countdown running to expiry IS the action (D-05); there is
//! no side-channel event log or scheduled OS job — `status()` reports
//! `Running` until a stored deadline elapses, then `Completed`, and
//! `dispatch()`'s poll loop (D-06/D-07) carries that through the normal
//! invoke -> status -> result flow like any other handler.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;

use crate::service::{RunHandle, RunStatus, Service, ServiceError};

/// Real `Service` impl backing the `set_timer` workflow's `timers.start`
/// handler. `unit` is the wall-clock duration one `duration_minutes` unit
/// represents — defaults to a real minute; tests inject a sub-second unit
/// (Pitfall 4) so integration tests never block for real minutes, without
/// changing the workflow's declared `int` parameter type.
pub struct TimerService {
    unit: Duration,
    deadlines: Mutex<HashMap<RunHandle, Instant>>,
    next_id: Mutex<u64>,
}

impl TimerService {
    /// Construct with the real-world unit: one `duration_minutes` unit is
    /// one minute.
    pub fn new() -> Self {
        Self::with_unit(Duration::from_secs(60))
    }

    /// Construct with an injectable time unit (test-only escape hatch —
    /// production callers use `new()`).
    pub fn with_unit(unit: Duration) -> Self {
        Self {
            unit,
            deadlines: Mutex::new(HashMap::new()),
            next_id: Mutex::new(0),
        }
    }
}

impl Default for TimerService {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Service for TimerService {
    async fn invoke(&self, input: serde_json::Value) -> Result<RunHandle, ServiceError> {
        let minutes = input
            .get("duration_minutes")
            .and_then(|value| value.as_i64())
            .ok_or_else(|| ServiceError {
                detail: "missing or non-integer `duration_minutes`".to_string(),
            })?;

        let deadline = Instant::now() + self.unit * (minutes.max(0) as u32);

        let id = {
            let mut next_id = self.next_id.lock().expect("TimerService next_id mutex poisoned");
            let id = *next_id;
            *next_id += 1;
            format!("timer-{id}")
        };
        let handle = RunHandle::new(id);

        self.deadlines
            .lock()
            .expect("TimerService deadlines mutex poisoned")
            .insert(handle.clone(), deadline);

        Ok(handle)
    }

    async fn status(&self, handle: &RunHandle) -> Result<RunStatus, ServiceError> {
        let deadlines = self.deadlines.lock().expect("TimerService deadlines mutex poisoned");
        let deadline = deadlines.get(handle).ok_or_else(|| ServiceError {
            detail: "unknown timer handle".to_string(),
        })?;

        if Instant::now() < *deadline {
            Ok(RunStatus::Running)
        } else {
            Ok(RunStatus::Completed)
        }
    }

    async fn cancel(&self, _handle: &RunHandle) -> Result<(), ServiceError> {
        // No-op (Deferred Ideas): client-side cancellation isn't wired to
        // any client in M1.
        Ok(())
    }

    async fn result(&self, handle: &RunHandle) -> Result<Option<serde_json::Value>, ServiceError> {
        let deadlines = self.deadlines.lock().expect("TimerService deadlines mutex poisoned");
        if deadlines.contains_key(handle) {
            // Structured JSON only (D-01) -- never a spoken-language string
            // like "Timer finished!"; presentation is a client concern.
            Ok(Some(json!({"timer_completed": true})))
        } else {
            Err(ServiceError {
                detail: "unknown timer handle".to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use crate::service::{RunStatus, Service};

    use super::TimerService;

    #[tokio::test]
    async fn invoke_reports_running_then_completed_after_deadline() {
        // Sub-second injected unit (Pitfall 4) — the workflow's declared
        // `duration_minutes` type stays `int`; only the test's time unit is
        // shrunk, never the parameter type.
        let timer = TimerService::with_unit(Duration::from_millis(20));
        let handle = timer
            .invoke(json!({"duration_minutes": 1}))
            .await
            .expect("invoke should succeed with a valid duration_minutes");

        assert_eq!(
            timer.status(&handle).await,
            Ok(RunStatus::Running),
            "expected Running immediately after invoke, before the deadline elapses"
        );

        tokio::time::sleep(Duration::from_millis(40)).await;

        assert_eq!(
            timer.status(&handle).await,
            Ok(RunStatus::Completed),
            "expected Completed once the deadline has elapsed"
        );
    }

    #[tokio::test]
    async fn result_output_is_structured_json_object_not_prose() {
        let timer = TimerService::with_unit(Duration::from_millis(1));
        let handle = timer
            .invoke(json!({"duration_minutes": 1}))
            .await
            .expect("invoke should succeed with a valid duration_minutes");

        tokio::time::sleep(Duration::from_millis(10)).await;

        let output = timer
            .result(&handle)
            .await
            .expect("result should succeed after the timer completes")
            .expect("result should produce Some output for a completed timer");

        assert!(
            output.is_object(),
            "expected a structured JSON object (D-01), never a spoken-language string; got: {output:?}"
        );
    }

    #[tokio::test]
    async fn invoke_rejects_missing_duration_minutes() {
        let timer = TimerService::with_unit(Duration::from_millis(1));

        let result = timer.invoke(json!({})).await;

        assert!(
            result.is_err(),
            "expected invoke to reject a payload missing duration_minutes"
        );
    }
}
