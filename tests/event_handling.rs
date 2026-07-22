//! Deterministic tests of the event-handling policy: greeting, replying, the
//! `ignore_groups` scope control, per-conversation ordering, and the pending
//! cap. These drive `bot::handle_event` / `bot::handle_outcome` directly with
//! constructed events and a recording client (no transport, no threads, no
//! timing) so they pin the decision logic exactly. A returned `Job` is
//! "executed" inline by a fixed backend, standing in for the worker pool.
//!
//! Like the other tests, compiling the crate needs the native `logos-chat`
//! build, so these run under the nix/CI build path (see the README).

use std::sync::Arc;
use std::time::{Duration, Instant};

use dialogos::bot::{
    BotState, Job, Outcome, PENDING_CAP, SendMessage, handle_event, handle_outcome,
};
use dialogos::config::{Behavior, Config, Identity, Llm};
use dialogos::limits::Limits;
use dialogos::llm::{Completion, LlmBackend, LlmError, Turn};

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
    fn complete(&self, _system: &str, _history: &[Turn]) -> Result<Completion, LlmError> {
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
            workers: 1,
        },
        limits: Limits {
            per_convo_window: Duration::from_secs(300),
            per_convo_max_messages: 100,
            daily_max_messages: 1_000,
            daily_max_tokens: 1_000_000,
            max_active_conversations: 1_024,
            max_known_conversations: 65_536,
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

/// Run a dispatched job (and any it chains) to completion with the fixed
/// backend, standing in for the worker pool.
fn settle(
    job: Option<Job>,
    state: &mut BotState,
    client: &mut Recorder,
    backend: &dyn LlmBackend,
    cfg: &Config,
) {
    let mut job = job;
    while let Some(j) = job.take() {
        let result = backend.complete(&cfg.behavior.system_prompt, &j.context);
        let outcome = Outcome {
            convo_id: j.convo_id,
            result,
        };
        job = handle_outcome(outcome, state, client, cfg, 0, true);
    }
}

/// Feed each event and settle any resulting reply immediately.
fn drive(events: Vec<Event>, cfg: &Config) -> Recorder {
    let mut state = BotState::new();
    let mut client = Recorder::default();
    let backend = FixedBackend;
    let now = Instant::now();
    for event in events {
        let job = handle_event(event, &mut state, &mut client, cfg, now, 0);
        settle(job, &mut state, &mut client, &backend, cfg);
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

#[test]
fn per_conversation_messages_are_answered_one_at_a_time_in_order() {
    let cfg = config(true);
    let mut state = BotState::new();
    let mut client = Recorder::default();
    let backend = FixedBackend;
    let now = Instant::now();

    handle_event(
        started("p", ConversationClass::Private),
        &mut state,
        &mut client,
        &cfg,
        now,
        0,
    );
    // First message dispatches a job; a second while it is in flight is queued.
    let job1 = handle_event(message("p", "m1"), &mut state, &mut client, &cfg, now, 0)
        .expect("first message dispatches");
    assert!(
        handle_event(message("p", "m2"), &mut state, &mut client, &cfg, now, 0).is_none(),
        "a second message while one is in flight must be queued, not dispatched"
    );

    // Completing m1 replies and dispatches m2, whose context carries m1's turns.
    let out1 = Outcome {
        convo_id: job1.convo_id,
        result: backend.complete("s", &job1.context),
    };
    let job2 =
        handle_outcome(out1, &mut state, &mut client, &cfg, 0, true).expect("m2 dispatches next");
    let ctx: Vec<&str> = job2.context.iter().map(|t| t.content.as_str()).collect();
    assert_eq!(ctx, ["m1", "reply", "m2"]);

    let out2 = Outcome {
        convo_id: job2.convo_id,
        result: backend.complete("s", &job2.context),
    };
    assert!(handle_outcome(out2, &mut state, &mut client, &cfg, 0, true).is_none());

    assert_eq!(client.texts(), ["hi", "reply", "reply"]);
}

#[test]
fn queued_messages_beyond_the_cap_are_dropped() {
    let cfg = config(true);
    let mut state = BotState::new();
    let mut client = Recorder::default();
    let backend = FixedBackend;
    let now = Instant::now();

    handle_event(
        started("p", ConversationClass::Private),
        &mut state,
        &mut client,
        &cfg,
        now,
        0,
    );
    let job1 = handle_event(message("p", "m1"), &mut state, &mut client, &cfg, now, 0)
        .expect("first message dispatches");
    // Enqueue more than the cap while m1 is in flight.
    for i in 0..(PENDING_CAP + 3) {
        handle_event(
            message("p", &format!("q{i}")),
            &mut state,
            &mut client,
            &cfg,
            now,
            0,
        );
    }

    // Drain everything the queue held.
    let mut completed = 0;
    let mut job = Some(job1);
    while let Some(j) = job.take() {
        let outcome = Outcome {
            convo_id: j.convo_id,
            result: backend.complete("s", &j.context),
        };
        completed += 1;
        job = handle_outcome(outcome, &mut state, &mut client, &cfg, 0, true);
    }
    // m1 plus exactly PENDING_CAP queued messages; the rest were dropped.
    assert_eq!(completed, 1 + PENDING_CAP);
}

#[test]
fn a_pending_reply_in_one_conversation_does_not_block_another() {
    let cfg = config(true);
    let mut state = BotState::new();
    let mut client = Recorder::default();
    let now = Instant::now();

    // "a" dispatches and is left in flight (never completed).
    handle_event(
        started("a", ConversationClass::Private),
        &mut state,
        &mut client,
        &cfg,
        now,
        0,
    );
    let job_a = handle_event(message("a", "qa"), &mut state, &mut client, &cfg, now, 0)
        .expect("a dispatches");

    // "b" still gets its own job while a's reply is outstanding.
    handle_event(
        started("b", ConversationClass::Private),
        &mut state,
        &mut client,
        &cfg,
        now,
        0,
    );
    let job_b = handle_event(message("b", "qb"), &mut state, &mut client, &cfg, now, 0)
        .expect("b dispatches while a is in flight");

    assert_ne!(job_a.convo_id, job_b.convo_id);
}
