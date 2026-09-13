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

/// Whether any age label a surface last painted would read differently now.
///
/// Each entry is `(origin_ms, label)`: the instant that label started counting
/// from, and the exact text that was painted for it.
///
/// The comparison is on the LABEL, never on the clock. [`format_age`] is a
/// step function, so a row that reads "3d" changes once a day where its age in
/// milliseconds changes every millisecond -- a repaint keyed on the age itself
/// would be a repaint every frame, and keyed on the label it is a repaint per
/// visible change and nothing else. That difference is the whole reason this
/// function exists rather than a `now - painted_at > 1s` test.
pub fn age_labels_changed(painted: &[(i64, String)], now_ms: i64) -> bool {
    painted
        .iter()
        .any(|(origin_ms, label)| format_age(now_ms.saturating_sub(*origin_ms)) != *label)
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

    /// Every key the clock is still holding. The caller diffs this against the
    /// keys it just observed and forgets the difference, so a purged or
    /// archived task cannot leave an entry behind forever. Exposing the keys
    /// rather than duplicating them in a second set keeps one source of truth
    /// for "which tasks does the clock know about".
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.entered.keys()
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

    /// The rule the idle repaint is keyed on: a changed LABEL, not a changed
    /// clock.
    ///
    /// A shell nobody is touching asks for no repaints at all (the perf lane
    /// measured 0 in 500 idle passes), so the ages simply stopped advancing.
    /// The remedy has to be exactly as cheap as the change it reports: a row
    /// reading "12s" earns a repaint a second later, and a row reading "3d"
    /// earns one a day later, not one a second later.
    #[test]
    fn only_a_changed_label_is_worth_a_repaint() {
        let young = vec![(0_i64, format_age(12_000))];
        assert!(!age_labels_changed(&young, 12_000), "the painted instant");
        assert!(!age_labels_changed(&young, 12_999), "still reads 12s");
        assert!(age_labels_changed(&young, 13_000), "now it reads 13s");

        // Three days old: a second later is not a change, a day later is. The
        // clock moved by 1,000 ms in the first case and the label did not.
        let old = vec![(0_i64, format_age(3 * 86_400_000))];
        assert!(!age_labels_changed(&old, 3 * 86_400_000 + 1_000));
        assert!(!age_labels_changed(&old, 3 * 86_400_000 + 3_600_000));
        assert!(age_labels_changed(&old, 4 * 86_400_000));

        // One changed label among many is enough, and none is not.
        let mixed = vec![
            (0_i64, format_age(3 * 86_400_000)),
            (0_i64, format_age(12_000)),
        ];
        assert!(age_labels_changed(&mixed, 13_000));

        assert!(
            !age_labels_changed(&[], 10_000_000),
            "a board with no rows asks for nothing"
        );
        // Clock skew backwards is a real label change -- "12s" would repaint
        // as "0s" -- and it is reported as one rather than hidden, because
        // `format_age` clamps the negative and the two texts genuinely differ.
        assert!(age_labels_changed(&young, -5_000));
    }

    /// The prune the shell runs every frame: whatever is still in the clock but
    /// was not observed this pass is a task that left the projection.
    #[test]
    fn keys_expose_exactly_what_forget_has_to_reach() {
        let mut clock = StateClock::new();
        clock.observe("a", BoardState::Working, 0);
        clock.observe("b", BoardState::Idle, 0);
        let mut keys: Vec<_> = clock.keys().copied().collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["a", "b"]);
        clock.forget(&"a");
        assert_eq!(clock.keys().copied().collect::<Vec<_>>(), vec!["b"]);
    }
}
