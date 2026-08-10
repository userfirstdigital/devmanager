//! Pure Task Cockpit focus, navigation, and pointer-ownership contract.
//!
//! The shell owns only local interaction state. It never emits terminal input
//! and never calls a host, terminal, provider, or component callback.

use std::sync::Arc;

use crate::client::ClientModel;
use crate::domain::id::TaskId;
use crate::ui::task_cockpit::header::{HeaderAction, TaskActionContext};
use crate::ui::task_cockpit::{TaskHeaderModel, TaskList};

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
    resource_generation: u64,
    connection_epoch: u64,
    focus_epoch: u64,
    client_epoch: u64,
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
            resource_generation: 0,
            connection_epoch: 0,
            focus_epoch: 0,
            client_epoch: 0,
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

    pub fn resource_generation(&self) -> u64 {
        self.resource_generation
    }

    pub fn connection_epoch(&self) -> u64 {
        self.connection_epoch
    }

    pub fn focus_epoch(&self) -> u64 {
        self.focus_epoch
    }

    pub fn client_epoch(&self) -> u64 {
        self.client_epoch
    }

    pub fn transient_priority(&self) -> Option<TransientPriority> {
        self.transient_priority
    }

    /// Project the selected task from the immutable client snapshot and the
    /// epochs currently owned by this shell. No caller-supplied context can
    /// mint or replace the action fence.
    pub fn task_header(&self, model: &ClientModel) -> Option<TaskHeaderModel> {
        if model.last_applied_sequence() != self.client_epoch {
            return None;
        }
        self.selected_task.and_then(|task_id| {
            TaskHeaderModel::from_model(
                model,
                task_id,
                TaskActionContext::new(
                    self.resource_generation,
                    self.connection_epoch,
                    self.focus_epoch,
                    self.client_epoch,
                    self.navigation_epoch,
                ),
            )
        })
    }

    /// Dispatch only a projected action carrying the current task and all
    /// action-fence epochs. The shell performs no side effect; a true result
    /// authorizes the caller's downstream action dispatcher to proceed.
    pub fn dispatch_task_action(&self, model: &ClientModel, action: &HeaderAction) -> bool {
        self.task_header(model)
            .is_some_and(|header| header.accepts_action(action))
    }

    /// Observe the current client subscription high-water. Older updates are
    /// ignored, so a stale subscription cannot revive an older projected row.
    pub fn sync_client_epoch(&mut self, client_epoch: u64) -> bool {
        if client_epoch < self.client_epoch {
            return false;
        }
        self.client_epoch = client_epoch;
        true
    }

    pub fn advance_resource_generation(&mut self) -> bool {
        advance_epoch(&mut self.resource_generation)
    }

    pub fn advance_connection_epoch(&mut self) -> bool {
        advance_epoch(&mut self.connection_epoch)
    }

    pub fn advance_focus_epoch(&mut self) -> bool {
        advance_epoch(&mut self.focus_epoch)
    }

    pub fn advance_client_epoch(&mut self) -> bool {
        advance_epoch(&mut self.client_epoch)
    }

    pub fn set_transient_priority(&mut self, priority: Option<TransientPriority>) {
        self.transient_priority = priority;
    }

    /// Commit selection only when the caller's navigation epoch is current.
    /// A navigation mouse-down is always consumed, including a stale or
    /// out-of-projection one.
    pub fn navigation_mouse_down(
        &mut self,
        task_id: TaskId,
        expected_epoch: u64,
        task_inbox: &TaskList,
    ) -> NavigationResult {
        if expected_epoch != self.navigation_epoch {
            self.invalidate_pointer_owner();
            return NavigationResult::Rejected {
                reason: NavigationRejection::StaleEpoch,
            };
        }
        if !task_inbox.task_ids().contains(&task_id) {
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
        if !self.advance_focus_epoch() {
            return false;
        }
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

fn advance_epoch(epoch: &mut u64) -> bool {
    let Some(next) = epoch.checked_add(1) else {
        return false;
    };
    *epoch = next;
    true
}
