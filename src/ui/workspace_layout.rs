//! Persisted geometry for the cockpit's resizable panes.
//!
//! The shell owns pane sizes, not the host: they are a local view preference
//! and must never be inferred from task or host truth. Sizes are stored beside
//! the profile they belong to, so a dev profile cannot inherit or overwrite the
//! installed profile's layout.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::TaskId;
use crate::ui::task_workspace::TaskWorkspace;

const LAYOUT_SCHEMA_V1: &str = "devmanager.workspace-layout/v1";
const LAYOUT_SCHEMA_V2: &str = "devmanager.workspace-layout/v2";
const LAYOUT_SCHEMA_V3: &str = "devmanager.workspace-layout/v3";
const LAYOUT_SCHEMA_V4: &str = "devmanager.workspace-layout/v4";
const LAYOUT_SCHEMA: &str = "devmanager.workspace-layout/v5";
const LAYOUT_FILE_NAME: &str = "workspace-layout.json";

pub const SIDEBAR_MIN: f32 = 180.0;
pub const SIDEBAR_MAX: f32 = 460.0;
pub const INBOX_MIN: f32 = 220.0;
pub const INBOX_MAX: f32 = 560.0;
pub const DOCK_MIN: f32 = 240.0;
pub const DOCK_MAX: f32 = 680.0;
pub const TERMINAL_MIN: f32 = 120.0;
pub const TERMINAL_MAX: f32 = 800.0;
/// The conversation column is the reason the cockpit exists, so it keeps a
/// floor that the draggable rails are not allowed to cross.
pub const CENTER_MIN: f32 = 320.0;

/// A pane edge the user can drag. Each one owns exactly one stored dimension.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaneEdge {
    Sidebar,
    Inbox,
    Dock,
    Terminal,
}

/// Where the window itself was last left, in the logical pixels GPUI reports.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowFrame {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
}

/// Smallest window worth restoring. Anything below this is storage damage or a
/// window that was mid-minimize, and restoring it hands back an unusable shell.
pub const MIN_WINDOW_WIDTH: f32 = 640.0;
pub const MIN_WINDOW_HEIGHT: f32 = 480.0;

impl WindowFrame {
    pub fn is_usable(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|value| value.is_finite())
            && self.width >= MIN_WINDOW_WIDTH
            && self.height >= MIN_WINDOW_HEIGHT
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceLayout {
    pub sidebar_width: f32,
    pub inbox_width: f32,
    pub dock_width: f32,
    pub terminal_height: f32,
    #[serde(default)]
    pub sidebar_collapsed: bool,
    #[serde(default)]
    pub dock_collapsed: bool,
    #[serde(default)]
    pub terminal_collapsed: bool,
    #[serde(default)]
    pub window: Option<WindowFrame>,
    /// Client-local navigation state. This is a view preference, not durable
    /// Task state, and is validated against the next host projection before it
    /// can become active.
    #[serde(default)]
    pub selected_task: Option<TaskId>,
    /// Recursive multi-task pane tree. The selected task remains as a compact
    /// compatibility cursor and is always synchronized to this tree's focus.
    #[serde(default)]
    pub task_workspace: Option<TaskWorkspace>,
    /// Active right-dock tab label (`Changes`, `Files`, …). Validated on restore.
    #[serde(default)]
    pub active_dock_tab: Option<String>,
    /// Optional workspace project id for the sidebar scope menu. `None` = All.
    #[serde(default)]
    pub project_scope_workspace_id: Option<String>,
    /// Per-task center canvas preference: true = provider terminal, false = conversation.
    #[serde(default)]
    pub task_center_terminal: BTreeMap<String, bool>,
    /// Last composer launch choices. Project-specific overrides can layer on
    /// these later without changing task creation semantics.
    #[serde(default)]
    pub composer_provider: Option<crate::providers::ProviderKind>,
    #[serde(default)]
    pub composer_launch_options: Option<crate::providers::ProviderLaunchOptions>,
}

impl Default for WorkspaceLayout {
    fn default() -> Self {
        Self {
            sidebar_width: 260.0,
            inbox_width: 320.0,
            dock_width: 360.0,
            // The conversation is the primary surface; an idle terminal that
            // opens taller than a third of the window inverts that on the
            // laptop-class windows this ships to.
            terminal_height: 200.0,
            sidebar_collapsed: false,
            dock_collapsed: true,
            terminal_collapsed: false,
            window: None,
            selected_task: None,
            task_workspace: None,
            active_dock_tab: None,
            project_scope_workspace_id: None,
            task_center_terminal: BTreeMap::new(),
            composer_provider: Some(crate::providers::ProviderKind::Codex),
            composer_launch_options: Some(crate::providers::ProviderLaunchOptions {
                model: crate::providers::ProviderModel::CodexSol,
                reasoning_effort: crate::providers::ProviderReasoningEffort::ExtraHigh,
                access: crate::providers::ProviderAccessMode::FullAccess,
            }),
        }
    }
}

impl WorkspaceLayout {
    pub fn value(&self, edge: PaneEdge) -> f32 {
        match edge {
            PaneEdge::Sidebar => self.sidebar_width,
            PaneEdge::Inbox => self.inbox_width,
            PaneEdge::Dock => self.dock_width,
            PaneEdge::Terminal => self.terminal_height,
        }
    }

    pub fn set_value(&mut self, edge: PaneEdge, value: f32) {
        let clamped = clamp_edge(edge, value);
        match edge {
            PaneEdge::Sidebar => self.sidebar_width = clamped,
            PaneEdge::Inbox => self.inbox_width = clamped,
            PaneEdge::Dock => self.dock_width = clamped,
            PaneEdge::Terminal => self.terminal_height = clamped,
        }
    }

    pub fn reset(&mut self, edge: PaneEdge) {
        let defaults = Self::default();
        self.set_value(edge, defaults.value(edge));
    }

    pub fn toggle(&mut self, edge: PaneEdge) {
        match edge {
            PaneEdge::Sidebar => self.sidebar_collapsed = !self.sidebar_collapsed,
            PaneEdge::Dock => self.dock_collapsed = !self.dock_collapsed,
            PaneEdge::Terminal => self.terminal_collapsed = !self.terminal_collapsed,
            PaneEdge::Inbox => {}
        }
    }

    /// Reject non-finite and out-of-range values from storage. A corrupt or
    /// hand-edited file must degrade to a usable window, never to a pane that
    /// swallows the workspace.
    pub fn sanitized(mut self) -> Self {
        for edge in [
            PaneEdge::Sidebar,
            PaneEdge::Inbox,
            PaneEdge::Dock,
            PaneEdge::Terminal,
        ] {
            let value = self.value(edge);
            let fallback = Self::default().value(edge);
            self.set_value(edge, if value.is_finite() { value } else { fallback });
        }
        self.window = self.window.filter(WindowFrame::is_usable);
        let defaults = Self::default();
        if self.composer_provider.is_none() {
            self.composer_provider = defaults.composer_provider;
        }
        if self.composer_launch_options.is_none() {
            self.composer_launch_options = defaults.composer_launch_options;
        }
        self.sanitize_task_workspace();
        self
    }

    /// Reconcile local pane membership against the canonical task projection.
    /// Unknown panes and per-task surface preferences are discarded locally;
    /// no host or durable task state is mutated.
    pub fn reconcile_task_workspace(&mut self, valid_task_ids: &[TaskId]) -> bool {
        let before = self.clone();
        self.sanitize_task_workspace();

        let valid: BTreeSet<TaskId> = valid_task_ids.iter().copied().collect();
        let mut prune_failed = false;
        if let Some(workspace) = self.task_workspace.as_mut() {
            let unknown: Vec<_> = workspace
                .task_ids()
                .into_iter()
                .filter(|task_id| !valid.contains(task_id))
                .collect();
            for task_id in unknown {
                if let Some(pane_id) = workspace.pane_for_task(task_id).map(|pane| pane.id) {
                    // The id was read from this exact validated tree. A failure
                    // here means the tree changed unexpectedly, so fail closed.
                    if workspace.remove_pane(pane_id).is_err() {
                        prune_failed = true;
                        break;
                    }
                }
            }
        }
        if prune_failed {
            self.task_workspace = None;
        }

        if self
            .task_workspace
            .as_ref()
            .is_some_and(|workspace| workspace.pane_count() == 0)
        {
            self.task_workspace = None;
        }
        if self.task_workspace.is_none() {
            self.task_workspace = self
                .selected_task
                .filter(|task_id| valid.contains(task_id))
                .map(TaskWorkspace::single);
        }
        self.selected_task = self
            .task_workspace
            .as_ref()
            .and_then(TaskWorkspace::focused_task);

        let valid_keys: BTreeSet<String> = valid.iter().map(ToString::to_string).collect();
        self.task_center_terminal
            .retain(|task_id, _| valid_keys.contains(task_id));
        *self != before
    }

    fn sanitize_task_workspace(&mut self) {
        let invalid_or_empty = self
            .task_workspace
            .as_ref()
            .is_some_and(|workspace| workspace.validate().is_err() || workspace.pane_count() == 0);
        if invalid_or_empty {
            self.task_workspace = None;
        }
        if self.task_workspace.is_none() {
            self.task_workspace = self.selected_task.map(TaskWorkspace::single);
        }
        self.selected_task = self
            .task_workspace
            .as_ref()
            .and_then(TaskWorkspace::focused_task);
    }

    /// The window frame to restore, if one was stored and is still usable.
    pub fn restorable_window(&self) -> Option<WindowFrame> {
        self.window.filter(WindowFrame::is_usable)
    }

    /// Shrink the rails so the conversation keeps its floor in this window.
    /// Stored sizes are preserved; only the rendered widths give way, so
    /// widening the window restores what the user chose.
    pub fn fitted(self, available_width: f32, available_height: f32) -> Self {
        let mut fitted = self.sanitized();
        if fitted.terminal_collapsed {
            fitted.terminal_height = 0.0;
        } else {
            // Half the window is the most an idle strip may claim, however
            // tall the user dragged it on a larger display.
            fitted.terminal_height = fitted
                .terminal_height
                .min((available_height * 0.5).max(TERMINAL_MIN));
        }
        if fitted.sidebar_collapsed {
            fitted.sidebar_width = 0.0;
        }
        if fitted.dock_collapsed {
            fitted.dock_width = 0.0;
        }
        let mut rails = fitted.sidebar_width + fitted.inbox_width + fitted.dock_width;
        let budget = (available_width - CENTER_MIN).max(0.0);
        if rails <= budget {
            return fitted;
        }
        // Give way in the order the panes are least likely to be the focus.
        for (edge, floor) in [
            (PaneEdge::Dock, DOCK_MIN),
            (PaneEdge::Inbox, INBOX_MIN),
            (PaneEdge::Sidebar, SIDEBAR_MIN),
        ] {
            let current = fitted.value(edge);
            if current <= 0.0 {
                continue;
            }
            let excess = rails - budget;
            if excess <= 0.0 {
                break;
            }
            let reduced = (current - excess).max(floor);
            rails -= current - reduced;
            match edge {
                PaneEdge::Dock => fitted.dock_width = reduced,
                PaneEdge::Inbox => fitted.inbox_width = reduced,
                PaneEdge::Sidebar => fitted.sidebar_width = reduced,
                PaneEdge::Terminal => {}
            }
        }
        fitted
    }
}

pub fn clamp_edge(edge: PaneEdge, value: f32) -> f32 {
    let (min, max) = match edge {
        PaneEdge::Sidebar => (SIDEBAR_MIN, SIDEBAR_MAX),
        PaneEdge::Inbox => (INBOX_MIN, INBOX_MAX),
        PaneEdge::Dock => (DOCK_MIN, DOCK_MAX),
        PaneEdge::Terminal => (TERMINAL_MIN, TERMINAL_MAX),
    };
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        min
    }
}

#[derive(Serialize, Deserialize)]
struct LayoutFile {
    schema: String,
    layout: WorkspaceLayout,
}

/// Profile-scoped store for the pane geometry.
#[derive(Clone, Debug)]
pub struct WorkspaceLayoutStore {
    path: PathBuf,
}

impl WorkspaceLayoutStore {
    pub fn at_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn at_profile_root(root: impl AsRef<Path>) -> Self {
        Self::at_path(root.as_ref().join(LAYOUT_FILE_NAME))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the stored layout, falling back to defaults. A view preference is
    /// never worth failing a launch over, so unreadable or foreign-schema
    /// storage degrades to the default geometry. Legacy layouts migrate their
    /// selected task into a one-pane workspace; version 1 also collapses the
    /// context dock once for the conversation-first shell.
    pub fn load(&self) -> WorkspaceLayout {
        let Ok(bytes) = fs::read(&self.path) else {
            return WorkspaceLayout::default();
        };
        let Ok(file) = serde_json::from_slice::<LayoutFile>(&bytes) else {
            return WorkspaceLayout::default();
        };
        match file.schema.as_str() {
            LAYOUT_SCHEMA => file.layout.sanitized(),
            LAYOUT_SCHEMA_V4 => file.layout.sanitized(),
            LAYOUT_SCHEMA_V3 => file.layout.sanitized(),
            LAYOUT_SCHEMA_V2 => file.layout.sanitized(),
            LAYOUT_SCHEMA_V1 => {
                let mut layout = file.layout.sanitized();
                layout.dock_collapsed = true;
                layout
            }
            _ => WorkspaceLayout::default(),
        }
    }

    pub fn save(&self, layout: WorkspaceLayout) -> io::Result<()> {
        let file = LayoutFile {
            schema: LAYOUT_SCHEMA.to_string(),
            layout: layout.sanitized(),
        };
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        write_atomically(&self.path, &bytes)
    }
}

pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        "{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(LAYOUT_FILE_NAME)
    ));
    {
        let mut handle = fs::File::create(&temporary)?;
        handle.write_all(bytes)?;
        handle.sync_all()?;
    }
    match replace_file(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

#[cfg(windows)]
fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let temporary: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR(temporary.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(io::Error::from)
    }
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_layout_round_trips_through_the_profile_store() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = WorkspaceLayoutStore::at_profile_root(directory.path());
        assert_eq!(store.load(), WorkspaceLayout::default());

        let mut layout = WorkspaceLayout::default();
        layout.set_value(PaneEdge::Inbox, 401.0);
        layout.set_value(PaneEdge::Terminal, 333.0);
        layout.sidebar_collapsed = true;
        let selected = TaskId::new();
        layout.selected_task = Some(selected);
        layout.task_workspace = Some(crate::ui::task_workspace::TaskWorkspace::single(selected));
        store.save(layout.clone()).expect("save layout");

        assert_eq!(store.load(), layout);
    }

    #[test]
    fn a_second_save_atomically_replaces_the_existing_layout() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = WorkspaceLayoutStore::at_profile_root(directory.path());
        let mut first = WorkspaceLayout::default();
        first.set_value(PaneEdge::Inbox, 401.0);
        store.save(first).expect("save first layout");

        let mut second = WorkspaceLayout::default();
        second.set_value(PaneEdge::Dock, 517.0);
        second.sidebar_collapsed = true;
        store.save(second.clone()).expect("replace existing layout");

        assert_eq!(store.load(), second);
    }

    #[test]
    fn corrupt_or_foreign_storage_degrades_to_defaults() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = WorkspaceLayoutStore::at_profile_root(directory.path());
        fs::write(store.path(), b"{not json").expect("write");
        assert_eq!(store.load(), WorkspaceLayout::default());

        fs::write(
            store.path(),
            br#"{"schema":"other/v9","layout":{"sidebar_width":1.0,"inbox_width":1.0,"dock_width":1.0,"terminal_height":1.0}}"#,
        )
        .expect("write");
        assert_eq!(store.load(), WorkspaceLayout::default());
    }

    #[test]
    fn an_unusable_stored_window_frame_is_not_restored() {
        let mut layout = WorkspaceLayout::default();
        layout.window = Some(WindowFrame {
            x: 40.0,
            y: 40.0,
            width: 120.0,
            height: 90.0,
            maximized: false,
        });
        assert_eq!(layout.clone().sanitized().restorable_window(), None);

        layout.window = Some(WindowFrame {
            x: 40.0,
            y: 40.0,
            width: f32::INFINITY,
            height: 900.0,
            maximized: false,
        });
        assert_eq!(layout.clone().sanitized().restorable_window(), None);

        let usable = WindowFrame {
            x: 120.0,
            y: 80.0,
            width: 1440.0,
            height: 900.0,
            maximized: true,
        };
        layout.window = Some(usable);
        assert_eq!(layout.sanitized().restorable_window(), Some(usable));
    }

    #[test]
    fn edges_clamp_to_their_own_bounds_and_reject_non_finite_values() {
        let mut layout = WorkspaceLayout::default();
        layout.set_value(PaneEdge::Sidebar, 10_000.0);
        assert_eq!(layout.sidebar_width, SIDEBAR_MAX);
        layout.set_value(PaneEdge::Dock, -50.0);
        assert_eq!(layout.dock_width, DOCK_MIN);
        layout.set_value(PaneEdge::Terminal, f32::NAN);
        assert_eq!(layout.terminal_height, TERMINAL_MIN);
    }

    #[test]
    fn a_narrow_window_yields_rails_before_the_conversation_floor() {
        let layout = WorkspaceLayout {
            sidebar_width: 400.0,
            inbox_width: 500.0,
            dock_width: 600.0,
            terminal_height: 260.0,
            sidebar_collapsed: false,
            dock_collapsed: false,
            terminal_collapsed: false,
            window: None,
            selected_task: None,
            ..WorkspaceLayout::default()
        };
        let fitted = layout.clone().fitted(1000.0, 900.0);
        let rails = fitted.sidebar_width + fitted.inbox_width + fitted.dock_width;
        assert!(rails <= 1000.0 - CENTER_MIN + f32::EPSILON, "rails {rails}");
        assert!(fitted.dock_width >= DOCK_MIN);
        assert!(fitted.inbox_width >= INBOX_MIN);
        assert!(fitted.sidebar_width >= SIDEBAR_MIN);
        // Stored preference is untouched: only the rendered geometry gave way.
        assert_eq!(layout.dock_width, 600.0);
    }

    #[test]
    fn collapsed_panes_render_at_zero_without_losing_their_stored_size() {
        let layout = WorkspaceLayout {
            sidebar_collapsed: true,
            dock_collapsed: true,
            terminal_collapsed: true,
            ..WorkspaceLayout::default()
        };
        let fitted = layout.clone().fitted(1600.0, 1000.0);
        assert_eq!(fitted.sidebar_width, 0.0);
        assert_eq!(fitted.dock_width, 0.0);
        assert_eq!(fitted.terminal_height, 0.0);
        assert_eq!(layout.dock_width, WorkspaceLayout::default().dock_width);
    }

    #[test]
    fn conversation_first_defaults_collapse_the_context_dock() {
        assert!(
            WorkspaceLayout::default().dock_collapsed,
            "new layouts must start with the context dock collapsed"
        );
    }

    #[test]
    fn v1_layout_migrates_once_to_v5_with_dock_collapsed() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = WorkspaceLayoutStore::at_profile_root(directory.path());
        let selected = TaskId::new();
        let window = WindowFrame {
            x: 48.0,
            y: 64.0,
            width: 1440.0,
            height: 900.0,
            maximized: false,
        };
        let v1 = serde_json::json!({
            "schema": "devmanager.workspace-layout/v1",
            "layout": {
                "sidebar_width": 275.0,
                "inbox_width": 410.0,
                "dock_width": 520.0,
                "terminal_height": 240.0,
                "sidebar_collapsed": true,
                "dock_collapsed": false,
                "terminal_collapsed": true,
                "window": window,
                "selected_task": selected
            }
        });
        fs::write(
            store.path(),
            serde_json::to_vec_pretty(&v1).expect("encode"),
        )
        .expect("write");

        let migrated = store.load();
        assert_eq!(migrated.sidebar_width, 275.0);
        assert_eq!(migrated.inbox_width, 410.0);
        assert_eq!(migrated.dock_width, 520.0);
        assert_eq!(migrated.terminal_height, 240.0);
        assert!(migrated.sidebar_collapsed);
        assert!(
            migrated.dock_collapsed,
            "v1 open docks must collapse once during conversation-first migration"
        );
        assert!(migrated.terminal_collapsed);
        assert_eq!(migrated.window, Some(window));
        assert_eq!(migrated.selected_task, Some(selected));
        assert_eq!(
            migrated
                .task_workspace
                .as_ref()
                .and_then(|workspace| workspace.focused_task()),
            Some(selected)
        );

        store
            .save(migrated.clone())
            .expect("persist migrated layout");
        let bytes = fs::read(store.path()).expect("read saved layout");
        let saved: serde_json::Value = serde_json::from_slice(&bytes).expect("parse saved");
        assert_eq!(
            saved["schema"], "devmanager.workspace-layout/v5",
            "save must write the current workspace-layout schema"
        );
        assert_eq!(store.load(), migrated);
    }

    #[test]
    fn v5_persists_dock_scope_center_surface_and_composer_defaults() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = WorkspaceLayoutStore::at_profile_root(directory.path());
        let layout = WorkspaceLayout {
            active_dock_tab: Some("Browser".into()),
            project_scope_workspace_id: Some("project-1".into()),
            task_center_terminal: {
                let mut map = BTreeMap::new();
                map.insert("task-a".into(), true);
                map
            },
            ..WorkspaceLayout::default()
        };
        store.save(layout).expect("save v5");
        let loaded = store.load();
        assert_eq!(loaded.active_dock_tab.as_deref(), Some("Browser"));
        assert_eq!(
            loaded.project_scope_workspace_id.as_deref(),
            Some("project-1")
        );
        assert_eq!(loaded.task_center_terminal.get("task-a"), Some(&true));
        assert_eq!(
            loaded.composer_launch_options,
            WorkspaceLayout::default().composer_launch_options
        );
        assert!(loaded.dock_collapsed);
    }

    #[test]
    fn v3_selected_task_migrates_to_a_single_pane_workspace() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = WorkspaceLayoutStore::at_profile_root(directory.path());
        let selected = TaskId::new();
        let v3 = serde_json::json!({
            "schema": "devmanager.workspace-layout/v3",
            "layout": {
                "sidebar_width": 285.0,
                "inbox_width": 320.0,
                "dock_width": 360.0,
                "terminal_height": 200.0,
                "selected_task": selected
            }
        });
        fs::write(
            store.path(),
            serde_json::to_vec_pretty(&v3).expect("encode"),
        )
        .expect("write");

        let migrated = store.load();
        let workspace = migrated.task_workspace.as_ref().expect("workspace");
        assert_eq!(workspace.pane_count(), 1);
        assert_eq!(workspace.focused_task(), Some(selected));
        assert_eq!(migrated.selected_task, Some(selected));
    }

    #[test]
    fn corrupt_v4_workspace_fails_closed_without_losing_shell_geometry() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = WorkspaceLayoutStore::at_profile_root(directory.path());
        let selected = TaskId::new();
        let corrupt = serde_json::json!({
            "schema": "devmanager.workspace-layout/v4",
            "layout": {
                "sidebar_width": 285.0,
                "inbox_width": 410.0,
                "dock_width": 520.0,
                "terminal_height": 240.0,
                "selected_task": selected,
                "task_workspace": {
                    "root": null,
                    "focused": selected,
                    "previous_focus": null,
                    "focus_clock": 1
                }
            }
        });
        fs::write(
            store.path(),
            serde_json::to_vec_pretty(&corrupt).expect("encode"),
        )
        .expect("write");

        let loaded = store.load();
        assert_eq!(loaded.sidebar_width, 285.0);
        assert_eq!(loaded.inbox_width, 410.0);
        assert_eq!(loaded.dock_width, 520.0);
        assert_eq!(loaded.terminal_height, 240.0);
        let workspace = loaded.task_workspace.as_ref().expect("fallback workspace");
        assert_eq!(workspace.pane_count(), 1);
        assert_eq!(workspace.focused_task(), Some(selected));
    }

    #[test]
    fn canonical_reconciliation_prunes_unknown_tasks_and_repairs_focus() {
        use crate::ui::task_workspace::{Axis, TaskWorkspace};

        let first = TaskId::new();
        let removed = TaskId::new();
        let third = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(removed, Axis::Horizontal)
            .expect("insert removed task");
        workspace
            .insert_after_focused(third, Axis::Vertical)
            .expect("insert third task");
        workspace.focus_task(removed).expect("focus removed task");
        let mut layout = WorkspaceLayout {
            selected_task: Some(removed),
            task_workspace: Some(workspace),
            task_center_terminal: BTreeMap::from([
                (first.to_string(), true),
                (removed.to_string(), true),
                ("not-a-task".into(), true),
            ]),
            ..WorkspaceLayout::default()
        };

        assert!(layout.reconcile_task_workspace(&[first, third]));
        let workspace = layout.task_workspace.as_ref().expect("workspace");
        assert_eq!(workspace.pane_count(), 2);
        assert!(workspace.contains_task(first));
        assert!(workspace.contains_task(third));
        assert!(!workspace.contains_task(removed));
        assert_eq!(layout.selected_task, workspace.focused_task());
        assert_eq!(
            layout.task_center_terminal,
            BTreeMap::from([(first.to_string(), true)])
        );
    }
}
