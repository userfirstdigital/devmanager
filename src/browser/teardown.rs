//! Source-level recovery and ordered teardown for Task-owned browser generations.
//!
//! This module does not launch WebView2, providers, or helper processes. It
//! records the exact settlement a future host executor must perform: cancel
//! operations, deny input, park the surface, close controllers, await helper
//! disappearance, reconcile ports/files, then mark closed.

use std::collections::BTreeMap;
use std::fmt;

use crate::browser::generation::{BrowserGenerationError, BrowserTaskGenerationAuthority};
use crate::domain::browser::BrowserHealth;
use crate::domain::id::{BrowserContextId, TaskId};

pub const BROWSER_TEARDOWN_STAGE_COUNT: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserRecoveryCause {
    NavigationFailure,
    UnresponsiveRenderer,
    WebViewProcessCrash,
    ClientCrash,
    HostShutdown,
    SleepWake,
    DisplayDpiChange,
    FailedCreate,
}

impl fmt::Display for BrowserRecoveryCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NavigationFailure => "navigation failure",
            Self::UnresponsiveRenderer => "unresponsive renderer",
            Self::WebViewProcessCrash => "webview process crash",
            Self::ClientCrash => "client crash",
            Self::HostShutdown => "host shutdown",
            Self::SleepWake => "sleep/wake",
            Self::DisplayDpiChange => "display/DPI change",
            Self::FailedCreate => "failed create",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BrowserTeardownStage {
    CancelOperations,
    DenyNewInput,
    DetachParkSurface,
    CloseControllers,
    AwaitHelperDisappearance,
    ReconcilePortsFiles,
    MarkClosed,
}

impl BrowserTeardownStage {
    pub const ORDER: [Self; BROWSER_TEARDOWN_STAGE_COUNT] = [
        Self::CancelOperations,
        Self::DenyNewInput,
        Self::DetachParkSurface,
        Self::CloseControllers,
        Self::AwaitHelperDisappearance,
        Self::ReconcilePortsFiles,
        Self::MarkClosed,
    ];

    pub fn next(self) -> Option<Self> {
        let index = Self::ORDER.iter().position(|stage| *stage == self)?;
        Self::ORDER.get(index + 1).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserRecoveryError {
    Generation(BrowserGenerationError),
    OutOfOrderTeardown,
    HelperResidue,
    SurfaceNotParked,
    StaleInputEpoch,
    InputDenied,
    AlreadyClosed,
}

impl fmt::Display for BrowserRecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Generation(error) => write!(f, "{error}"),
            Self::OutOfOrderTeardown => write!(f, "browser teardown stages must run in order"),
            Self::HelperResidue => {
                write!(f, "browser helper residue remains after teardown")
            }
            Self::SurfaceNotParked => {
                write!(f, "client attach is denied until the surface is parked")
            }
            Self::StaleInputEpoch => {
                write!(f, "browser input requires a fresh bounds/focus epoch")
            }
            Self::InputDenied => write!(f, "browser input is denied during teardown or recovery"),
            Self::AlreadyClosed => write!(f, "browser context is already closed"),
        }
    }
}

impl std::error::Error for BrowserRecoveryError {}

impl From<BrowserGenerationError> for BrowserRecoveryError {
    fn from(error: BrowserGenerationError) -> Self {
        Self::Generation(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRecoveryOutcome {
    pub task_id: TaskId,
    pub context_id: BrowserContextId,
    pub from_generation: u64,
    pub to_generation: Option<u64>,
    pub health: BrowserHealth,
    pub cause: BrowserRecoveryCause,
    pub interruption: bool,
    pub helper_residue: usize,
    pub bounds_epoch: u64,
    pub focus_epoch: u64,
    pub surface_parked: bool,
    pub input_denied: bool,
    pub teardown_stage: Option<BrowserTeardownStage>,
}

#[derive(Debug, Clone)]
struct RecoveryState {
    task_id: TaskId,
    teardown_stage: Option<BrowserTeardownStage>,
    closed: bool,
    input_denied: bool,
    surface_parked: bool,
    bounds_epoch: u64,
    focus_epoch: u64,
    helper_residue: usize,
    last_cause: Option<BrowserRecoveryCause>,
}

/// Source-level recovery controller. No process launch or HWND attach.
#[derive(Debug)]
pub struct BrowserRecoveryController {
    authority: BrowserTaskGenerationAuthority,
    states: BTreeMap<BrowserContextId, RecoveryState>,
}

impl Default for BrowserRecoveryController {
    fn default() -> Self {
        Self {
            authority: BrowserTaskGenerationAuthority::new(),
            states: BTreeMap::new(),
        }
    }
}

impl BrowserRecoveryController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn authority(&self) -> &BrowserTaskGenerationAuthority {
        &self.authority
    }

    pub fn authority_mut(&mut self) -> &mut BrowserTaskGenerationAuthority {
        &mut self.authority
    }

    pub fn open_context(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
    ) -> Result<u64, BrowserRecoveryError> {
        self.authority.open_task(task_id)?;
        let generation = self.authority.create_context(task_id, context_id)?;
        self.states.insert(
            context_id,
            RecoveryState {
                task_id,
                teardown_stage: None,
                closed: false,
                input_denied: false,
                surface_parked: false,
                bounds_epoch: 1,
                focus_epoch: 1,
                helper_residue: 0,
                last_cause: None,
            },
        );
        Ok(generation)
    }

    pub fn recover(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
        cause: BrowserRecoveryCause,
    ) -> Result<BrowserRecoveryOutcome, BrowserRecoveryError> {
        let from_generation = self.authority.live_generation(task_id, context_id)?;
        match cause {
            BrowserRecoveryCause::HostShutdown | BrowserRecoveryCause::FailedCreate => {
                self.run_teardown(task_id, context_id, cause, true)
            }
            BrowserRecoveryCause::ClientCrash => {
                self.ensure_parked(task_id, context_id)?;
                self.invalidate_epochs(context_id)?;
                let to_generation =
                    self.authority
                        .recover_generation(task_id, context_id, from_generation)?;
                self.mark_cause(context_id, cause)?;
                Ok(self.outcome(
                    task_id,
                    context_id,
                    from_generation,
                    Some(to_generation),
                    cause,
                )?)
            }
            BrowserRecoveryCause::SleepWake | BrowserRecoveryCause::DisplayDpiChange => {
                self.ensure_parked(task_id, context_id)?;
                self.invalidate_epochs(context_id)?;
                self.deny_input(context_id, true)?;
                self.mark_cause(context_id, cause)?;
                Ok(self.outcome(task_id, context_id, from_generation, None, cause)?)
            }
            BrowserRecoveryCause::NavigationFailure
            | BrowserRecoveryCause::UnresponsiveRenderer
            | BrowserRecoveryCause::WebViewProcessCrash => {
                let dropped =
                    self.authority
                        .cancel_generation(task_id, context_id, from_generation)?;
                if !dropped.is_empty() && self.authority.has_orphans(task_id, context_id) {
                    return Err(BrowserRecoveryError::Generation(
                        BrowserGenerationError::QueueOrphan,
                    ));
                }
                let to_generation =
                    self.authority
                        .recover_generation(task_id, context_id, from_generation)?;
                self.invalidate_epochs(context_id)?;
                self.mark_cause(context_id, cause)?;
                Ok(self.outcome(
                    task_id,
                    context_id,
                    from_generation,
                    Some(to_generation),
                    cause,
                )?)
            }
        }
    }

    pub fn advance_teardown(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
        stage: BrowserTeardownStage,
    ) -> Result<BrowserTeardownStage, BrowserRecoveryError> {
        let (owner, closed, current_stage) = {
            let state = self
                .states
                .get(&context_id)
                .ok_or(BrowserRecoveryError::Generation(
                    BrowserGenerationError::InvalidRequest,
                ))?;
            (state.task_id, state.closed, state.teardown_stage)
        };
        if owner != task_id {
            return Err(BrowserRecoveryError::Generation(
                BrowserGenerationError::CrossTask,
            ));
        }
        if closed && stage == BrowserTeardownStage::MarkClosed {
            return Ok(BrowserTeardownStage::MarkClosed);
        }
        let expected = match current_stage {
            None => BrowserTeardownStage::CancelOperations,
            Some(current) if current == stage => {
                return Ok(stage);
            }
            Some(current) => current.next().ok_or(BrowserRecoveryError::AlreadyClosed)?,
        };
        if stage != expected {
            return Err(BrowserRecoveryError::OutOfOrderTeardown);
        }
        match stage {
            BrowserTeardownStage::CancelOperations => {
                if let Ok(generation) = self.authority.live_generation(task_id, context_id) {
                    let _ = self
                        .authority
                        .cancel_generation(task_id, context_id, generation);
                }
            }
            BrowserTeardownStage::DenyNewInput => {
                if let Some(state) = self.states.get_mut(&context_id) {
                    state.input_denied = true;
                }
            }
            BrowserTeardownStage::DetachParkSurface => {
                if let Some(state) = self.states.get_mut(&context_id) {
                    state.surface_parked = true;
                }
            }
            BrowserTeardownStage::CloseControllers => {}
            BrowserTeardownStage::AwaitHelperDisappearance => {
                let residue = self
                    .states
                    .get(&context_id)
                    .map(|state| state.helper_residue)
                    .unwrap_or(0);
                if residue != 0 {
                    return Err(BrowserRecoveryError::HelperResidue);
                }
            }
            BrowserTeardownStage::ReconcilePortsFiles => {}
            BrowserTeardownStage::MarkClosed => {
                self.authority.close_task(task_id)?;
                if let Some(state) = self.states.get_mut(&context_id) {
                    state.closed = true;
                    state.input_denied = true;
                    state.surface_parked = true;
                }
            }
        }
        if let Some(state) = self.states.get_mut(&context_id) {
            state.teardown_stage = Some(stage);
        }
        Ok(stage)
    }

    pub fn run_teardown(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
        cause: BrowserRecoveryCause,
        close: bool,
    ) -> Result<BrowserRecoveryOutcome, BrowserRecoveryError> {
        let from_generation = self
            .authority
            .live_generation(task_id, context_id)
            .unwrap_or(0);
        for stage in BrowserTeardownStage::ORDER {
            if !close && stage == BrowserTeardownStage::MarkClosed {
                break;
            }
            self.advance_teardown(task_id, context_id, stage)?;
        }
        self.mark_cause(context_id, cause)?;
        let to_generation = if close {
            None
        } else {
            Some(self.authority.live_generation(task_id, context_id)?)
        };
        Ok(self.outcome(task_id, context_id, from_generation, to_generation, cause)?)
    }

    pub fn accept_attach(
        &self,
        task_id: TaskId,
        context_id: BrowserContextId,
    ) -> Result<(), BrowserRecoveryError> {
        let state = self.state(task_id, context_id)?;
        if state.closed {
            return Err(BrowserRecoveryError::AlreadyClosed);
        }
        if !state.surface_parked
            && matches!(state.last_cause, Some(BrowserRecoveryCause::ClientCrash))
        {
            return Err(BrowserRecoveryError::SurfaceNotParked);
        }
        Ok(())
    }

    pub fn accept_input(
        &self,
        task_id: TaskId,
        context_id: BrowserContextId,
        bounds_epoch: u64,
        focus_epoch: u64,
    ) -> Result<(), BrowserRecoveryError> {
        let state = self.state(task_id, context_id)?;
        if state.closed {
            return Err(BrowserRecoveryError::AlreadyClosed);
        }
        if state.input_denied {
            return Err(BrowserRecoveryError::InputDenied);
        }
        if bounds_epoch != state.bounds_epoch || focus_epoch != state.focus_epoch {
            return Err(BrowserRecoveryError::StaleInputEpoch);
        }
        Ok(())
    }

    pub fn record_fresh_layout(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
        bounds_epoch: u64,
        focus_epoch: u64,
    ) -> Result<(), BrowserRecoveryError> {
        let state = self
            .states
            .get_mut(&context_id)
            .ok_or(BrowserRecoveryError::Generation(
                BrowserGenerationError::InvalidRequest,
            ))?;
        if state.task_id != task_id {
            return Err(BrowserRecoveryError::Generation(
                BrowserGenerationError::CrossTask,
            ));
        }
        if bounds_epoch != state.bounds_epoch || focus_epoch != state.focus_epoch {
            return Err(BrowserRecoveryError::StaleInputEpoch);
        }
        state.input_denied = false;
        state.surface_parked = true;
        Ok(())
    }

    #[cfg(test)]
    pub fn inject_helper_residue_for_test(&mut self, context_id: BrowserContextId, residue: usize) {
        if let Some(state) = self.states.get_mut(&context_id) {
            state.helper_residue = residue;
        }
    }

    fn ensure_parked(
        &mut self,
        task_id: TaskId,
        context_id: BrowserContextId,
    ) -> Result<(), BrowserRecoveryError> {
        let state = self
            .states
            .get_mut(&context_id)
            .ok_or(BrowserRecoveryError::Generation(
                BrowserGenerationError::InvalidRequest,
            ))?;
        if state.task_id != task_id {
            return Err(BrowserRecoveryError::Generation(
                BrowserGenerationError::CrossTask,
            ));
        }
        state.surface_parked = true;
        Ok(())
    }

    fn invalidate_epochs(
        &mut self,
        context_id: BrowserContextId,
    ) -> Result<(), BrowserRecoveryError> {
        let state = self
            .states
            .get_mut(&context_id)
            .ok_or(BrowserRecoveryError::Generation(
                BrowserGenerationError::InvalidRequest,
            ))?;
        state.bounds_epoch =
            state
                .bounds_epoch
                .checked_add(1)
                .ok_or(BrowserRecoveryError::Generation(
                    BrowserGenerationError::BoundExceeded,
                ))?;
        state.focus_epoch =
            state
                .focus_epoch
                .checked_add(1)
                .ok_or(BrowserRecoveryError::Generation(
                    BrowserGenerationError::BoundExceeded,
                ))?;
        Ok(())
    }

    fn deny_input(
        &mut self,
        context_id: BrowserContextId,
        denied: bool,
    ) -> Result<(), BrowserRecoveryError> {
        let state = self
            .states
            .get_mut(&context_id)
            .ok_or(BrowserRecoveryError::Generation(
                BrowserGenerationError::InvalidRequest,
            ))?;
        state.input_denied = denied;
        Ok(())
    }

    fn mark_cause(
        &mut self,
        context_id: BrowserContextId,
        cause: BrowserRecoveryCause,
    ) -> Result<(), BrowserRecoveryError> {
        let state = self
            .states
            .get_mut(&context_id)
            .ok_or(BrowserRecoveryError::Generation(
                BrowserGenerationError::InvalidRequest,
            ))?;
        state.last_cause = Some(cause);
        Ok(())
    }

    fn state(
        &self,
        task_id: TaskId,
        context_id: BrowserContextId,
    ) -> Result<&RecoveryState, BrowserRecoveryError> {
        let state = self
            .states
            .get(&context_id)
            .ok_or(BrowserRecoveryError::Generation(
                BrowserGenerationError::InvalidRequest,
            ))?;
        if state.task_id != task_id {
            return Err(BrowserRecoveryError::Generation(
                BrowserGenerationError::CrossTask,
            ));
        }
        Ok(state)
    }

    fn outcome(
        &self,
        task_id: TaskId,
        context_id: BrowserContextId,
        from_generation: u64,
        to_generation: Option<u64>,
        cause: BrowserRecoveryCause,
    ) -> Result<BrowserRecoveryOutcome, BrowserRecoveryError> {
        let state = self.state(task_id, context_id)?;
        let health = self
            .authority
            .health(task_id, context_id)
            .unwrap_or(BrowserHealth::Interrupted);
        Ok(BrowserRecoveryOutcome {
            task_id,
            context_id,
            from_generation,
            to_generation,
            health,
            cause,
            interruption: true,
            helper_residue: state.helper_residue,
            bounds_epoch: state.bounds_epoch,
            focus_epoch: state.focus_epoch,
            surface_parked: state.surface_parked,
            input_denied: state.input_denied,
            teardown_stage: state.teardown_stage,
        })
    }
}
