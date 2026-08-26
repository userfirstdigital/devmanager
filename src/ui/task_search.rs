//! Pure native inline task-search projection.
//!
//! Filters over stable task identity using the same caseless title bound as
//! [`crate::ui::task_cockpit::InboxFilter`]. Never mutates `ClientModel`.

use crate::client::model::{normalize_bounded_search_text, MAX_INDEXED_TITLE_CHARS};
use crate::domain::id::TaskId;
use crate::ui::task_cockpit::inbox::{InboxFilter, MAX_SEARCH_CHARS};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSearchCandidate {
    pub task_id: TaskId,
    pub title: String,
    pub project_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSearchState {
    query: String,
    open: bool,
    selected_index: usize,
    focused: bool,
}

impl Default for TaskSearchState {
    fn default() -> Self {
        Self {
            query: String::new(),
            open: false,
            selected_index: 0,
            focused: false,
        }
    }
}

impl TaskSearchState {
    pub fn open(&self) -> bool {
        self.open
    }

    pub fn focused(&self) -> bool {
        self.focused
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    pub fn filter(&self) -> InboxFilter {
        InboxFilter::new(&self.query)
    }

    pub fn open_picker(&mut self) {
        self.open = true;
        self.focused = true;
        self.selected_index = 0;
    }

    pub fn close(&mut self) {
        *self = Self::default();
    }

    pub fn clear_query(&mut self) {
        self.query.clear();
        self.selected_index = 0;
    }

    pub fn set_query(&mut self, query: impl AsRef<str>) {
        self.query = normalize_bounded_search_text(query.as_ref(), MAX_SEARCH_CHARS).0;
        self.selected_index = 0;
        self.open = true;
        self.focused = true;
    }

    pub fn filtered<'a>(
        &self,
        candidates: &'a [TaskSearchCandidate],
    ) -> Vec<&'a TaskSearchCandidate> {
        let filter = self.filter();
        candidates
            .iter()
            .filter(|candidate| filter.matches_title(&candidate.title))
            .collect()
    }

    pub fn move_selection(&mut self, delta: isize, result_count: usize) {
        if result_count == 0 {
            self.selected_index = 0;
            return;
        }
        let count = result_count as isize;
        let current = self.selected_index.min(result_count.saturating_sub(1)) as isize;
        self.selected_index = ((current + delta).rem_euclid(count)) as usize;
    }

    pub fn select_index(&mut self, index: usize, result_count: usize) -> Option<usize> {
        if result_count == 0 || index >= result_count {
            return None;
        }
        self.selected_index = index;
        Some(index)
    }

    pub fn selected_task<'a>(
        &self,
        candidates: &'a [TaskSearchCandidate],
    ) -> Option<&'a TaskSearchCandidate> {
        let filtered = self.filtered(candidates);
        filtered
            .get(self.selected_index.min(filtered.len().saturating_sub(1)))
            .copied()
    }

    pub fn confirm_selection<'a>(
        &mut self,
        candidates: &'a [TaskSearchCandidate],
    ) -> Option<TaskId> {
        let task_id = self.selected_task(candidates)?.task_id;
        self.close();
        Some(task_id)
    }
}

/// Local extension so search can reuse InboxFilter without mutating ClientModel.
pub trait InboxFilterTitleMatch {
    fn matches_title(&self, title: &str) -> bool;
}

impl InboxFilterTitleMatch for InboxFilter {
    fn matches_title(&self, title: &str) -> bool {
        let query = self.query().trim();
        if query.is_empty() {
            return true;
        }
        let title = normalize_bounded_search_text(title, MAX_INDEXED_TITLE_CHARS).0;
        title.contains(query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(title: &str) -> TaskSearchCandidate {
        TaskSearchCandidate {
            task_id: TaskId::new(),
            title: title.to_string(),
            project_label: "Demo".to_string(),
        }
    }

    #[test]
    fn filters_case_insensitively_without_mutating_candidates() {
        let mut state = TaskSearchState::default();
        state.open_picker();
        let first = candidate("Fix Auth Flow");
        let second = candidate("Docs Pass");
        let candidates = vec![first.clone(), second.clone()];
        state.set_query("auth");
        let filtered = state.filtered(&candidates);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].task_id, first.task_id);
        assert_eq!(candidates[0].title, "Fix Auth Flow");
    }

    #[test]
    fn arrow_keys_wrap_and_enter_selects_stable_identity() {
        let mut state = TaskSearchState::default();
        state.open_picker();
        let a = candidate("Alpha");
        let b = candidate("Beta");
        let c = candidate("Gamma");
        let candidates = vec![a.clone(), b.clone(), c.clone()];
        state.move_selection(1, 3);
        assert_eq!(state.selected_index(), 1);
        state.move_selection(1, 3);
        assert_eq!(state.selected_index(), 2);
        state.move_selection(1, 3);
        assert_eq!(state.selected_index(), 0);
        state.move_selection(-1, 3);
        assert_eq!(state.selected_index(), 2);
        assert_eq!(state.confirm_selection(&candidates), Some(c.task_id));
        assert!(!state.open());
    }

    #[test]
    fn escape_clear_and_empty_results_are_deterministic() {
        let mut state = TaskSearchState::default();
        state.open_picker();
        state.set_query("zzz");
        let candidates = vec![candidate("Alpha")];
        assert!(state.filtered(&candidates).is_empty());
        assert!(state.confirm_selection(&candidates).is_none());
        state.clear_query();
        assert_eq!(state.query(), "");
        state.close();
        assert!(!state.open());
        assert!(!state.focused());
    }

    #[test]
    fn mouse_select_index_clamps_to_filtered_results() {
        let mut state = TaskSearchState::default();
        state.open_picker();
        let candidates = vec![candidate("One"), candidate("Two")];
        assert_eq!(state.select_index(1, 2), Some(1));
        assert_eq!(state.select_index(5, 2), None);
        assert_eq!(
            state
                .selected_task(&candidates)
                .map(|row| row.title.as_str()),
            Some("Two")
        );
    }
}
