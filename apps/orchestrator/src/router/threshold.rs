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
/// matches, a score one representable `f32` step below does not.
///
/// MEASURED (plan 09-05, ROUT-02), replacing plan 09-01's 0.60 placeholder
/// (RESEARCH A3). Measured against `nomic-embed-text` on 2026-09-06 via
/// `tests/router_calibration.rs`'s live-gated corpus run:
/// - lowest observed known-positive score: 0.7231 (`countdown`: "give me a
///   countdown to start")
/// - highest observed known-negative score: 0.6714 (`ai_summarize`
///   close-but-wrong: "read the reference material out loud to me, word for
///   word", scored against `ai_summarize`'s own intent)
/// - observed margin: 0.0517 (>= REQUIRED_SEPARATION_MARGIN)
///
/// `MATCH_THRESHOLD` is positioned strictly above the highest observed
/// negative and at/below the lowest observed positive: 0.69. Re-run the
/// calibration and update these constants (plus this comment) any time a
/// shipped workflow's `intent:` sentence changes.
pub const MATCH_THRESHOLD: f32 = 0.69;

/// The stricter bar a `ConfirmRequired`-tier candidate must clear before it
/// is even proposed as a match (Pitfall C: a single global threshold would
/// let a destructive/agent-type workflow fire on the same permissive
/// threshold a benign workflow uses). Always `>= MATCH_THRESHOLD` -- see
/// `confirm_required_threshold_is_never_below_match_threshold`, which plan
/// 09-05's retuning must never break. Also inclusive-at-the-boundary.
///
/// MEASURED (plan 09-05, ROUT-04/Pitfall C), confirming plan 09-01's 0.70
/// placeholder (RESEARCH A3) against real data rather than replacing it.
/// Measured against `nomic-embed-text` on 2026-09-06, independently of the
/// general threshold above, using ONLY the close-but-wrong negatives
/// adjacent to `ai_summarize` (the sole confirm-required shipped workflow):
/// - lowest observed `ai_summarize` positive score: 0.7289
/// - highest observed close-but-wrong negative adjacent to `ai_summarize`:
///   0.6714
///
/// `CONFIRM_REQUIRED_MATCH_THRESHOLD` sits strictly above the observed
/// adjacent negative and at/below the observed `ai_summarize` positive
/// minimum: 0.70.
pub const CONFIRM_REQUIRED_MATCH_THRESHOLD: f32 = 0.70;

/// The maximum utterance length, in Unicode code points (`chars().count()`,
/// never a raw byte length), `Router::route` accepts before any
/// allocation-heavy work or Ollama call. An utterance longer than this is
/// REJECTED, never truncated -- mirrors `server/mod.rs`'s
/// `MAX_CLIENT_NAME_LEN` reject-not-truncate discipline. A value of exactly
/// this many code points is accepted (inclusive upper bound).
pub const MAX_UTTERANCE_CHARS: usize = 1000;

/// The minimum required gap (in cosine-similarity units) between the lowest
/// observed known-positive score and the highest observed known-negative
/// score, asserted by `tests/router_calibration.rs`'s live calibration
/// (plan 09-05, ROUT-02). RESEARCH's Assumptions Log (A3) flags this
/// starting value as illustrative, not measured -- if the real corpus
/// separates by less than this, the correct response is to revise the
/// `intent:` sentences and re-run, never to lower this number. That
/// sentence is the whole point of the constant: it exists so a future
/// reader cannot quietly trade separation for a green test.
pub const REQUIRED_SEPARATION_MARGIN: f32 = 0.05;

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
