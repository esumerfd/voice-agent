//! Ollama HTTP client seam (ROUT-01). `OllamaApi` is a pluggable trait
//! object seam (`Arc<dyn OllamaApi>`) mirroring `handlers/ai_agent.rs`'s
//! `Arc<dyn AgentRuntime>` precedent -- every router unit test runs against
//! a mock implementation of this trait; only `tests/router_calibration.rs`
//! (plan 09-05) ever constructs `HttpOllamaClient` against a live server.
//! Plan 09-04 adds a second method (`generate_json`) to this same trait.
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

/// The Ollama HTTP calls behind a pluggable trait object seam. `Send +
/// Sync` so it can be held as `Arc<dyn OllamaApi>` and shared across the
/// daemon's concurrent connections (mirrors `Arc<dyn AgentRuntime>`).
#[async_trait]
pub trait OllamaApi: Send + Sync {
    /// One embedding attempt for a batch of inputs. Implementations make
    /// exactly one HTTP round trip (or, for a mock, one canned response) --
    /// no retry logic belongs here; see [`embed_with_retry`].
    async fn embed(&self, model: &str, inputs: &[String]) -> Result<Vec<Vec<f32>>, RouterError>;
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
