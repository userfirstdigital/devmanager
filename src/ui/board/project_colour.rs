//! One muted hue per project, assigned at first sight in creation order and
//! persisted in the workspace layout store. Hues are dim and cool so amber and
//! red stay the only saturated colours on screen (spec 5.3).

use std::collections::BTreeMap;

use crate::domain::id::ProjectId;
use crate::ui::tokens::Color;

/// The palette itself lives in the canonical token module: it is the only file
/// allowed to write a colour literal. Re-exported so every existing
/// `board::PROJECT_PALETTE` import keeps resolving.
pub use crate::ui::tokens::PROJECT_PALETTE;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectColourBook {
    assignments: BTreeMap<String, u8>,
    next: u8,
}

impl ProjectColourBook {
    /// Restore from the layout store. An index storage cannot honour is a
    /// hand-edited or forward-version file, so it fails closed to slot zero
    /// rather than panicking on a palette lookup.
    pub fn from_persisted(map: &BTreeMap<String, u8>) -> Self {
        let assignments: BTreeMap<String, u8> = map
            .iter()
            .map(|(key, index)| (key.clone(), clamp_slot(*index)))
            .collect();
        let next = (assignments.len() % PROJECT_PALETTE.len()) as u8;
        Self { assignments, next }
    }

    pub fn to_persisted(&self) -> BTreeMap<String, u8> {
        self.assignments.clone()
    }

    /// Whether the persisted map already matches what this book holds.
    /// The caller asks on every paint and almost always gets `true`, so it
    /// must not have to clone the map to find out.
    pub fn matches_persisted(&self, stored: &BTreeMap<String, u8>) -> bool {
        self.assignments == *stored
    }

    pub fn colour_index_if_known(&self, project_id: ProjectId) -> Option<u8> {
        self.assignments.get(&project_id.to_string()).copied()
    }

    /// The project's palette slot, assigning the next one at first sight.
    pub fn colour_index(&mut self, project_id: ProjectId) -> u8 {
        let key = project_id.to_string();
        if let Some(index) = self.assignments.get(&key) {
            return *index;
        }
        let index = self.next;
        self.next = (self.next + 1) % PROJECT_PALETTE.len() as u8;
        self.assignments.insert(key, index);
        index
    }

    /// Override one project's slot with the user's choice.
    pub fn set(&mut self, project_id: ProjectId, index: u8) {
        self.assignments
            .insert(project_id.to_string(), clamp_slot(index));
    }

    pub fn colour(&self, index: u8) -> Color {
        PROJECT_PALETTE[(index as usize) % PROJECT_PALETTE.len()]
    }
}

/// Fail an unusable palette slot closed to the first hue.
fn clamp_slot(index: u8) -> u8 {
    if (index as usize) < PROJECT_PALETTE.len() {
        index
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::ProjectId;

    #[test]
    fn first_sight_assigns_the_next_palette_slot_and_wraps() {
        let mut book = ProjectColourBook::default();
        let ids: Vec<_> = (0..9).map(|_| ProjectId::new()).collect();
        let indices: Vec<_> = ids.iter().map(|id| book.colour_index(*id)).collect();
        assert_eq!(indices, vec![0, 1, 2, 3, 4, 5, 6, 7, 0]);
        assert_eq!(book.colour_index(ids[3]), 3, "stable on re-ask");
    }

    #[test]
    fn matches_persisted_agrees_with_a_cloned_comparison() {
        let mut book = ProjectColourBook::default();
        let id = ProjectId::new();
        book.colour_index(id);
        let stored = book.to_persisted();
        assert!(book.matches_persisted(&stored));
        book.colour_index(ProjectId::new());
        assert!(!book.matches_persisted(&stored));
        assert_eq!(
            book.matches_persisted(&stored),
            book.to_persisted() == stored,
            "the cheap check and the cloning one must never disagree"
        );
    }

    #[test]
    fn set_overrides_and_persistence_round_trips() {
        let mut book = ProjectColourBook::default();
        let id = ProjectId::new();
        book.colour_index(id);
        book.set(id, 6);
        let restored = ProjectColourBook::from_persisted(&book.to_persisted());
        assert_eq!(restored.colour_index_if_known(id), Some(6));
    }

    #[test]
    fn out_of_range_persisted_indices_fail_closed_to_slot_zero() {
        let mut map = std::collections::BTreeMap::new();
        map.insert(ProjectId::new().to_string(), 200u8);
        let book = ProjectColourBook::from_persisted(&map);
        assert!(book.to_persisted().values().all(|v| *v < 8));
    }
}
