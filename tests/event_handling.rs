//! Deterministic tests of the event-handling policy: greeting, replying, and
//! the `ignore_groups` scope control. These drive `bot::handle_event` directly
//! with constructed events and a recording client (no transport, no timing)
//! so they pin the decision logic exactly.
//!
//! Like the other tests, compiling the crate needs the native `logos-chat`
//! build, so these run under the nix/CI build path (see the README).

use std::sync::Arc;
use std::time::Duration;

use dialogos::bot::{BotState, SendMessage, handle_event};
use dialogos::config::{Behavior, Config, Identity, Llm};
use dialogos::limits::Limits;
use dialogos::llm::{Completion, LlmBackend, Turn};

use libchat::{ConversationClass, IdentId};
use logos_chat::{Event, MessageSender};

/// Records everything the bot sends, so a test can assert on it.
#[derive(Default)]
struct Recorder {
    sent: Vec<Vec<u8>>,
}

impl SendMessage for Recorder {
    fn send(&mut self, _convo_id: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.sent.push(bytes.to_vec());
        Ok(())
    }
}

impl Recorder {
    fn texts(&self) -> Vec<&str> {
        self.sent
            .iter()
            .map(|b| std::str::from_utf8(b).unwrap())
            .collect()
    }
}

struct FixedBackend;

impl LlmBackend for FixedBackend {
    fn complete(&self, _system: &str, _history: &[Turn]) -> anyhow::Result<Completion> {
        Ok(Completion {
            text: "reply".to_string(),
            tokens_used: Some(1),
        })
    }
}

fn config(ignore_groups: bool) -> Config {
    Config {
        identity: Identity {
            db_path: String::new(),
            db_key: String::new(),
            registry_url: None,
        },
        llm: Llm {
            base_url: String::new(),
            model: String::new(),
            api_key: String::new(),
            max_output_tokens: 128,
            temperature: 0.0,
            request_timeout: Duration::from_secs(1),
        },
        limits: Limits {
            per_convo_window: Duration::from_secs(300),
            per_convo_max_messages: 10,
            daily_max_messages: 1_000,
            daily_max_tokens: 1_000_000,
        },
        behavior: Behavior {
            greeting: "hi".to_string(),
            system_prompt: "test".to_string(),
            history_turns: 4,
            ignore_groups,
        },
    }
}

fn started(convo_id: &str, class: ConversationClass) -> Event {
    Event::ConversationStarted {
        convo_id: Arc::from(convo_id),
        class,
    }
}

fn message(convo_id: &str, text: &str) -> Event {
    Event::MessageReceived {
        convo_id: Arc::from(convo_id),
        content: text.as_bytes().to_vec(),
        sender: MessageSender {
            account: None,
            local_identity: IdentId::new("device"),
        },
    }
}

fn drive(events: Vec<Event>, cfg: &Config) -> Recorder {
    let mut state = BotState::new();
    let mut client = Recorder::default();
    let backend = FixedBackend;
    for event in events {
        handle_event(event, &mut state, &mut client, &backend, cfg);
    }
    client
}

#[test]
fn greets_and_replies_in_a_private_conversation() {
    let client = drive(
        vec![
            started("p1", ConversationClass::Private),
            message("p1", "hello"),
        ],
        &config(true),
    );
    assert_eq!(client.texts(), ["hi", "reply"]);
}

#[test]
fn ignore_groups_suppresses_both_the_group_greeting_and_the_reply() {
    let client = drive(
        vec![
            started("g1", ConversationClass::Group),
            message("g1", "hello"),
        ],
        &config(true),
    );
    assert!(
        client.sent.is_empty(),
        "bot must neither greet nor reply in an ignored group"
    );
}

#[test]
fn groups_are_answered_when_ignore_groups_is_disabled() {
    let client = drive(
        vec![
            started("g1", ConversationClass::Group),
            message("g1", "hello"),
        ],
        &config(false),
    );
    assert_eq!(client.texts(), ["hi", "reply"]);
}
