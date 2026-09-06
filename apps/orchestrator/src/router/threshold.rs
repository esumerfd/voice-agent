//! Named threshold/limit constants for the router (ROUT-01/02/04). Every
//! constant here carries a doc comment stating the contract it enforces, so
//! a later calibration pass (plan 09-05) changes ONE number in ONE place
//! rather than a magic literal scattered across `router::mod`.

/// The `reqwest::Client` timeout (seconds) applied to every Ollama call.
/// RESEARCH A2: an adequate ceiling for a local-loopback call, not
/// independently tuned -- recorded as a named constant so a single edit
/// changes it for every call site built through `HttpOllamaClient::new`.
pub const OLLAMA_TIMEOUT_SECS: u64 = 10;

/// The minimum cosine-similarity score a candidate must meet or exceed to
/// be reported as a match -- inclusive: a score exactly equal to this value
/// matches, a score one representable `f32` step below does not. RESEARCH
/// A3: this initial value is a PLACEHOLDER, not a measurement -- plan 09-05
/// replaces it with a value derived from the real fixture corpus once one
/// exists.
pub const MATCH_THRESHOLD: f32 = 0.60;

/// The stricter bar a `ConfirmRequired`-tier candidate must clear before it
/// is even proposed as a match (Pitfall C: a single global threshold would
/// let a destructive/agent-type workflow fire on the same permissive
/// threshold a benign workflow uses). Always `>= MATCH_THRESHOLD` -- see
/// `confirm_required_threshold_is_never_below_match_threshold`, which plan
/// 09-05's retuning must never break. Also inclusive-at-the-boundary, and
/// also a placeholder (RESEARCH A3), not a measurement.
pub const CONFIRM_REQUIRED_MATCH_THRESHOLD: f32 = 0.70;

/// The maximum utterance length, in Unicode code points (`chars().count()`,
/// never a raw byte length), `Router::route` accepts before any
/// allocation-heavy work or Ollama call. An utterance longer than this is
/// REJECTED, never truncated -- mirrors `server/mod.rs`'s
/// `MAX_CLIENT_NAME_LEN` reject-not-truncate discipline. A value of exactly
/// this many code points is accepted (inclusive upper bound).
pub const MAX_UTTERANCE_CHARS: usize = 1000;

#[cfg(test)]
mod tests {
    use super::*;

    // Both operands are `const` today, so clippy sees this as tautological
    // -- but the whole point of keeping it as a real `#[test]` (rather than
    // a `const _: () = assert!(...)` compile-time check) is that it must
    // keep running, and keep failing loudly, if plan 09-05 ever retunes
    // either constant in a way that breaks the invariant.
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn confirm_required_threshold_is_never_below_match_threshold() {
        assert!(
            CONFIRM_REQUIRED_MATCH_THRESHOLD >= MATCH_THRESHOLD,
            "CONFIRM_REQUIRED_MATCH_THRESHOLD ({CONFIRM_REQUIRED_MATCH_THRESHOLD}) must never be \
             below MATCH_THRESHOLD ({MATCH_THRESHOLD}) -- a confirmation-required workflow must \
             clear a stricter bar than a freely-routable one, never a laxer one (Pitfall C)"
        );
    }
}
