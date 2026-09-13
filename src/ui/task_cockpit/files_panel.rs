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
    /// Exact host-relative path retained for the read or list request.
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
    /// Host-relative parent listing. Absent at workspace root.
    pub navigate_parent: Option<PanelAction>,
    /// Host-relative root listing. Absent at workspace root.
    pub navigate_root: Option<PanelAction>,
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
        let refresh_request = files_list_request(task_id, relative_directory.clone());
        let (navigate_parent, navigate_root) =
            directory_navigation_actions(identity, task_id, relative_directory.as_deref());
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
                navigate_parent,
                navigate_root,
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
            navigate_parent,
            navigate_root,
            disabled_reason: None,
        }
    }

    pub fn summary(&self) -> String {
        if let Some(reason) = self.disabled_reason {
            return reason.label().to_owned();
        }
        let suffix = self.truncated.then_some("+ truncated").unwrap_or("");
        let location = self
            .relative_directory
            .as_deref()
            .filter(|path| !path.is_empty())
            .unwrap_or(".");
        format!("{location} · {} file(s) {}", self.rows.len(), suffix)
            .trim()
            .to_owned()
    }
}

/// Parent of a host-relative directory. `None` means workspace root; `Some(None)`
/// is returned only when the current directory is already root (no parent).
pub fn parent_host_relative_directory(relative_directory: Option<&str>) -> Option<Option<String>> {
    let current = relative_directory?.trim_matches('/');
    if current.is_empty() {
        return None;
    }
    match current.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => Some(Some(parent.to_owned())),
        _ => Some(None),
    }
}

fn files_list_request(task_id: TaskId, relative_directory: Option<String>) -> ActionRequest {
    ActionRequest::TaskCockpit {
        task_id,
        query: TaskCockpitQuery::FilesList {
            relative_directory,
            limit: crate::domain::cockpit::MAX_COCKPIT_FILE_LIST,
        },
    }
}

fn directory_navigation_actions(
    identity: PanelIdentity,
    task_id: TaskId,
    relative_directory: Option<&str>,
) -> (Option<PanelAction>, Option<PanelAction>) {
    let Some(parent) = parent_host_relative_directory(relative_directory) else {
        return (None, None);
    };
    (
        Some(PanelAction::enabled(
            identity,
            files_list_request(task_id, parent),
        )),
        Some(PanelAction::enabled(
            identity,
            files_list_request(task_id, None),
        )),
    )
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
    if entry.secret {
        return FilePanelRow {
            relative_path: entry.relative_path.clone(),
            label,
            is_directory: entry.is_directory,
            secret: true,
            read: PanelAction::disabled(
                identity,
                ActionRequest::TaskCockpit {
                    task_id: identity.task_id,
                    query: TaskCockpitQuery::FilesRead {
                        relative_path: entry.relative_path.clone(),
                        max_bytes: crate::domain::cockpit::MAX_COCKPIT_READ_BYTES,
                    },
                },
                PanelDisabledReason::SecretPath,
            ),
        };
    }
    if entry.is_directory {
        return FilePanelRow {
            relative_path: entry.relative_path.clone(),
            label,
            is_directory: true,
            secret: false,
            read: PanelAction::enabled(
                identity,
                files_list_request(identity.task_id, Some(entry.relative_path.clone())),
            ),
        };
    }
    FilePanelRow {
        relative_path: entry.relative_path.clone(),
        label,
        is_directory: false,
        secret: false,
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
        assert!(panel.navigate_parent.is_none());
        assert!(panel.navigate_root.is_none());
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
        let parent = panel.navigate_parent.expect("parent from src");
        assert!(matches!(
            parent.request,
            ActionRequest::TaskCockpit {
                query: TaskCockpitQuery::FilesList {
                    relative_directory: None,
                    ..
                },
                ..
            }
        ));
        assert!(panel.navigate_root.is_some());
    }

    #[test]
    fn files_panel_disables_secret_rows_and_lists_directories() {
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
        assert!(!panel.rows[0].read.is_enabled());
        assert!(panel.rows[1].read.is_enabled());
        assert!(matches!(
            panel.rows[1].read.request,
            ActionRequest::TaskCockpit {
                query: TaskCockpitQuery::FilesList {
                    relative_directory: Some(ref path),
                    ..
                },
                ..
            } if path == "src"
        ));
    }

    #[test]
    fn parent_host_relative_directory_never_escapes_root() {
        assert_eq!(parent_host_relative_directory(None), None);
        assert_eq!(parent_host_relative_directory(Some("")), None);
        assert_eq!(parent_host_relative_directory(Some("src")), Some(None));
        assert_eq!(
            parent_host_relative_directory(Some("src/ui")),
            Some(Some("src".into()))
        );
        assert_eq!(
            parent_host_relative_directory(Some("src/ui/native")),
            Some(Some("src/ui".into()))
        );
    }

    #[test]
    fn nested_directory_refresh_preserves_current_path_and_parent_nav() {
        let task_id = TaskId::new();
        let panel = FilesPanelProjection::from_host(
            Some(&TaskFilesListProjection {
                task_id,
                entries: vec![TaskFileEntry {
                    relative_path: "src/ui/mod.rs".into(),
                    is_directory: false,
                    secret: false,
                }],
                truncated: false,
            }),
            None,
            task_id,
            Some(2),
            Some("src/ui".into()),
        );
        assert_eq!(panel.relative_directory.as_deref(), Some("src/ui"));
        assert!(matches!(
            panel.refresh.request,
            ActionRequest::TaskCockpit {
                query: TaskCockpitQuery::FilesList {
                    relative_directory: Some(ref path),
                    ..
                },
                ..
            } if path == "src/ui"
        ));
        let parent = panel.navigate_parent.expect("parent");
        assert!(matches!(
            parent.request,
            ActionRequest::TaskCockpit {
                query: TaskCockpitQuery::FilesList {
                    relative_directory: Some(ref path),
                    ..
                },
                ..
            } if path == "src"
        ));
        let root = panel.navigate_root.expect("root");
        assert!(matches!(
            root.request,
            ActionRequest::TaskCockpit {
                query: TaskCockpitQuery::FilesList {
                    relative_directory: None,
                    ..
                },
                ..
            }
        ));
    }
}
