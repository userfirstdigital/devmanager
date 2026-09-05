//! Pure Task Cockpit presentation metadata over the shared client action catalog.
//!
//! This module does not define action identifiers, GPUI actions, command
//! factories, or host availability. It only decorates the existing catalog
//! with local presentation and accessibility metadata.

use crate::client::action::{
    self, ActionDescriptor, ActionRisk, ActionScope, ACTION_HOST_ACTIONS, ACTION_HOST_STATUS,
    ACTION_TASK_ARCHIVE, ACTION_TASK_CREATE, ACTION_TASK_LIST, ACTION_TASK_RENAME,
    ACTION_TASK_SHOW,
};
use crate::domain::id::TaskId;
use crate::ui::components::interaction::FocusEpoch;
use crate::ui::components::{AccessibilityMetadata, AccessibleRole, InteractionStateModel};
use crate::ui::task_workspace::layout::{Edge, PaneView};
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

/// Switch the selected task's center canvas between Conversation and Terminal.
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

/// Open one new plain shell terminal on the selected Task.
#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.open_shell_terminal")]
pub struct NativeOpenShellTerminal;

/// Focus the next chip on the selected Task's terminal strip.
#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.cycle_terminal")]
pub struct NativeCycleTerminal;

/// Focus the previous chip on the selected Task's terminal strip.
#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.cycle_terminal_back")]
pub struct NativeCycleTerminalBack;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.dismiss_transient")]
pub struct NativeDismissTransient;

// The panel's own chords (spec 10). Each is a unit action bound to exactly one
// keystroke here and resolved through [`KeyboardModel`] in the shell, so the
// chord table and the meaning table cannot drift: the binding below names the
// keystroke, and `KeyboardModel::default` names what it does.

/// Settle the focused panel's task (Ctrl+D).
#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.panel_settle")]
pub struct NativePanelSettle;

/// Fill the canvas with the focused panel, and back (Ctrl+Shift+Z).
#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.panel_zoom")]
pub struct NativePanelZoom;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.panel_view_1")]
pub struct NativePanelView1;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.panel_view_2")]
pub struct NativePanelView2;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.panel_view_3")]
pub struct NativePanelView3;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.panel_view_4")]
pub struct NativePanelView4;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.panel_view_5")]
pub struct NativePanelView5;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.panel_focus_left")]
pub struct NativePanelFocusLeft;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.panel_focus_right")]
pub struct NativePanelFocusRight;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.panel_focus_up")]
pub struct NativePanelFocusUp;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.panel_focus_down")]
pub struct NativePanelFocusDown;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.panel_move_left")]
pub struct NativePanelMoveLeft;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.panel_move_right")]
pub struct NativePanelMoveRight;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.panel_move_up")]
pub struct NativePanelMoveUp;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "native.panel_move_down")]
pub struct NativePanelMoveDown;

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
        // The Alt+digit dock chords are gone with the right dock: the digits
        // belong to the panel's view tabs now, and `KeyboardModel` says None
        // for Alt+digit. The `NativeDock*` actions survive only because the
        // dock's own affordances still name them.
        KeyBinding::new("ctrl-1", NativePanelView1, None),
        KeyBinding::new("ctrl-2", NativePanelView2, None),
        KeyBinding::new("ctrl-3", NativePanelView3, None),
        KeyBinding::new("ctrl-4", NativePanelView4, None),
        KeyBinding::new("ctrl-5", NativePanelView5, None),
        KeyBinding::new("ctrl-d", NativePanelSettle, None),
        KeyBinding::new("ctrl-shift-z", NativePanelZoom, None),
        KeyBinding::new("ctrl-left", NativePanelFocusLeft, None),
        KeyBinding::new("ctrl-right", NativePanelFocusRight, None),
        KeyBinding::new("ctrl-up", NativePanelFocusUp, None),
        KeyBinding::new("ctrl-down", NativePanelFocusDown, None),
        KeyBinding::new("ctrl-shift-left", NativePanelMoveLeft, None),
        KeyBinding::new("ctrl-shift-right", NativePanelMoveRight, None),
        KeyBinding::new("ctrl-shift-up", NativePanelMoveUp, None),
        KeyBinding::new("ctrl-shift-down", NativePanelMoveDown, None),
        KeyBinding::new("ctrl-`", NativeOpenTerminal, None),
        KeyBinding::new("ctrl-shift-`", NativeOpenShellTerminal, None),
        // Cycling is scoped to the terminal surface's own key context so it
        // never steals Ctrl+Tab from the rest of the shell.
        KeyBinding::new("ctrl-tab", NativeCycleTerminal, Some("TerminalFocused")),
        KeyBinding::new(
            "ctrl-shift-tab",
            NativeCycleTerminalBack,
            Some("TerminalFocused"),
        ),
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

/// The four arrow keys, as a shortcut key in their own right. Directional pane
/// moves and directional pane focus are the two things a person expects to
/// reach with an arrow, and both need to name a workspace [`Edge`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ArrowKey {
    Left,
    Right,
    Up,
    Down,
}

impl ArrowKey {
    pub const ALL: [ArrowKey; 4] = [Self::Left, Self::Right, Self::Up, Self::Down];

    /// The workspace edge the arrow points at. One definition, so a chord table
    /// and a pane move can never disagree about which way "up" is.
    pub const fn edge(self) -> Edge {
        match self {
            Self::Left => Edge::Left,
            Self::Right => Edge::Right,
            Self::Up => Edge::Top,
            Self::Down => Edge::Bottom,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ShortcutKey {
    Character(char),
    Digit(u8),
    Arrow(ArrowKey),
    Backtick,
    Tab,
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

impl DockTool {
    /// The digit the dock tool used to answer to, kept as a lookup now that the
    /// digits themselves belong to the view tabs. The dock's own affordances --
    /// its tabs, the tools menu and the header's Open -- still identify a tool
    /// by that number, and this is the single place the mapping lives.
    pub const fn from_digit(digit: u8) -> Option<Self> {
        match digit {
            1 => Some(Self::Changes),
            2 => Some(Self::Files),
            3 => Some(Self::Terminal),
            4 => Some(Self::Browser),
            5 => Some(Self::Services),
            6 => Some(Self::Artifacts),
            7 => Some(Self::Review),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyboardAction {
    OpenPalette,
    OpenTaskSwitcher,
    OpenCommandPalette,
    OpenTaskDetails,
    /// Show one view in the selected task's pane. The redesign's five tabs
    /// ([`PaneView::TABS`]) are what the digits now mean.
    SelectView(PaneView),
    /// Dock-tool selection. No longer has a chord of its own -- the digits went
    /// to the view tabs -- but the dock's tabs, menu items and the header's
    /// Open still reach the shell through it.
    SelectDock(DockTool),
    /// Mark the selected task done from the keyboard.
    SettleTask,
    /// Grow the focused pane to the whole workspace, and back.
    ToggleZoom,
    /// Move the focused pane to one edge of the workspace.
    MovePane(Edge),
    /// Move focus to the nearest pane in one direction.
    FocusPane(Edge),
    OpenTerminal,
    /// Open one new plain shell terminal on the selected Task.
    OpenShellTerminal,
    /// Move strip focus one chip forward (or backward) on the selected Task.
    CycleTerminal {
        backwards: bool,
    },
    DismissTransient,
}

/// Whether an armed provider terminal owns this chord instead of the shell.
///
/// Ctrl+D is EOF and the Ctrl+arrows are word motion inside a PTY, so while
/// terminal input is armed those three keystrokes belong to the program on the
/// other end. Every other panel chord -- the view digits, zoom, the palette --
/// has no meaning in a terminal and stays with the shell, so a person does not
/// lose the panel while typing in it.
///
/// This is a rule about the ACTION, not about the keystroke: the binding table
/// is the only place a chord is named, and this is the only place the terminal
/// is allowed to take one back.
pub const fn keyboard_action_yields_to_armed_terminal(action: KeyboardAction) -> bool {
    matches!(
        action,
        KeyboardAction::SettleTask | KeyboardAction::FocusPane(_) | KeyboardAction::MovePane(_)
    )
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
        // The digits select a view, not a dock tool: Ctrl+1..5 are the five
        // tabs in `PaneView::TABS`, in the order they are painted. The Alt+digit
        // dock chords the redesign replaces are gone from the model; the dock's
        // own affordances now reach `SelectDock` directly.
        for (index, view) in PaneView::TABS.iter().enumerate() {
            let digit = u8::try_from(index + 1).expect("five view tabs fit in a digit");
            bindings.push(KeyboardBinding {
                shortcut: KeyboardShortcut::ctrl(ShortcutKey::Digit(digit)),
                action: KeyboardAction::SelectView(*view),
            });
        }
        // Directional pane work: Ctrl+arrow moves focus, Ctrl+Shift+arrow moves
        // the pane itself. Both name the same edge, so the two tables are one.
        for arrow in ArrowKey::ALL {
            bindings.push(KeyboardBinding {
                shortcut: KeyboardShortcut::ctrl(ShortcutKey::Arrow(arrow)),
                action: KeyboardAction::FocusPane(arrow.edge()),
            });
            bindings.push(KeyboardBinding {
                shortcut: KeyboardShortcut::ctrl_shift(ShortcutKey::Arrow(arrow)),
                action: KeyboardAction::MovePane(arrow.edge()),
            });
        }
        bindings.extend([
            KeyboardBinding {
                shortcut: KeyboardShortcut::ctrl(ShortcutKey::Character('d')),
                action: KeyboardAction::SettleTask,
            },
            KeyboardBinding {
                shortcut: KeyboardShortcut::ctrl_shift(ShortcutKey::Character('z')),
                action: KeyboardAction::ToggleZoom,
            },
            KeyboardBinding {
                shortcut: KeyboardShortcut::ctrl(ShortcutKey::Character('m')),
                action: KeyboardAction::OpenTaskDetails,
            },
            KeyboardBinding {
                shortcut: KeyboardShortcut::ctrl(ShortcutKey::Backtick),
                action: KeyboardAction::OpenTerminal,
            },
            KeyboardBinding {
                shortcut: KeyboardShortcut::ctrl_shift(ShortcutKey::Backtick),
                action: KeyboardAction::OpenShellTerminal,
            },
            KeyboardBinding {
                shortcut: KeyboardShortcut::ctrl(ShortcutKey::Tab),
                action: KeyboardAction::CycleTerminal { backwards: false },
            },
            KeyboardBinding {
                shortcut: KeyboardShortcut::ctrl_shift(ShortcutKey::Tab),
                action: KeyboardAction::CycleTerminal { backwards: true },
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
        ACTION_TASK_ARCHIVE => (ActionPresentationKind::Destructive, None),
        _ if descriptor.risk == ActionRisk::Mutating => (ActionPresentationKind::Primary, None),
        _ if descriptor.scope == ActionScope::Task => (ActionPresentationKind::Secondary, None),
        _ => (ActionPresentationKind::Navigation, None),
    }
}

fn requires_selected_task(descriptor: &ActionDescriptor) -> bool {
    matches!(
        descriptor.id,
        ACTION_TASK_SHOW | ACTION_TASK_RENAME | ACTION_TASK_ARCHIVE
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_armed_terminal_takes_back_eof_and_word_motion_and_nothing_else() {
        for action in [
            KeyboardAction::SettleTask,
            KeyboardAction::FocusPane(Edge::Left),
            KeyboardAction::MovePane(Edge::Bottom),
        ] {
            assert!(
                keyboard_action_yields_to_armed_terminal(action),
                "{action:?} is a PTY key while the terminal is armed"
            );
        }
        for action in [
            KeyboardAction::ToggleZoom,
            KeyboardAction::SelectView(PaneView::Terminal),
            KeyboardAction::OpenPalette,
            KeyboardAction::DismissTransient,
        ] {
            assert!(
                !keyboard_action_yields_to_armed_terminal(action),
                "{action:?} has no meaning inside a PTY and stays with the shell"
            );
        }
    }

    #[test]
    fn every_panel_chord_the_gpui_table_binds_is_one_the_model_resolves() {
        // The gpui bindings and `KeyboardModel` are two halves of one table.
        // A chord bound to an action the model has no answer for would arrive
        // at the shell and do nothing, silently.
        let model = KeyboardModel::default();
        let panel_chords = [
            KeyboardShortcut::ctrl(ShortcutKey::Character('d')),
            KeyboardShortcut::ctrl_shift(ShortcutKey::Character('z')),
            KeyboardShortcut::ctrl(ShortcutKey::Digit(1)),
            KeyboardShortcut::ctrl(ShortcutKey::Digit(5)),
            KeyboardShortcut::ctrl(ShortcutKey::Arrow(ArrowKey::Left)),
            KeyboardShortcut::ctrl(ShortcutKey::Arrow(ArrowKey::Right)),
            KeyboardShortcut::ctrl(ShortcutKey::Arrow(ArrowKey::Up)),
            KeyboardShortcut::ctrl(ShortcutKey::Arrow(ArrowKey::Down)),
            KeyboardShortcut::ctrl_shift(ShortcutKey::Arrow(ArrowKey::Left)),
            KeyboardShortcut::ctrl_shift(ShortcutKey::Arrow(ArrowKey::Right)),
            KeyboardShortcut::ctrl_shift(ShortcutKey::Arrow(ArrowKey::Up)),
            KeyboardShortcut::ctrl_shift(ShortcutKey::Arrow(ArrowKey::Down)),
        ];
        for shortcut in panel_chords {
            assert!(
                model.resolve(shortcut).is_some(),
                "{shortcut:?} is bound in gpui but means nothing to the model"
            );
        }
    }

    #[test]
    fn the_gpui_binding_table_no_longer_carries_the_alt_digit_dock_chords() {
        let source = include_str!("actions.rs");
        let table = source
            .split("pub fn register_native_keyboard_bindings(")
            .nth(1)
            .expect("native binding table");
        for chord in ["\"alt-1\"", "\"alt-2\"", "\"alt-7\""] {
            assert!(
                !table.contains(chord),
                "{chord} must not be bound: the digits belong to the view tabs"
            );
        }
        for chord in [
            "\"ctrl-1\"",
            "\"ctrl-d\"",
            "\"ctrl-shift-z\"",
            "\"ctrl-left\"",
        ] {
            assert!(table.contains(chord), "{chord} must reach the shell");
        }
    }

    #[test]
    fn shell_terminal_chords_resolve() {
        let model = KeyboardModel::default();
        assert_eq!(
            model.resolve(KeyboardShortcut::ctrl_shift(ShortcutKey::Backtick)),
            Some(KeyboardAction::OpenShellTerminal)
        );
        assert_eq!(
            model.resolve(KeyboardShortcut::ctrl(ShortcutKey::Tab)),
            Some(KeyboardAction::CycleTerminal { backwards: false })
        );
        assert_eq!(
            model.resolve(KeyboardShortcut::ctrl_shift(ShortcutKey::Tab)),
            Some(KeyboardAction::CycleTerminal { backwards: true })
        );
    }

    #[test]
    fn view_tabs_are_ctrl_digits_and_the_dock_bindings_are_gone() {
        let model = KeyboardModel::default();
        assert_eq!(
            model.resolve(KeyboardShortcut::ctrl(ShortcutKey::Digit(1))),
            Some(KeyboardAction::SelectView(PaneView::Conversation))
        );
        assert_eq!(
            model.resolve(KeyboardShortcut::ctrl(ShortcutKey::Digit(5))),
            Some(KeyboardAction::SelectView(PaneView::Browser))
        );
        assert_eq!(
            model.resolve(KeyboardShortcut::alt(ShortcutKey::Digit(1))),
            None
        );
        assert_eq!(
            model.resolve(KeyboardShortcut::alt(ShortcutKey::Digit(7))),
            None
        );
        assert_eq!(
            model.resolve(KeyboardShortcut::ctrl(ShortcutKey::Character('d'))),
            Some(KeyboardAction::SettleTask)
        );
        assert_eq!(
            model.resolve(KeyboardShortcut::ctrl_shift(ShortcutKey::Character('z'))),
            Some(KeyboardAction::ToggleZoom)
        );
    }

    #[test]
    fn every_view_tab_has_its_own_digit_in_painted_order() {
        let model = KeyboardModel::default();
        for (index, view) in PaneView::TABS.iter().enumerate() {
            let digit = u8::try_from(index + 1).expect("five tabs fit in a digit");
            assert_eq!(
                model.resolve(KeyboardShortcut::ctrl(ShortcutKey::Digit(digit))),
                Some(KeyboardAction::SelectView(*view)),
                "Ctrl+{digit} must select the {}th painted tab",
                index + 1
            );
        }
        // The views behind the panel menu deliberately have no chord: there are
        // five digits' worth of tabs, and `MORE` is not one of them.
        assert_eq!(
            model.resolve(KeyboardShortcut::ctrl(ShortcutKey::Digit(6))),
            None
        );
    }

    #[test]
    fn arrows_move_focus_and_shift_arrows_move_the_pane_to_the_same_edge() {
        let model = KeyboardModel::default();
        for arrow in ArrowKey::ALL {
            assert_eq!(
                model.resolve(KeyboardShortcut::ctrl(ShortcutKey::Arrow(arrow))),
                Some(KeyboardAction::FocusPane(arrow.edge()))
            );
            assert_eq!(
                model.resolve(KeyboardShortcut::ctrl_shift(ShortcutKey::Arrow(arrow))),
                Some(KeyboardAction::MovePane(arrow.edge()))
            );
        }
        assert_eq!(ArrowKey::Up.edge(), Edge::Top);
        assert_eq!(ArrowKey::Down.edge(), Edge::Bottom);
    }

    #[test]
    fn the_default_binding_table_is_conflict_free_and_the_dock_keeps_its_digit_lookup() {
        // `KeyboardModel::new` refuses a duplicate shortcut, so building the
        // default at all is the conflict assertion -- but say so out loud, since
        // this run adds six chords to a table that already had thirteen.
        let bindings = KeyboardModel::default().bindings().to_vec();
        let mut seen = std::collections::HashSet::new();
        for binding in &bindings {
            assert!(
                seen.insert(binding.shortcut),
                "{:?} is bound twice",
                binding.shortcut
            );
        }
        assert!(KeyboardModel::new(bindings).is_ok());
        assert_eq!(DockTool::from_digit(1), Some(DockTool::Changes));
        assert_eq!(DockTool::from_digit(7), Some(DockTool::Review));
        assert_eq!(DockTool::from_digit(8), None);
    }

    #[test]
    fn the_shell_terminal_chords_do_not_displace_the_provider_terminal_chord() {
        let model = KeyboardModel::default();
        assert_eq!(
            model.resolve(KeyboardShortcut::ctrl(ShortcutKey::Backtick)),
            Some(KeyboardAction::OpenTerminal)
        );
    }
}
