//! Tests of the worker-pool loop `bot::run` itself: that it replies and shuts
//! down cleanly, and that a blocked reply in one conversation does not stall
//! another (the reason the pool exists). Events are fed over a plain channel and
//! replies captured by a shared, `Send` recorder, so no transport and no
//! `ChatClient: Send` assumption is involved; only `run`'s own threading is.
//!
//! Compiling the crate needs the native `logos-chat` build, so these run under
//! the nix/CI build path (see the README).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded, unbounded};

use dialogos::bot::{SendMessage, run};
use dialogos::config::{Behavior, Config, Identity, Llm};
use dialogos::limits::Limits;
use dialogos::llm::{Completion, LlmBackend, LlmError, Turn};

use libchat::{ConversationClass, IdentId};
use logos_chat::{Event, MessageSender};

/// A `Send` client that records sent payloads behind a shared lock, so the test
/// thread can observe what `run` (on another thread) sent.
#[derive(Clone, Default)]
struct SharedRecorder {
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl SendMessage for SharedRecorder {
    fn send(&mut self, _convo_id: &str, bytes: &[u8]) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(bytes.to_vec());
        Ok(())
    }
}

impl SharedRecorder {
    fn contains(&self, needle: &[u8]) -> bool {
        self.sent.lock().unwrap().iter().any(|m| m == needle)
    }
}

/// Replies immediately, echoing the last user turn, unless that turn is "slow",
/// in which case it blocks until the gate is released.
struct GatedBackend {
    gate: Receiver<()>,
}

impl LlmBackend for GatedBackend {
    fn complete(&self, _system: &str, history: &[Turn]) -> Result<Completion, LlmError> {
        let last = history.last().map(|t| t.content.as_str()).unwrap_or("");
        if last == "slow" {
            let _ = self.gate.recv();
        }
        Ok(Completion {
            text: format!("re:{last}"),
            tokens_used: Some(1),
        })
    }
}

fn unique() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn config(workers: usize) -> Config {
    let db = std::env::temp_dir().join(format!(
        "dialogos-worker-pool-{}-{}.db",
        std::process::id(),
        unique()
    ));
    Config {
        identity: Identity {
            db_path: db.to_string_lossy().into_owned(),
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
            workers,
        },
        limits: Limits {
            per_convo_window: Duration::from_secs(300),
            per_convo_max_messages: 100,
            daily_max_messages: 10_000,
            daily_max_tokens: 10_000_000,
            max_active_conversations: 1_024,
            max_known_conversations: 65_536,
        },
        behavior: Behavior {
            greeting: "hi".to_string(),
            system_prompt: "s".to_string(),
            history_turns: 4,
            ignore_groups: true,
        },
    }
}

fn started(convo_id: &str) -> Event {
    Event::ConversationStarted {
        convo_id: Arc::from(convo_id),
        class: ConversationClass::Private,
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

fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    cond()
}

#[test]
fn run_replies_then_shuts_down_cleanly() {
    let (_gate_tx, gate_rx) = bounded::<()>(1);
    let backend: Arc<dyn LlmBackend> = Arc::new(GatedBackend { gate: gate_rx });
    let recorder = SharedRecorder::default();
    let sink = recorder.clone();
    let (ev_tx, ev_rx) = unbounded::<Event>();
    let (sd_tx, sd_rx) = bounded::<()>(1);

    let bot = thread::spawn(move || {
        let cfg = config(2);
        run(ev_rx, sink, backend, &cfg, sd_rx, || {});
    });

    ev_tx.send(started("p")).unwrap();
    ev_tx.send(message("p", "hi there")).unwrap();

    assert!(
        wait_until(|| recorder.contains(b"re:hi there"), Duration::from_secs(5)),
        "the reply should have been sent"
    );
    assert!(
        recorder.contains(b"hi"),
        "the greeting should have been sent"
    );

    sd_tx.send(()).unwrap();
    bot.join().unwrap();
}

#[test]
fn a_blocked_conversation_does_not_stall_another() {
    let (gate_tx, gate_rx) = bounded::<()>(1);
    let backend: Arc<dyn LlmBackend> = Arc::new(GatedBackend { gate: gate_rx });
    let recorder = SharedRecorder::default();
    let sink = recorder.clone();
    let (ev_tx, ev_rx) = unbounded::<Event>();
    let (sd_tx, sd_rx) = bounded::<()>(1);

    // Two workers: one can be stuck on "slow" while the other answers "fast".
    let bot = thread::spawn(move || {
        let cfg = config(2);
        run(ev_rx, sink, backend, &cfg, sd_rx, || {});
    });

    // The slow conversation occupies a worker (it blocks on the gate).
    ev_tx.send(started("slow-convo")).unwrap();
    ev_tx.send(message("slow-convo", "slow")).unwrap();
    // The fast conversation must still get answered.
    ev_tx.send(started("fast-convo")).unwrap();
    ev_tx.send(message("fast-convo", "fast")).unwrap();

    assert!(
        wait_until(|| recorder.contains(b"re:fast"), Duration::from_secs(5)),
        "the fast reply must arrive while the slow one is blocked"
    );
    assert!(
        !recorder.contains(b"re:slow"),
        "the slow reply must still be blocked on the gate"
    );

    // Releasing the gate lets the slow reply complete.
    gate_tx.send(()).unwrap();
    assert!(
        wait_until(|| recorder.contains(b"re:slow"), Duration::from_secs(5)),
        "the slow reply should arrive once unblocked"
    );

    sd_tx.send(()).unwrap();
    bot.join().unwrap();
}
