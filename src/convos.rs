//! Bounded per-conversation state.
//!
//! Devnet identities are free to mint, so the set of conversations the bot has
//! seen is attacker-growable and cannot be kept in full forever. Two tiers bound
//! it:
//!
//! - the **active** tier holds full state (history, flood limiter, the in-flight
//!   reply and its queue) for the most recently active conversations, capped and
//!   evicted least-recently-active; an in-flight conversation is never evicted so
//!   a reply in progress always finds its state, and
//! - the **skeleton** tier holds just two bits (greeted, ignored) for demoted
//!   conversations, so a returning peer is neither greeted twice nor
//!   misclassified, capped and evicted oldest-first.
//!
//! A conversation absent from both tiers is genuinely unknown; the caller
//! decides how to treat it (see the event loop's handling of `ignore_groups`).

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use crate::limits::ConvoLimiter;
use crate::llm::Turn;

/// Full state for an actively-tracked conversation.
pub struct ConvoState {
    pub greeted: bool,
    /// True for a conversation the bot stays out of (a group under
    /// `ignore_groups`); messages on it are dropped.
    pub ignored: bool,
    pub history: Vec<Turn>,
    pub limiter: ConvoLimiter,
    /// Admitted messages that arrived while an LLM call for this conversation
    /// was in flight, kept so its replies stay in order.
    pub pending: VecDeque<String>,
    /// The user message currently being answered, or `None` when idle. Set on
    /// dispatch and cleared on completion.
    pub in_flight: Option<String>,
    pub last_active: Instant,
}

impl ConvoState {
    fn fresh(now: Instant) -> Self {
        Self {
            greeted: false,
            ignored: false,
            history: Vec::new(),
            limiter: ConvoLimiter::default(),
            pending: VecDeque::new(),
            in_flight: None,
            last_active: now,
        }
    }

    fn from_skeleton(skeleton: Skeleton, now: Instant) -> Self {
        Self {
            greeted: skeleton.greeted,
            ignored: skeleton.ignored,
            ..Self::fresh(now)
        }
    }

    /// Evictable when idle: no reply in flight and nothing queued, so demoting
    /// it loses no in-progress work.
    fn is_evictable(&self) -> bool {
        self.in_flight.is_none() && self.pending.is_empty()
    }
}

/// The two bits kept for a demoted conversation.
#[derive(Clone, Copy)]
struct Skeleton {
    greeted: bool,
    ignored: bool,
}

/// The active tier plus the skeleton tier, each independently capped.
pub struct ConvoStore {
    active: HashMap<String, ConvoState>,
    /// id -> (skeleton, insertion sequence); the sequence gives oldest-first
    /// eviction without an auxiliary order structure.
    known: HashMap<String, (Skeleton, u64)>,
    next_seq: u64,
    max_active: usize,
    max_known: usize,
    demotions: u64,
}

impl ConvoStore {
    pub fn new(max_active: usize, max_known: usize) -> Self {
        Self {
            active: HashMap::new(),
            known: HashMap::new(),
            next_seq: 0,
            max_active,
            max_known,
            demotions: 0,
        }
    }

    /// Total conversations demoted from the active tier since start (for stats).
    pub fn demotion_count(&self) -> u64 {
        self.demotions
    }

    pub fn get(&self, id: &str) -> Option<&ConvoState> {
        self.active.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut ConvoState> {
        self.active.get_mut(id)
    }

    pub fn active_len(&self) -> usize {
        self.active.len()
    }

    /// How many active conversations have a reply in flight. Used during
    /// shutdown to know when the drain is complete.
    pub fn in_flight_count(&self) -> usize {
        self.active
            .values()
            .filter(|state| state.in_flight.is_some())
            .count()
    }

    /// Whether a conversation is known to be ignored: `Some(ignored)` if it is
    /// in either tier, `None` if it has never been seen (or has aged out of
    /// both).
    pub fn classification(&self, id: &str) -> Option<bool> {
        if let Some(state) = self.active.get(id) {
            return Some(state.ignored);
        }
        self.known.get(id).map(|(skeleton, _)| skeleton.ignored)
    }

    /// Ensure `id` is in the active tier (promoting it from the skeleton tier,
    /// carrying its bits, or creating fresh state), mark it most-recently-active,
    /// and return its state. Evicts a least-recently-active idle conversation
    /// first if the active tier is full.
    pub fn activate(&mut self, id: &str, now: Instant) -> &mut ConvoState {
        if !self.active.contains_key(id) {
            self.make_room();
            let state = match self.known.remove(id) {
                Some((skeleton, _)) => ConvoState::from_skeleton(skeleton, now),
                None => ConvoState::fresh(now),
            };
            self.active.insert(id.to_string(), state);
        }
        let state = self.active.get_mut(id).expect("just ensured present");
        state.last_active = now;
        state
    }

    /// Demote the least-recently-active idle conversation if the active tier is
    /// at capacity. If every active conversation is busy (in flight or with a
    /// queue), nothing is evicted and the tier is briefly over its cap rather
    /// than dropping in-progress work.
    fn make_room(&mut self) {
        if self.active.len() < self.max_active {
            return;
        }
        let victim = self
            .active
            .iter()
            .filter(|(_, state)| state.is_evictable())
            .min_by_key(|(_, state)| state.last_active)
            .map(|(id, _)| id.clone());
        if let Some(id) = victim {
            let state = self.active.remove(&id).expect("victim just selected");
            self.demote(
                id,
                Skeleton {
                    greeted: state.greeted,
                    ignored: state.ignored,
                },
            );
        }
    }

    fn demote(&mut self, id: String, skeleton: Skeleton) {
        self.demotions += 1;
        let seq = self.next_seq;
        self.next_seq += 1;
        self.known.insert(id, (skeleton, seq));
        if self.known.len() > self.max_known
            && let Some(oldest) = self
                .known
                .iter()
                .min_by_key(|(_, (_, seq))| *seq)
                .map(|(id, _)| id.clone())
        {
            self.known.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn store() -> ConvoStore {
        ConvoStore::new(3, 4)
    }

    #[test]
    fn activate_creates_fresh_state_and_marks_it_active() {
        let mut s = store();
        let now = Instant::now();
        let state = s.activate("a", now);
        assert!(!state.greeted);
        assert!(!state.ignored);
        assert_eq!(state.last_active, now);
        assert_eq!(s.classification("a"), Some(false));
        assert_eq!(s.classification("never-seen"), None);
    }

    #[test]
    fn full_active_tier_demotes_the_least_recently_active() {
        let mut s = store();
        let t0 = Instant::now();
        // Fill the active tier (cap 3), oldest to newest: a, b, c.
        s.activate("a", t0);
        s.activate("b", t0 + Duration::from_secs(1));
        s.activate("c", t0 + Duration::from_secs(2));
        // A fourth activation demotes the least-recently-active, "a".
        s.activate("d", t0 + Duration::from_secs(3));

        assert_eq!(s.active_len(), 3);
        assert!(s.get("a").is_none(), "a should have been demoted");
        assert!(s.get("d").is_some());
        // Demoted, not forgotten: still classifiable via the skeleton tier.
        assert_eq!(s.classification("a"), Some(false));
    }

    #[test]
    fn an_in_flight_conversation_is_never_demoted() {
        let mut s = store();
        let t0 = Instant::now();
        // "a" is the oldest but has a reply in flight.
        s.activate("a", t0).in_flight = Some("q".to_string());
        s.activate("b", t0 + Duration::from_secs(1));
        s.activate("c", t0 + Duration::from_secs(2));
        // The next activation must skip in-flight "a" and demote "b" instead.
        s.activate("d", t0 + Duration::from_secs(3));

        assert!(s.get("a").is_some(), "in-flight a must be kept");
        assert!(s.get("b").is_none(), "idle, oldest-evictable b should go");
    }

    #[test]
    fn promotion_restores_the_greeted_and_ignored_bits() {
        let mut s = store();
        let t0 = Instant::now();
        {
            let a = s.activate("a", t0);
            a.greeted = true;
            a.ignored = true;
        }
        s.activate("b", t0 + Duration::from_secs(1));
        s.activate("c", t0 + Duration::from_secs(2));
        s.activate("d", t0 + Duration::from_secs(3)); // demotes "a"
        assert!(s.get("a").is_none());

        // Re-activating "a" promotes it from the skeleton with its bits intact.
        let a = s.activate("a", t0 + Duration::from_secs(4));
        assert!(a.greeted);
        assert!(a.ignored);
    }

    #[test]
    fn skeleton_tier_evicts_oldest_when_full() {
        // Active cap 1 so every new conversation demotes the previous one.
        let mut s = ConvoStore::new(1, 2);
        let t0 = Instant::now();
        for (i, id) in ["a", "b", "c", "d"].iter().enumerate() {
            s.activate(id, t0 + Duration::from_secs(i as u64));
        }
        // Skeleton cap 2: the two most recently demoted (b, c) survive; a, the
        // oldest, was evicted. d is still active.
        assert_eq!(s.classification("a"), None, "oldest skeleton evicted");
        assert_eq!(s.classification("b"), Some(false));
        assert_eq!(s.classification("c"), Some(false));
        assert!(s.get("d").is_some());
    }
}
