//! Pure Task Cockpit focus, navigation, and pointer-ownership contract.
//!
//! The shell owns only local interaction state. It never emits terminal input
//! and never calls a host, terminal, provider, or component callback.

use std::sync::Arc;

use crate::domain::id::TaskId;
use crate::ui::task_cockpit::{Inbox, InboxPresentationWidth, InboxRenderModel};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerButton {
    Primary,
    Secondary,
    Middle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransientPriority {
    Low,
    Normal,
    High,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationRejection {
    StaleEpoch,
    TaskNotInInbox,
    EpochExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NavigationResult {
    Committed {
        task_id: TaskId,
        navigation_epoch: u64,
    },
    Rejected {
        reason: NavigationRejection,
    },
}

/// An action captured from an inbox row. The task identity and the shell's
/// current navigation epoch travel together through asynchronous work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapturedInboxAction {
    task_id: TaskId,
    navigation_epoch: u64,
}

impl CapturedInboxAction {
    pub fn task_id(self) -> TaskId {
        self.task_id
    }

    pub fn navigation_epoch(self) -> u64 {
        self.navigation_epoch
    }
}

impl NavigationResult {
    /// Navigation mouse-down is consumed whether the epoch commits or rejects.
    pub fn consumed(self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalPressRejection {
    StaleEpoch,
    TaskNotSelected,
    PointerAlreadyOwned,
    GenerationExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseRejection {
    NoOwner,
    MismatchedOwner,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalRelease {
    Authorized,
    Rejected(ReleaseRejection),
}

impl TerminalRelease {
    /// Every terminal mouse-up is consumed by this contract. Authorization
    /// only determines whether a future terminal view may act on it.
    pub fn consumed(self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidationReason {
    ViewSwitch,
    FocusLoss,
    Deactivate,
    Resync,
}

#[derive(Debug)]
pub struct PointerOwner {
    identity: Arc<()>,
    pointer_id: u64,
    task_id: TaskId,
    generation: u64,
    button: PointerButton,
    navigation_epoch: u64,
}

impl PartialEq for PointerOwner {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.identity, &other.identity)
            && self.pointer_id == other.pointer_id
            && self.task_id == other.task_id
            && self.generation == other.generation
            && self.button == other.button
            && self.navigation_epoch == other.navigation_epoch
    }
}

impl Eq for PointerOwner {}

#[derive(Debug, Eq, PartialEq)]
struct PointerCapture {
    identity: Arc<()>,
    pointer_id: u64,
    task_id: TaskId,
    generation: u64,
    button: PointerButton,
    navigation_epoch: u64,
}

impl PointerCapture {
    fn matches(&self, release: &PointerOwner) -> bool {
        Arc::ptr_eq(&self.identity, &release.identity)
            && self.pointer_id == release.pointer_id
            && self.task_id == release.task_id
            && self.generation == release.generation
            && self.button == release.button
            && self.navigation_epoch == release.navigation_epoch
    }
}

pub type TerminalPointerOwner = PointerOwner;

#[derive(Debug, Eq, PartialEq)]
pub struct Shell {
    selected_task: Option<TaskId>,
    navigation_epoch: u64,
    transient_priority: Option<TransientPriority>,
    pointer_owner: Option<PointerCapture>,
    generation: u64,
}

pub type TaskCockpitShell = Shell;

impl Shell {
    pub fn new(selected_task: Option<TaskId>) -> Self {
        Self {
            selected_task,
            navigation_epoch: 0,
            transient_priority: None,
            pointer_owner: None,
            generation: 0,
        }
    }

    pub fn selected_task(&self) -> Option<TaskId> {
        self.selected_task
    }

    pub fn navigation_epoch(&self) -> u64 {
        self.navigation_epoch
    }

    pub fn focus_navigation_epoch(&self) -> u64 {
        self.navigation_epoch
    }

    pub fn transient_priority(&self) -> Option<TransientPriority> {
        self.transient_priority
    }

    pub fn set_transient_priority(&mut self, priority: Option<TransientPriority>) {
        self.transient_priority = priority;
    }

    pub fn inbox_render_model(
        &self,
        inbox: &Inbox,
        width: InboxPresentationWidth,
    ) -> InboxRenderModel {
        inbox.render_model(width)
    }

    pub fn capture_inbox_action(
        &self,
        task_id: TaskId,
        expected_epoch: u64,
        inbox: &Inbox,
    ) -> Result<CapturedInboxAction, NavigationRejection> {
        if expected_epoch != self.navigation_epoch {
            return Err(NavigationRejection::StaleEpoch);
        }
        if !inbox.contains_active_task(task_id) {
            return Err(NavigationRejection::TaskNotInInbox);
        }
        Ok(CapturedInboxAction {
            task_id,
            navigation_epoch: self.navigation_epoch,
        })
    }

    pub fn resolve_inbox_action(
        &self,
        action: CapturedInboxAction,
        inbox: &Inbox,
    ) -> Result<TaskId, NavigationRejection> {
        if action.navigation_epoch != self.navigation_epoch {
            return Err(NavigationRejection::StaleEpoch);
        }
        if !inbox.contains_active_task(action.task_id) {
            return Err(NavigationRejection::TaskNotInInbox);
        }
        Ok(action.task_id)
    }

    /// Commit selection only when the caller's navigation epoch is current.
    /// A navigation mouse-down is always consumed, including a stale or
    /// out-of-projection one.
    pub fn navigation_mouse_down(
        &mut self,
        task_id: TaskId,
        expected_epoch: u64,
        task_inbox: &Inbox,
    ) -> NavigationResult {
        if expected_epoch != self.navigation_epoch {
            self.invalidate_pointer_owner();
            return NavigationResult::Rejected {
                reason: NavigationRejection::StaleEpoch,
            };
        }
        if !task_inbox.contains_active_task(task_id) {
            self.invalidate_pointer_owner();
            return NavigationResult::Rejected {
                reason: NavigationRejection::TaskNotInInbox,
            };
        }

        let Some(next_epoch) = self.navigation_epoch.checked_add(1) else {
            return NavigationResult::Rejected {
                reason: NavigationRejection::EpochExhausted,
            };
        };
        self.selected_task = Some(task_id);
        self.navigation_epoch = next_epoch;
        self.invalidate_pointer_owner();
        NavigationResult::Committed {
            task_id,
            navigation_epoch: next_epoch,
        }
    }

    /// Navigation mouse-up is always consumed and has no activation effect.
    pub fn navigation_mouse_up(&mut self) -> bool {
        true
    }

    /// Capture exactly one terminal pointer for the selected task.
    pub fn terminal_mouse_down(
        &mut self,
        pointer_id: u64,
        task_id: TaskId,
        button: PointerButton,
        expected_navigation_epoch: u64,
        projected_selected_task: Option<TaskId>,
    ) -> Result<PointerOwner, TerminalPressRejection> {
        if expected_navigation_epoch != self.navigation_epoch {
            return Err(TerminalPressRejection::StaleEpoch);
        }
        if projected_selected_task != self.selected_task || projected_selected_task != Some(task_id)
        {
            return Err(TerminalPressRejection::TaskNotSelected);
        }
        if self.pointer_owner.is_some() {
            return Err(TerminalPressRejection::PointerAlreadyOwned);
        }
        let Some(generation) = self.generation.checked_add(1) else {
            return Err(TerminalPressRejection::GenerationExhausted);
        };
        self.generation = generation;
        let identity = Arc::new(());
        let capture = PointerCapture {
            identity: Arc::clone(&identity),
            pointer_id,
            task_id,
            generation,
            button,
            navigation_epoch: expected_navigation_epoch,
        };
        let owner = PointerOwner {
            identity,
            pointer_id,
            task_id,
            generation,
            button,
            navigation_epoch: expected_navigation_epoch,
        };
        self.pointer_owner = Some(capture);
        Ok(owner)
    }

    /// Consume every terminal mouse-up; only an exact host-issued current
    /// owner token is authorized, and all other releases are rejected without
    /// synthesis. The token is moved in so it cannot be replayed.
    pub fn terminal_mouse_up(&mut self, release: Option<PointerOwner>) -> TerminalRelease {
        match (self.pointer_owner.take(), release) {
            (Some(owner), Some(release)) if owner.matches(&release) => TerminalRelease::Authorized,
            (Some(_), _) => TerminalRelease::Rejected(ReleaseRejection::MismatchedOwner),
            (None, _) => TerminalRelease::Rejected(ReleaseRejection::NoOwner),
        }
    }

    /// Consume a view/focus lifecycle boundary and invalidate the owner.
    pub fn invalidate(&mut self, _reason: InvalidationReason) -> bool {
        self.invalidate_pointer_owner();
        let Some(next_epoch) = self.navigation_epoch.checked_add(1) else {
            return false;
        };
        self.navigation_epoch = next_epoch;
        true
    }

    pub fn on_view_switch(&mut self) -> bool {
        self.invalidate(InvalidationReason::ViewSwitch)
    }

    pub fn on_focus_loss(&mut self) -> bool {
        self.invalidate(InvalidationReason::FocusLoss)
    }

    pub fn on_deactivate(&mut self) -> bool {
        self.invalidate(InvalidationReason::Deactivate)
    }

    pub fn on_resync(&mut self) -> bool {
        self.invalidate(InvalidationReason::Resync)
    }

    fn invalidate_pointer_owner(&mut self) {
        self.pointer_owner = None;
        self.generation = self.generation.saturating_add(1);
    }
}
