//! A passive AI chat bot on the Logos chat network.
//!
//! The bot holds direct (1:1) conversations and answers each message with a
//! free online LLM, over libchat's opinionated [`logos_chat`] facade. The
//! logic is split so the event loop can be exercised over an in-process
//! transport in tests without opening a real Logos node:
//!
//! - [`config`]: the TOML file plus the two secrets from the environment.
//! - [`llm`]: the swappable LLM backend (OpenAI Chat Completions wire format).
//! - [`limits`]: per-conversation flood control and the global daily budget.
//! - [`bot`]: the event loop, per-conversation state, and the reply path.
#![forbid(unsafe_code)]

pub mod bot;
pub mod config;
pub mod limits;
pub mod llm;
