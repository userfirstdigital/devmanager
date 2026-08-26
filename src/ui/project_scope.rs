//! Project-scope preference for the native task rail.
//!
//! Scope is a client-local view preference validated against the current
//! config projection before it can filter the inbox.

use crate::domain::id::ProjectId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectScope {
    All,
    Project(ProjectId),
}

impl Default for ProjectScope {
    fn default() -> Self {
        Self::All
    }
}

impl ProjectScope {
    pub fn label<'a, F>(&'a self, resolve: F) -> String
    where
        F: FnOnce(ProjectId) -> Option<&'a str>,
    {
        match self {
            Self::All => "All projects".to_string(),
            Self::Project(project_id) => resolve(*project_id)
                .map(str::to_string)
                .unwrap_or_else(|| "Unknown project".to_string()),
        }
    }

    /// Drop a stale project id that is no longer present in config.
    pub fn validated(self, configured: &[ProjectId]) -> Self {
        match self {
            Self::All => Self::All,
            Self::Project(project_id) => {
                if configured.iter().any(|id| *id == project_id) {
                    Self::Project(project_id)
                } else {
                    Self::All
                }
            }
        }
    }

    pub fn includes(self, project_id: ProjectId) -> bool {
        match self {
            Self::All => true,
            Self::Project(selected) => selected == project_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectScopeMenuState {
    open: bool,
    scope: ProjectScope,
    selected_index: usize,
}

impl Default for ProjectScopeMenuState {
    fn default() -> Self {
        Self {
            open: false,
            scope: ProjectScope::All,
            selected_index: 0,
        }
    }
}

impl ProjectScopeMenuState {
    pub fn open(&self) -> bool {
        self.open
    }

    pub fn scope(&self) -> ProjectScope {
        self.scope
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    pub fn set_scope(&mut self, scope: ProjectScope, configured: &[ProjectId]) {
        self.scope = scope.validated(configured);
        self.selected_index = match self.scope {
            ProjectScope::All => 0,
            ProjectScope::Project(project_id) => configured
                .iter()
                .position(|id| *id == project_id)
                .map(|index| index + 1)
                .unwrap_or(0),
        };
    }

    pub fn toggle_menu(&mut self) {
        self.open = !self.open;
    }

    pub fn close_menu(&mut self) {
        self.open = false;
    }

    pub fn option_count(configured_len: usize) -> usize {
        configured_len.saturating_add(1)
    }

    pub fn move_selection(&mut self, delta: isize, configured_len: usize) {
        let count = Self::option_count(configured_len) as isize;
        if count == 0 {
            self.selected_index = 0;
            return;
        }
        let current = self.selected_index.min(count as usize - 1) as isize;
        self.selected_index = ((current + delta).rem_euclid(count)) as usize;
    }

    pub fn confirm_selection(&mut self, configured: &[ProjectId]) -> ProjectScope {
        let scope = if self.selected_index == 0 {
            ProjectScope::All
        } else {
            configured
                .get(self.selected_index - 1)
                .copied()
                .map(ProjectScope::Project)
                .unwrap_or(ProjectScope::All)
        };
        self.scope = scope.validated(configured);
        self.open = false;
        self.scope
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_project_scope_falls_back_to_all() {
        let live = ProjectId::new();
        let stale = ProjectId::new();
        assert_eq!(
            ProjectScope::Project(stale).validated(&[live]),
            ProjectScope::All
        );
        assert_eq!(
            ProjectScope::Project(live).validated(&[live]),
            ProjectScope::Project(live)
        );
    }

    #[test]
    fn menu_keyboard_selects_all_and_project_options() {
        let mut menu = ProjectScopeMenuState::default();
        let a = ProjectId::new();
        let b = ProjectId::new();
        let configured = [a, b];
        menu.toggle_menu();
        menu.move_selection(1, configured.len());
        assert_eq!(
            menu.confirm_selection(&configured),
            ProjectScope::Project(a)
        );
        menu.toggle_menu();
        menu.move_selection(-1, configured.len());
        assert_eq!(menu.confirm_selection(&configured), ProjectScope::All);
        assert!(!menu.open());
    }

    #[test]
    fn scope_filters_project_membership() {
        let project = ProjectId::new();
        assert!(ProjectScope::All.includes(project));
        assert!(ProjectScope::Project(project).includes(project));
        assert!(!ProjectScope::Project(ProjectId::new()).includes(project));
    }
}
