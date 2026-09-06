//! Ollama HTTP client seam (ROUT-01). `OllamaApi` is a pluggable trait
//! object seam (`Arc<dyn OllamaApi>`) mirroring `handlers/ai_agent.rs`'s
//! `Arc<dyn AgentRuntime>` precedent -- every router unit test runs against
//! a mock implementation of this trait; only `tests/router_calibration.rs`
//! (plan 09-05) ever constructs `HttpOllamaClient` against a live server.
//!
//! Plan 09-04 (ROUT-03) adds a second method, `generate_json`, to this same
//! trait -- structured parameter extraction via `POST /api/generate` with a
//! JSON-schema `format` constraint. `system` and `prompt` are two
//! STRUCTURALLY SEPARATE request fields: `system` carries the fixed
//! extraction instruction, `prompt` carries the untrusted utterance alone
//! (T-09-01) -- never concatenated into one string.
//!
//! Each `OllamaApi::embed` implementation makes exactly ONE attempt --
//! retry-on-timeout is a shared concern every call site inherits via
//! [`embed_with_retry`], below, not a duty each implementation re-derives.

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::RouterError;
use crate::router::threshold::OLLAMA_TIMEOUT_SECS;

/// `POST {base_url}/api/embed` request body (RESEARCH Pattern 2, verified
/// live shape: `input` accepts a batch array even for a single string).
#[derive(serde::Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

/// `embeddings` is always an array-of-arrays, even for a single input
/// (RESEARCH Pattern 2, verified live against Ollama v0.33.2). Every other
/// field Ollama's response carries (`model`, `total_duration`, ...) is
/// deliberately NOT modeled here -- an explicit allowlist, mirroring
/// `ai_agent.rs`'s `envelope_subset` "allowlist, not everything parses"
/// discipline.
#[derive(Deserialize)]
pub struct EmbedResponse {
    pub embeddings: Vec<Vec<f32>>,
}

/// Ollama's JSON error body shape on a non-2xx response (RESEARCH Pattern
/// 2/4, verified live: `{"error": "..."}`). Never string-matched on its
/// punctuation -- the two Ollama endpoints quote the model name
/// differently -- only parsed as JSON and read via this one field.
#[derive(Deserialize)]
pub struct OllamaErrorBody {
    pub error: String,
}

/// `POST {base_url}/api/generate` request body (RESEARCH Pattern 3, verified
/// live shape). `format` carries the schema OBJECT, never the string
/// `"json"`. `system` and `prompt` are two structurally separate fields
/// (T-09-01) -- `system` is the fixed extraction instruction, `prompt` is
/// the untrusted utterance alone.
#[derive(serde::Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    system: &'a str,
    prompt: &'a str,
    stream: bool,
    format: &'a serde_json::Value,
    options: GenerateOptions,
}

/// Temperature 0 with a fixed seed makes extraction reproducible -- the same
/// utterance and schema produce the same request bytes and, in practice, the
/// same model output across runs (COVERAGE.md row 10).
#[derive(serde::Serialize)]
struct GenerateOptions {
    temperature: f32,
    seed: i64,
}

/// Extraction is deterministic by construction (temperature 0 + fixed
/// seed) -- the exact seed value carries no meaning beyond "always the
/// same".
const EXTRACTION_TEMPERATURE: f32 = 0.0;
const EXTRACTION_SEED: i64 = 0;

/// Ollama's `/api/generate` response envelope (RESEARCH Pattern 3). `response`
/// is a JSON-ENCODED STRING, not a nested object -- `format` constrains the
/// CONTENT of this string, never the envelope's own shape (RESEARCH Pitfall
/// A). Every field this router never needs (`created_at`, `context`,
/// `total_duration`, ...) is deliberately NOT modeled here, mirroring
/// `EmbedResponse`'s own explicit-allowlist discipline.
#[derive(Deserialize)]
struct GenerateResponse {
    response: String,
    #[allow(dead_code)]
    done: bool,
}

/// The Ollama HTTP calls behind a pluggable trait object seam. `Send +
/// Sync` so it can be held as `Arc<dyn OllamaApi>` and shared across the
/// daemon's concurrent connections (mirrors `Arc<dyn AgentRuntime>`).
#[async_trait]
pub trait OllamaApi: Send + Sync {
    /// One embedding attempt for a batch of inputs. Implementations make
    /// exactly one HTTP round trip (or, for a mock, one canned response) --
    /// no retry logic belongs here; see [`embed_with_retry`].
    async fn embed(&self, model: &str, inputs: &[String]) -> Result<Vec<Vec<f32>>, RouterError>;

    /// One structured-generation attempt (ROUT-03, plan 09-04). `system`
    /// carries the fixed extraction instruction and `prompt` carries the
    /// untrusted utterance ALONE -- structurally separate request fields,
    /// never concatenated into one string, which is the stronger injection
    /// boundary T-09-01 relies on. `schema` constrains the CONTENT of the
    /// `response` string Ollama returns; implementations must parse that
    /// string a second time (RESEARCH Pitfall A) before returning.
    ///
    /// Deviation note (Rule 3, plan 09-04): the plan's own prose specified
    /// this method as `generate_json(&self, model, prompt, schema)`, with no
    /// `system` parameter -- but the SAME task also requires the fixed
    /// instruction and the untrusted utterance to travel in structurally
    /// separate request fields (`system` vs `prompt`), which is impossible
    /// without a fourth parameter. Added `system: &str` here so the
    /// caller-facing seam can actually carry both.
    async fn generate_json(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        schema: &serde_json::Value,
    ) -> Result<serde_json::Value, RouterError>;
}

/// Real `OllamaApi` impl backed by `reqwest`. Holds exactly one
/// `reqwest::Client`, built once in `new()` -- never a client per call
/// (RESEARCH Pattern 1, Anti-Patterns: constructing a new client per
/// request defeats connection pooling for no benefit on loopback HTTP).
pub struct HttpOllamaClient {
    http: reqwest::Client,
    base_url: String,
}

impl HttpOllamaClient {
    /// `.expect()` here is acceptable per RESEARCH Pitfall D:
    /// `Client::builder().build()` only fails on a TLS-backend
    /// misconfiguration, a build-environment constant checked once at
    /// startup -- never a per-request condition. Every per-request Ollama
    /// failure is mapped into `RouterError` instead, never `.expect()`'d.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(OLLAMA_TIMEOUT_SECS))
                .build()
                .expect(
                    "reqwest client construction must not fail on the default (no-TLS-backend) build",
                ),
            base_url: base_url.into(),
        }
    }
}

/// Classifies a failed `.send()` into the matching `RouterError` (RESEARCH
/// Pattern 4): a connection failure, then a timeout, then anything else --
/// checked in that priority order, never by matching on error-message text.
fn classify_send_error(e: &reqwest::Error, base_url: &str, endpoint: &str) -> RouterError {
    if e.is_connect() {
        RouterError::OllamaUnreachable {
            base_url: base_url.to_string(),
            detail: e.to_string(),
        }
    } else if e.is_timeout() {
        RouterError::Timeout {
            endpoint: endpoint.to_string(),
            seconds: OLLAMA_TIMEOUT_SECS,
        }
    } else {
        RouterError::MalformedResponse {
            endpoint: endpoint.to_string(),
            detail: e.to_string(),
        }
    }
}

#[async_trait]
impl OllamaApi for HttpOllamaClient {
    async fn embed(&self, model: &str, inputs: &[String]) -> Result<Vec<Vec<f32>>, RouterError> {
        let endpoint = format!("{}/api/embed", self.base_url);
        let body = EmbedRequest { model, input: inputs };

        let response = self
            .http
            .post(&endpoint)
            .json(&body)
            .send()
            .await
            .map_err(|e| classify_send_error(&e, &self.base_url, &endpoint))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            let detail = response
                .text()
                .await
                .ok()
                .and_then(|raw| serde_json::from_str::<OllamaErrorBody>(&raw).ok())
                .map(|e| e.error)
                .unwrap_or_else(|| "model not found".to_string());
            return Err(RouterError::ModelNotFound {
                model: model.to_string(),
                detail,
            });
        }

        if !response.status().is_success() {
            let status = response.status();
            let detail = response.text().await.unwrap_or_default();
            return Err(RouterError::MalformedResponse {
                endpoint,
                detail: format!("HTTP {status}: {detail}"),
            });
        }

        let parsed: EmbedResponse = response.json().await.map_err(|e| RouterError::MalformedResponse {
            endpoint,
            detail: e.to_string(),
        })?;

        Ok(parsed.embeddings)
    }

    async fn generate_json(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        schema: &serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        let endpoint = format!("{}/api/generate", self.base_url);
        let body = GenerateRequest {
            model,
            system,
            prompt,
            stream: false,
            format: schema,
            options: GenerateOptions { temperature: EXTRACTION_TEMPERATURE, seed: EXTRACTION_SEED },
        };

        let response = self
            .http
            .post(&endpoint)
            .json(&body)
            .send()
            .await
            .map_err(|e| classify_send_error(&e, &self.base_url, &endpoint))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            let detail = response
                .text()
                .await
                .ok()
                .and_then(|raw| serde_json::from_str::<OllamaErrorBody>(&raw).ok())
                .map(|e| e.error)
                .unwrap_or_else(|| "model not found".to_string());
            return Err(RouterError::ModelNotFound {
                model: model.to_string(),
                detail,
            });
        }

        if !response.status().is_success() {
            let status = response.status();
            let detail = response.text().await.unwrap_or_default();
            return Err(RouterError::MalformedResponse {
                endpoint,
                detail: format!("HTTP {status}: {detail}"),
            });
        }

        let raw_body = response.text().await.map_err(|e| RouterError::MalformedResponse {
            endpoint: endpoint.clone(),
            detail: e.to_string(),
        })?;

        parse_generate_envelope(&raw_body, &endpoint)
    }
}

/// Parses the `/api/generate` wire envelope, then re-parses its `response`
/// field's own STRING CONTENT as JSON a second time (RESEARCH Pitfall A) --
/// extracted as its own pure function (no I/O) so the mandatory double-parse
/// is directly unit-testable against a canned response body, without a
/// live/mocked HTTP layer. `format` constrains the CONTENT of the `response`
/// string, never the wire envelope's own shape: a body where `response` is a
/// nested JSON object (rather than a string) fails the FIRST parse here
/// (`GenerateResponse.response` is typed `String`), and a body where
/// `response`'s string content is not itself valid JSON fails the SECOND.
/// Both failures return `RouterError::MalformedResponse`, never a panic and
/// never a silent partial success.
fn parse_generate_envelope(raw_body: &str, endpoint: &str) -> Result<serde_json::Value, RouterError> {
    let parsed: GenerateResponse = serde_json::from_str(raw_body).map_err(|e| RouterError::MalformedResponse {
        endpoint: endpoint.to_string(),
        detail: e.to_string(),
    })?;

    serde_json::from_str(&parsed.response).map_err(|e| RouterError::MalformedResponse {
        endpoint: endpoint.to_string(),
        detail: format!("`response` field was not valid JSON: {e}"),
    })
}

/// Retries a timed-out embed call exactly once, then fails -- the pattern
/// 09-RESEARCH.md's Don't-Hand-Roll table names: a single explicit
/// retry-then-fail mirroring `AiAgentService`'s own documented one-retry
/// behaviour for the analogous envelope-parse case, rather than a retry or
/// backoff crate. Every `Router` call site that talks to Ollama goes
/// through this ONE function -- "the client" every call site inherits the
/// retry from -- so the attempt ceiling can never drift between call
/// sites. Retries ONLY the `Timeout` classification: a connection refusal,
/// a 404, or a malformed body are never retried, because retrying any of
/// those only doubles the wait before the same answer arrives. Attempt
/// ceiling: at most 2 calls to `client.embed`, ever.
pub(crate) async fn embed_with_retry(
    client: &dyn OllamaApi,
    model: &str,
    inputs: &[String],
) -> Result<Vec<Vec<f32>>, RouterError> {
    match client.embed(model, inputs).await {
        Err(RouterError::Timeout { .. }) => client.embed(model, inputs).await,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_envelope_with_a_json_encoded_response_string_parses_to_the_inner_value() {
        let raw = r#"{"response": "{\"duration_minutes\": 10}", "done": true}"#;

        let value = parse_generate_envelope(raw, "http://x/api/generate").expect("expected a parsed value");

        assert_eq!(value, serde_json::json!({"duration_minutes": 10}));
    }

    #[test]
    fn a_response_field_that_is_a_nested_object_rather_than_a_string_fails_as_malformed_response() {
        // The single most easily-missed failure mode (RESEARCH Pitfall A):
        // `format` constrains the CONTENT of `response`, never the wire
        // envelope's own shape -- a naive implementation that expects
        // `response` to already be a nested object gets exactly this shape
        // from a real Ollama server and must fail loudly, not silently.
        let raw = r#"{"response": {"duration_minutes": 10}, "done": true}"#;

        let err = parse_generate_envelope(raw, "http://x/api/generate")
            .expect_err("a nested-object response field must fail, never silently succeed");

        assert!(matches!(err, RouterError::MalformedResponse { .. }));
    }

    #[test]
    fn a_response_string_that_is_not_valid_json_fails_as_malformed_response() {
        let raw = r#"{"response": "not valid json at all", "done": true}"#;

        let err = parse_generate_envelope(raw, "http://x/api/generate")
            .expect_err("an unparseable response string must fail, never silently succeed");

        assert!(matches!(err, RouterError::MalformedResponse { .. }));
    }

    #[test]
    fn a_response_string_that_parses_to_a_json_array_is_not_rejected_at_this_layer() {
        // This function's ONLY job is the double parse -- rejecting a
        // non-object result (an array or scalar) is `Router::extract_params`'s
        // job (plan 09-04, Task 2), not this pure envelope-parsing function's.
        let raw = r#"{"response": "[1, 2, 3]", "done": true}"#;

        let value = parse_generate_envelope(raw, "http://x/api/generate").expect("array parses fine here");

        assert!(value.is_array());
    }

    #[test]
    fn a_completely_malformed_envelope_fails_the_first_parse() {
        let raw = "not even json";

        let err = parse_generate_envelope(raw, "http://x/api/generate")
            .expect_err("a malformed envelope must fail at the first parse");

        assert!(matches!(err, RouterError::MalformedResponse { .. }));
    }
}
