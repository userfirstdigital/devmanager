//! Compact project-action (command) primary + editor workflow.
//!
//! Mutations travel only through typed TaskCockpit config command queries.
//! Run uses ServiceControl with current fences. The UI never writes
//! `config.json` and never invents shell execution.

use crate::domain::id::ProjectId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectActionEditorField {
    Name,
    Command,
}

impl ProjectActionEditorField {
    pub fn toggle(self) -> Self {
        match self {
            Self::Name => Self::Command,
            Self::Command => Self::Name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectActionRow {
    pub project_config_id: String,
    pub folder_id: String,
    pub command_id: String,
    pub label: String,
}

impl ProjectActionRow {
    /// Configured commands and supervised services share the command id as
    /// their canonical service id. Keep that conversion typed and fallible so
    /// malformed config never becomes an unscoped shell launch.
    pub fn service_id(&self) -> Result<crate::services::model::ServiceId, String> {
        crate::services::model::ServiceId::new(self.command_id.clone())
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectActionDraft {
    pub project_config_id: String,
    pub folder_id: String,
    pub command_id: Option<String>,
    pub label: String,
    pub command: String,
    pub focused_field: ProjectActionEditorField,
    pub error: Option<String>,
}

impl ProjectActionDraft {
    pub fn new_for_project(
        project_config_id: impl Into<String>,
        folder_id: impl Into<String>,
    ) -> Self {
        Self {
            project_config_id: project_config_id.into(),
            folder_id: folder_id.into(),
            command_id: None,
            label: String::new(),
            command: String::new(),
            focused_field: ProjectActionEditorField::Name,
            error: None,
        }
    }

    pub fn from_row(row: &ProjectActionRow, command: impl Into<String>) -> Self {
        Self {
            project_config_id: row.project_config_id.clone(),
            folder_id: row.folder_id.clone(),
            command_id: Some(row.command_id.clone()),
            label: row.label.clone(),
            command: command.into(),
            focused_field: ProjectActionEditorField::Name,
            error: None,
        }
    }

    pub fn focus_field(&mut self, field: ProjectActionEditorField) {
        self.focused_field = field;
        self.error = None;
    }

    pub fn push_char(&mut self, ch: char) {
        match self.focused_field {
            ProjectActionEditorField::Name => self.label.push(ch),
            ProjectActionEditorField::Command => self.command.push(ch),
        }
        self.error = None;
    }

    pub fn backspace(&mut self) {
        match self.focused_field {
            ProjectActionEditorField::Name => {
                self.label.pop();
            }
            ProjectActionEditorField::Command => {
                self.command.pop();
            }
        }
        self.error = None;
    }

    pub fn delete_forward_noop(&mut self) {
        // Single-line editor keeps caret at end; Delete mirrors Backspace honesty.
        self.backspace();
    }

    pub fn validate(&mut self) -> bool {
        let label = self.label.trim();
        let command = self.command.trim();
        if label.is_empty() {
            self.error = Some("Action name is required.".to_string());
            return false;
        }
        if command.is_empty() {
            self.error = Some("Command is required.".to_string());
            return false;
        }
        if self.project_config_id.trim().is_empty() || self.folder_id.trim().is_empty() {
            self.error = Some("Project folder is unavailable.".to_string());
            return false;
        }
        self.label = label.to_string();
        self.command = command.to_string();
        self.error = None;
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectActionMenuMode {
    Closed,
    Menu { selected_index: usize },
    Editor(ProjectActionDraft),
    ArchiveConfirm { row: ProjectActionRow },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectActionWorkflow {
    pub project_id: Option<ProjectId>,
    pub mode: ProjectActionMenuMode,
    pub last_error: Option<String>,
    pub last_result: Option<String>,
    pub pending_detail_command_id: Option<String>,
}

impl Default for ProjectActionWorkflow {
    fn default() -> Self {
        Self {
            project_id: None,
            mode: ProjectActionMenuMode::Closed,
            last_error: None,
            last_result: None,
            pending_detail_command_id: None,
        }
    }
}

impl ProjectActionWorkflow {
    pub fn open_menu(&mut self, project_id: ProjectId) {
        self.project_id = Some(project_id);
        self.mode = ProjectActionMenuMode::Menu { selected_index: 0 };
        self.last_error = None;
        self.last_result = None;
        self.pending_detail_command_id = None;
    }

    pub fn close(&mut self) {
        self.mode = ProjectActionMenuMode::Closed;
        self.pending_detail_command_id = None;
    }

    pub fn begin_add(&mut self, project_config_id: String, folder_id: String) {
        self.mode = ProjectActionMenuMode::Editor(ProjectActionDraft::new_for_project(
            project_config_id,
            folder_id,
        ));
        self.pending_detail_command_id = None;
    }

    pub fn begin_edit_loading(&mut self, row: &ProjectActionRow) {
        self.pending_detail_command_id = Some(row.command_id.clone());
        self.last_error = None;
    }

    pub fn apply_command_detail(&mut self, row: &ProjectActionRow, command: String) {
        if self.pending_detail_command_id.as_deref() != Some(row.command_id.as_str()) {
            return;
        }
        self.pending_detail_command_id = None;
        self.mode = ProjectActionMenuMode::Editor(ProjectActionDraft::from_row(row, command));
    }

    pub fn begin_archive_confirm(&mut self, row: ProjectActionRow) {
        self.mode = ProjectActionMenuMode::ArchiveConfirm { row };
    }

    pub fn move_menu_selection(&mut self, delta: isize, row_count: usize) {
        let ProjectActionMenuMode::Menu { selected_index } = &mut self.mode else {
            return;
        };
        // +1 option for "Add action…"
        let count = row_count.saturating_add(1) as isize;
        if count == 0 {
            *selected_index = 0;
            return;
        }
        let current = (*selected_index).min(count as usize - 1) as isize;
        *selected_index = ((current + delta).rem_euclid(count)) as usize;
    }

    pub fn cancel_editor(&mut self) {
        if matches!(
            self.mode,
            ProjectActionMenuMode::Editor(_) | ProjectActionMenuMode::ArchiveConfirm { .. }
        ) {
            self.mode = ProjectActionMenuMode::Menu { selected_index: 0 };
            self.pending_detail_command_id = None;
        }
    }

    pub fn take_validated_draft(&mut self) -> Option<ProjectActionDraft> {
        let ProjectActionMenuMode::Editor(draft) = &mut self.mode else {
            return None;
        };
        if !draft.validate() {
            return None;
        }
        Some(draft.clone())
    }

    pub fn take_archive_row(&mut self) -> Option<ProjectActionRow> {
        match std::mem::replace(
            &mut self.mode,
            ProjectActionMenuMode::Menu { selected_index: 0 },
        ) {
            ProjectActionMenuMode::ArchiveConfirm { row } => Some(row),
            other => {
                self.mode = other;
                None
            }
        }
    }

    pub fn surface_error(&mut self, message: impl Into<String>) {
        let message = message.into();
        if let ProjectActionMenuMode::Editor(draft) = &mut self.mode {
            draft.error = Some(message.clone());
        }
        self.last_error = Some(message);
    }

    pub fn surface_result(&mut self, message: impl Into<String>) {
        self.last_result = Some(message.into());
        self.last_error = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_routes_keys_to_focused_field_with_tab_and_backspace() {
        let mut draft = ProjectActionDraft::new_for_project("p", "f");
        draft.push_char('A');
        draft.push_char('p');
        draft.focus_field(ProjectActionEditorField::Command);
        draft.push_char('c');
        draft.push_char('m');
        draft.backspace();
        assert_eq!(draft.label, "Ap");
        assert_eq!(draft.command, "c");
        draft.focus_field(draft.focused_field.toggle());
        assert_eq!(draft.focused_field, ProjectActionEditorField::Name);
    }

    #[test]
    fn archive_confirm_and_edit_detail_correlation() {
        let mut workflow = ProjectActionWorkflow::default();
        let project = ProjectId::new();
        workflow.open_menu(project);
        let row = ProjectActionRow {
            project_config_id: "p".into(),
            folder_id: "f".into(),
            command_id: "api".into(),
            label: "API".into(),
        };
        assert_eq!(row.service_id().expect("typed service id").as_str(), "api");
        workflow.begin_edit_loading(&row);
        workflow.apply_command_detail(&row, "npm run api".into());
        assert!(matches!(workflow.mode, ProjectActionMenuMode::Editor(_)));
        workflow.begin_archive_confirm(row.clone());
        assert_eq!(
            workflow
                .take_archive_row()
                .as_ref()
                .map(|row| row.command_id.as_str()),
            Some("api")
        );
    }
}
