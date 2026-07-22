//! Configuration: a TOML file for everything non-secret, the two secrets from
//! the environment, and the system prompt from its own file.
//!
//! Secrets (`DIALOGOS_DB_KEY`, `LLM_API_KEY`) are read from the environment and
//! never from the file, so the file is safe to commit and share. Loading fails
//! fast with a clear message if a secret or a referenced file is missing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::limits::Limits;

/// Environment variable holding the encrypted-store key.
pub const DB_KEY_ENV: &str = "DIALOGOS_DB_KEY";
/// Environment variable holding the LLM provider API key.
pub const LLM_API_KEY_ENV: &str = "LLM_API_KEY";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("reading system prompt {path}: {source}")]
    SystemPrompt {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "missing required environment variable {0} (it holds a secret; set it in the environment, never in the config file)"
    )]
    MissingSecret(&'static str),
}

/// The fully-resolved runtime configuration.
pub struct Config {
    pub identity: Identity,
    pub llm: Llm,
    pub limits: Limits,
    pub behavior: Behavior,
}

pub struct Identity {
    pub db_path: String,
    pub db_key: String,
    pub registry_url: Option<String>,
}

impl Identity {
    /// Sidecar path for the persisted daily budget, next to the on-disk store.
    /// Keyed to the (stable) store path so the spend cap survives a restart even
    /// though the identity behind the store does not.
    pub fn budget_path(&self) -> PathBuf {
        PathBuf::from(format!("{}.budget.json", self.db_path))
    }
}

pub struct Llm {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub max_output_tokens: u32,
    pub temperature: f32,
    pub request_timeout: Duration,
    /// Worker threads making concurrent LLM calls, so a slow reply in one
    /// conversation does not stall the others.
    pub workers: usize,
}

pub struct Behavior {
    pub greeting: String,
    pub system_prompt: String,
    pub history_turns: usize,
    pub ignore_groups: bool,
}

impl Config {
    /// Load from `path`, merge the environment secrets, and read the system
    /// prompt (its path is resolved relative to the config file's directory).
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let file: FileConfig = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

        let db_key = require_secret(DB_KEY_ENV)?;
        let api_key = require_secret(LLM_API_KEY_ENV)?;

        let prompt_path = resolve_relative(path, &file.llm.system_prompt_path);
        let system_prompt = std::fs::read_to_string(&prompt_path)
            .map_err(|source| ConfigError::SystemPrompt {
                path: prompt_path.clone(),
                source,
            })?
            .trim()
            .to_string();

        Ok(Config {
            identity: Identity {
                db_path: file.identity.db_path,
                db_key,
                registry_url: file.identity.registry_url,
            },
            llm: Llm {
                base_url: file.llm.base_url,
                model: file.llm.model,
                api_key,
                max_output_tokens: file.llm.max_output_tokens,
                temperature: file.llm.temperature,
                request_timeout: Duration::from_secs(file.llm.request_timeout_secs),
                workers: file.llm.workers,
            },
            limits: Limits {
                per_convo_window: Duration::from_secs(file.limits.per_convo_window_secs),
                per_convo_max_messages: file.limits.per_convo_max_messages,
                daily_max_messages: file.limits.global_daily_max_messages,
                daily_max_tokens: file.limits.global_daily_max_tokens,
                max_active_conversations: file.limits.max_active_conversations,
                max_known_conversations: file.limits.max_known_conversations,
            },
            behavior: Behavior {
                greeting: file.behavior.greeting,
                system_prompt,
                history_turns: file.behavior.history_turns,
                ignore_groups: file.behavior.ignore_groups,
            },
        })
    }
}

fn require_secret(var: &'static str) -> Result<String, ConfigError> {
    match std::env::var(var) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => Err(ConfigError::MissingSecret(var)),
    }
}

/// Resolve `target` relative to the config file's directory, unless it is
/// already absolute.
fn resolve_relative(config_path: &Path, target: &str) -> PathBuf {
    let target = Path::new(target);
    if target.is_absolute() {
        return target.to_path_buf();
    }
    match config_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(target),
        _ => target.to_path_buf(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    identity: FileIdentity,
    llm: FileLlm,
    #[serde(default)]
    limits: FileLimits,
    #[serde(default)]
    behavior: FileBehavior,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileIdentity {
    db_path: String,
    #[serde(default)]
    registry_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileLlm {
    base_url: String,
    model: String,
    system_prompt_path: String,
    #[serde(default = "default_max_output_tokens")]
    max_output_tokens: u32,
    #[serde(default = "default_temperature")]
    temperature: f32,
    #[serde(default = "default_request_timeout_secs")]
    request_timeout_secs: u64,
    #[serde(default = "default_workers")]
    workers: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileLimits {
    per_convo_max_messages: usize,
    per_convo_window_secs: u64,
    global_daily_max_messages: u32,
    global_daily_max_tokens: u64,
    #[serde(default = "default_max_active_conversations")]
    max_active_conversations: usize,
    #[serde(default = "default_max_known_conversations")]
    max_known_conversations: usize,
}

impl Default for FileLimits {
    fn default() -> Self {
        Self {
            per_convo_max_messages: 10,
            per_convo_window_secs: 300,
            global_daily_max_messages: 2_000,
            global_daily_max_tokens: 500_000,
            max_active_conversations: default_max_active_conversations(),
            max_known_conversations: default_max_known_conversations(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileBehavior {
    #[serde(default = "default_greeting")]
    greeting: String,
    #[serde(default = "default_history_turns")]
    history_turns: usize,
    #[serde(default = "default_ignore_groups")]
    ignore_groups: bool,
}

impl Default for FileBehavior {
    fn default() -> Self {
        Self {
            greeting: default_greeting(),
            history_turns: default_history_turns(),
            ignore_groups: default_ignore_groups(),
        }
    }
}

fn default_max_output_tokens() -> u32 {
    512
}
fn default_temperature() -> f32 {
    0.7
}
fn default_request_timeout_secs() -> u64 {
    60
}
fn default_workers() -> usize {
    4
}
fn default_max_active_conversations() -> usize {
    1_024
}
fn default_max_known_conversations() -> usize {
    65_536
}
fn default_greeting() -> String {
    "Hi, I'm an AI assistant. Ask me anything.".to_string()
}
fn default_history_turns() -> usize {
    8
}
fn default_ignore_groups() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_file_with_defaulted_sections() {
        let toml = r#"
            [identity]
            db_path = "./dialogos.db"

            [llm]
            base_url = "https://openrouter.ai/api/v1"
            model = "some/model:free"
            system_prompt_path = "./system-prompt.txt"
        "#;
        let file: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(file.llm.max_output_tokens, 512);
        assert_eq!(file.llm.workers, 4);
        assert_eq!(file.limits.per_convo_max_messages, 10);
        assert_eq!(file.limits.max_active_conversations, 1_024);
        assert_eq!(file.limits.max_known_conversations, 65_536);
        assert!(file.behavior.ignore_groups);
        assert_eq!(file.identity.registry_url, None);
    }

    #[test]
    fn rejects_unknown_keys() {
        let toml = r#"
            [identity]
            db_path = "./dialogos.db"
            typo_key = 1

            [llm]
            base_url = "x"
            model = "y"
            system_prompt_path = "z"
        "#;
        assert!(toml::from_str::<FileConfig>(toml).is_err());
    }

    #[test]
    fn resolves_prompt_path_relative_to_config_dir() {
        let resolved =
            resolve_relative(Path::new("/etc/dialogos/config.toml"), "system-prompt.txt");
        assert_eq!(resolved, PathBuf::from("/etc/dialogos/system-prompt.txt"));

        let absolute = resolve_relative(Path::new("/etc/dialogos/config.toml"), "/opt/prompt.txt");
        assert_eq!(absolute, PathBuf::from("/opt/prompt.txt"));
    }
}
