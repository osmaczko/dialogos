//! The LLM backend: one trait over the OpenAI Chat Completions wire format.
//!
//! Almost every provider the operator might switch to speaks
//! `POST {base_url}/chat/completions`, so the provider is a `(base_url, model,
//! api_key)` config value rather than a code path: swapping a free model for a
//! paid one, or one vendor for another, is an edit to the config file.

use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};

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

/// The one thing the bot needs from a model: system prompt + prior turns in,
/// assistant reply out. `Send + Sync` so a future thread pool can share it.
pub trait LlmBackend: Send + Sync {
    fn complete(&self, system: &str, history: &[Turn]) -> anyhow::Result<Completion>;
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
    fn complete(&self, system: &str, history: &[Turn]) -> anyhow::Result<Completion> {
        let request = ChatRequest {
            model: &self.model,
            messages: build_messages(system, history),
            max_tokens: self.max_tokens,
            temperature: self.temperature,
        };

        let response = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .context("sending the chat completion request")?;

        let status = response.status();
        // The body may echo the prompt or user content, so it is read for
        // parsing but never surfaced in an error (only the status is).
        let body = response
            .text()
            .context("reading the chat completion response body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "chat completion request failed with status {status}"
            ));
        }
        parse_response(&body)
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
}
