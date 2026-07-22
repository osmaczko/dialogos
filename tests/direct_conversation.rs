//! End-to-end over the in-process transport: a peer opens a direct conversation
//! with the bot and exchanges a message, and the real event-handling code path
//! (`bot::handle_event`) greets and replies. The LLM backend is a deterministic
//! fake and a returned `Job` is settled inline (standing in for the worker
//! pool), so the assertions are stable and no network is touched.
//!
//! This mirrors libchat's own `crates/generic-chat/tests/saro_and_raya.rs`. It
//! needs the native `liblogosdelivery` to compile the crate (a transitive
//! `logos-chat` build dependency) even though the test itself uses only the
//! pure-Rust `InProcessDelivery`, so it runs under the nix/CI build path, not a
//! plain `cargo test` on a box without the native library. See the README.
//!
//! The worker-pool loop (`bot::run`) itself is exercised in `worker_pool.rs`.

use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use dialogos::bot::{BotState, Outcome, SendMessage, handle_event, handle_outcome};
use dialogos::config::{Behavior, Config, Identity, Llm};
use dialogos::limits::Limits;
use dialogos::llm::{Completion, LlmBackend, LlmError, Turn};

use logos_account::TestLogosAccount;
use logos_chat::{
    ChatClientBuilder, DelegateSigner, EphemeralRegistry, Event, InProcessDelivery, MessageBus,
};

struct FakeBackend {
    reply: String,
}

impl LlmBackend for FakeBackend {
    fn complete(&self, _system: &str, _history: &[Turn]) -> Result<Completion, LlmError> {
        Ok(Completion {
            text: self.reply.clone(),
            tokens_used: Some(7),
        })
    }
}

fn test_config() -> Config {
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
            workers: 1,
        },
        limits: Limits {
            per_convo_window: Duration::from_secs(300),
            per_convo_max_messages: 10,
            daily_max_messages: 1_000,
            daily_max_tokens: 1_000_000,
            max_active_conversations: 1_024,
            max_known_conversations: 65_536,
        },
        behavior: Behavior {
            greeting: "hello from the bot".to_string(),
            system_prompt: "you are a test bot".to_string(),
            history_turns: 4,
            ignore_groups: true,
        },
    }
}

/// Block for the next MessageReceived on `events`, skipping other event kinds.
fn recv_message(events: &Receiver<Event>, timeout: Duration) -> Vec<u8> {
    loop {
        match events.recv_timeout(timeout) {
            Ok(Event::MessageReceived { content, .. }) => return content,
            Ok(_) => continue,
            Err(e) => panic!("timed out waiting for a message: {e:?}"),
        }
    }
}

#[test]
fn bot_greets_and_replies_over_in_process_transport() {
    let bus = MessageBus::default();
    let mut reg = EphemeralRegistry::new();

    // The bot's account and client.
    let bot_account = TestLogosAccount::new();
    let bot_delegate = DelegateSigner::random();
    bot_account
        .add_delegate_signer(&mut reg, bot_delegate.public_key())
        .unwrap();
    let (mut bot_client, bot_events) = ChatClientBuilder::new(bot_account.address())
        .ident(bot_delegate)
        .transport(InProcessDelivery::new(bus.clone()))
        .registration(reg.clone())
        .build()
        .unwrap();

    // A peer that opens the conversation and talks to the bot.
    let peer_account = TestLogosAccount::new();
    let peer_delegate = DelegateSigner::random();
    peer_account
        .add_delegate_signer(&mut reg, peer_delegate.public_key())
        .unwrap();
    let (mut peer, peer_events) = ChatClientBuilder::new(peer_account.address())
        .ident(peer_delegate)
        .transport(InProcessDelivery::new(bus.clone()))
        .registration(reg.clone())
        .build()
        .unwrap();

    let cfg = test_config();
    let backend = FakeBackend {
        reply: "the answer is 42".to_string(),
    };
    let mut state = BotState::new();

    // Peer opens a direct conversation with the bot by its address.
    let peer_convo = peer.create_direct_conversation(bot_client.addr()).unwrap();

    // Drive the bot's ConversationStarted -> it greets.
    drain_bot_events(&bot_events, &mut state, &mut bot_client, &backend, &cfg);
    assert_eq!(
        recv_message(&peer_events, Duration::from_secs(5)),
        b"hello from the bot"
    );

    // Peer asks a question; drive the bot's MessageReceived -> it replies.
    peer.send_message(&peer_convo, b"what is the answer?")
        .unwrap();
    drain_bot_events(&bot_events, &mut state, &mut bot_client, &backend, &cfg);
    assert_eq!(
        recv_message(&peer_events, Duration::from_secs(5)),
        b"the answer is 42"
    );
}

/// Process every currently-pending bot event, settling any dispatched reply
/// inline with the fake backend.
fn drain_bot_events(
    events: &Receiver<Event>,
    state: &mut BotState,
    client: &mut impl SendMessage,
    backend: &dyn LlmBackend,
    cfg: &Config,
) {
    let mut timeout = Duration::from_secs(5);
    while let Ok(event) = events.recv_timeout(timeout) {
        let now = Instant::now();
        let today = 0;
        let mut job = handle_event(event, state, client, cfg, now, today);
        while let Some(j) = job.take() {
            let result = backend.complete(&cfg.behavior.system_prompt, &j.context);
            let outcome = Outcome {
                convo_id: j.convo_id,
                result,
            };
            job = handle_outcome(outcome, state, client, cfg, today, true);
        }
        timeout = Duration::from_millis(300);
    }
}
