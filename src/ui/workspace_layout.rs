//! Persisted geometry for the cockpit's resizable panes.
//!
//! The shell owns pane sizes, not the host: they are a local view preference
//! and must never be inferred from task or host truth. Sizes are stored beside
//! the profile they belong to, so a dev profile cannot inherit or overwrite the
//! installed profile's layout.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::domain::TaskId;
#[cfg(test)]
use crate::ui::task_workspace::TaskWorkspace;
use crate::ui::task_workspace::Workspace;
use crate::ui::task_workspace::{PanePresentation, PaneView};

const LAYOUT_SCHEMA_V1: &str = "devmanager.workspace-layout/v1";
const LAYOUT_SCHEMA_V2: &str = "devmanager.workspace-layout/v2";
const LAYOUT_SCHEMA_V3: &str = "devmanager.workspace-layout/v3";
const LAYOUT_SCHEMA_V4: &str = "devmanager.workspace-layout/v4";
const LAYOUT_SCHEMA_V5: &str = "devmanager.workspace-layout/v5";
/// Legacy raw-`TaskId` layout schema written by [`WorkspaceLayoutStore::save`].
const LAYOUT_SCHEMA: &str = LAYOUT_SCHEMA_V5;
/// Host-qualified layout schema written by [`WorkspaceLayoutStore::save_keyed`].
const LAYOUT_SCHEMA_V6: &str = "devmanager.workspace-layout/v6";
const LAYOUT_FILE_NAME: &str = "workspace-layout.json";
const MAX_LAYOUT_FILE_BYTES: u64 = 2 * 1024 * 1024;

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

/// User-owned composer choices for one exact task. Provider discovery supplies
/// the available values; this record preserves what the user selected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskComposerPreferences {
    pub provider: crate::providers::ProviderKind,
    pub launch_options: crate::providers::ProviderLaunchOptions,
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
#[serde(bound(serialize = "K: Serialize", deserialize = "K: Deserialize<'de> + Ord"))]
pub struct KeyedWorkspaceLayout<K = TaskId> {
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
    pub selected_task: Option<K>,
    /// Recursive multi-task pane tree. The selected task remains as a compact
    /// compatibility cursor and is always synchronized to this tree's focus.
    #[serde(default)]
    pub task_workspace: Option<Workspace<K>>,
    /// Active right-dock tab label (`Changes`, `Files`, …). Validated on restore.
    #[serde(default)]
    pub active_dock_tab: Option<String>,
    /// Optional workspace project id for the sidebar scope menu. `None` = All.
    #[serde(default)]
    pub project_scope_workspace_id: Option<String>,
    /// Per-task center canvas preference: true = provider terminal, false = conversation.
    /// Map keys are [`TerminalCenterKey::center_preference_key`] encodings.
    #[serde(default)]
    pub task_center_terminal: BTreeMap<String, bool>,
    /// Exact task-owned composer selections. Keys use the same stable full-key
    /// encoding as `task_center_terminal`, because JSON object keys must be
    /// strings and raw TaskIds would collide across hosts.
    #[serde(default)]
    pub task_composer_preferences: BTreeMap<String, TaskComposerPreferences>,
    /// Last composer launch choices. Project-specific overrides can layer on
    /// these later without changing task creation semantics.
    #[serde(default)]
    pub composer_provider: Option<crate::providers::ProviderKind>,
    #[serde(default)]
    pub composer_launch_options: Option<crate::providers::ProviderLaunchOptions>,
    /// Project palette slot per project id, assigned at first sight (spec 5.3).
    /// Keys are `ProjectId` rendered with its `Display`; slots outside the
    /// palette are clamped by [`Self::sanitized`].
    #[serde(default)]
    pub project_colours: BTreeMap<String, u8>,
    /// Board column collapsed to the 36 px rail.
    #[serde(default)]
    pub board_rail: bool,
}

/// Local raw-`TaskId` layout (existing public API).
pub type WorkspaceLayout = KeyedWorkspaceLayout<TaskId>;

/// Fallible migration / mapping failures for host-qualified layout keys.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceLayoutMapError {
    DuplicateMappedKey,
    InvalidWorkspace,
}

/// Stable full-key preference string for `task_center_terminal` map entries.
///
/// Serializes the **complete** owner key as compact JSON. Host-qualified keys
/// must use this (or an equivalent full-key encoding via [`TerminalCenterKey`])
/// — never a raw `TaskId` UUID alone, which would collide across hosts.
pub fn task_center_terminal_preference_key<K: Serialize>(
    key: &K,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(key)
}

/// Encoding used when reconciling / retaining center-surface preference entries.
///
/// Legacy [`TaskId`] layouts keep bare UUID strings for on-disk compatibility.
/// Host-qualified keys should delegate to [`task_center_terminal_preference_key`].
pub trait TerminalCenterKey {
    fn center_preference_key(&self) -> String;
}

impl TerminalCenterKey for TaskId {
    fn center_preference_key(&self) -> String {
        self.to_string()
    }
}

impl TerminalCenterKey for crate::client::HostTaskKey {
    fn center_preference_key(&self) -> String {
        // This concrete key contains only strings, bytes and a UUID, so JSON
        // serialization is infallible. Preserve the complete owner identity.
        task_center_terminal_preference_key(self).expect("serialize host task key")
    }
}

impl<K> Default for KeyedWorkspaceLayout<K> {
    fn default() -> Self {
        Self {
            sidebar_width: 260.0,
            // The board column's own width. `01-composition-A.html` pins the
            // chosen composition at 236 px and the option card states the
            // cost in those terms, so that is what a fresh profile opens at.
            inbox_width: crate::ui::board::layout::BOARD_COLUMN_WIDTH,
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
            task_composer_preferences: BTreeMap::new(),
            composer_provider: Some(crate::providers::ProviderKind::Codex),
            composer_launch_options: Some(crate::providers::ProviderLaunchOptions {
                model: crate::providers::ProviderModel::CodexSol,
                reasoning_effort: crate::providers::ProviderReasoningEffort::ExtraHigh,
                access: crate::providers::ProviderAccessMode::FullAccess,
                ..crate::providers::ProviderLaunchOptions::default()
            }),
            project_colours: BTreeMap::new(),
            board_rail: false,
        }
    }
}

impl<K: Clone + Ord + Eq> KeyedWorkspaceLayout<K> {
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
    pub fn sanitized(mut self) -> Self
    where
        K: TerminalCenterKey,
    {
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
        // A hand-edited or forward-version file can name a palette slot this
        // build does not have; fail it closed to the first hue rather than
        // letting a render-time lookup decide.
        let slots = crate::ui::board::PROJECT_PALETTE.len() as u8;
        for index in self.project_colours.values_mut() {
            if *index >= slots {
                *index = 0;
            }
        }
        self.sanitize_task_workspace();
        self.adopt_legacy_terminal_preferences();
        self.restore_minimised_panes();
        // The right dock is retired (spec 6.2: its tools are panel views). A
        // file written before it was retired can still carry an expanded dock,
        // and honouring that would put a second Files panel on screen beside
        // the panel already showing Files. Forced here rather than at the
        // paint so exactly one place decides it.
        self.dock_collapsed = true;
        self
    }

    /// Minimisation is a fact about the current viewport, not a stored user
    /// choice, so a restored file opens every pane Full and lets allocation
    /// re-derive the strips. This is also what maps a retired `CompactManual`
    /// pane — which aliases in as `Minimised` — back to a full pane.
    fn restore_minimised_panes(&mut self) {
        let Some(workspace) = self.task_workspace.as_mut() else {
            return;
        };
        for task_id in workspace.task_ids() {
            if workspace.presentation(task_id.clone()) == Some(PanePresentation::Minimised) {
                let _ = workspace.set_presentation(task_id, PanePresentation::Full);
            }
        }
    }

    /// Fold the retired `task_center_terminal` map into the pane that owns the
    /// view. The map is consumed rather than kept: a pane's `view` is now the
    /// only place the Conversation/Terminal choice lives, and two sources would
    /// drift the moment one of them is written.
    fn adopt_legacy_terminal_preferences(&mut self)
    where
        K: TerminalCenterKey,
    {
        if self.task_center_terminal.is_empty() {
            return;
        }
        if let Some(workspace) = self.task_workspace.as_mut() {
            for task_id in workspace.task_ids() {
                let key = task_id.center_preference_key();
                if self.task_center_terminal.get(&key).copied() == Some(true) {
                    let _ = workspace.set_view(task_id, PaneView::Terminal);
                }
            }
        }
        self.task_center_terminal.clear();
    }

    /// Reconcile local pane membership against the canonical task projection.
    /// Unknown panes and per-task surface preferences are discarded locally;
    /// no host or durable task state is mutated.
    pub fn reconcile_task_workspace(&mut self, valid_task_ids: &[K]) -> bool
    where
        K: TerminalCenterKey,
    {
        let before = self.clone();
        self.sanitize_task_workspace();

        let valid: BTreeSet<K> = valid_task_ids.iter().cloned().collect();
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
                .clone()
                .filter(|task_id| valid.contains(task_id))
                .map(Workspace::single);
        }
        self.selected_task = self
            .task_workspace
            .as_ref()
            .and_then(Workspace::focused_task);

        let valid_keys: BTreeSet<String> = valid
            .iter()
            .map(TerminalCenterKey::center_preference_key)
            .collect();
        self.task_center_terminal
            .retain(|task_id, _| valid_keys.contains(task_id));
        self.task_composer_preferences
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
            self.task_workspace = self.selected_task.clone().map(Workspace::single);
        }
        self.selected_task = self
            .task_workspace
            .as_ref()
            .and_then(Workspace::focused_task);
    }

    /// The window frame to restore, if one was stored and is still usable.
    pub fn restorable_window(&self) -> Option<WindowFrame> {
        self.window.filter(WindowFrame::is_usable)
    }

    /// Shrink the rails so the conversation keeps its floor in this window.
    /// Stored sizes are preserved; only the rendered widths give way, so
    /// widening the window restores what the user chose.
    pub fn fitted(self, available_width: f32, available_height: f32) -> Self
    where
        K: TerminalCenterKey,
    {
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

impl KeyedWorkspaceLayout<TaskId> {
    /// Map a legacy raw-`TaskId` layout onto host-qualified owner keys.
    ///
    /// Selected task, recursive workspace panes, and terminal-preference entries
    /// that parse as `TaskId` are rewritten through `map_legacy_task`. Mapping
    /// collisions fail closed without merging panes or preference rows.
    pub fn map_legacy_task_keys<K, F>(
        self,
        mut map_legacy_task: F,
    ) -> Result<KeyedWorkspaceLayout<K>, WorkspaceLayoutMapError>
    where
        K: Clone + Ord + Eq + TerminalCenterKey,
        F: FnMut(TaskId) -> K,
    {
        let task_workspace = match self.task_workspace {
            Some(workspace) => Some(
                workspace
                    .map_task_keys(|task_id| map_legacy_task(*task_id))
                    .map_err(|error| match error {
                        crate::ui::task_workspace::WorkspaceError::DuplicateTask => {
                            WorkspaceLayoutMapError::DuplicateMappedKey
                        }
                        _ => WorkspaceLayoutMapError::InvalidWorkspace,
                    })?,
            ),
            None => None,
        };
        let selected_task = match self.selected_task {
            Some(task_id) => Some(map_legacy_task(task_id)),
            None => None,
        };
        let mut task_center_terminal = BTreeMap::new();
        for (raw_key, value) in self.task_center_terminal {
            let Ok(task_id) = TaskId::parse(&raw_key) else {
                continue;
            };
            let mapped = map_legacy_task(task_id);
            let preference_key = mapped.center_preference_key();
            if task_center_terminal.insert(preference_key, value).is_some() {
                return Err(WorkspaceLayoutMapError::DuplicateMappedKey);
            }
        }
        let mut task_composer_preferences = BTreeMap::new();
        for (raw_key, preferences) in self.task_composer_preferences {
            let Ok(task_id) = TaskId::parse(&raw_key) else {
                continue;
            };
            let mapped = map_legacy_task(task_id);
            if task_composer_preferences
                .insert(mapped.center_preference_key(), preferences)
                .is_some()
            {
                return Err(WorkspaceLayoutMapError::DuplicateMappedKey);
            }
        }
        Ok(KeyedWorkspaceLayout {
            sidebar_width: self.sidebar_width,
            inbox_width: self.inbox_width,
            dock_width: self.dock_width,
            terminal_height: self.terminal_height,
            sidebar_collapsed: self.sidebar_collapsed,
            dock_collapsed: self.dock_collapsed,
            terminal_collapsed: self.terminal_collapsed,
            window: self.window,
            selected_task,
            task_workspace,
            active_dock_tab: self.active_dock_tab,
            project_scope_workspace_id: self.project_scope_workspace_id,
            task_center_terminal,
            task_composer_preferences,
            composer_provider: self.composer_provider,
            composer_launch_options: self.composer_launch_options,
            project_colours: self.project_colours,
            board_rail: self.board_rail,
        })
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
#[serde(bound(serialize = "K: Serialize", deserialize = "K: Deserialize<'de> + Ord"))]
struct LayoutFile<K = TaskId> {
    schema: String,
    layout: KeyedWorkspaceLayout<K>,
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
    ///
    /// Remains the raw-`TaskId` path (`v1`–`v5`). Host-qualified loads use
    /// [`Self::load_keyed`].
    pub fn load(&self) -> WorkspaceLayout {
        let Some(bytes) = read_bounded(&self.path) else {
            return WorkspaceLayout::default();
        };
        let Ok(file) = serde_json::from_slice::<LayoutFile<TaskId>>(&bytes) else {
            return WorkspaceLayout::default();
        };
        match file.schema.as_str() {
            LAYOUT_SCHEMA_V5 => file.layout.sanitized(),
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

    /// Load a host-qualified layout.
    ///
    /// - `v1`–`v5` parse through the legacy raw-`TaskId` path, then each task is
    ///   rewritten with the caller-supplied local-profile owner mapper.
    /// - `v6` deserializes `K` directly and never reinterprets a foreign/corrupt
    ///   owner as local.
    ///
    /// Read-only: never writes. Mapping collisions and corrupt keyed payloads
    /// fail closed to defaults without touching disk.
    pub fn load_keyed<K, F>(&self, map_legacy_task: F) -> KeyedWorkspaceLayout<K>
    where
        K: Clone + Ord + Eq + Serialize + DeserializeOwned + TerminalCenterKey,
        F: FnMut(TaskId) -> K,
    {
        let Some(bytes) = read_bounded(&self.path) else {
            return KeyedWorkspaceLayout::default();
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return KeyedWorkspaceLayout::default();
        };
        let Some(schema) = value
            .get("schema")
            .and_then(|schema| schema.as_str())
            .map(str::to_owned)
        else {
            return KeyedWorkspaceLayout::default();
        };
        match schema.as_str() {
            LAYOUT_SCHEMA_V6 => {
                let Ok(file) = serde_json::from_value::<LayoutFile<K>>(value) else {
                    return KeyedWorkspaceLayout::default();
                };
                file.layout.sanitized()
            }
            LAYOUT_SCHEMA_V1 | LAYOUT_SCHEMA_V2 | LAYOUT_SCHEMA_V3 | LAYOUT_SCHEMA_V4
            | LAYOUT_SCHEMA_V5 => {
                let Ok(file) = serde_json::from_value::<LayoutFile<TaskId>>(value) else {
                    return KeyedWorkspaceLayout::default();
                };
                let mut layout = match schema.as_str() {
                    LAYOUT_SCHEMA_V1 => {
                        let mut layout = file.layout.sanitized();
                        layout.dock_collapsed = true;
                        layout
                    }
                    _ => file.layout.sanitized(),
                };
                layout.sanitize_task_workspace();
                match layout.map_legacy_task_keys(map_legacy_task) {
                    Ok(mapped) => mapped.sanitized(),
                    Err(_) => KeyedWorkspaceLayout::default(),
                }
            }
            _ => KeyedWorkspaceLayout::default(),
        }
    }

    pub fn save(&self, layout: WorkspaceLayout) -> io::Result<()> {
        let file = LayoutFile {
            schema: LAYOUT_SCHEMA.to_string(),
            layout: layout.sanitized(),
        };
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.write_bounded(&bytes)
    }

    /// Persist a host-qualified layout as schema `v6`.
    pub fn save_keyed<K>(&self, layout: KeyedWorkspaceLayout<K>) -> io::Result<()>
    where
        K: Clone + Ord + Eq + Serialize + DeserializeOwned + TerminalCenterKey,
    {
        let file = LayoutFile {
            schema: LAYOUT_SCHEMA_V6.to_string(),
            layout: layout.sanitized(),
        };
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.write_bounded(&bytes)
    }

    fn write_bounded(&self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() as u64 > MAX_LAYOUT_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "workspace layout exceeds its storage limit",
            ));
        }
        write_atomically(&self.path, bytes)
    }
}

fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_LAYOUT_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_LAYOUT_FILE_BYTES).then_some(bytes)
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

    /// Ruling (f): Done on a ZOOMED panel must not leave the workspace zoomed
    /// on a pane that no longer exists.
    ///
    /// Done settles the task, the task leaves the live projection, and THIS is
    /// the pass that removes its pane -- so the invariant is asserted where it
    /// actually has to hold, not on `remove_pane` in isolation.
    #[test]
    fn settling_a_zoomed_task_removes_its_pane_and_leaves_nothing_zoomed() {
        let settled = TaskId::new();
        let survivor = TaskId::new();
        let mut workspace = crate::ui::task_workspace::TaskWorkspace::single(survivor);
        workspace
            .insert_after_focused(settled, crate::ui::task_workspace::Axis::Horizontal)
            .expect("second pane");
        let zoomed_pane = workspace.pane_for_task(settled).expect("settled pane").id;
        workspace
            .zoom(zoomed_pane)
            .expect("zoom the pane Done acts on");
        assert_eq!(workspace.zoomed(), Some(zoomed_pane));

        let mut layout = WorkspaceLayout {
            selected_task: Some(survivor),
            task_workspace: Some(workspace),
            ..WorkspaceLayout::default()
        };
        layout.reconcile_task_workspace(&[survivor]);

        let workspace = layout.task_workspace.as_ref().expect("workspace survives");
        assert_eq!(workspace.pane_count(), 1);
        assert!(!workspace.contains_task(settled));
        assert_eq!(
            workspace.zoomed(),
            None,
            "a zoom pointing at a removed pane would fill the canvas with nothing"
        );
    }

    /// The right dock is retired: a file written before it was, which carries
    /// an expanded dock, must not reopen it beside the panel already showing
    /// the same tool.
    #[test]
    fn a_stored_expanded_dock_loads_collapsed() {
        let layout = WorkspaceLayout {
            dock_collapsed: false,
            ..WorkspaceLayout::default()
        };
        assert!(layout.sanitized().dock_collapsed);
    }

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
        // The dock is retired, so its rail is zero at every window size and the
        // `>= DOCK_MIN` floor this line used to assert is now unreachable by
        // construction. Asserting the zero instead of deleting the line: the
        // fitting order below -- inbox and sidebar keep their floors while the
        // rails give way -- is what the test is actually for, and it stands.
        assert_eq!(fitted.dock_width, 0.0);
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
        let task = TaskId::new();
        let layout = WorkspaceLayout {
            active_dock_tab: Some("Browser".into()),
            project_scope_workspace_id: Some("project-1".into()),
            task_workspace: Some(TaskWorkspace::single(task)),
            task_center_terminal: {
                let mut map = BTreeMap::new();
                map.insert(task.center_preference_key(), true);
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
        assert_eq!(
            loaded
                .task_workspace
                .as_ref()
                .and_then(|workspace| workspace.view_of(task)),
            Some(PaneView::Terminal),
            "the retired center-surface map lands on the pane that owns the view"
        );
        assert!(loaded.task_center_terminal.is_empty());
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

    #[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
    struct TestOwnerKey {
        host: String,
        task: TaskId,
    }

    impl TerminalCenterKey for TestOwnerKey {
        fn center_preference_key(&self) -> String {
            task_center_terminal_preference_key(self).expect("serialize owner key")
        }
    }

    fn local_owner(task: TaskId) -> TestOwnerKey {
        TestOwnerKey {
            host: "local-profile".into(),
            task,
        }
    }

    #[test]
    fn fleet_center_preferences_preserve_host_for_the_same_task() {
        use crate::client::{HostId, HostTaskKey};
        let task = TaskId::new();
        let local = HostTaskKey::new(HostId::LocalProfile("dev".into()), task);
        let remote = HostTaskKey::new(HostId::Remote([1; 16]), task);
        assert_ne!(
            local.center_preference_key(),
            remote.center_preference_key()
        );
        assert_eq!(
            serde_json::from_str::<HostTaskKey>(&local.center_preference_key()).unwrap(),
            local
        );
        let mut layout = KeyedWorkspaceLayout::<HostTaskKey>::default();
        layout
            .task_center_terminal
            .insert(local.center_preference_key(), true);
        layout
            .task_center_terminal
            .insert(remote.center_preference_key(), false);
        layout.reconcile_task_workspace(&[remote.clone()]);
        assert_eq!(
            layout.task_center_terminal,
            BTreeMap::from([(remote.center_preference_key(), false)])
        );
    }

    #[test]
    fn keyed_layout_round_trips_two_owners_sharing_raw_task_id() {
        use crate::ui::task_workspace::{Axis, WorkspaceNode};

        let shared = TaskId::new();
        let local = local_owner(shared);
        let remote = TestOwnerKey {
            host: "remote-host".into(),
            task: shared,
        };
        let mut workspace = Workspace::single(local.clone());
        workspace
            .insert_after_focused(remote.clone(), Axis::Horizontal)
            .unwrap();
        let split_id = match workspace.root().unwrap() {
            WorkspaceNode::Split { id, .. } => *id,
            _ => panic!("expected split"),
        };
        workspace.pin_task_axis_size(local.clone(), 260.0).unwrap();
        let mut layout = KeyedWorkspaceLayout {
            selected_task: Some(remote.clone()),
            task_workspace: Some(workspace),
            task_center_terminal: BTreeMap::from([
                (local.center_preference_key(), true),
                (remote.center_preference_key(), false),
            ]),
            task_composer_preferences: BTreeMap::from([
                (
                    local.center_preference_key(),
                    TaskComposerPreferences {
                        provider: crate::providers::ProviderKind::Codex,
                        launch_options: crate::providers::ProviderLaunchOptions {
                            model: crate::providers::ProviderModel::CodexTerra,
                            ..crate::providers::ProviderLaunchOptions::default()
                        },
                    },
                ),
                (
                    remote.center_preference_key(),
                    TaskComposerPreferences {
                        provider: crate::providers::ProviderKind::ClaudeCode,
                        launch_options: crate::providers::ProviderLaunchOptions {
                            model: crate::providers::ProviderModel::ClaudeOpus,
                            ..crate::providers::ProviderLaunchOptions::default()
                        },
                    },
                ),
            ]),
            ..KeyedWorkspaceLayout::default()
        };
        layout.sanitize_task_workspace();

        let directory = tempfile::tempdir().expect("temp dir");
        let store = WorkspaceLayoutStore::at_profile_root(directory.path());
        store.save_keyed(layout.clone()).expect("save keyed");
        let loaded = store.load_keyed(local_owner);
        assert_eq!(loaded.selected_task, Some(remote.clone()));
        let workspace = loaded.task_workspace.as_ref().expect("workspace");
        assert_eq!(workspace.pane_count(), 2);
        assert!(workspace.contains_task(local.clone()));
        assert!(workspace.contains_task(remote.clone()));
        assert_eq!(
            workspace.split_child_allocation(split_id, 0),
            Some(crate::ui::task_workspace::Allocation::Pinned { logical_px: 260.0 })
        );
        assert_eq!(
            workspace.view_of(local.clone()),
            Some(PaneView::Terminal),
            "the full owner key still separates two hosts' views"
        );
        assert_eq!(
            loaded
                .task_composer_preferences
                .get(&local.center_preference_key())
                .map(|preferences| preferences.launch_options.model),
            Some(crate::providers::ProviderModel::CodexTerra)
        );
        assert_eq!(
            loaded
                .task_composer_preferences
                .get(&remote.center_preference_key())
                .map(|preferences| preferences.launch_options.model),
            Some(crate::providers::ProviderModel::ClaudeOpus)
        );
        assert_eq!(
            workspace.view_of(remote.clone()),
            Some(PaneView::Conversation),
            "the other owner keeps its own view rather than inheriting one"
        );
        let bytes = fs::read(store.path()).expect("read");
        let saved: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(saved["schema"], LAYOUT_SCHEMA_V6);
        // load_keyed is read-only: disk unchanged by a second load.
        let _ = store.load_keyed(local_owner);
        assert_eq!(fs::read(store.path()).expect("reread"), bytes);
    }

    #[test]
    fn legacy_task_center_terminal_true_seeds_the_terminal_view_once() {
        let terminal = TaskId::new();
        let conversation = TaskId::new();
        let mut workspace = TaskWorkspace::single(terminal);
        workspace
            .insert_after_focused(conversation, crate::ui::task_workspace::Axis::Horizontal)
            .expect("second pane");
        let mut layout = KeyedWorkspaceLayout::<TaskId>::default();
        layout.task_workspace = Some(workspace);
        layout
            .task_center_terminal
            .insert(terminal.center_preference_key(), true);
        layout
            .task_center_terminal
            .insert(conversation.center_preference_key(), false);

        let sanitized = layout.sanitized();

        let workspace = sanitized.task_workspace.as_ref().expect("workspace");
        assert_eq!(workspace.view_of(terminal), Some(PaneView::Terminal));
        assert_eq!(
            workspace.view_of(conversation),
            Some(PaneView::Conversation)
        );
        assert!(
            sanitized.task_center_terminal.is_empty(),
            "the map is consumed, not kept"
        );
    }

    #[test]
    fn v5_split_tree_migrates_through_load_keyed_preserving_geometry() {
        use crate::ui::task_workspace::{Allocation, Axis, PanePresentation, WorkspaceNode};

        let first = TaskId::new();
        let second = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, Axis::Horizontal)
            .unwrap();
        let focused = workspace.focused_pane_id();
        let previous = workspace.previous_focus();
        let split_id = match workspace.root().unwrap() {
            WorkspaceNode::Split { id, .. } => *id,
            _ => panic!("expected split"),
        };
        workspace.pin_task_axis_size(first, 300.0).unwrap();
        workspace
            .set_presentation(second, PanePresentation::Minimised)
            .unwrap();
        let layout = WorkspaceLayout {
            selected_task: Some(second),
            task_workspace: Some(workspace),
            task_center_terminal: BTreeMap::from([(first.to_string(), true)]),
            sidebar_width: 275.0,
            ..WorkspaceLayout::default()
        };
        let directory = tempfile::tempdir().expect("temp dir");
        let store = WorkspaceLayoutStore::at_profile_root(directory.path());
        store.save(layout).expect("save v5");

        let migrated = store.load_keyed(local_owner);
        assert_eq!(migrated.sidebar_width, 275.0);
        assert_eq!(migrated.selected_task, Some(local_owner(second)));
        let workspace = migrated.task_workspace.as_ref().expect("workspace");
        assert_eq!(workspace.focused_pane_id(), focused);
        assert_eq!(workspace.previous_focus(), previous);
        assert_eq!(
            workspace.presentation(local_owner(second)),
            Some(PanePresentation::Full),
            "minimisation belongs to the viewport, so it never restores from disk"
        );
        assert_eq!(
            workspace.split_child_allocation(split_id, 0),
            Some(Allocation::Pinned { logical_px: 300.0 })
        );
        assert_eq!(
            workspace.view_of(local_owner(first)),
            Some(PaneView::Terminal),
            "the legacy raw-TaskId preference migrates onto the owning pane"
        );
        let bytes = fs::read(store.path()).expect("read");
        let saved: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");
        assert_eq!(
            saved["schema"], LAYOUT_SCHEMA_V5,
            "load_keyed must not rewrite legacy disk"
        );
    }

    #[test]
    fn v1_layout_load_keyed_uses_explicit_local_owner_and_collapses_dock() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = WorkspaceLayoutStore::at_profile_root(directory.path());
        let selected = TaskId::new();
        let v1 = serde_json::json!({
            "schema": "devmanager.workspace-layout/v1",
            "layout": {
                "sidebar_width": 275.0,
                "inbox_width": 410.0,
                "dock_width": 520.0,
                "terminal_height": 240.0,
                "dock_collapsed": false,
                "selected_task": selected
            }
        });
        fs::write(store.path(), serde_json::to_vec_pretty(&v1).unwrap()).unwrap();

        let migrated = store.load_keyed(local_owner);
        assert!(migrated.dock_collapsed);
        assert_eq!(migrated.selected_task, Some(local_owner(selected)));
        assert_eq!(
            migrated
                .task_workspace
                .as_ref()
                .and_then(|workspace| workspace.focused_task()),
            Some(local_owner(selected))
        );
    }

    #[test]
    fn mapping_collision_fails_closed_without_modifying_disk() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = WorkspaceLayoutStore::at_profile_root(directory.path());
        let first = TaskId::new();
        let second = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        workspace
            .insert_after_focused(second, crate::ui::task_workspace::Axis::Horizontal)
            .unwrap();
        let layout = WorkspaceLayout {
            selected_task: Some(second),
            task_workspace: Some(workspace),
            ..WorkspaceLayout::default()
        };
        store.save(layout).expect("save");
        let before = fs::read(store.path()).expect("read before");

        let collided = local_owner(TaskId::new());
        let loaded = store.load_keyed(|_| collided.clone());
        assert_eq!(loaded, KeyedWorkspaceLayout::default());
        assert_eq!(fs::read(store.path()).expect("read after"), before);
    }

    #[test]
    fn corrupt_or_foreign_v6_fails_closed() {
        let directory = tempfile::tempdir().expect("temp dir");
        let store = WorkspaceLayoutStore::at_profile_root(directory.path());
        fs::write(store.path(), b"{not json").unwrap();
        assert_eq!(
            store.load_keyed(local_owner),
            KeyedWorkspaceLayout::default()
        );

        let foreign = serde_json::json!({
            "schema": "devmanager.workspace-layout/v6",
            "layout": {
                "sidebar_width": 260.0,
                "inbox_width": 320.0,
                "dock_width": 360.0,
                "terminal_height": 200.0,
                "selected_task": { "host": 1, "task": "not-a-uuid" }
            }
        });
        fs::write(store.path(), serde_json::to_vec_pretty(&foreign).unwrap()).unwrap();
        assert_eq!(
            store.load_keyed(local_owner),
            KeyedWorkspaceLayout::default()
        );
    }

    #[test]
    fn oversized_keyed_layout_does_not_replace_saved_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let store = WorkspaceLayoutStore::at_profile_root(directory.path());
        store.save(WorkspaceLayout::default()).unwrap();
        let before = fs::read(store.path()).unwrap();
        let layout: KeyedWorkspaceLayout<TestOwnerKey> = KeyedWorkspaceLayout {
            active_dock_tab: Some("x".repeat(MAX_LAYOUT_FILE_BYTES as usize)),
            ..KeyedWorkspaceLayout::default()
        };
        assert_eq!(
            store.save_keyed(layout).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(store.path()).unwrap(), before);
    }

    #[test]
    fn layout_without_project_colours_or_rail_still_loads() {
        let json = serde_json::to_value(KeyedWorkspaceLayout::<TaskId>::default()).expect("json");
        let mut stripped = json.as_object().cloned().expect("object");
        stripped.remove("project_colours");
        stripped.remove("board_rail");
        let layout: KeyedWorkspaceLayout<TaskId> =
            serde_json::from_value(serde_json::Value::Object(stripped)).expect("older file loads");
        assert!(layout.project_colours.is_empty());
        assert!(!layout.board_rail);
    }

    #[test]
    fn sanitized_clamps_out_of_range_project_colour_slots() {
        let layout = KeyedWorkspaceLayout::<TaskId> {
            project_colours: BTreeMap::from([
                ("project-a".to_owned(), 3u8),
                ("project-b".to_owned(), 200u8),
            ]),
            ..KeyedWorkspaceLayout::default()
        }
        .sanitized();
        assert_eq!(layout.project_colours.get("project-a").copied(), Some(3));
        assert_eq!(layout.project_colours.get("project-b").copied(), Some(0));
    }
}
