//! The event loop: drain libchat's event channel, and for each inbound message
//! rate-limit, call the LLM, and reply.
//!
//! Processing is sequential: one message is handled fully (admit, complete,
//! send) before the next. The rate limiter bounds load, so this is enough for
//! v1; a slow reply in one conversation delaying others is the one cost, and
//! the upgrade (a bounded pool preserving per-conversation order) is left as
//! future work.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::Receiver;
use logos_chat::{
    AccountDirectory, ChatClient, ChatStore, ConversationClass, Event, MessageSender,
    RegistrationService, Transport,
};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::limits::{Admission, ConvoLimiter, DailyBudget, admit};
use crate::llm::{LlmBackend, Turn};

/// The one client capability the bot needs: send bytes to a conversation. A
/// trait so the event loop can be driven by an in-process test transport
/// without opening a real Logos node.
pub trait SendMessage {
    fn send(&mut self, convo_id: &str, bytes: &[u8]) -> anyhow::Result<()>;
}

impl<T, R, S> SendMessage for ChatClient<T, R, S>
where
    T: Transport + Send + 'static,
    R: RegistrationService + AccountDirectory + Clone + Send + 'static,
    S: ChatStore + Send + 'static,
{
    fn send(&mut self, convo_id: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.send_message(convo_id, bytes)
            .map_err(|e| anyhow::anyhow!("send_message failed: {e}"))
    }
}

/// All mutable bot state: per-conversation history and flood control, plus the
/// global daily budget shared across conversations.
pub struct BotState {
    convos: HashMap<String, ConvoState>,
    budget: DailyBudget,
}

impl BotState {
    pub fn new() -> Self {
        Self {
            convos: HashMap::new(),
            budget: DailyBudget::default(),
        }
    }
}

impl Default for BotState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct ConvoState {
    greeted: bool,
    /// True for a conversation the bot is configured to stay out of (a group
    /// when `ignore_groups` is set). Recorded from `ConversationStarted`, whose
    /// `class` the message path can't see, so messages on it are dropped.
    ignored: bool,
    history: Vec<Turn>,
    limiter: ConvoLimiter,
}

/// Run the bot until the event channel closes. Owns `client` for the loop's
/// lifetime, so its `Drop` (which joins the transport worker) runs on return.
pub fn run(
    events: Receiver<Event>,
    mut client: impl SendMessage,
    backend: Arc<dyn LlmBackend>,
    cfg: &Config,
) {
    let mut state = BotState::new();
    while let Ok(event) = events.recv() {
        handle_event(event, &mut state, &mut client, backend.as_ref(), cfg);
    }
    info!("event channel closed; bot shutting down");
}

/// Handle one event. Public so the integration test can drive it over the
/// in-process transport without going through `run`.
pub fn handle_event(
    event: Event,
    state: &mut BotState,
    client: &mut impl SendMessage,
    backend: &dyn LlmBackend,
    cfg: &Config,
) {
    match event {
        Event::ConversationStarted { convo_id, class } => {
            // Record the class now: MessageReceived carries no class, so this is
            // the only chance to learn whether to engage with this conversation.
            let ignored = cfg.behavior.ignore_groups && class != ConversationClass::Private;
            let convo = convo_id.to_string();
            let first_time = {
                let convo_state = state.convos.entry(convo.clone()).or_default();
                convo_state.ignored = ignored;
                !std::mem::replace(&mut convo_state.greeted, true)
            };
            if ignored {
                debug!(%convo_id, "ignoring a non-private conversation");
                return;
            }
            if first_time {
                greet(client, &convo, cfg);
            }
        }
        Event::MessageReceived {
            convo_id,
            content,
            sender,
        } => {
            let Ok(text) = std::str::from_utf8(&content) else {
                warn!(%convo_id, "ignoring a non-UTF-8 message");
                return;
            };
            respond(
                state,
                client,
                backend,
                cfg,
                &convo_id.to_string(),
                text,
                &sender,
            );
        }
        Event::ConversationMembersChanged { .. } => {}
        Event::InboundError { message } => warn!(%message, "inbound error"),
        // `Event` is #[non_exhaustive]: ignore variants added by future libchat.
        _ => {}
    }
}

fn respond(
    state: &mut BotState,
    client: &mut impl SendMessage,
    backend: &dyn LlmBackend,
    cfg: &Config,
    convo: &str,
    text: &str,
    sender: &MessageSender,
) {
    // Drop messages on a conversation recorded as ignored (e.g. a group when
    // `ignore_groups` is set). ConversationStarted is delivered before a
    // conversation's first message, so the class is already known here.
    if state.convos.get(convo).is_some_and(|c| c.ignored) {
        return;
    }

    let now = Instant::now();
    let today = current_day();

    // Greet lazily if a message arrived before (or without) ConversationStarted.
    let first_time = {
        let convo_state = state.convos.entry(convo.to_string()).or_default();
        !std::mem::replace(&mut convo_state.greeted, true)
    };
    if first_time {
        greet(client, convo, cfg);
    }

    // Admit borrows the conversation's limiter and the global budget together;
    // they are disjoint fields of `state`.
    let admission = {
        let convo_state = state.convos.get_mut(convo).expect("just inserted above");
        admit(
            &mut convo_state.limiter,
            &mut state.budget,
            now,
            today,
            &cfg.limits,
        )
    };
    match admission {
        Admission::SilentlyDrop => return,
        Admission::Deny(reply) => {
            send(client, convo, reply.as_bytes());
            return;
        }
        Admission::Allow => {}
    }

    // Build the request context as history + this user turn, without yet
    // committing the user turn: a failed call must not leave an unanswered user
    // turn that breaks role alternation and re-inflates the next request.
    let mut context = state.convos.get(convo).expect("present").history.clone();
    context.push(Turn::user(text));

    let completion = match backend.complete(&cfg.behavior.system_prompt, &context) {
        Ok(completion) => completion,
        Err(err) => {
            // Stay silent so no failure detail leaks to the peer; log for the operator.
            warn!(%convo, error = %err, "LLM backend call failed; not replying");
            return;
        }
    };

    {
        let convo_state = state.convos.get_mut(convo).expect("present");
        convo_state.history.push(Turn::user(text));
        convo_state
            .history
            .push(Turn::assistant(completion.text.clone()));
        trim_history(&mut convo_state.history, cfg.behavior.history_turns);
    }
    state
        .budget
        .record(today, completion.tokens_used.unwrap_or(0) as u64);

    match &sender.account {
        Some(account) => debug!(%convo, account = %account.as_str(), "replied"),
        None => debug!(%convo, device = %sender.local_identity.as_str(), "replied"),
    }
    send(client, convo, completion.text.as_bytes());
}

/// Keep only the most recent `max_turns` exchanges (a user and an assistant
/// message each), dropping the oldest.
fn trim_history(history: &mut Vec<Turn>, max_turns: usize) {
    let cap = max_turns.saturating_mul(2).max(1);
    if history.len() > cap {
        history.drain(0..history.len() - cap);
    }
}

fn greet(client: &mut impl SendMessage, convo: &str, cfg: &Config) {
    if !cfg.behavior.greeting.is_empty() {
        send(client, convo, cfg.behavior.greeting.as_bytes());
    }
}

fn send(client: &mut impl SendMessage, convo: &str, bytes: &[u8]) {
    if let Err(err) = client.send(convo, bytes) {
        warn!(%convo, error = %err, "failed to send a message");
    }
}

/// Days since the Unix epoch in UTC: the daily budget's reset boundary.
fn current_day() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
}
