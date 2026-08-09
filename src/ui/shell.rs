//! Pure Task Cockpit focus, navigation, and pointer-ownership contract.
//!
//! The shell owns only local interaction state. It never emits terminal input
//! and never calls a host, terminal, provider, or component callback.

use crate::domain::id::TaskId;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PointerOwner {
    pub pointer_id: u64,
    pub task_id: TaskId,
    pub generation: u64,
    pub button: PointerButton,
    pub navigation_epoch: u64,
}

pub type TerminalPointerOwner = PointerOwner;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Shell {
    selected_task: Option<TaskId>,
    navigation_epoch: u64,
    transient_priority: Option<TransientPriority>,
    pointer_owner: Option<PointerOwner>,
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

    pub fn transient_priority(&self) -> Option<TransientPriority> {
        self.transient_priority
    }

    pub fn set_transient_priority(&mut self, priority: Option<TransientPriority>) {
        self.transient_priority = priority;
    }

    pub fn pointer_owner(&self) -> Option<PointerOwner> {
        self.pointer_owner
    }

    /// Commit selection only when the caller's navigation epoch is current.
    /// A navigation mouse-down is always consumed, including a stale one.
    pub fn navigation_mouse_down(
        &mut self,
        task_id: TaskId,
        expected_epoch: u64,
    ) -> NavigationResult {
        if expected_epoch != self.navigation_epoch {
            self.invalidate_pointer_owner();
            return NavigationResult::Rejected {
                reason: NavigationRejection::StaleEpoch,
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
    ) -> Result<PointerOwner, TerminalPressRejection> {
        if self.pointer_owner.is_some() {
            return Err(TerminalPressRejection::PointerAlreadyOwned);
        }
        if self.selected_task != Some(task_id) {
            return Err(TerminalPressRejection::TaskNotSelected);
        }
        let Some(generation) = self.generation.checked_add(1) else {
            return Err(TerminalPressRejection::GenerationExhausted);
        };
        self.generation = generation;
        let owner = PointerOwner {
            pointer_id,
            task_id,
            generation,
            button,
            navigation_epoch: self.navigation_epoch,
        };
        self.pointer_owner = Some(owner);
        Ok(owner)
    }

    /// Consume every terminal mouse-up; only an exact current owner is
    /// authorized, and all other releases are rejected without synthesis.
    pub fn terminal_mouse_up(&mut self, release: PointerOwner) -> TerminalRelease {
        match self.pointer_owner.take() {
            Some(owner) if owner == release => TerminalRelease::Authorized,
            Some(_) => TerminalRelease::Rejected(ReleaseRejection::MismatchedOwner),
            None => TerminalRelease::Rejected(ReleaseRejection::NoOwner),
        }
    }

    /// Consume a view/focus lifecycle boundary and invalidate the owner.
    pub fn invalidate(&mut self, _reason: InvalidationReason) -> bool {
        self.invalidate_pointer_owner();
        self.navigation_epoch = self.navigation_epoch.saturating_add(1);
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
