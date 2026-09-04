//! Time-in-state for board rows. The kernel records when events occurred,
//! not when a task entered its current visible state, so the client keeps a
//! transient clock keyed by task. It is never persisted.

use std::collections::HashMap;
use std::hash::Hash;

use super::model::BoardState;

pub fn format_age(elapsed_ms: i64) -> String {
    let seconds = elapsed_ms.max(0) / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

#[derive(Debug, Default)]
pub struct StateClock<K: Hash + Eq> {
    entered: HashMap<K, (BoardState, i64)>,
}

impl<K: Hash + Eq> StateClock<K> {
    pub fn new() -> Self {
        Self {
            entered: HashMap::new(),
        }
    }

    /// Records the state seen now and returns how long it has been held.
    pub fn observe(&mut self, key: K, state: BoardState, now_ms: i64) -> i64 {
        match self.entered.get(&key) {
            Some((seen, since)) if *seen == state => (now_ms - since).max(0),
            _ => {
                self.entered.insert(key, (state, now_ms));
                0
            }
        }
    }

    pub fn forget(&mut self, key: &K) {
        self.entered.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::board::model::BoardState;

    #[test]
    fn age_uses_the_spec_units() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(12_000), "12s");
        assert_eq!(format_age(59_999), "59s");
        assert_eq!(format_age(60_000), "1m");
        assert_eq!(format_age(4 * 60_000 + 30_000), "4m");
        assert_eq!(format_age(2 * 3_600_000), "2h");
        assert_eq!(format_age(3 * 86_400_000 + 3_600_000), "3d");
        assert_eq!(format_age(-5_000), "0s", "clock skew never shows negative");
    }

    #[test]
    fn state_clock_counts_from_the_last_state_change() {
        let mut clock = StateClock::new();
        assert_eq!(clock.observe("a", BoardState::Working, 1_000), 0);
        assert_eq!(clock.observe("a", BoardState::Working, 5_000), 4_000);
        assert_eq!(
            clock.observe("a", BoardState::Question, 9_000),
            0,
            "state changed"
        );
        assert_eq!(clock.observe("a", BoardState::Question, 9_500), 500);
        clock.forget(&"a");
        assert_eq!(clock.observe("a", BoardState::Question, 20_000), 0);
    }
}
