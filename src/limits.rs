//! Bot-side rate limiting. Two layers:
//!
//! - a per-conversation sliding window (anti-flood / fairness), and
//! - a global daily budget of replies and tokens (the real spend cap).
//!
//! All of it is best-effort: conversation state is in-memory and devnet
//! identities are free to mint, so a determined sybil evades the per-conversation
//! window. The daily budget is what actually bounds spend.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// Rate-limit thresholds, resolved from config.
#[derive(Debug, Clone)]
pub struct Limits {
    pub per_convo_window: Duration,
    pub per_convo_max_messages: usize,
    pub daily_max_messages: u32,
    pub daily_max_tokens: u64,
    /// Cap on conversations kept with full state (history, limiter). Bounds
    /// memory against a flood of throwaway devnet identities.
    pub max_active_conversations: usize,
    /// Cap on demoted conversations kept as a two-bit skeleton (greeted,
    /// ignored), so a returning peer is neither re-greeted nor misclassified.
    pub max_known_conversations: usize,
}

/// Per-conversation flood control: a sliding window of recent inbound times
/// plus a mute deadline set when the window trips, so the peer is told once and
/// then met with silence for the rest of the window.
#[derive(Debug, Default)]
pub struct ConvoLimiter {
    window: VecDeque<Instant>,
    muted_until: Option<Instant>,
}

/// Global spend cap, reset at each UTC-midnight day boundary. `day` is days
/// since the Unix epoch; a change rolls the counters back to zero.
///
/// When constructed with [`DailyBudget::load`] the counters are persisted to a
/// sidecar file on every change, so a restart (or crash loop) resumes today's
/// spend instead of resetting the cap. A budget built with `default()` (used in
/// tests) has no path and never persists.
#[derive(Debug, Default)]
pub struct DailyBudget {
    day: u64,
    messages: u32,
    tokens: u64,
    path: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BudgetSnapshot {
    day: u64,
    messages: u32,
    tokens: u64,
}

impl DailyBudget {
    /// Restore counters from `path` (best-effort: a missing, unreadable, or
    /// unparsable file yields a zeroed budget), and persist future changes to it.
    pub fn load(path: PathBuf) -> Self {
        let mut budget = Self {
            path: Some(path.clone()),
            ..Default::default()
        };
        match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<BudgetSnapshot>(&raw) {
                Ok(snapshot) => {
                    budget.day = snapshot.day;
                    budget.messages = snapshot.messages;
                    budget.tokens = snapshot.tokens;
                }
                Err(err) => warn!(?path, error = %err, "ignoring an unparsable budget file"),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => warn!(?path, error = %err, "could not read the budget file"),
        }
        budget
    }

    fn rollover(&mut self, today: u64) {
        if self.day != today {
            self.day = today;
            self.messages = 0;
            self.tokens = 0;
            self.persist();
        }
    }

    /// Record a produced reply and its token usage against today's budget.
    pub fn record(&mut self, today: u64, tokens: u64) {
        self.rollover(today);
        self.messages = self.messages.saturating_add(1);
        self.tokens = self.tokens.saturating_add(tokens);
        self.persist();
    }

    fn exhausted(&self, limits: &Limits) -> bool {
        self.messages >= limits.daily_max_messages || self.tokens >= limits.daily_max_tokens
    }

    /// Today's spend (messages, tokens) for stats; zero once the day has rolled.
    pub fn spent(&self, today: u64) -> (u32, u64) {
        if self.day == today {
            (self.messages, self.tokens)
        } else {
            (0, 0)
        }
    }

    /// Force a final write, e.g. on shutdown. Records already write through, so
    /// this is belt-and-suspenders against a future non-persisting mutation.
    pub fn flush(&self) {
        self.persist();
    }

    /// Write the counters to the sidecar file via a temp-then-rename, so a
    /// crash mid-write cannot leave a torn file. A write failure is logged, not
    /// fatal: the cap keeps working in memory.
    fn persist(&self) {
        let Some(path) = &self.path else { return };
        let snapshot = BudgetSnapshot {
            day: self.day,
            messages: self.messages,
            tokens: self.tokens,
        };
        let Ok(json) = serde_json::to_string(&snapshot) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(err) = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, path)) {
            warn!(?path, error = %err, "could not persist the budget");
        }
    }
}

/// The decision for one inbound message.
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// Answer it.
    Allow,
    /// Send this one canned reply, then go quiet for the window.
    Deny(String),
    /// Already told the peer once this window; send nothing.
    SilentlyDrop,
}

/// Decide whether to answer an inbound message arriving at `now` on `today`.
///
/// Mutates the per-conversation window and mute deadline and rolls the daily
/// budget over to `today`. It does not count the reply: the caller records that
/// on a successful send via [`DailyBudget::record`], so denied and failed
/// messages never consume budget.
pub fn admit(
    convo: &mut ConvoLimiter,
    budget: &mut DailyBudget,
    now: Instant,
    today: u64,
    limits: &Limits,
) -> Admission {
    budget.rollover(today);

    if let Some(until) = convo.muted_until {
        if now < until {
            return Admission::SilentlyDrop;
        }
        convo.muted_until = None;
    }

    if budget.exhausted(limits) {
        // Tell the peer once, then go silent for the window like a flood trip,
        // so an exhausted budget does not turn every inbound message into a
        // canned reply.
        convo.muted_until = Some(now + limits.per_convo_window);
        return Admission::Deny("Daily limit reached. Please try again tomorrow.".to_string());
    }

    // Drop timestamps older than the window. `checked_sub` guards the case
    // where the process has been up for less than one window: then every
    // recorded timestamp is within it and none are dropped.
    if let Some(cutoff) = now.checked_sub(limits.per_convo_window) {
        while convo.window.front().is_some_and(|t| *t <= cutoff) {
            convo.window.pop_front();
        }
    }

    if convo.window.len() >= limits.per_convo_max_messages {
        convo.muted_until = Some(now + limits.per_convo_window);
        let minutes = limits.per_convo_window.as_secs() / 60 + 1;
        return Admission::Deny(format!(
            "You're sending messages too quickly. Please try again in about {minutes} minute(s)."
        ));
    }

    convo.window.push_back(now);
    Admission::Allow
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            per_convo_window: Duration::from_secs(60),
            per_convo_max_messages: 3,
            daily_max_messages: 5,
            daily_max_tokens: 1_000,
            max_active_conversations: 1_024,
            max_known_conversations: 65_536,
        }
    }

    #[test]
    fn allows_up_to_the_window_max_then_denies_and_mutes() {
        let l = limits();
        let mut convo = ConvoLimiter::default();
        let mut budget = DailyBudget::default();
        let t0 = Instant::now();

        for i in 0..l.per_convo_max_messages {
            let now = t0 + Duration::from_secs(i as u64);
            assert_eq!(admit(&mut convo, &mut budget, now, 0, &l), Admission::Allow);
        }

        // The next one within the window trips: one canned reply...
        let tripped = admit(&mut convo, &mut budget, t0 + Duration::from_secs(3), 0, &l);
        assert!(matches!(tripped, Admission::Deny(_)));

        // ...then silence for the rest of the window.
        assert_eq!(
            admit(&mut convo, &mut budget, t0 + Duration::from_secs(4), 0, &l),
            Admission::SilentlyDrop
        );
    }

    #[test]
    fn allows_again_after_the_window_slides_past() {
        let l = limits();
        let mut convo = ConvoLimiter::default();
        let mut budget = DailyBudget::default();
        let t0 = Instant::now();

        for i in 0..l.per_convo_max_messages {
            let _ = admit(
                &mut convo,
                &mut budget,
                t0 + Duration::from_secs(i as u64),
                0,
                &l,
            );
        }
        // Trip, which mutes until t0 + 3s + window.
        assert!(matches!(
            admit(&mut convo, &mut budget, t0 + Duration::from_secs(3), 0, &l),
            Admission::Deny(_)
        ));

        // Well past the mute and the window: the old timestamps have aged out.
        let later = t0 + Duration::from_secs(200);
        assert_eq!(
            admit(&mut convo, &mut budget, later, 0, &l),
            Admission::Allow
        );
    }

    #[test]
    fn daily_message_cap_denies_further_replies() {
        let l = limits();
        let mut budget = DailyBudget::default();

        // Simulate replies until the daily message cap is reached.
        for _ in 0..l.daily_max_messages {
            budget.record(0, 1);
        }
        // A fresh conversation (so the window is not the limiter) is told once...
        let mut fresh = ConvoLimiter::default();
        let t0 = Instant::now();
        assert!(matches!(
            admit(&mut fresh, &mut budget, t0, 0, &l),
            Admission::Deny(_)
        ));
        // ...then goes silent for the window rather than replying every time.
        assert_eq!(
            admit(&mut fresh, &mut budget, t0 + Duration::from_secs(1), 0, &l),
            Admission::SilentlyDrop
        );
    }

    #[test]
    fn daily_token_cap_denies_further_replies() {
        let l = limits();
        let mut budget = DailyBudget::default();
        budget.record(0, l.daily_max_tokens);
        let mut fresh = ConvoLimiter::default();
        assert!(matches!(
            admit(&mut fresh, &mut budget, Instant::now(), 0, &l),
            Admission::Deny(_)
        ));
    }

    fn temp_budget_path() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "dialogos-budget-test-{}-{n}.json",
            std::process::id()
        ))
    }

    #[test]
    fn budget_persists_and_reloads_todays_counters() {
        let path = temp_budget_path();
        let _ = std::fs::remove_file(&path);

        let mut budget = DailyBudget::load(path.clone());
        budget.record(20_000, 100);
        budget.record(20_000, 50);

        // A restart on the same day resumes the accumulated spend.
        let reloaded = DailyBudget::load(path.clone());
        assert_eq!(reloaded.messages, 2);
        assert_eq!(reloaded.tokens, 150);
        assert_eq!(reloaded.day, 20_000);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_corrupt_budget_file_is_tolerated() {
        let path = temp_budget_path();
        std::fs::write(&path, "not json").unwrap();

        let budget = DailyBudget::load(path.clone());
        assert_eq!(budget.messages, 0);
        assert_eq!(budget.tokens, 0);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn budget_rolls_over_on_a_new_day() {
        let l = limits();
        let mut budget = DailyBudget::default();
        for _ in 0..l.daily_max_messages {
            budget.record(0, 1);
        }
        assert!(budget.exhausted(&l));

        // A new day resets the counters, so the next message is allowed.
        let mut fresh = ConvoLimiter::default();
        assert_eq!(
            admit(&mut fresh, &mut budget, Instant::now(), 1, &l),
            Admission::Allow
        );
        assert!(!budget.exhausted(&l));
    }
}
