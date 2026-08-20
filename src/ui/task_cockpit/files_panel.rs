//! Bounded files panel projection backed by Task Cockpit file queries.

use crate::client::action::ActionRequest;
use crate::domain::cockpit::{
    truncate_to_max_bytes, TaskCockpitQuery, TaskFileEntry, TaskFilesListProjection,
    TaskFilesReadProjection,
};
use crate::domain::id::TaskId;

use super::panel::{
    task_identity, PanelAction, PanelDisabledReason, PanelIdentity, MAX_PANEL_LABEL_BYTES,
    MAX_PANEL_ROWS,
};

const MAX_FILE_PREVIEW_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePanelRow {
    /// Exact host-relative path retained for the read request.
    pub relative_path: String,
    /// Bounded label used by the renderer.
    pub label: String,
    pub is_directory: bool,
    pub secret: bool,
    pub read: PanelAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePanelPreview {
    pub relative_path: String,
    pub content: String,
    pub byte_len: u32,
    pub binary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesPanelProjection {
    pub identity: PanelIdentity,
    pub relative_directory: Option<String>,
    pub rows: Vec<FilePanelRow>,
    pub preview: Option<FilePanelPreview>,
    pub truncated: bool,
    pub refresh: PanelAction,
    pub disabled_reason: Option<PanelDisabledReason>,
}

impl FilesPanelProjection {
    pub fn from_host(
        projection: Option<&TaskFilesListProjection>,
        file_read: Option<&TaskFilesReadProjection>,
        task_id: TaskId,
        revision: Option<u64>,
        relative_directory: Option<String>,
    ) -> Self {
        let identity = task_identity(task_id, revision);
        let refresh_request = ActionRequest::TaskCockpit {
            task_id,
            query: TaskCockpitQuery::FilesList {
                relative_directory: relative_directory.clone(),
                limit: crate::domain::cockpit::MAX_COCKPIT_FILE_LIST,
            },
        };
        let Some(projection) = projection.filter(|projection| projection.task_id == task_id) else {
            return Self {
                identity,
                relative_directory,
                rows: Vec::new(),
                preview: file_preview(file_read, task_id),
                truncated: false,
                refresh: PanelAction::disabled(
                    identity,
                    refresh_request,
                    PanelDisabledReason::HostProjectionMissing,
                ),
                disabled_reason: Some(PanelDisabledReason::HostProjectionMissing),
            };
        };
        let rows = projection
            .entries
            .iter()
            .take(MAX_PANEL_ROWS)
            .map(|entry| file_row(entry, identity))
            .collect();
        Self {
            identity,
            relative_directory,
            rows,
            preview: file_preview(file_read, task_id),
            truncated: projection.truncated || projection.entries.len() > MAX_PANEL_ROWS,
            refresh: PanelAction::enabled(identity, refresh_request),
            disabled_reason: None,
        }
    }

    pub fn summary(&self) -> String {
        if let Some(reason) = self.disabled_reason {
            return reason.label().to_owned();
        }
        let suffix = self.truncated.then_some("+ truncated").unwrap_or("");
        format!("{} file(s) {}", self.rows.len(), suffix)
            .trim()
            .to_owned()
    }
}

fn file_preview(
    projection: Option<&TaskFilesReadProjection>,
    task_id: TaskId,
) -> Option<FilePanelPreview> {
    let projection =
        projection.filter(|projection| projection.task_id == task_id && !projection.secret)?;
    let (content, binary) = match projection.utf8_prefix.as_deref() {
        Some(content) => (
            truncate_to_max_bytes(content, MAX_FILE_PREVIEW_BYTES),
            false,
        ),
        None => ("Binary file preview unavailable".to_owned(), true),
    };
    Some(FilePanelPreview {
        relative_path: truncate_to_max_bytes(&projection.relative_path, MAX_PANEL_LABEL_BYTES),
        content,
        byte_len: projection.byte_len,
        binary,
    })
}

fn file_row(entry: &TaskFileEntry, identity: PanelIdentity) -> FilePanelRow {
    let label = truncate_to_max_bytes(&entry.relative_path, MAX_PANEL_LABEL_BYTES);
    let disabled_reason = if entry.secret {
        PanelDisabledReason::SecretPath
    } else if entry.is_directory {
        PanelDisabledReason::Directory
    } else {
        // Kept as an explicit match so every row has a typed action state;
        // path validation remains host-owned.
        return FilePanelRow {
            relative_path: entry.relative_path.clone(),
            label,
            is_directory: entry.is_directory,
            secret: entry.secret,
            read: PanelAction::enabled(
                identity,
                ActionRequest::TaskCockpit {
                    task_id: identity.task_id,
                    query: TaskCockpitQuery::FilesRead {
                        relative_path: entry.relative_path.clone(),
                        max_bytes: crate::domain::cockpit::MAX_COCKPIT_READ_BYTES,
                    },
                },
            ),
        };
    };
    FilePanelRow {
        relative_path: entry.relative_path.clone(),
        label,
        is_directory: entry.is_directory,
        secret: entry.secret,
        read: PanelAction::disabled(
            identity,
            ActionRequest::TaskCockpit {
                task_id: identity.task_id,
                query: TaskCockpitQuery::FilesRead {
                    relative_path: entry.relative_path.clone(),
                    max_bytes: crate::domain::cockpit::MAX_COCKPIT_READ_BYTES,
                },
            },
            disabled_reason,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_panel_projects_the_selected_text_file_for_display() {
        let task_id = TaskId::new();
        let panel = FilesPanelProjection::from_host(
            Some(&TaskFilesListProjection {
                task_id,
                entries: vec![TaskFileEntry {
                    relative_path: "README.md".into(),
                    is_directory: false,
                    secret: false,
                }],
                truncated: false,
            }),
            Some(&crate::domain::cockpit::TaskFilesReadProjection {
                task_id,
                relative_path: "README.md".into(),
                utf8_prefix: Some("hello from the selected file".into()),
                byte_len: 28,
                secret: false,
            }),
            task_id,
            Some(3),
            None,
        );

        let preview = panel.preview.expect("selected file preview");
        assert_eq!(preview.relative_path, "README.md");
        assert_eq!(preview.content, "hello from the selected file");
        assert!(!preview.binary);
        assert_eq!(preview.byte_len, 28);
    }

    #[test]
    fn files_panel_bounds_rows_and_keeps_read_action_task_scoped() {
        let task_id = TaskId::new();
        let mut entries = Vec::new();
        for index in 0..(MAX_PANEL_ROWS + 3) {
            entries.push(TaskFileEntry {
                relative_path: format!("src/{index}.rs"),
                is_directory: false,
                secret: false,
            });
        }
        let panel = FilesPanelProjection::from_host(
            Some(&TaskFilesListProjection {
                task_id,
                entries,
                truncated: false,
            }),
            None,
            task_id,
            Some(3),
            Some("src".into()),
        );
        assert_eq!(panel.rows.len(), MAX_PANEL_ROWS);
        assert!(panel.truncated);
        let read = &panel.rows[0].read;
        assert_eq!(read.identity.task_id, task_id);
        assert_eq!(read.identity.revision, Some(3));
        assert!(matches!(
            read.request,
            ActionRequest::TaskCockpit {
                task_id: request_task,
                query: TaskCockpitQuery::FilesRead { .. },
            } if request_task == task_id
        ));
    }

    #[test]
    fn files_panel_disables_secret_and_directory_reads_with_typed_reasons() {
        let task_id = TaskId::new();
        let panel = FilesPanelProjection::from_host(
            Some(&TaskFilesListProjection {
                task_id,
                entries: vec![
                    TaskFileEntry {
                        relative_path: ".env".into(),
                        is_directory: false,
                        secret: true,
                    },
                    TaskFileEntry {
                        relative_path: "src".into(),
                        is_directory: true,
                        secret: false,
                    },
                ],
                truncated: false,
            }),
            None,
            task_id,
            None,
            None,
        );
        assert_eq!(
            panel.rows[0].read.disabled_reason,
            Some(PanelDisabledReason::SecretPath)
        );
        assert_eq!(
            panel.rows[1].read.disabled_reason,
            Some(PanelDisabledReason::Directory)
        );
        assert!(!panel.rows[0].read.is_enabled());
        assert!(!panel.rows[1].read.is_enabled());
    }
}
