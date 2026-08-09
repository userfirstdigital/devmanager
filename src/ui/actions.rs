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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessibilityRole {
    Button,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActionAccessibility {
    pub role: AccessibilityRole,
    pub name: &'static str,
    pub description: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActionPresentation {
    descriptor: &'static ActionDescriptor,
    presentation: ActionPresentationKind,
    shortcut: Option<&'static str>,
    availability: ActionAvailability,
    disabled: bool,
    disabled_reason: Option<&'static str>,
    accessibility: ActionAccessibility,
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

    pub fn accessibility(&self) -> &ActionAccessibility {
        &self.accessibility
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
                accessibility: ActionAccessibility {
                    role: AccessibilityRole::Button,
                    name: descriptor.title,
                    description: descriptor.description,
                },
            }
        })
        .collect()
}
