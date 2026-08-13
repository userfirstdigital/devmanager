//! Pure Task Cockpit presentation metadata over the shared client action catalog.
//!
//! This module does not define action identifiers, GPUI actions, command
//! factories, or host availability. It only decorates the existing catalog
//! with local presentation and accessibility metadata.

use crate::client::action::{
    self, ActionDescriptor, ActionRisk, ActionScope, ACTION_HOST_ACTIONS, ACTION_HOST_STATUS,
    ACTION_TASK_CREATE, ACTION_TASK_LIST, ACTION_TASK_RENAME, ACTION_TASK_SHOW,
};
use crate::domain::id::TaskId;
use crate::ui::components::interaction::FocusEpoch;
use crate::ui::components::{AccessibilityMetadata, AccessibleRole, InteractionStateModel};
use gpui::KeyBinding;

// GPUI adapters are the single native dispatch surface for the shared client
// action catalog. They carry no host state and do not mint additional IDs.
#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "host.actions")]
pub struct HostActions;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "host.status")]
pub struct HostStatus;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "task.list")]
pub struct TaskListAction;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "task.show")]
pub struct TaskShow;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "task.create")]
pub struct TaskCreate;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "task.rename")]
pub struct TaskRename;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.open_palette")]
pub struct NativeOpenPalette;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.open_task_switcher")]
pub struct NativeOpenTaskSwitcher;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.open_command_palette")]
pub struct NativeOpenCommandPalette;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.dock_changes")]
pub struct NativeDockChanges;

/// Collapse or restore the configuration rail.
#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.toggle_sidebar")]
pub struct NativeToggleSidebar;

/// Collapse or restore the context dock column.
#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.toggle_dock")]
pub struct NativeToggleDock;

/// Collapse or restore the terminal strip.
#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.toggle_terminal")]
pub struct NativeToggleTerminal;

/// Restore every pane to its shipped size.
#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.reset_layout")]
pub struct NativeResetLayout;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.dock_files")]
pub struct NativeDockFiles;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.dock_terminal")]
pub struct NativeDockTerminal;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.dock_browser")]
pub struct NativeDockBrowser;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.dock_services")]
pub struct NativeDockServices;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.dock_artifacts")]
pub struct NativeDockArtifacts;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.dock_review")]
pub struct NativeDockReview;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.open_terminal")]
pub struct NativeOpenTerminal;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.dismiss_transient")]
pub struct NativeDismissTransient;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "dock.tool.changes")]
pub struct DockSelectChanges;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "dock.tool.files")]
pub struct DockSelectFiles;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "dock.tool.terminal")]
pub struct DockSelectTerminal;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "dock.tool.browser")]
pub struct DockSelectBrowser;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "dock.tool.services")]
pub struct DockSelectServices;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "dock.tool.artifacts")]
pub struct DockSelectArtifacts;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "dock.tool.review")]
pub struct DockSelectReview;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "dock.terminal.toggle")]
pub struct DockToggleRawTerminal;

pub const TASK_COCKPIT_ACTION_NAMES: [&str; 6] = [
    ACTION_HOST_ACTIONS,
    ACTION_HOST_STATUS,
    ACTION_TASK_LIST,
    ACTION_TASK_SHOW,
    ACTION_TASK_CREATE,
    ACTION_TASK_RENAME,
];

pub fn register_task_cockpit_bindings(cx: &mut gpui::App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-alt-1", HostActions, None),
        KeyBinding::new("ctrl-alt-2", HostStatus, None),
        KeyBinding::new("ctrl-alt-3", TaskListAction, None),
        KeyBinding::new("ctrl-alt-4", TaskShow, None),
        KeyBinding::new("ctrl-alt-5", TaskCreate, None),
        KeyBinding::new("ctrl-alt-6", TaskRename, None),
    ]);
}

pub fn register_native_keyboard_bindings(cx: &mut gpui::App) {
    register_task_cockpit_bindings(cx);
    cx.bind_keys([
        KeyBinding::new("ctrl-k", NativeOpenPalette, None),
        KeyBinding::new("ctrl-p", NativeOpenTaskSwitcher, None),
        KeyBinding::new("ctrl-shift-p", NativeOpenCommandPalette, None),
        KeyBinding::new("alt-1", NativeDockChanges, None),
        KeyBinding::new("alt-2", NativeDockFiles, None),
        KeyBinding::new("alt-3", NativeDockTerminal, None),
        KeyBinding::new("alt-4", NativeDockBrowser, None),
        KeyBinding::new("alt-5", NativeDockServices, None),
        KeyBinding::new("alt-6", NativeDockArtifacts, None),
        KeyBinding::new("alt-7", NativeDockReview, None),
        KeyBinding::new("ctrl-`", NativeOpenTerminal, None),
        KeyBinding::new("ctrl-b", NativeToggleSidebar, None),
        KeyBinding::new("ctrl-alt-b", NativeToggleDock, None),
        KeyBinding::new("ctrl-j", NativeToggleTerminal, None),
        KeyBinding::new("ctrl-alt-0", NativeResetLayout, None),
        KeyBinding::new("escape", NativeDismissTransient, None),
    ]);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionPresentationKind {
    Navigation,
    Primary,
    Secondary,
    Destructive,
}

impl ActionPresentationKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Navigation => "Navigation",
            Self::Primary => "Primary",
            Self::Secondary => "Secondary",
            Self::Destructive => "Destructive",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionAvailability {
    Available,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionPresentation {
    descriptor: &'static ActionDescriptor,
    presentation: ActionPresentationKind,
    shortcut: Option<&'static str>,
    availability: ActionAvailability,
    disabled: bool,
    disabled_reason: Option<&'static str>,
    accessibility: AccessibilityMetadata,
}

impl ActionPresentation {
    pub fn descriptor(&self) -> &'static ActionDescriptor {
        self.descriptor
    }

    pub fn id(&self) -> &'static str {
        self.descriptor.id
    }

    pub fn presentation(&self) -> ActionPresentationKind {
        self.presentation
    }

    pub fn presentation_label(&self) -> &'static str {
        self.presentation.label()
    }

    pub fn shortcut(&self) -> Option<&'static str> {
        self.shortcut
    }

    pub fn availability(&self) -> ActionAvailability {
        self.availability
    }

    pub fn disabled(&self) -> bool {
        self.disabled
    }

    pub fn disabled_reason(&self) -> Option<&'static str> {
        self.disabled_reason
    }

    pub fn accessibility(&self) -> &AccessibilityMetadata {
        &self.accessibility
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ShortcutKey {
    Character(char),
    Digit(u8),
    Backtick,
    Escape,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeyboardShortcut {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: ShortcutKey,
}

impl KeyboardShortcut {
    pub const fn new(ctrl: bool, shift: bool, alt: bool, key: ShortcutKey) -> Self {
        Self {
            ctrl,
            shift,
            alt,
            key,
        }
    }

    pub const fn ctrl(key: ShortcutKey) -> Self {
        Self::new(true, false, false, key)
    }

    pub const fn ctrl_shift(key: ShortcutKey) -> Self {
        Self::new(true, true, false, key)
    }

    pub const fn alt(key: ShortcutKey) -> Self {
        Self::new(false, false, true, key)
    }

    pub const fn escape() -> Self {
        Self::new(false, false, false, ShortcutKey::Escape)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DockTool {
    Changes,
    Files,
    Terminal,
    Browser,
    Services,
    Artifacts,
    Review,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyboardAction {
    OpenPalette,
    OpenTaskSwitcher,
    OpenCommandPalette,
    OpenTaskDetails,
    SelectDock(DockTool),
    OpenTerminal,
    DismissTransient,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyboardBinding {
    pub shortcut: KeyboardShortcut,
    pub action: KeyboardAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyboardModelError {
    ShortcutConflict(KeyboardShortcut),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyboardModel {
    bindings: Vec<KeyboardBinding>,
}

impl KeyboardModel {
    pub fn new(bindings: Vec<KeyboardBinding>) -> Result<Self, KeyboardModelError> {
        for (index, binding) in bindings.iter().enumerate() {
            if bindings[..index]
                .iter()
                .any(|prior| prior.shortcut == binding.shortcut)
            {
                return Err(KeyboardModelError::ShortcutConflict(binding.shortcut));
            }
        }
        Ok(Self { bindings })
    }

    pub fn bindings(&self) -> &[KeyboardBinding] {
        &self.bindings
    }

    pub fn resolve(&self, shortcut: KeyboardShortcut) -> Option<KeyboardAction> {
        self.bindings
            .iter()
            .find(|binding| binding.shortcut == shortcut)
            .map(|binding| binding.action)
    }

    /// Resolve a local shortcut through the shared interaction policy.
    /// Exact Escape remains available to dismiss a transient layer even when
    /// the active control is disabled or loading; every action still requires
    /// the current focus epoch, and every other shortcut also requires
    /// activation state.
    pub fn activate(
        &self,
        shortcut: KeyboardShortcut,
        interaction: &InteractionStateModel,
        focus_epoch: FocusEpoch,
    ) -> Option<KeyboardAction> {
        let action = self.resolve(shortcut)?;
        if interaction.focus_epoch() != focus_epoch {
            return None;
        }
        let exact_escape_dismiss =
            shortcut == KeyboardShortcut::escape() && action == KeyboardAction::DismissTransient;
        if exact_escape_dismiss || interaction.state().can_activate() {
            Some(action)
        } else {
            None
        }
    }
}

impl Default for KeyboardModel {
    fn default() -> Self {
        let mut bindings = vec![
            KeyboardBinding {
                shortcut: KeyboardShortcut::ctrl(ShortcutKey::Character('k')),
                action: KeyboardAction::OpenPalette,
            },
            KeyboardBinding {
                shortcut: KeyboardShortcut::ctrl(ShortcutKey::Character('p')),
                action: KeyboardAction::OpenTaskSwitcher,
            },
        ];
        let command_palette_alias = KeyboardShortcut::ctrl_shift(ShortcutKey::Character('p'));
        if !bindings
            .iter()
            .any(|binding| binding.shortcut == command_palette_alias)
        {
            bindings.push(KeyboardBinding {
                shortcut: command_palette_alias,
                action: KeyboardAction::OpenCommandPalette,
            });
        }
        for (digit, action) in [
            (1, DockTool::Changes),
            (2, DockTool::Files),
            (3, DockTool::Terminal),
            (4, DockTool::Browser),
            (5, DockTool::Services),
            (6, DockTool::Artifacts),
            (7, DockTool::Review),
        ] {
            bindings.push(KeyboardBinding {
                shortcut: KeyboardShortcut::alt(ShortcutKey::Digit(digit)),
                action: KeyboardAction::SelectDock(action),
            });
        }
        bindings.extend([
            KeyboardBinding {
                shortcut: KeyboardShortcut::ctrl(ShortcutKey::Character('m')),
                action: KeyboardAction::OpenTaskDetails,
            },
            KeyboardBinding {
                shortcut: KeyboardShortcut::ctrl(ShortcutKey::Backtick),
                action: KeyboardAction::OpenTerminal,
            },
            KeyboardBinding {
                shortcut: KeyboardShortcut::escape(),
                action: KeyboardAction::DismissTransient,
            },
        ]);
        Self::new(bindings).expect("default Task Cockpit shortcuts are conflict-free")
    }
}

const NO_SELECTED_TASK: &str = "Select a task before using this action.";

fn presentation_for(
    descriptor: &ActionDescriptor,
) -> (ActionPresentationKind, Option<&'static str>) {
    match descriptor.id {
        ACTION_HOST_ACTIONS => (ActionPresentationKind::Navigation, Some("Ctrl+Alt+1")),
        ACTION_HOST_STATUS => (ActionPresentationKind::Navigation, Some("Ctrl+Alt+2")),
        ACTION_TASK_LIST => (ActionPresentationKind::Navigation, Some("Ctrl+Alt+3")),
        ACTION_TASK_SHOW => (ActionPresentationKind::Secondary, Some("Ctrl+Alt+4")),
        ACTION_TASK_CREATE => (ActionPresentationKind::Primary, Some("Ctrl+Alt+5")),
        ACTION_TASK_RENAME => (ActionPresentationKind::Secondary, Some("Ctrl+Alt+6")),
        _ if descriptor.risk == ActionRisk::Mutating => (ActionPresentationKind::Primary, None),
        _ if descriptor.scope == ActionScope::Task => (ActionPresentationKind::Secondary, None),
        _ => (ActionPresentationKind::Navigation, None),
    }
}

fn requires_selected_task(descriptor: &ActionDescriptor) -> bool {
    matches!(descriptor.id, ACTION_TASK_SHOW | ACTION_TASK_RENAME)
}

/// Decorate the shared catalog for the current local selection.
///
/// `selected_task` is local shell state. No host capability, subscription,
/// or mutation is consulted here, and every returned descriptor points back
/// to an entry from [`crate::client::action::catalog`].
pub fn catalog(selected_task: Option<TaskId>) -> Vec<ActionPresentation> {
    action::catalog()
        .iter()
        .map(|descriptor| {
            let unavailable = requires_selected_task(descriptor) && selected_task.is_none();
            let (presentation, shortcut) = presentation_for(descriptor);
            ActionPresentation {
                descriptor,
                presentation,
                shortcut,
                availability: if unavailable {
                    ActionAvailability::Unavailable
                } else {
                    ActionAvailability::Available
                },
                disabled: unavailable,
                disabled_reason: unavailable.then_some(NO_SELECTED_TASK),
                accessibility: {
                    let mut metadata =
                        AccessibilityMetadata::new(AccessibleRole::Button, descriptor.title)
                            .expect("catalog action title is valid accessibility text");
                    metadata.set_disabled(unavailable);
                    metadata
                        .set_description(if unavailable {
                            NO_SELECTED_TASK
                        } else {
                            descriptor.description
                        })
                        .expect("catalog action description is valid accessibility text");
                    metadata
                },
            }
        })
        .collect()
}
