//! The LLM backend: one trait over the OpenAI Chat Completions wire format.
//!
//! Almost every provider the operator might switch to speaks
//! `POST {base_url}/chat/completions`, so the provider is a `(base_url, model,
//! api_key)` config value rather than a code path: swapping a free model for a
//! paid one, or one vendor for another, is an edit to the config file.

use std::thread;
use std::time::Duration;

use anyhow::{Context, anyhow};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::Llm;

/// A conversation role, mapped to its wire string on serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    fn wire(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// One turn of conversation history handed to the backend.
#[derive(Debug, Clone)]
pub struct Turn {
    pub role: Role,
    pub content: String,
}

impl Turn {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// A backend's reply plus optional token usage for the daily budget.
#[derive(Debug)]
pub struct Completion {
    pub text: String,
    pub tokens_used: Option<u32>,
}

/// A failed completion, classified by whether retrying could help.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// A connection error, timeout, HTTP 429, or 5xx: transient, worth retrying.
    #[error("{0}")]
    Transient(anyhow::Error),
    /// A non-retryable 4xx (bad key or request) or an unparsable response.
    #[error("{0}")]
    Permanent(anyhow::Error),
}

impl LlmError {
    pub fn is_transient(&self) -> bool {
        matches!(self, LlmError::Transient(_))
    }
}

/// The one thing the bot needs from a model: system prompt + prior turns in,
/// assistant reply out. `Send + Sync` so the worker pool can share it.
pub trait LlmBackend: Send + Sync {
    fn complete(&self, system: &str, history: &[Turn]) -> Result<Completion, LlmError>;
}

/// How [`complete_with_retry`] backs off between transient failures.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }
}

/// Exponential backoff `base * 2^attempt`, capped at `max_delay` (attempt is
/// 0-based). No jitter: at this worker count there is no herd to decorrelate.
fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    let factor = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    let ms = (policy.base_delay.as_millis() as u64)
        .saturating_mul(factor)
        .min(policy.max_delay.as_millis() as u64);
    Duration::from_millis(ms)
}

/// Call `backend.complete`, retrying transient failures up to
/// `policy.max_attempts` with exponential backoff. Permanent failures return
/// immediately.
pub fn complete_with_retry(
    backend: &dyn LlmBackend,
    system: &str,
    history: &[Turn],
    policy: &RetryPolicy,
) -> Result<Completion, LlmError> {
    let mut attempt = 0;
    loop {
        match backend.complete(system, history) {
            Ok(completion) => return Ok(completion),
            Err(err) => {
                attempt += 1;
                if attempt >= policy.max_attempts || !err.is_transient() {
                    return Err(err);
                }
                let delay = backoff_delay(policy, attempt - 1);
                warn!(attempt, ?delay, error = %err, "LLM call failed transiently; retrying");
                if !delay.is_zero() {
                    thread::sleep(delay);
                }
            }
        }
    }
}

/// An [`LlmBackend`] against any OpenAI-compatible `/chat/completions` endpoint.
pub struct OpenAiCompatBackend {
    endpoint: String,
    model: String,
    api_key: String,
    max_tokens: u32,
    temperature: f32,
    http: reqwest::blocking::Client,
}

impl OpenAiCompatBackend {
    pub fn new(cfg: &Llm) -> anyhow::Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(cfg.request_timeout)
            .build()
            .context("building the HTTP client")?;
        let base = cfg.base_url.trim_end_matches('/');
        Ok(Self {
            endpoint: format!("{base}/chat/completions"),
            model: cfg.model.clone(),
            api_key: cfg.api_key.clone(),
            max_tokens: cfg.max_output_tokens,
            temperature: cfg.temperature,
            http,
        })
    }
}

impl LlmBackend for OpenAiCompatBackend {
    fn complete(&self, system: &str, history: &[Turn]) -> Result<Completion, LlmError> {
        let request = ChatRequest {
            model: &self.model,
            messages: build_messages(system, history),
            max_tokens: self.max_tokens,
            temperature: self.temperature,
        };

        // Connection and timeout errors are transient; retrying can succeed.
        let response = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .context("sending the chat completion request")
            .map_err(LlmError::Transient)?;

        let status = response.status();
        // The body may echo the prompt or user content, so it is read for
        // parsing but never surfaced in an error (only the status is).
        let body = response
            .text()
            .context("reading the chat completion response body")
            .map_err(LlmError::Transient)?;
        if !status.is_success() {
            let err = anyhow!("chat completion request failed with status {status}");
            // 429 and 5xx are worth retrying; other 4xx are configuration or
            // request errors that will not fix themselves.
            return Err(
                if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                    LlmError::Transient(err)
                } else {
                    LlmError::Permanent(err)
                },
            );
        }
        parse_response(&body).map_err(LlmError::Permanent)
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    max_tokens: u32,
    temperature: f32,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: RespMessage,
}

#[derive(Deserialize)]
struct RespMessage {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    total_tokens: Option<u32>,
}

fn build_messages<'a>(system: &'a str, history: &'a [Turn]) -> Vec<WireMessage<'a>> {
    let mut messages = Vec::with_capacity(history.len() + 1);
    if !system.is_empty() {
        messages.push(WireMessage {
            role: Role::System.wire(),
            content: system,
        });
    }
    for turn in history {
        messages.push(WireMessage {
            role: turn.role.wire(),
            content: &turn.content,
        });
    }
    messages
}

fn parse_response(body: &str) -> anyhow::Result<Completion> {
    let response: ChatResponse =
        serde_json::from_str(body).context("decoding the chat completion response")?;
    let text = response
        .choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message.content)
        .map(|content| content.trim().to_string())
        .filter(|content| !content.is_empty())
        .ok_or_else(|| anyhow!("chat completion response carried no message content"))?;
    let tokens_used = response.usage.and_then(|usage| usage.total_tokens);
    Ok(Completion { text, tokens_used })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepends_system_prompt_and_maps_roles() {
        let history = [
            Turn::user("hi"),
            Turn::assistant("hello"),
            Turn::user("bye"),
        ];
        let messages = build_messages("be terse", &history);
        let roles: Vec<_> = messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, ["system", "user", "assistant", "user"]);
        assert_eq!(messages[0].content, "be terse");
        assert_eq!(messages[3].content, "bye");
    }

    #[test]
    fn empty_system_prompt_is_omitted() {
        let history = [Turn::user("hi")];
        let messages = build_messages("", &history);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
    }

    #[test]
    fn parses_content_and_usage() {
        let body = r#"{
            "choices": [{"message": {"role": "assistant", "content": "  42  "}}],
            "usage": {"total_tokens": 123}
        }"#;
        let completion = parse_response(body).unwrap();
        assert_eq!(completion.text, "42");
        assert_eq!(completion.tokens_used, Some(123));
    }

    #[test]
    fn missing_usage_yields_no_token_count() {
        let body = r#"{"choices": [{"message": {"content": "hi"}}]}"#;
        let completion = parse_response(body).unwrap();
        assert_eq!(completion.text, "hi");
        assert_eq!(completion.tokens_used, None);
    }

    #[test]
    fn empty_choices_is_an_error() {
        assert!(parse_response(r#"{"choices": []}"#).is_err());
    }

    #[test]
    fn null_or_blank_content_is_an_error() {
        assert!(parse_response(r#"{"choices": [{"message": {"content": null}}]}"#).is_err());
        assert!(parse_response(r#"{"choices": [{"message": {"content": "   "}}]}"#).is_err());
    }

    use std::sync::atomic::{AtomicU32, Ordering};

    /// Fails transiently `fail_times`, then succeeds; counts calls.
    struct Flaky {
        fail_times: u32,
        permanent: bool,
        calls: AtomicU32,
    }

    impl LlmBackend for Flaky {
        fn complete(&self, _system: &str, _history: &[Turn]) -> Result<Completion, LlmError> {
            let n = self.calls.fetch_add(1, Ordering::Relaxed);
            if n < self.fail_times {
                let err = anyhow!("boom");
                Err(if self.permanent {
                    LlmError::Permanent(err)
                } else {
                    LlmError::Transient(err)
                })
            } else {
                Ok(Completion {
                    text: "ok".to_string(),
                    tokens_used: None,
                })
            }
        }
    }

    fn instant_policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    #[test]
    fn retries_transient_failures_until_success() {
        let backend = Flaky {
            fail_times: 2,
            permanent: false,
            calls: AtomicU32::new(0),
        };
        let completion =
            complete_with_retry(&backend, "s", &[], &instant_policy(3)).expect("should succeed");
        assert_eq!(completion.text, "ok");
        assert_eq!(backend.calls.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn does_not_retry_a_permanent_failure() {
        let backend = Flaky {
            fail_times: 5,
            permanent: true,
            calls: AtomicU32::new(0),
        };
        let err = complete_with_retry(&backend, "s", &[], &instant_policy(3)).unwrap_err();
        assert!(matches!(err, LlmError::Permanent(_)));
        assert_eq!(backend.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn gives_up_after_max_attempts() {
        let backend = Flaky {
            fail_times: 10,
            permanent: false,
            calls: AtomicU32::new(0),
        };
        let err = complete_with_retry(&backend, "s", &[], &instant_policy(3)).unwrap_err();
        assert!(err.is_transient());
        assert_eq!(backend.calls.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn backoff_grows_exponentially_and_caps() {
        let policy = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        };
        assert_eq!(backoff_delay(&policy, 0), Duration::from_secs(1));
        assert_eq!(backoff_delay(&policy, 1), Duration::from_secs(2));
        assert_eq!(backoff_delay(&policy, 2), Duration::from_secs(4));
        assert_eq!(backoff_delay(&policy, 20), Duration::from_secs(30));
    }
}
