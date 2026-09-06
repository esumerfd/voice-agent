//! Named threshold/limit constants for the router (ROUT-01/02/04). Every
//! constant here carries a doc comment stating the contract it enforces, so
//! a later calibration pass (plan 09-05) changes ONE number in ONE place
//! rather than a magic literal scattered across `router::mod`.
//!
//! Task 1 (this commit) adds only `OLLAMA_TIMEOUT_SECS`, needed by
//! `HttpOllamaClient::new` -- Task 2 adds `MATCH_THRESHOLD`,
//! `CONFIRM_REQUIRED_MATCH_THRESHOLD`, and `MAX_UTTERANCE_CHARS` to this
//! same file.

/// The `reqwest::Client` timeout (seconds) applied to every Ollama call.
/// RESEARCH A2: an adequate ceiling for a local-loopback call, not
/// independently tuned -- recorded as a named constant so a single edit
/// changes it for every call site built through `HttpOllamaClient::new`.
pub const OLLAMA_TIMEOUT_SECS: u64 = 10;
