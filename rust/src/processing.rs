//! Mode text processing: OpenAI-compatible and Ollama chat endpoints.
//! `quick` mode never reaches a provider; failures surface as `Err` so the
//! coordinator keeps the recognized text with a warning.
//!
//! Position in the pipeline: after the `[text]` cleanup of the final
//! transcript, `coordinator::run_session` calls `TextProcessor::process`
//! for every mode other than `quick`. `stream::PipelineProcessor` looks up
//! the mode's prompt template (`modes::prompt_table`, where a non-empty
//! `[processing].prompt` replaces the builtin prompt-optimize template),
//! renders it with `modes::render_template` (`{text}` becomes the cleaned
//! transcript) and runs `ChatProcessor::process` on a blocking thread
//! (`spawn_blocking`). This module never sees the template: it receives
//! the finished prompt and returns the model's reply.
//!
//! Configuration: `[processing]` (`provider`, `base_url`, `model`,
//! `api_key` / `api_key_env`, `timeout_seconds`). `from_config` builds the
//! production processor once per session in `stream::build_pipeline`, so an
//! unknown provider or a missing `base_url`/`model` is a session start-up
//! error, not a per-utterance one.
//!
//! Failure contract: every `ProcessingError` (transport, non-200 status,
//! unparsable or blank reply) propagates as `Err`. The coordinator then
//! emits `OutputEvent::Warning("processing failed, keeping recognized text:
//! …")`, injects the cleaned transcript and records the row with
//! `processed_text = None`. Nothing here retries, and the raw transcript is
//! never sent anywhere but the configured endpoint.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::ProcessingConfig;

/// Why a processing call produced no text. All variants end up as the
/// warning text of the session; none is retried.
#[derive(Debug, thiserror::Error)]
pub enum ProcessingError {
    /// No provider configured. Not produced inside this module today:
    /// `from_config` returns `Ok(None)` for `none` and the pipeline reports
    /// the missing provider as a plain string.
    #[error("no text processing provider configured")]
    NoProvider,
    /// The request could not be built or sent: unknown provider, missing
    /// `base_url`/`model`, DNS, TLS, connection or timeout failure.
    #[error("processing request failed: {0}")]
    Request(String),
    /// HTTP 200 but the body could not be read, is not JSON, or carries no
    /// non-blank content at the provider's content path.
    #[error("processing response malformed: {0}")]
    Response(String),
    /// Any status other than 200 (401/403 bad credentials, 404 wrong path
    /// or model, 429 rate limit, 5xx).
    #[error("processing provider rejected the request: HTTP {0}")]
    Status(u16),
}

/// HTTP boundary; production uses ureq, tests inject canned responses.
pub trait HttpPoster {
    /// POST `body` (already JSON) to `url` with
    /// `Content-Type: application/json` and, when `bearer` is given,
    /// `Authorization: Bearer <token>`, waiting at most `timeout` for the
    /// whole exchange. Returns `(status, body)` for every HTTP response,
    /// 4xx/5xx included; `Err(Request)` only for transport failures.
    /// Blocking.
    fn post_json(
        &self,
        url: &str,
        body: &str,
        bearer: Option<&str>,
        timeout: Duration,
    ) -> Result<(u16, String), ProcessingError>;
}

/// Production `HttpPoster` over `ureq` (synchronous). Stateless, so the
/// processor can be cloned into a blocking task per utterance.
#[derive(Clone)]
pub struct UreqPoster;

impl HttpPoster for UreqPoster {
    fn post_json(
        &self,
        url: &str,
        body: &str,
        bearer: Option<&str>,
        timeout: Duration,
    ) -> Result<(u16, String), ProcessingError> {
        let mut request = ureq::post(url)
            .timeout(timeout)
            .set("Content-Type", "application/json");
        if let Some(bearer) = bearer {
            request = request.set("Authorization", &format!("Bearer {bearer}"));
        }
        match request.send_string(body) {
            Ok(response) => {
                let status = response.status();
                let body = response
                    .into_string()
                    .map_err(|error| ProcessingError::Response(error.to_string()))?;
                Ok((status, body))
            }
            // ureq reports 4xx/5xx as errors; hand them back as a plain
            // `(status, body)` so `process` maps every non-200 reply to
            // `ProcessingError::Status` the same way.
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                Ok((status, body))
            }
            Err(error) => Err(ProcessingError::Request(error.to_string())),
        }
    }
}

/// One configured chat endpoint. The fields are public so tests and
/// diagnostics can inspect what `from_config` resolved (key included; never
/// log it). `process` is synchronous; the pipeline runs it off the async
/// runtime.
#[derive(Clone)]
pub struct ChatProcessor<H: HttpPoster> {
    /// `openai-compatible` or `ollama`; any other value makes `process`
    /// fail with `Request`.
    pub provider: String,
    /// Endpoint root; a trailing `/` is tolerated.
    pub base_url: String,
    pub model: String,
    /// Bearer token; empty sends no `Authorization` header (Ollama, local
    /// gateways).
    pub api_key: String,
    /// Whole-request timeout of one call.
    pub timeout: Duration,
    poster: H,
}

impl<H: HttpPoster> ChatProcessor<H> {
    /// Assemble a processor from already-resolved values (no validation;
    /// see `from_config` for the config-driven path).
    pub fn new(
        provider: String,
        base_url: String,
        model: String,
        api_key: String,
        timeout: Duration,
        poster: H,
    ) -> Self {
        Self {
            provider,
            base_url,
            model,
            api_key,
            timeout,
            poster,
        }
    }

    /// The HTTP boundary in use; tests read the last request back from
    /// their scripted poster.
    pub fn poster_ref(&self) -> &H {
        &self.poster
    }

    /// Send `prompt` as a single user message and return the assistant's
    /// reply text.
    ///
    /// - `openai-compatible`: `POST {base_url}/chat/completions` with
    ///   `{model, messages}`; the reply is `choices[0].message.content`.
    /// - `ollama`: `POST {base_url}/api/chat` with `stream: false` so the
    ///   reply is one JSON object instead of NDJSON chunks; the reply is
    ///   `message.content`.
    ///
    /// Errors: unknown provider or transport failure (`Request`), any
    /// status other than 200 (`Status`), unparsable JSON or missing/blank
    /// content (`Response`). The content is returned untrimmed. Blocks for
    /// up to `timeout`.
    pub fn process(&self, prompt: &str) -> Result<String, ProcessingError> {
        let (url, payload) = match self.provider.as_str() {
            "openai-compatible" => {
                let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
                let payload = serde_json::json!({
                    "model": self.model,
                    "messages": [{"role": "user", "content": prompt}],
                });
                (url, payload)
            }
            "ollama" => {
                let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));
                let payload = serde_json::json!({
                    "model": self.model,
                    "stream": false,
                    "messages": [{"role": "user", "content": prompt}],
                });
                (url, payload)
            }
            other => {
                return Err(ProcessingError::Request(format!(
                    "unknown provider: {other}"
                )))
            }
        };
        // No key, no header: Ollama and local gateways reject or ignore an
        // empty bearer depending on the server.
        let bearer = (!self.api_key.is_empty()).then_some(self.api_key.as_str());
        let (status, body) =
            self.poster
                .post_json(&url, &payload.to_string(), bearer, self.timeout)?;
        // Credential failures and every other non-200 reply map to the same
        // `Status` variant; the warning carries the code for the user.
        if status == 401 || status == 403 {
            return Err(ProcessingError::Status(status));
        }
        if status != 200 {
            return Err(ProcessingError::Status(status));
        }
        let parsed: serde_json::Value = serde_json::from_str(&body)
            .map_err(|error| ProcessingError::Response(error.to_string()))?;
        let content = match self.provider.as_str() {
            "openai-compatible" => parsed["choices"][0]["message"]["content"].as_str(),
            _ => parsed["message"]["content"].as_str(),
        };
        // A blank reply would inject nothing and silently lose the
        // transcript; treat it as a failure so the raw text is kept.
        match content {
            Some(text) if !text.trim().is_empty() => Ok(text.to_owned()),
            _ => Err(ProcessingError::Response(
                "provider returned no text content".to_owned(),
            )),
        }
    }
}

/// Build the production processor from config; `none` maps to `None`.
///
/// Key resolution: a non-blank `api_key` wins; otherwise the environment
/// variable named by `api_key_env` is read (unset or empty variable: no
/// key, no header). `base_url` and `model` must be non-empty and the
/// provider must be `openai-compatible` or `ollama`; anything else is a
/// `Request` error, raised when the session pipeline is built. Reads the
/// environment only; no network.
pub fn from_config(
    config: &ProcessingConfig,
) -> Result<Option<ChatProcessor<UreqPoster>>, ProcessingError> {
    match config.provider.as_str() {
        "none" => Ok(None),
        "openai-compatible" | "ollama" => {
            if config.base_url.is_empty() || config.model.is_empty() {
                return Err(ProcessingError::Request(
                    "processing.base_url and processing.model are required".to_owned(),
                ));
            }
            let api_key = if !config.api_key.trim().is_empty() {
                config.api_key.clone()
            } else if config.api_key_env.is_empty() {
                String::new()
            } else {
                std::env::var(&config.api_key_env).unwrap_or_default()
            };
            Ok(Some(ChatProcessor::new(
                config.provider.clone(),
                config.base_url.clone(),
                config.model.clone(),
                api_key,
                Duration::from_secs_f64(config.timeout_seconds),
                UreqPoster,
            )))
        }
        other => Err(ProcessingError::Request(format!(
            "unknown processing provider: {other}"
        ))),
    }
}

/// Serializable `{text, provider}` pair. Currently unused inside the
/// crate.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProcessedText {
    pub text: String,
    pub provider: String,
}
