//! The event loop: drain libchat's event channel, and for each inbound message
//! rate-limit, ask the LLM, and reply.
//!
//! The loop is single-threaded and owns all state, so there are no locks. It
//! does not block on the LLM: an admitted message is dispatched as a `Job` to a
//! pool of worker threads, and their `Outcome`s come back on a channel that the
//! same loop selects over, so a slow reply in one conversation never stalls
//! another. Per conversation the order is preserved: only one call is in flight
//! at a time and further admitted messages wait in a small bounded queue.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, select};
use logos_chat::{
    AccountDirectory, ChatClient, ChatStore, ConversationClass, Event, MessageSender,
    RegistrationService, Transport,
};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::convos::{ConvoState, ConvoStore};
use crate::limits::{Admission, DailyBudget, admit};
use crate::llm::{Completion, LlmBackend, LlmError, RetryPolicy, Turn, complete_with_retry};

/// Per-conversation cap on messages queued behind an in-flight reply. The rate
/// limiter already bounds arrival, so this is only a backstop; the newest
/// message beyond it is dropped.
pub const PENDING_CAP: usize = 4;

/// How long, at most, to keep draining in-flight replies after a shutdown
/// signal before exiting.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

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

/// A unit of work for a worker: answer `context` (history plus the new user
/// turn) for `convo_id`.
pub struct Job {
    pub convo_id: String,
    pub context: Vec<Turn>,
}

/// A worker's result for one job.
pub struct Outcome {
    pub convo_id: String,
    pub result: Result<Completion, LlmError>,
}

/// All mutable bot state: the bounded per-conversation store and the global
/// daily budget shared across conversations.
pub struct BotState {
    convos: ConvoStore,
    budget: DailyBudget,
    /// The day a "provider omitted token usage" warning was last logged, so it
    /// is emitted at most once per UTC day rather than per message.
    missing_usage_warned_day: Option<u64>,
}

impl BotState {
    /// In-memory state with generous caps and a non-persisted budget, for tests.
    pub fn new() -> Self {
        Self {
            convos: ConvoStore::new(1_024, 65_536),
            budget: DailyBudget::default(),
            missing_usage_warned_day: None,
        }
    }

    fn from_config(cfg: &Config) -> Self {
        Self {
            convos: ConvoStore::new(
                cfg.limits.max_active_conversations,
                cfg.limits.max_known_conversations,
            ),
            budget: DailyBudget::load(cfg.identity.budget_path()),
            missing_usage_warned_day: None,
        }
    }
}

impl Default for BotState {
    fn default() -> Self {
        Self::new()
    }
}

/// Run the bot until the event channel closes or `shutdown` fires. Owns `client`
/// for the loop's lifetime, so its `Drop` (which joins the transport worker)
/// runs on return.
pub fn run(
    events: Receiver<Event>,
    mut client: impl SendMessage,
    backend: Arc<dyn LlmBackend>,
    cfg: &Config,
    shutdown: Receiver<()>,
) {
    let worker_count = cfg.llm.workers.max(1);
    let (job_tx, job_rx) = crossbeam_channel::unbounded::<Job>();
    let (out_tx, out_rx) = crossbeam_channel::unbounded::<Outcome>();
    let policy = RetryPolicy::default();

    for _ in 0..worker_count {
        let job_rx = job_rx.clone();
        let out_tx = out_tx.clone();
        let backend = Arc::clone(&backend);
        let system = cfg.behavior.system_prompt.clone();
        let policy = policy.clone();
        thread::spawn(move || worker_loop(job_rx, out_tx, backend, system, policy));
    }
    // Drop the loop's own handles so the worker clones are the only holders: the
    // job channel closing is what tells workers to exit at shutdown.
    drop(job_rx);
    drop(out_tx);

    let mut state = BotState::from_config(cfg);

    loop {
        select! {
            recv(events) -> msg => match msg {
                Ok(event) => {
                    let now = Instant::now();
                    let today = current_day();
                    if let Some(job) = handle_event(event, &mut state, &mut client, cfg, now, today) {
                        let _ = job_tx.send(job);
                    }
                }
                Err(_) => break, // transport closed
            },
            recv(out_rx) -> msg => {
                if let Ok(outcome) = msg {
                    let today = current_day();
                    if let Some(job) = handle_outcome(outcome, &mut state, &mut client, cfg, today, true) {
                        let _ = job_tx.send(job);
                    }
                }
            },
            recv(shutdown) -> _ => break,
        }
    }

    info!("shutting down; draining in-flight replies");
    drop(job_tx); // no new work; workers exit once their current call returns
    let deadline = Instant::now() + SHUTDOWN_DRAIN;
    while state.convos.in_flight_count() > 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match out_rx.recv_timeout(remaining) {
            // Do not dispatch further queued messages while shutting down.
            Ok(outcome) => {
                let today = current_day();
                handle_outcome(outcome, &mut state, &mut client, cfg, today, false);
            }
            Err(_) => break,
        }
    }
    state.budget.flush();
    // Workers may still be mid-call; the process is exiting, so we do not block
    // on joining them (a slow provider call would otherwise stall shutdown).
}

fn worker_loop(
    jobs: Receiver<Job>,
    outcomes: Sender<Outcome>,
    backend: Arc<dyn LlmBackend>,
    system: String,
    policy: RetryPolicy,
) {
    while let Ok(job) = jobs.recv() {
        let result = complete_with_retry(backend.as_ref(), &system, &job.context, &policy);
        if outcomes
            .send(Outcome {
                convo_id: job.convo_id,
                result,
            })
            .is_err()
        {
            break; // the main loop is gone
        }
    }
}

/// Handle one inbound event, returning a `Job` to dispatch if it produced work.
/// Public so tests can drive it directly with constructed events.
pub fn handle_event(
    event: Event,
    state: &mut BotState,
    client: &mut impl SendMessage,
    cfg: &Config,
    now: Instant,
    today: u64,
) -> Option<Job> {
    match event {
        Event::ConversationStarted { convo_id, class } => {
            // Record the class now: MessageReceived carries no class, so this is
            // the only chance to learn whether to engage with this conversation.
            let ignored = cfg.behavior.ignore_groups && class != ConversationClass::Private;
            let convo = convo_id.to_string();
            let first_time = {
                let convo_state = state.convos.activate(&convo, now);
                convo_state.ignored = ignored;
                !std::mem::replace(&mut convo_state.greeted, true)
            };
            if ignored {
                debug!(%convo_id, "ignoring a non-private conversation");
                return None;
            }
            if first_time {
                greet(client, &convo, cfg);
            }
            None
        }
        Event::MessageReceived {
            convo_id,
            content,
            sender,
        } => {
            let Ok(text) = std::str::from_utf8(&content) else {
                warn!(%convo_id, "ignoring a non-UTF-8 message");
                return None;
            };
            respond(
                state,
                client,
                cfg,
                &convo_id.to_string(),
                text,
                &sender,
                now,
                today,
            )
        }
        Event::ConversationMembersChanged { .. } => None,
        Event::InboundError { message } => {
            warn!(%message, "inbound error");
            None
        }
        // `Event` is #[non_exhaustive]: ignore variants added by future libchat.
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn respond(
    state: &mut BotState,
    client: &mut impl SendMessage,
    cfg: &Config,
    convo: &str,
    text: &str,
    sender: &MessageSender,
    now: Instant,
    today: u64,
) -> Option<Job> {
    // Engagement: a conversation known (in either tier) as ignored is dropped;
    // a genuinely unknown one is dropped only when ignoring groups, which is the
    // safe default (a legitimate 1:1 is known from its ConversationStarted).
    let ignored = match state.convos.classification(convo) {
        Some(ignored) => ignored,
        None => cfg.behavior.ignore_groups,
    };
    if ignored {
        return None;
    }

    // Greet lazily if a message arrived before (or without) ConversationStarted.
    let first_time = {
        let convo_state = state.convos.activate(convo, now);
        !std::mem::replace(&mut convo_state.greeted, true)
    };
    if first_time {
        greet(client, convo, cfg);
    }

    // Admit borrows the conversation's limiter and the global budget together;
    // they are disjoint fields of `state`.
    let admission = {
        let convo_state = state.convos.get_mut(convo).expect("just activated above");
        admit(
            &mut convo_state.limiter,
            &mut state.budget,
            now,
            today,
            &cfg.limits,
        )
    };
    match admission {
        Admission::SilentlyDrop => return None,
        Admission::Deny(reply) => {
            send(client, convo, reply.as_bytes());
            return None;
        }
        Admission::Allow => {}
    }

    match &sender.account {
        Some(account) => debug!(%convo, account = %account.as_str(), "admitted"),
        None => debug!(%convo, device = %sender.local_identity.as_str(), "admitted"),
    }

    // Dispatch immediately if idle, else queue behind the in-flight reply so
    // this conversation's replies stay ordered.
    let convo_state = state.convos.get_mut(convo).expect("present");
    if convo_state.in_flight.is_some() {
        if convo_state.pending.len() < PENDING_CAP {
            convo_state.pending.push_back(text.to_string());
        } else {
            debug!(%convo, "pending queue full; dropping the message");
        }
        None
    } else {
        Some(start_job(convo, convo_state, text.to_string()))
    }
}

/// Handle one worker outcome: commit the turn (on success) and reply, then, if
/// `dispatch_next`, return the conversation's next queued message as a `Job`.
/// Public so tests can drive the reply path directly.
pub fn handle_outcome(
    outcome: Outcome,
    state: &mut BotState,
    client: &mut impl SendMessage,
    cfg: &Config,
    today: u64,
    dispatch_next: bool,
) -> Option<Job> {
    let convo = outcome.convo_id;

    // Commit the completed turn (or log its failure) and clear the in-flight
    // mark. Scoped so the store borrow ends before the budget update below.
    {
        let Some(convo_state) = state.convos.get_mut(&convo) else {
            // Evicted between dispatch and completion; nothing to commit.
            return None;
        };
        let user_text = convo_state.in_flight.take();
        match &outcome.result {
            Ok(completion) => {
                if let Some(text) = user_text {
                    convo_state.history.push(Turn::user(text));
                    convo_state
                        .history
                        .push(Turn::assistant(completion.text.clone()));
                    trim_history(&mut convo_state.history, cfg.behavior.history_turns);
                }
            }
            Err(err) => warn!(%convo, error = %err, "LLM call failed; not replying"),
        }
    }

    if let Ok(completion) = &outcome.result {
        record_reply(state, today, completion.tokens_used);
        send(client, &convo, completion.text.as_bytes());
    }

    if !dispatch_next {
        return None;
    }
    let convo_state = state.convos.get_mut(&convo)?;
    let next = convo_state.pending.pop_front()?;
    Some(start_job(&convo, convo_state, next))
}

/// Mark the conversation in flight for `text` and build its job. The user turn
/// is committed only on success (in `handle_outcome`), so a failed call leaves
/// no unanswered turn to break role alternation.
fn start_job(convo: &str, convo_state: &mut ConvoState, text: String) -> Job {
    let mut context = convo_state.history.clone();
    context.push(Turn::user(text.clone()));
    convo_state.in_flight = Some(text);
    Job {
        convo_id: convo.to_string(),
        context,
    }
}

/// Record a produced reply against the daily budget, warning once per day if the
/// provider omitted usage (so only the message cap, not the token cap, applies).
fn record_reply(state: &mut BotState, today: u64, tokens: Option<u32>) {
    if tokens.is_none() && state.missing_usage_warned_day != Some(today) {
        state.missing_usage_warned_day = Some(today);
        warn!(
            "the LLM provider omitted token usage; the daily token cap is not being enforced (only the message cap)"
        );
    }
    state.budget.record(today, tokens.unwrap_or(0) as u64);
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
