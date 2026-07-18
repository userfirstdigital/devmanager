use super::{
    BrowserAction, BrowserActionTarget, BrowserCommand, BrowserError, BrowserRecipeAction,
    BrowserRecipeLocator, BrowserRecipeValue, BrowserRecipeViewport, BrowserRecipeWait,
    BrowserRecordingAction, BrowserRecordingActor, BrowserRecordingCommit, BrowserRecordingError,
    BrowserRecordingInstance, BrowserRecordingReservation, BrowserRecordingReview,
    BrowserRecordingStatus, BrowserResponse, BrowserRisk, BrowserRuntimeTarget,
    BrowserScreenshotMode, BrowserWaitCondition, BrowserWorkflowRecorder, BrowserWorkspaceKey,
    MAX_BROWSER_ACTIONS,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

const MAX_RECORDING_TAB_ALIASES: usize = 64;
const USER_CHROME_WORKSPACE_TAB: &str = "__workspace__";

/// Opaque ownership of one user-chrome recording slot. It deliberately has no
/// `Debug`, `Clone`, or serialization implementation so neither the sanitized
/// intent nor the reservation can escape through logging or wire formats.
pub struct BrowserUserChromeCapture {
    instance: BrowserRecordingInstance,
    reservation: BrowserRecordingReservation,
    intent: BrowserUserChromeIntent,
}

enum BrowserUserChromeIntent {
    CreateTab {
        capture_url: bool,
    },
    SelectTab {
        runtime_tab_id: String,
    },
    CloseTab {
        runtime_tab_id: String,
    },
    Navigate {
        runtime_tab_id: String,
    },
    Back {
        runtime_tab_id: String,
    },
    Forward {
        runtime_tab_id: String,
    },
    Reload {
        runtime_tab_id: String,
    },
    UpdateViewport {
        runtime_tab_id: String,
        viewport: BrowserRecipeViewport,
    },
}

impl BrowserUserChromeIntent {
    fn reservation_tab_id(&self) -> &str {
        match self {
            Self::CreateTab { .. } => USER_CHROME_WORKSPACE_TAB,
            Self::SelectTab { runtime_tab_id }
            | Self::CloseTab { runtime_tab_id }
            | Self::Navigate { runtime_tab_id }
            | Self::Back { runtime_tab_id }
            | Self::Forward { runtime_tab_id }
            | Self::Reload { runtime_tab_id }
            | Self::UpdateViewport { runtime_tab_id, .. } => runtime_tab_id,
        }
    }
}

struct BrowserRecordingTabAliases {
    instance_id: u64,
    next_alias: u64,
    runtime_to_alias: HashMap<String, String>,
}

impl BrowserRecordingTabAliases {
    fn new(instance_id: u64) -> Self {
        Self {
            instance_id,
            next_alias: 0,
            runtime_to_alias: HashMap::new(),
        }
    }

    fn alias_for(&mut self, runtime_tab_id: &str) -> Result<String, BrowserRecordingError> {
        if runtime_tab_id.trim().is_empty() {
            return Err(BrowserRecordingError::InvalidAction);
        }
        if let Some(alias) = self.runtime_to_alias.get(runtime_tab_id) {
            return Ok(alias.clone());
        }
        if self.runtime_to_alias.len() >= MAX_RECORDING_TAB_ALIASES {
            return Err(BrowserRecordingError::CapacityExceeded);
        }
        self.next_alias = self.next_alias.saturating_add(1);
        let alias = format!("tab-{}", self.next_alias);
        self.runtime_to_alias
            .insert(runtime_tab_id.to_string(), alias.clone());
        Ok(alias)
    }
}

struct BrowserWorkflowCoordinatorState {
    recorder: BrowserWorkflowRecorder,
    tab_aliases: HashMap<BrowserWorkspaceKey, BrowserRecordingTabAliases>,
    agent_commands: HashMap<(BrowserWorkspaceKey, String), PendingAgentRecording>,
}

struct PendingAgentRecording {
    instance: BrowserRecordingInstance,
    reservations: Vec<BrowserRecordingReservation>,
    prepared_actions: Vec<Option<BrowserRecordingAction>>,
}

/// Cloneable access to the one in-memory workflow recorder shared by browser
/// producers. The mutex defines one reservation order before asynchronous work
/// can complete on different host/controller paths.
#[derive(Clone)]
pub struct BrowserWorkflowCoordinator {
    state: Arc<Mutex<BrowserWorkflowCoordinatorState>>,
}

impl Default for BrowserWorkflowCoordinator {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(BrowserWorkflowCoordinatorState {
                recorder: BrowserWorkflowRecorder::default(),
                tab_aliases: HashMap::new(),
                agent_commands: HashMap::new(),
            })),
        }
    }
}

impl BrowserWorkflowCoordinator {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(BrowserWorkflowCoordinatorState {
                recorder: BrowserWorkflowRecorder::with_capacity(capacity),
                tab_aliases: HashMap::new(),
                agent_commands: HashMap::new(),
            })),
        }
    }

    pub fn status(&self, workspace_key: &BrowserWorkspaceKey) -> BrowserRecordingStatus {
        self.lock().recorder.status(workspace_key)
    }

    pub fn active_instance(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Option<BrowserRecordingInstance> {
        self.lock().recorder.active_instance(workspace_key)
    }

    pub fn current_instance(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Option<BrowserRecordingInstance> {
        self.lock().recorder.current_instance(workspace_key)
    }

    pub fn current_project_instances(&self, project_id: &str) -> Vec<BrowserRecordingInstance> {
        self.lock().recorder.current_project_instances(project_id)
    }

    pub fn active_project_instances(&self, project_id: &str) -> Vec<BrowserRecordingInstance> {
        let state = self.lock();
        let mut instances = state
            .tab_aliases
            .keys()
            .filter(|workspace_key| workspace_key.project_id == project_id)
            .filter_map(|workspace_key| state.recorder.active_instance(workspace_key))
            .collect::<Vec<_>>();
        instances.sort_by(|left, right| {
            left.workspace_key()
                .ai_tab_id
                .cmp(&right.workspace_key().ai_tab_id)
        });
        instances
    }

    pub fn start(
        &self,
        workspace_key: BrowserWorkspaceKey,
    ) -> Result<BrowserRecordingInstance, BrowserRecordingError> {
        self.start_with_optional_selected_tab(workspace_key, None)
    }

    pub fn start_with_selected_tab(
        &self,
        workspace_key: BrowserWorkspaceKey,
        selected_tab_id: impl Into<String>,
    ) -> Result<BrowserRecordingInstance, BrowserRecordingError> {
        let selected_tab_id = selected_tab_id.into();
        if selected_tab_id.trim().is_empty() {
            return Err(BrowserRecordingError::InvalidAction);
        }
        self.start_with_optional_selected_tab(workspace_key, Some(selected_tab_id))
    }

    fn start_with_optional_selected_tab(
        &self,
        workspace_key: BrowserWorkspaceKey,
        selected_tab_id: Option<String>,
    ) -> Result<BrowserRecordingInstance, BrowserRecordingError> {
        let mut state = self.lock();
        let instance = state.recorder.start(workspace_key.clone())?;
        let mut aliases = BrowserRecordingTabAliases::new(instance.id());
        if let Some(selected_tab_id) = selected_tab_id {
            if let Err(error) = aliases.alias_for(&selected_tab_id) {
                if state.recorder.stop(&instance).is_ok() {
                    let _ = state.recorder.discard(&instance);
                }
                return Err(error);
            }
        }
        state.tab_aliases.insert(workspace_key, aliases);
        Ok(instance)
    }

    pub fn reserve_on(
        &self,
        instance: &BrowserRecordingInstance,
        actor: BrowserRecordingActor,
        tab_id: impl Into<String>,
        risk: BrowserRisk,
    ) -> Result<BrowserRecordingReservation, BrowserRecordingError> {
        self.lock()
            .recorder
            .reserve_on(instance, actor, tab_id, risk)
    }

    pub fn commit(
        &self,
        reservation: BrowserRecordingReservation,
        action: BrowserRecordingAction,
    ) -> Result<BrowserRecordingCommit, BrowserRecordingError> {
        self.lock().recorder.commit(reservation, action)
    }

    pub fn cancel(
        &self,
        reservation: BrowserRecordingReservation,
    ) -> Result<BrowserRecordingCommit, BrowserRecordingError> {
        self.lock().recorder.cancel(reservation)
    }

    pub fn stop(
        &self,
        instance: &BrowserRecordingInstance,
    ) -> Result<BrowserRecordingReview, BrowserRecordingError> {
        let mut state = self.lock();
        let review = state.recorder.stop(instance)?;
        state.tab_aliases.remove(instance.workspace_key());
        state
            .agent_commands
            .retain(|(workspace_key, _), _| workspace_key != instance.workspace_key());
        Ok(review)
    }

    pub fn discard(
        &self,
        instance: &BrowserRecordingInstance,
    ) -> Result<(), BrowserRecordingError> {
        let mut state = self.lock();
        state.recorder.discard(instance)?;
        state.tab_aliases.remove(instance.workspace_key());
        state
            .agent_commands
            .retain(|(workspace_key, _), _| workspace_key != instance.workspace_key());
        Ok(())
    }

    pub fn reserve_agent_command(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        operation_id: &str,
        command: &BrowserCommand,
        risk: BrowserRisk,
    ) -> Result<(), BrowserRecordingError> {
        let mut state = self.lock();
        let Some(instance) = state.recorder.active_instance(workspace_key) else {
            return Ok(());
        };
        if operation_id.trim().is_empty() || operation_id.trim() != operation_id {
            return Err(BrowserRecordingError::InvalidAction);
        }
        let slot_count = match command {
            BrowserCommand::Act { actions, .. } => {
                if actions.is_empty() || actions.len() > MAX_BROWSER_ACTIONS {
                    return Err(BrowserRecordingError::InvalidAction);
                }
                actions.len()
            }
            BrowserCommand::CreateTab { .. }
            | BrowserCommand::SelectTab { .. }
            | BrowserCommand::CloseTab { .. }
            | BrowserCommand::Navigate { .. }
            | BrowserCommand::Back { .. }
            | BrowserCommand::Forward { .. }
            | BrowserCommand::Reload { .. }
            | BrowserCommand::UpdateViewport { .. }
            | BrowserCommand::Wait { .. }
            | BrowserCommand::Screenshot { .. }
            | BrowserCommand::Upload { .. }
            | BrowserCommand::Cdp { .. }
            | BrowserCommand::SecretType { .. } => 1,
            _ => return Ok(()),
        };
        let key = (workspace_key.clone(), operation_id.to_string());
        if state.agent_commands.contains_key(&key) {
            return Err(BrowserRecordingError::InvalidAction);
        }
        let tab_id = command.tab_id().unwrap_or("__workspace__");
        let mut reservations = Vec::with_capacity(slot_count);
        for _ in 0..slot_count {
            match state
                .recorder
                .reserve_on(&instance, BrowserRecordingActor::Agent, tab_id, risk)
            {
                Ok(reservation) => reservations.push(reservation),
                Err(error) => {
                    for reservation in reservations {
                        let _ = state.recorder.cancel(reservation);
                    }
                    return Err(error);
                }
            }
        }
        let source_order_input = match source_order_input_action(command) {
            Ok(source_order_input) => source_order_input,
            Err(error) => {
                for reservation in reservations {
                    let _ = state.recorder.cancel(reservation);
                }
                return Err(error);
            }
        };
        if let Some(prepared) = source_order_input {
            if let Err(error) = state.recorder.prepare(&reservations[0], prepared) {
                for reservation in reservations {
                    let _ = state.recorder.cancel(reservation);
                }
                return Err(error);
            }
        }
        state.agent_commands.insert(
            key,
            PendingAgentRecording {
                instance: instance.clone(),
                prepared_actions: (0..slot_count).map(|_| None).collect(),
                reservations,
            },
        );
        Ok(())
    }

    pub fn inspect_agent_actions(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        operation_id: &str,
        command: &BrowserCommand,
        runtime_targets: &[BrowserRuntimeTarget],
        effective_risk: BrowserRisk,
    ) -> Result<(), BrowserRecordingError> {
        let BrowserCommand::Act { actions, .. } = command else {
            return Err(BrowserRecordingError::InvalidAction);
        };
        let key = (workspace_key.clone(), operation_id.to_string());
        let mut state = self.lock();
        let Some(mut pending) = state.agent_commands.remove(&key) else {
            return Ok(());
        };
        if pending.reservations.len() != actions.len() {
            state.agent_commands.insert(key, pending);
            return Err(BrowserRecordingError::StaleReservation);
        }
        let prepared = match prepare_agent_actions(actions, runtime_targets) {
            Ok(prepared) => prepared,
            Err(error) => {
                state.agent_commands.insert(key, pending);
                return Err(error);
            }
        };
        for reservation in &pending.reservations {
            if let Err(error) = state
                .recorder
                .set_reservation_risk(reservation, effective_risk)
            {
                state.agent_commands.insert(key, pending);
                return Err(error);
            }
        }
        pending.prepared_actions = prepared;
        state.agent_commands.insert(key, pending);
        Ok(())
    }

    pub fn inspect_agent_secret_type(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        operation_id: &str,
        command: &BrowserCommand,
        _runtime_target: &BrowserRuntimeTarget,
        effective_risk: BrowserRisk,
    ) -> Result<(), BrowserRecordingError> {
        let BrowserCommand::SecretType {
            target, input_name, ..
        } = command
        else {
            return Err(BrowserRecordingError::InvalidAction);
        };
        let key = (workspace_key.clone(), operation_id.to_string());
        let mut state = self.lock();
        let Some(pending) = state.agent_commands.remove(&key) else {
            return Ok(());
        };
        if pending.reservations.len() != 1 || pending.prepared_actions.len() != 1 {
            state.agent_commands.insert(key, pending);
            return Err(BrowserRecordingError::StaleReservation);
        }
        let prepared =
            match BrowserRecordingAction::type_secret_input(recipe_locator(target), input_name) {
                Ok(prepared) => prepared,
                Err(error) => {
                    state.agent_commands.insert(key, pending);
                    return Err(error);
                }
            };
        if let Err(error) = state
            .recorder
            .set_reservation_risk(&pending.reservations[0], effective_risk)
        {
            state.agent_commands.insert(key, pending);
            return Err(error);
        }
        if let Err(error) = state.recorder.prepare(&pending.reservations[0], prepared) {
            state.agent_commands.insert(key, pending);
            return Err(error);
        }
        state.agent_commands.insert(key, pending);
        Ok(())
    }

    pub fn complete_agent_command(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        operation_id: &str,
        command: &BrowserCommand,
        result: &Result<BrowserResponse, BrowserError>,
    ) -> Result<(), BrowserRecordingError> {
        let key = (workspace_key.clone(), operation_id.to_string());
        let mut state = self.lock();
        let Some(mut pending) = state.agent_commands.remove(&key) else {
            return Ok(());
        };
        if result.is_err() {
            cancel_pending_agent(&mut state.recorder, pending);
            return Ok(());
        }

        match command {
            BrowserCommand::Act { .. } => {
                let completed = match result {
                    Ok(BrowserResponse::Action { result }) => result.completed_actions,
                    _ => 0,
                };
                let mut first_error = None;
                for (index, (reservation, action)) in pending
                    .reservations
                    .drain(..)
                    .zip(pending.prepared_actions.drain(..))
                    .enumerate()
                {
                    let outcome = if index < completed {
                        if let Some(action) = action {
                            state.recorder.commit(reservation, action).map(|_| ())
                        } else {
                            state.recorder.cancel(reservation).map(|_| ())
                        }
                    } else {
                        state.recorder.cancel(reservation).map(|_| ())
                    };
                    if let Err(error) = outcome {
                        first_error.get_or_insert(error);
                    }
                }
                first_error.map_or(Ok(()), Err)
            }
            BrowserCommand::SecretType { .. } => {
                let exact_instance = pending.instance.clone();
                if pending.reservations.len() != 1 {
                    cancel_pending_agent(&mut state.recorder, pending);
                    return Err(BrowserRecordingError::StaleReservation);
                }
                let completed = matches!(
                    result,
                    Ok(BrowserResponse::Action { result }) if result.completed_actions == 1
                );
                let reservation = pending
                    .reservations
                    .pop()
                    .expect("secret reservation count checked");
                let outcome = if completed {
                    match state.recorder.commit_prepared(reservation) {
                        Ok(BrowserRecordingCommit::Recorded | BrowserRecordingCommit::Buffered) => {
                            Ok(())
                        }
                        Ok(BrowserRecordingCommit::Ignored) => {
                            Err(BrowserRecordingError::StaleReservation)
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    let _ = state.recorder.cancel(reservation);
                    Err(BrowserRecordingError::InvalidAction)
                };
                if let Err(error) = outcome {
                    invalidate_exact_recording(&mut state, &exact_instance);
                    return Err(error);
                }
                Ok(())
            }
            _ => {
                let mut action = match prepared_agent_command_action(
                    &mut state.tab_aliases,
                    workspace_key,
                    command,
                    result,
                ) {
                    Ok(action) => action,
                    Err(error) => {
                        cancel_pending_agent(&mut state.recorder, pending);
                        return Err(error);
                    }
                };
                let mut first = true;
                let mut first_error = None;
                for reservation in pending.reservations.drain(..) {
                    let outcome = if first {
                        first = false;
                        if let Some(action) = action.take() {
                            state.recorder.commit(reservation, action).map(|_| ())
                        } else {
                            state.recorder.cancel(reservation).map(|_| ())
                        }
                    } else {
                        state.recorder.cancel(reservation).map(|_| ())
                    };
                    if let Err(error) = outcome {
                        first_error.get_or_insert(error);
                    }
                }
                first_error.map_or(Ok(()), Err)
            }
        }
    }

    /// Sanitizes a supported user-chrome command and reserves its source-order
    /// slot before browser state can change. A preflight failure invalidates
    /// only the exact recording that attempted the capture.
    pub fn begin_user_chrome_capture(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        command: &BrowserCommand,
    ) -> Result<Option<BrowserUserChromeCapture>, BrowserRecordingError> {
        let mut state = self.lock();
        let Some(instance) = state.recorder.active_instance(workspace_key) else {
            return Ok(None);
        };
        let intent = match prepare_user_chrome_intent(command) {
            Ok(Some(intent)) => intent,
            Ok(None) => return Ok(None),
            Err(error) => {
                invalidate_exact_recording(&mut state, &instance);
                return Err(error);
            }
        };
        let reservation = match state.recorder.reserve_on(
            &instance,
            BrowserRecordingActor::User,
            intent.reservation_tab_id(),
            BrowserRisk::Normal,
        ) {
            Ok(reservation) => reservation,
            Err(error) => {
                invalidate_exact_recording(&mut state, &instance);
                return Err(error);
            }
        };
        Ok(Some(BrowserUserChromeCapture {
            instance,
            reservation,
            intent,
        }))
    }

    /// Cancels a failed browser mutation or commits the response-derived typed
    /// action. Any post-success capture failure invalidates only this token's
    /// exact recording, so an incomplete draft can never remain saveable.
    pub fn complete_user_chrome_capture(
        &self,
        capture: BrowserUserChromeCapture,
        result: &Result<BrowserResponse, BrowserError>,
    ) -> Result<BrowserRecordingCommit, BrowserRecordingError> {
        let BrowserUserChromeCapture {
            instance,
            reservation,
            intent,
        } = capture;
        let mut state = self.lock();

        if result.is_err() {
            return match state.recorder.cancel(reservation) {
                Ok(BrowserRecordingCommit::Recorded | BrowserRecordingCommit::Buffered) => {
                    Ok(BrowserRecordingCommit::Ignored)
                }
                Ok(BrowserRecordingCommit::Ignored) => {
                    invalidate_exact_recording(&mut state, &instance);
                    Err(BrowserRecordingError::StaleReservation)
                }
                Err(error) => {
                    invalidate_exact_recording(&mut state, &instance);
                    Err(error)
                }
            };
        }

        if !active_instance_matches(&state, &instance) {
            invalidate_exact_recording(&mut state, &instance);
            return Err(BrowserRecordingError::StaleInstance);
        }
        let prepared =
            prepare_user_chrome_action(&mut state.tab_aliases, &instance, &intent, result);
        let (action, closed_runtime_tab) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                invalidate_exact_recording(&mut state, &instance);
                return Err(error);
            }
        };
        let committed = match state.recorder.commit(reservation, action) {
            Ok(commit @ (BrowserRecordingCommit::Recorded | BrowserRecordingCommit::Buffered)) => {
                commit
            }
            Ok(BrowserRecordingCommit::Ignored) => {
                invalidate_exact_recording(&mut state, &instance);
                return Err(BrowserRecordingError::StaleReservation);
            }
            Err(error) => {
                invalidate_exact_recording(&mut state, &instance);
                return Err(error);
            }
        };
        if let Some(runtime_tab_id) = closed_runtime_tab {
            if let Some(aliases) = state.tab_aliases.get_mut(instance.workspace_key()) {
                aliases.runtime_to_alias.remove(&runtime_tab_id);
            }
        }
        Ok(committed)
    }

    pub(crate) fn with_recorder<R>(
        &self,
        apply: impl FnOnce(&mut BrowserWorkflowRecorder) -> R,
    ) -> R {
        apply(&mut self.lock().recorder)
    }

    fn lock(&self) -> MutexGuard<'_, BrowserWorkflowCoordinatorState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn prepare_user_chrome_intent(
    command: &BrowserCommand,
) -> Result<Option<BrowserUserChromeIntent>, BrowserRecordingError> {
    let intent = match command {
        BrowserCommand::CreateTab { url } => {
            if let Some(url) = url {
                BrowserRecordingAction::navigate(url)?;
            }
            BrowserUserChromeIntent::CreateTab {
                capture_url: url.is_some(),
            }
        }
        BrowserCommand::SelectTab { tab_id } => BrowserUserChromeIntent::SelectTab {
            runtime_tab_id: tab_id.clone(),
        },
        BrowserCommand::CloseTab { tab_id } => BrowserUserChromeIntent::CloseTab {
            runtime_tab_id: tab_id.clone(),
        },
        BrowserCommand::Navigate { tab_id, url } => {
            BrowserRecordingAction::navigate(url)?;
            BrowserUserChromeIntent::Navigate {
                runtime_tab_id: tab_id.clone(),
            }
        }
        BrowserCommand::Back { tab_id } => BrowserUserChromeIntent::Back {
            runtime_tab_id: tab_id.clone(),
        },
        BrowserCommand::Forward { tab_id } => BrowserUserChromeIntent::Forward {
            runtime_tab_id: tab_id.clone(),
        },
        BrowserCommand::Reload { tab_id } => BrowserUserChromeIntent::Reload {
            runtime_tab_id: tab_id.clone(),
        },
        BrowserCommand::UpdateViewport { tab_id, viewport } => {
            let viewport = BrowserRecipeViewport::from(viewport.clone());
            BrowserRecordingAction::recipe(BrowserRecipeAction::SetViewport {
                viewport: viewport.clone(),
            })?;
            BrowserUserChromeIntent::UpdateViewport {
                runtime_tab_id: tab_id.clone(),
                viewport,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(intent))
}

fn prepare_user_chrome_action(
    tab_aliases: &mut HashMap<BrowserWorkspaceKey, BrowserRecordingTabAliases>,
    instance: &BrowserRecordingInstance,
    intent: &BrowserUserChromeIntent,
    result: &Result<BrowserResponse, BrowserError>,
) -> Result<(BrowserRecordingAction, Option<String>), BrowserRecordingError> {
    let Ok(BrowserResponse::Workspace { mutation }) = result else {
        return Err(BrowserRecordingError::InvalidAction);
    };
    let runtime_tab_id = match intent {
        BrowserUserChromeIntent::CreateTab { .. } => mutation
            .snapshot
            .selected_tab_id
            .as_deref()
            .ok_or(BrowserRecordingError::InvalidAction)?,
        BrowserUserChromeIntent::SelectTab { runtime_tab_id }
        | BrowserUserChromeIntent::CloseTab { runtime_tab_id }
        | BrowserUserChromeIntent::Navigate { runtime_tab_id }
        | BrowserUserChromeIntent::Back { runtime_tab_id }
        | BrowserUserChromeIntent::Forward { runtime_tab_id }
        | BrowserUserChromeIntent::Reload { runtime_tab_id }
        | BrowserUserChromeIntent::UpdateViewport { runtime_tab_id, .. } => runtime_tab_id,
    };
    let tab_alias = tab_aliases
        .get_mut(instance.workspace_key())
        .filter(|aliases| aliases.instance_id == instance.id())
        .ok_or(BrowserRecordingError::StaleInstance)?
        .alias_for(runtime_tab_id)?;

    let action = match intent {
        BrowserUserChromeIntent::CreateTab { capture_url } => {
            let captured_url = if *capture_url {
                Some(BrowserRecipeValue::Literal {
                    value: mutation
                        .snapshot
                        .tabs
                        .iter()
                        .find(|tab| tab.id == runtime_tab_id)
                        .map(|tab| tab.url.clone())
                        .ok_or(BrowserRecordingError::InvalidAction)?,
                })
            } else {
                None
            };
            BrowserRecordingAction::recipe(BrowserRecipeAction::CreateTab {
                tab: tab_alias,
                url: captured_url,
            })?
        }
        BrowserUserChromeIntent::SelectTab { .. } => {
            BrowserRecordingAction::recipe(BrowserRecipeAction::SelectTab { tab: tab_alias })?
        }
        BrowserUserChromeIntent::CloseTab { .. } => {
            BrowserRecordingAction::recipe(BrowserRecipeAction::CloseTab { tab: tab_alias })?
        }
        BrowserUserChromeIntent::Navigate { .. } => {
            let url = mutation
                .snapshot
                .tabs
                .iter()
                .find(|tab| tab.id == runtime_tab_id)
                .map(|tab| tab.url.as_str())
                .ok_or(BrowserRecordingError::InvalidAction)?;
            BrowserRecordingAction::navigate(url)?
        }
        BrowserUserChromeIntent::Back { .. } => {
            BrowserRecordingAction::recipe(BrowserRecipeAction::Back)?
        }
        BrowserUserChromeIntent::Forward { .. } => {
            BrowserRecordingAction::recipe(BrowserRecipeAction::Forward)?
        }
        BrowserUserChromeIntent::Reload { .. } => {
            BrowserRecordingAction::recipe(BrowserRecipeAction::Reload)?
        }
        BrowserUserChromeIntent::UpdateViewport { viewport, .. } => {
            BrowserRecordingAction::recipe(BrowserRecipeAction::SetViewport {
                viewport: viewport.clone(),
            })?
        }
    };
    let closed_runtime_tab = matches!(intent, BrowserUserChromeIntent::CloseTab { .. })
        .then(|| runtime_tab_id.to_string());
    Ok((action, closed_runtime_tab))
}

fn active_instance_matches(
    state: &BrowserWorkflowCoordinatorState,
    instance: &BrowserRecordingInstance,
) -> bool {
    state
        .recorder
        .active_instance(instance.workspace_key())
        .is_some_and(|active| active.id() == instance.id())
}

fn invalidate_exact_recording(
    state: &mut BrowserWorkflowCoordinatorState,
    instance: &BrowserRecordingInstance,
) -> bool {
    if active_instance_matches(state, instance) {
        if state.recorder.stop(instance).is_err() {
            return false;
        }
    } else if state.recorder.review(instance).is_err() {
        return false;
    }
    if state.recorder.discard(instance).is_err() {
        return false;
    }
    state.tab_aliases.remove(instance.workspace_key());
    state
        .agent_commands
        .retain(|(workspace_key, _), _| workspace_key != instance.workspace_key());
    true
}

fn cancel_pending_agent(recorder: &mut BrowserWorkflowRecorder, pending: PendingAgentRecording) {
    for reservation in pending.reservations {
        let _ = recorder.cancel(reservation);
    }
}

fn source_order_input_action(
    command: &BrowserCommand,
) -> Result<Option<BrowserRecordingAction>, BrowserRecordingError> {
    match command {
        BrowserCommand::Upload { target, .. } => {
            BrowserRecordingAction::upload(recipe_locator(target)).map(Some)
        }
        BrowserCommand::SecretType {
            target, input_name, ..
        } => {
            BrowserRecordingAction::type_secret_input(recipe_locator(target), input_name).map(Some)
        }
        _ => Ok(None),
    }
}

fn prepare_agent_actions(
    actions: &[BrowserAction],
    runtime_targets: &[BrowserRuntimeTarget],
) -> Result<Vec<Option<BrowserRecordingAction>>, BrowserRecordingError> {
    let expected_targets = actions
        .iter()
        .map(|action| usize::from(matches!(action, BrowserAction::DragDrop { .. })) + 1)
        .sum::<usize>();
    if runtime_targets.len() != expected_targets {
        return Err(BrowserRecordingError::InvalidAction);
    }

    let mut runtime_targets = runtime_targets.iter();
    let mut prepared = Vec::with_capacity(actions.len());
    for action in actions {
        let runtime_target = runtime_targets
            .next()
            .ok_or(BrowserRecordingError::InvalidAction)?;
        let converted = prepare_agent_action(action, runtime_target, &mut runtime_targets);
        prepared.push(converted.ok());
    }
    Ok(prepared)
}

fn prepare_agent_action<'a>(
    action: &BrowserAction,
    runtime_target: &BrowserRuntimeTarget,
    remaining_runtime_targets: &mut impl Iterator<Item = &'a BrowserRuntimeTarget>,
) -> Result<BrowserRecordingAction, BrowserRecordingError> {
    let recipe = match action {
        BrowserAction::Click { target } => BrowserRecipeAction::Click {
            locator: recipe_locator(target),
        },
        BrowserAction::Hover { target } => BrowserRecipeAction::Hover {
            locator: recipe_locator(target),
        },
        BrowserAction::Focus { target } => BrowserRecipeAction::Focus {
            locator: recipe_locator(target),
        },
        BrowserAction::Type { target, text } => {
            let locator = recipe_locator(target);
            if runtime_target_is_sensitive(runtime_target) {
                // Inspect the live target before copying command text into any
                // recorder-owned value. Sensitive values become an unset
                // Secret input and the command text is never retained.
                return BrowserRecordingAction::type_password(locator);
            }
            return BrowserRecordingAction::type_text(locator, text);
        }
        BrowserAction::Clear { target } => BrowserRecipeAction::Clear {
            locator: recipe_locator(target),
        },
        BrowserAction::Select { target, values } => {
            if runtime_target_is_sensitive(runtime_target) {
                return Err(BrowserRecordingError::InvalidAction);
            }
            BrowserRecipeAction::Select {
                locator: recipe_locator(target),
                values: values
                    .iter()
                    .map(|value| BrowserRecipeValue::Literal {
                        value: value.clone(),
                    })
                    .collect(),
            }
        }
        BrowserAction::Keypress { target, key } => {
            if runtime_target_is_sensitive(runtime_target) && !safe_sensitive_keypress(key) {
                return Err(BrowserRecordingError::InvalidAction);
            }
            BrowserRecipeAction::Keypress {
                locator: target.as_ref().map(recipe_locator),
                key: BrowserRecipeValue::Literal { value: key.clone() },
            }
        }
        BrowserAction::Scroll {
            target,
            delta_x,
            delta_y,
        } => BrowserRecipeAction::Scroll {
            locator: target.as_ref().map(recipe_locator),
            delta_x: *delta_x,
            delta_y: *delta_y,
        },
        BrowserAction::DragDrop {
            source,
            destination,
        } => {
            remaining_runtime_targets
                .next()
                .ok_or(BrowserRecordingError::InvalidAction)?;
            BrowserRecipeAction::DragDrop {
                source: recipe_locator(source),
                destination: recipe_locator(destination),
            }
        }
    };
    BrowserRecordingAction::recipe(recipe)
}

fn recipe_locator(target: &BrowserActionTarget) -> BrowserRecipeLocator {
    BrowserRecipeLocator::from(target.locator.clone())
}

fn runtime_target_is_sensitive(target: &BrowserRuntimeTarget) -> bool {
    if target
        .input_type
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("password"))
    {
        return true;
    }
    let combined = [
        target.role.as_deref(),
        target.name.as_deref(),
        target.input_type.as_deref(),
        target.autocomplete.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_ascii_lowercase();
    [
        "password",
        "current-password",
        "new-password",
        "one-time-code",
        "security",
        "secret",
        "credential",
        "token",
        "2fa",
        "mfa",
    ]
    .iter()
    .any(|marker| combined.contains(marker))
}

fn safe_sensitive_keypress(key: &str) -> bool {
    // Exact non-text allowlist for password/security targets. Printable keys,
    // whitespace, modifiers, chords, and arbitrary browser key names are
    // intentionally not retained.
    matches!(
        key,
        "Enter"
            | "Tab"
            | "Escape"
            | "Backspace"
            | "Delete"
            | "ArrowUp"
            | "ArrowDown"
            | "ArrowLeft"
            | "ArrowRight"
            | "Home"
            | "End"
            | "PageUp"
            | "PageDown"
    )
}

fn prepared_agent_command_action(
    tab_aliases: &mut HashMap<BrowserWorkspaceKey, BrowserRecordingTabAliases>,
    workspace_key: &BrowserWorkspaceKey,
    command: &BrowserCommand,
    result: &Result<BrowserResponse, BrowserError>,
) -> Result<Option<BrowserRecordingAction>, BrowserRecordingError> {
    let Ok(response) = result else {
        return Ok(None);
    };
    if !agent_response_matches_command(command, response) {
        return Ok(None);
    }
    let action = match command {
        BrowserCommand::CreateTab { url } => {
            let BrowserResponse::Workspace { mutation } = response else {
                unreachable!("response type checked above")
            };
            let runtime_tab_id = mutation
                .snapshot
                .selected_tab_id
                .as_deref()
                .ok_or(BrowserRecordingError::InvalidAction)?;
            let tab = alias_for(tab_aliases, workspace_key, runtime_tab_id)?;
            let url = if url.is_some() {
                mutation
                    .snapshot
                    .tabs
                    .iter()
                    .find(|candidate| candidate.id == runtime_tab_id)
                    .map(|candidate| BrowserRecipeValue::Literal {
                        value: candidate.url.clone(),
                    })
            } else {
                None
            };
            Some(BrowserRecordingAction::recipe(
                BrowserRecipeAction::CreateTab { tab, url },
            )?)
        }
        BrowserCommand::SelectTab { tab_id } => Some(BrowserRecordingAction::recipe(
            BrowserRecipeAction::SelectTab {
                tab: alias_for(tab_aliases, workspace_key, tab_id)?,
            },
        )?),
        BrowserCommand::CloseTab { tab_id } => {
            let tab = alias_for(tab_aliases, workspace_key, tab_id)?;
            if let Some(aliases) = tab_aliases.get_mut(workspace_key) {
                aliases.runtime_to_alias.remove(tab_id);
            }
            Some(BrowserRecordingAction::recipe(
                BrowserRecipeAction::CloseTab { tab },
            )?)
        }
        BrowserCommand::Navigate { tab_id, .. } => {
            let BrowserResponse::Workspace { mutation } = response else {
                unreachable!("response type checked above")
            };
            let url = mutation
                .snapshot
                .tabs
                .iter()
                .find(|candidate| candidate.id == *tab_id)
                .map(|candidate| candidate.url.as_str())
                .ok_or(BrowserRecordingError::InvalidAction)?;
            Some(BrowserRecordingAction::navigate(url)?)
        }
        BrowserCommand::Back { .. } => {
            Some(BrowserRecordingAction::recipe(BrowserRecipeAction::Back)?)
        }
        BrowserCommand::Forward { .. } => Some(BrowserRecordingAction::recipe(
            BrowserRecipeAction::Forward,
        )?),
        BrowserCommand::Reload { .. } => {
            Some(BrowserRecordingAction::recipe(BrowserRecipeAction::Reload)?)
        }
        BrowserCommand::UpdateViewport { viewport, .. } => Some(BrowserRecordingAction::recipe(
            BrowserRecipeAction::SetViewport {
                viewport: BrowserRecipeViewport::from(viewport.clone()),
            },
        )?),
        BrowserCommand::Wait {
            condition,
            timeout_ms,
            ..
        } => prepare_wait_action(condition, *timeout_ms).ok(),
        BrowserCommand::Screenshot { mode, .. } => Some(BrowserRecordingAction::recipe(
            BrowserRecipeAction::Screenshot {
                full_page: matches!(mode, BrowserScreenshotMode::FullPage),
            },
        )?),
        BrowserCommand::Upload { target, .. } => {
            Some(BrowserRecordingAction::upload(recipe_locator(target))?)
        }
        BrowserCommand::Cdp { method, .. } => Some(BrowserRecordingAction::recipe(
            BrowserRecipeAction::CdpMarker {
                method: method.clone(),
            },
        )?),
        _ => None,
    };
    Ok(action)
}

fn agent_response_matches_command(command: &BrowserCommand, response: &BrowserResponse) -> bool {
    matches!(
        (command, response),
        (
            BrowserCommand::CreateTab { .. }
                | BrowserCommand::SelectTab { .. }
                | BrowserCommand::CloseTab { .. }
                | BrowserCommand::Navigate { .. }
                | BrowserCommand::Back { .. }
                | BrowserCommand::Forward { .. }
                | BrowserCommand::Reload { .. }
                | BrowserCommand::UpdateViewport { .. },
            BrowserResponse::Workspace { .. }
        ) | (BrowserCommand::Wait { .. }, BrowserResponse::Wait { .. })
            | (
                BrowserCommand::Screenshot { .. },
                BrowserResponse::Screenshot { .. }
            )
            | (
                BrowserCommand::Upload { .. },
                BrowserResponse::Upload { .. }
            )
            | (BrowserCommand::Cdp { .. }, BrowserResponse::Cdp { .. })
    )
}

fn alias_for(
    tab_aliases: &mut HashMap<BrowserWorkspaceKey, BrowserRecordingTabAliases>,
    workspace_key: &BrowserWorkspaceKey,
    runtime_tab_id: &str,
) -> Result<String, BrowserRecordingError> {
    tab_aliases
        .get_mut(workspace_key)
        .ok_or(BrowserRecordingError::StaleInstance)?
        .alias_for(runtime_tab_id)
}

fn prepare_wait_action(
    condition: &BrowserWaitCondition,
    timeout_ms: u64,
) -> Result<BrowserRecordingAction, BrowserRecordingError> {
    let timeout_ms = timeout_ms.clamp(1, 300_000);
    let condition = match condition {
        BrowserWaitCondition::Duration { duration_ms } => BrowserRecipeWait::Duration {
            duration_ms: *duration_ms,
        },
        BrowserWaitCondition::Url { value, exact } => BrowserRecipeWait::Url {
            value: BrowserRecipeValue::Literal {
                value: value.clone(),
            },
            exact: *exact,
            timeout_ms,
        },
        BrowserWaitCondition::Load => BrowserRecipeWait::Load { timeout_ms },
        BrowserWaitCondition::NetworkIdle => BrowserRecipeWait::NetworkIdle { timeout_ms },
        BrowserWaitCondition::ElementPresent { target } => BrowserRecipeWait::ElementPresent {
            locator: recipe_locator(target),
            timeout_ms,
        },
        BrowserWaitCondition::ElementVisible { target } => BrowserRecipeWait::ElementVisible {
            locator: recipe_locator(target),
            timeout_ms,
        },
        BrowserWaitCondition::ElementHidden { target } => BrowserRecipeWait::ElementHidden {
            locator: recipe_locator(target),
            timeout_ms,
        },
        BrowserWaitCondition::TextPresent { text } => BrowserRecipeWait::TextPresent {
            value: BrowserRecipeValue::Literal {
                value: text.clone(),
            },
            timeout_ms,
        },
        BrowserWaitCondition::TextAbsent { text } => BrowserRecipeWait::TextAbsent {
            value: BrowserRecipeValue::Literal {
                value: text.clone(),
            },
            timeout_ms,
        },
        BrowserWaitCondition::Title { .. }
        | BrowserWaitCondition::ElementAbsent { .. }
        | BrowserWaitCondition::ElementValue { .. }
        | BrowserWaitCondition::JavaScript { .. } => {
            return Err(BrowserRecordingError::InvalidAction)
        }
    };
    BrowserRecordingAction::recipe(BrowserRecipeAction::Wait { condition })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{
        BrowserActionResult, BrowserLocator, BrowserRevision, BrowserWorkspaceMutation,
        BrowserWorkspaceSnapshot,
    };

    fn prepared_secret_command() -> BrowserCommand {
        BrowserCommand::SecretType {
            tab_id: "tab-a".to_string(),
            target: BrowserActionTarget {
                locator: BrowserLocator {
                    test_id: Some("credential".to_string()),
                    ..BrowserLocator::default()
                },
                ..BrowserActionTarget::default()
            },
            input_name: "credential".to_string(),
        }
    }

    fn successful_secret_response() -> Result<BrowserResponse, BrowserError> {
        Ok(BrowserResponse::Action {
            result: BrowserActionResult {
                completed_actions: 1,
                revision: BrowserRevision(12),
            },
        })
    }

    #[test]
    fn post_success_secret_commit_failure_invalidates_only_its_exact_recording() {
        let workspace = BrowserWorkspaceKey {
            project_id: "project-secret-commit-failure".to_string(),
            ai_tab_id: "conversation-secret-commit-failure".to_string(),
        };
        let command = prepared_secret_command();

        let coordinator = BrowserWorkflowCoordinator::default();
        coordinator
            .start(workspace.clone())
            .expect("start exact recording");
        coordinator
            .reserve_agent_command(&workspace, "secret", &command, BrowserRisk::Normal)
            .expect("reserve secret");
        coordinator
            .inspect_agent_secret_type(
                &workspace,
                "secret",
                &command,
                &BrowserRuntimeTarget::default(),
                BrowserRisk::AccountSecurity,
            )
            .expect("prepare secret");
        {
            let mut state = coordinator.lock();
            let key = (workspace.clone(), "secret".to_string());
            let reservation = state.agent_commands[&key].reservations[0].clone();
            state
                .recorder
                .cancel(reservation)
                .expect("induce stale prepared reservation");
        }
        assert_eq!(
            coordinator.complete_agent_command(
                &workspace,
                "secret",
                &command,
                &successful_secret_response(),
            ),
            Err(BrowserRecordingError::StaleReservation),
        );
        assert_eq!(
            coordinator.status(&workspace),
            BrowserRecordingStatus::Inactive
        );

        let replacement_coordinator = BrowserWorkflowCoordinator::default();
        let retired = replacement_coordinator
            .start(workspace.clone())
            .expect("start retired recording");
        replacement_coordinator
            .reserve_agent_command(&workspace, "retired", &command, BrowserRisk::Normal)
            .expect("reserve retired secret");
        replacement_coordinator
            .inspect_agent_secret_type(
                &workspace,
                "retired",
                &command,
                &BrowserRuntimeTarget::default(),
                BrowserRisk::AccountSecurity,
            )
            .expect("prepare retired secret");
        let replacement = {
            let mut state = replacement_coordinator.lock();
            state.recorder.stop(&retired).expect("stop retired");
            state.recorder.discard(&retired).expect("discard retired");
            state
                .recorder
                .start(workspace.clone())
                .expect("start replacement")
        };
        assert_eq!(
            replacement_coordinator.complete_agent_command(
                &workspace,
                "retired",
                &command,
                &successful_secret_response(),
            ),
            Err(BrowserRecordingError::StaleReservation),
        );
        assert_eq!(
            replacement_coordinator
                .active_instance(&workspace)
                .expect("replacement stays active")
                .id(),
            replacement.id(),
        );
    }

    #[test]
    fn post_success_commit_failure_invalidates_the_exact_user_chrome_recording() {
        let coordinator = BrowserWorkflowCoordinator::default();
        let workspace = BrowserWorkspaceKey {
            project_id: "project-commit-failure".to_string(),
            ai_tab_id: "conversation-commit-failure".to_string(),
        };
        coordinator
            .start(workspace.clone())
            .expect("start exact recording");
        let capture = coordinator
            .begin_user_chrome_capture(
                &workspace,
                &BrowserCommand::Reload {
                    tab_id: "tab-a".to_string(),
                },
            )
            .expect("preflight user chrome action")
            .expect("reload reserves before mutation");

        coordinator
            .cancel(capture.reservation.clone())
            .expect("induce a stale commit reservation");
        let result = Ok(BrowserResponse::Workspace {
            mutation: BrowserWorkspaceMutation {
                revision: BrowserRevision(1),
                snapshot: BrowserWorkspaceSnapshot {
                    revision: BrowserRevision(1),
                    ..BrowserWorkspaceSnapshot::default()
                },
            },
        });
        assert_eq!(
            coordinator.complete_user_chrome_capture(capture, &result),
            Err(BrowserRecordingError::StaleReservation),
        );
        assert_eq!(
            coordinator.status(&workspace),
            BrowserRecordingStatus::Inactive,
            "a successful browser mutation with a failed commit must not leave a draft"
        );
    }
}
