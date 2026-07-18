use super::commands::verified_authenticated_local_project_root;
use super::{
    browser_cdp_method_risk, classify_upload_path, BrowserAction, BrowserActionTarget,
    BrowserCommand, BrowserController, BrowserError, BrowserInvocationActor,
    BrowserInvocationContext, BrowserLocator, BrowserRecipeAction, BrowserRecipeAssertion,
    BrowserRecipeElementState, BrowserRecipeInputKind, BrowserRecipeLocator, BrowserRecipeValue,
    BrowserRecipeWait, BrowserReplayCoordinator, BrowserReplayError, BrowserReplayExecutionHandle,
    BrowserReplayFailureCode, BrowserReplayInstance, BrowserReplayPlan, BrowserReplayProjection,
    BrowserReplayStatus, BrowserResponse, BrowserRisk, BrowserScreenshotMode, BrowserTabSnapshot,
    BrowserViewport, BrowserWaitCondition, BrowserWorkspaceSnapshot,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const MAX_REPLAY_TAB_ALIASES: usize = 64;
const MAX_REPLAY_RUNTIME_TABS: usize = 256;
const ASSERTION_TIMEOUT_MS: u64 = 250;

pub async fn execute_browser_replay(
    controller: &BrowserController,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    execution: BrowserReplayExecutionHandle,
    actor: BrowserInvocationActor,
    authenticated_local_project_root: &Path,
) -> Result<BrowserReplayProjection, BrowserReplayError> {
    let _secret_store_close = BrowserReplaySecretStoreCloseGuard(&execution);
    if !execution.same_instance(instance) {
        return Err(BrowserReplayError::StaleInstance);
    }
    if controller.workspace_key() != instance.workspace_key()
        || actor != BrowserInvocationActor::Agent
    {
        return Err(BrowserReplayError::InvalidExecutionAuthority);
    }
    let local_project_root =
        verified_authenticated_local_project_root(authenticated_local_project_root)
            .map_err(|_| BrowserReplayError::InvalidExecutionAuthority)?;
    if let Some(cancelled) = cancelled_projection(&execution, coordinator, instance)? {
        return Ok(cancelled);
    }
    match coordinator.status(instance)? {
        projection if projection.status == BrowserReplayStatus::Pending => {
            if let Err(error) = coordinator.begin(instance) {
                return retained_terminal_after_transition_error(coordinator, instance, error);
            }
        }
        projection if projection.status == BrowserReplayStatus::Running => {}
        projection
            if matches!(
                projection.status,
                BrowserReplayStatus::Completed
                    | BrowserReplayStatus::Failed
                    | BrowserReplayStatus::Cancelled
            ) =>
        {
            return Ok(projection);
        }
        _ => return Err(BrowserReplayError::InvalidTransition),
    }
    let plan = execution.plan();
    let create = match checked_request(
        controller,
        coordinator,
        instance,
        &execution,
        actor,
        "replay setup create tab",
        BrowserCommand::CreateTab { url: None },
    )
    .await
    {
        Ok(response) => response,
        Err(failure) => return finish_failure(coordinator, instance, failure),
    };
    let BrowserResponse::Workspace { mutation } = create else {
        return fail_step(coordinator, instance);
    };
    let Some(tab_id) = exact_selected_tab(&mutation.snapshot).map(str::to_string) else {
        return fail_step(coordinator, instance);
    };

    let viewport = BrowserViewport::from(plan.viewport());
    let response = match checked_request(
        controller,
        coordinator,
        instance,
        &execution,
        actor,
        "replay setup apply viewport",
        BrowserCommand::UpdateViewport {
            tab_id: tab_id.clone(),
            viewport: viewport.clone(),
        },
    )
    .await
    {
        Ok(response) => response,
        Err(failure) => return finish_failure(coordinator, instance, failure),
    };
    let BrowserResponse::Workspace { mutation } = response else {
        return fail_step(coordinator, instance);
    };
    if !snapshot_proves_selected_tab(&mutation.snapshot, &tab_id, Some(&viewport), None) {
        return fail_step(coordinator, instance);
    }

    let response = match checked_request(
        controller,
        coordinator,
        instance,
        &execution,
        actor,
        "replay setup navigate start",
        BrowserCommand::Navigate {
            tab_id: tab_id.clone(),
            url: plan.start_url().to_string(),
        },
    )
    .await
    {
        Ok(response) => response,
        Err(failure) => return finish_failure(coordinator, instance, failure),
    };
    let BrowserResponse::Workspace { mutation } = response else {
        return fail_step(coordinator, instance);
    };
    if !snapshot_proves_selected_tab(
        &mutation.snapshot,
        &tab_id,
        Some(&viewport),
        Some(plan.start_url()),
    ) {
        return fail_step(coordinator, instance);
    }
    let legacy_creates_tab_one = plan.steps().iter().any(|step| {
        matches!(
            &step.action,
            BrowserRecipeAction::CreateTab { tab, .. } if tab == "tab-1"
        )
    });
    let Some(mut tabs) = ReplayTabState::new(mutation.snapshot, tab_id, !legacy_creates_tab_one)
    else {
        return fail_step(coordinator, instance);
    };

    for (step_index, step) in plan.steps().iter().enumerate() {
        match execute_action(
            controller,
            coordinator,
            instance,
            &execution,
            actor,
            plan,
            &local_project_root,
            &mut tabs,
            &step.action,
        )
        .await
        {
            Ok(()) => {}
            Err(failure) => return finish_failure(coordinator, instance, failure),
        }
        if let Some(wait) = &step.wait {
            match execute_step_wait(
                controller,
                coordinator,
                instance,
                &execution,
                actor,
                plan,
                &tabs,
                wait,
            )
            .await
            {
                Ok(()) => {}
                Err(failure) => return finish_failure(coordinator, instance, failure),
            }
        }
        for assertion in &step.assertions {
            match execute_assertion(
                controller,
                coordinator,
                instance,
                &execution,
                actor,
                plan,
                &tabs,
                assertion,
            )
            .await
            {
                Ok(()) => {}
                Err(failure) => return finish_failure(coordinator, instance, failure),
            }
        }
        if let Some(cancelled) = cancelled_projection(&execution, coordinator, instance)? {
            return Ok(cancelled);
        }
        if let Err(error) = coordinator.advance_step(instance, step_index) {
            return retained_terminal_after_transition_error(coordinator, instance, error);
        }
    }
    match coordinator.complete(instance) {
        Ok(projection) => Ok(projection),
        Err(error) => retained_terminal_after_transition_error(coordinator, instance, error),
    }
}

enum ReplayActionFailure {
    StepFailed,
    AssertionFailed,
    PageConditionTimeout,
    Terminal(BrowserReplayProjection),
    Replay(BrowserReplayError),
}

struct ReplayTabState {
    current: String,
    aliases: HashMap<String, String>,
    seen_aliases: HashSet<String>,
    known_runtime_tabs: HashSet<String>,
}

impl ReplayTabState {
    fn new(
        snapshot: BrowserWorkspaceSnapshot,
        setup_tab_id: String,
        bind_initial_tab_one: bool,
    ) -> Option<Self> {
        let known_runtime_tabs = validated_runtime_tabs(&snapshot)?;
        if exact_selected_tab(&snapshot) != Some(setup_tab_id.as_str()) {
            return None;
        }
        let mut aliases = HashMap::new();
        let mut seen_aliases = HashSet::new();
        if bind_initial_tab_one {
            aliases.insert("tab-1".to_string(), setup_tab_id.clone());
            seen_aliases.insert("tab-1".to_string());
        }
        Some(Self {
            current: setup_tab_id,
            aliases,
            seen_aliases,
            known_runtime_tabs,
        })
    }

    fn runtime_id(&self, alias: &str) -> Option<&str> {
        self.aliases.get(alias).map(String::as_str)
    }

    fn replace_snapshot(&mut self, snapshot: &BrowserWorkspaceSnapshot) -> bool {
        let Some(selected) = exact_selected_tab(snapshot).map(str::to_string) else {
            return false;
        };
        let Some(known_runtime_tabs) = validated_runtime_tabs(snapshot) else {
            return false;
        };
        self.current = selected;
        self.known_runtime_tabs = known_runtime_tabs;
        true
    }
}

async fn execute_action(
    controller: &BrowserController,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    execution: &BrowserReplayExecutionHandle,
    actor: BrowserInvocationActor,
    plan: &BrowserReplayPlan,
    local_project_root: &Path,
    tabs: &mut ReplayTabState,
    action: &BrowserRecipeAction,
) -> Result<(), ReplayActionFailure> {
    match action {
        BrowserRecipeAction::CreateTab { tab, url } => {
            let url = match url {
                Some(value) => Some(
                    resolve_value(plan, value)
                        .map(str::to_string)
                        .ok_or(ReplayActionFailure::StepFailed)?,
                ),
                None => None,
            };
            let response = checked_request(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                "replay step create tab",
                BrowserCommand::CreateTab { url: url.clone() },
            )
            .await?;
            let BrowserResponse::Workspace { mutation } = response else {
                return Err(ReplayActionFailure::StepFailed);
            };
            let Some(runtime_tab_id) = exact_selected_tab(&mutation.snapshot).map(str::to_string)
            else {
                return Err(ReplayActionFailure::StepFailed);
            };
            if tabs.known_runtime_tabs.contains(&runtime_tab_id)
                || url.as_deref().is_some_and(|expected| {
                    exact_tab(&mutation.snapshot, &runtime_tab_id)
                        .is_none_or(|runtime| runtime.url != expected)
                })
            {
                return Err(ReplayActionFailure::StepFailed);
            }
            if tabs.aliases.len() >= MAX_REPLAY_TAB_ALIASES || tabs.seen_aliases.contains(tab) {
                return Err(ReplayActionFailure::StepFailed);
            }
            if !tabs.replace_snapshot(&mutation.snapshot) {
                return Err(ReplayActionFailure::StepFailed);
            }
            tabs.seen_aliases.insert(tab.clone());
            tabs.aliases.insert(tab.clone(), runtime_tab_id);
            Ok(())
        }
        BrowserRecipeAction::SelectTab { tab } => {
            let runtime_tab_id = tabs
                .runtime_id(tab)
                .map(str::to_string)
                .ok_or(ReplayActionFailure::StepFailed)?;
            let response = checked_request(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                "replay step select tab",
                BrowserCommand::SelectTab {
                    tab_id: runtime_tab_id.clone(),
                },
            )
            .await?;
            let BrowserResponse::Workspace { mutation } = response else {
                return Err(ReplayActionFailure::StepFailed);
            };
            if exact_selected_tab(&mutation.snapshot) != Some(runtime_tab_id.as_str())
                || !tabs.replace_snapshot(&mutation.snapshot)
            {
                return Err(ReplayActionFailure::StepFailed);
            }
            Ok(())
        }
        BrowserRecipeAction::CloseTab { tab } => {
            let runtime_tab_id = tabs
                .runtime_id(tab)
                .map(str::to_string)
                .ok_or(ReplayActionFailure::StepFailed)?;
            let response = checked_request(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                "replay step close tab",
                BrowserCommand::CloseTab {
                    tab_id: runtime_tab_id.clone(),
                },
            )
            .await?;
            let BrowserResponse::Workspace { mutation } = response else {
                return Err(ReplayActionFailure::StepFailed);
            };
            if mutation
                .snapshot
                .tabs
                .iter()
                .any(|runtime| runtime.id == runtime_tab_id)
                || !tabs.replace_snapshot(&mutation.snapshot)
            {
                return Err(ReplayActionFailure::StepFailed);
            }
            tabs.aliases.remove(tab);
            Ok(())
        }
        BrowserRecipeAction::Back => {
            acknowledged_command(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                "replay step navigate back",
                BrowserCommand::Back {
                    tab_id: tabs.current.clone(),
                },
            )
            .await
        }
        BrowserRecipeAction::Forward => {
            acknowledged_command(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                "replay step navigate forward",
                BrowserCommand::Forward {
                    tab_id: tabs.current.clone(),
                },
            )
            .await
        }
        BrowserRecipeAction::Reload => {
            acknowledged_command(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                "replay step reload tab",
                BrowserCommand::Reload {
                    tab_id: tabs.current.clone(),
                },
            )
            .await
        }
        BrowserRecipeAction::SetViewport { viewport } => {
            let viewport = BrowserViewport::from(*viewport);
            let response = checked_request(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                "replay step set viewport",
                BrowserCommand::UpdateViewport {
                    tab_id: tabs.current.clone(),
                    viewport: viewport.clone(),
                },
            )
            .await?;
            let BrowserResponse::Workspace { mutation } = response else {
                return Err(ReplayActionFailure::StepFailed);
            };
            if !snapshot_proves_selected_tab(
                &mutation.snapshot,
                &tabs.current,
                Some(&viewport),
                None,
            ) || !tabs.replace_snapshot(&mutation.snapshot)
            {
                return Err(ReplayActionFailure::StepFailed);
            }
            Ok(())
        }
        BrowserRecipeAction::CdpMarker { method } => {
            let response = checked_request_with_options(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                "replay reviewed CDP marker",
                browser_cdp_method_risk(method),
                None,
                BrowserCommand::Cdp {
                    tab_id: tabs.current.clone(),
                    method: method.clone(),
                    params: serde_json::json!({}),
                },
            )
            .await?;
            matches!(response, BrowserResponse::Cdp { .. })
                .then_some(())
                .ok_or(ReplayActionFailure::StepFailed)
        }
        BrowserRecipeAction::Navigate { url } => {
            let url = resolve_value(plan, url)
                .map(str::to_string)
                .ok_or(ReplayActionFailure::StepFailed)?;
            let response = checked_request(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                "replay step navigate",
                BrowserCommand::Navigate {
                    tab_id: tabs.current.clone(),
                    url: url.clone(),
                },
            )
            .await?;
            let BrowserResponse::Workspace { mutation } = response else {
                return Err(ReplayActionFailure::StepFailed);
            };
            if !snapshot_proves_selected_tab(&mutation.snapshot, &tabs.current, None, Some(&url))
                || !tabs.replace_snapshot(&mutation.snapshot)
            {
                return Err(ReplayActionFailure::StepFailed);
            }
            Ok(())
        }
        BrowserRecipeAction::Click { locator } => {
            semantic_action(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                tabs,
                BrowserAction::Click {
                    target: action_target(locator),
                },
            )
            .await
        }
        BrowserRecipeAction::Hover { locator } => {
            semantic_action(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                tabs,
                BrowserAction::Hover {
                    target: action_target(locator),
                },
            )
            .await
        }
        BrowserRecipeAction::Focus { locator } => {
            semantic_action(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                tabs,
                BrowserAction::Focus {
                    target: action_target(locator),
                },
            )
            .await
        }
        BrowserRecipeAction::Type { locator, value } => {
            if let BrowserRecipeValue::Input { name } = value {
                if plan.input_kind(name) == Some(BrowserRecipeInputKind::Secret) {
                    return secret_semantic_action(
                        controller,
                        coordinator,
                        instance,
                        execution,
                        actor,
                        tabs,
                        locator,
                        name,
                    )
                    .await;
                }
            }
            let text = resolve_value(plan, value)
                .map(str::to_string)
                .ok_or(ReplayActionFailure::StepFailed)?;
            semantic_action(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                tabs,
                BrowserAction::Type {
                    target: action_target(locator),
                    text,
                },
            )
            .await
        }
        BrowserRecipeAction::Clear { locator } => {
            semantic_action(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                tabs,
                BrowserAction::Clear {
                    target: action_target(locator),
                },
            )
            .await
        }
        BrowserRecipeAction::Select { locator, values } => {
            let values = values
                .iter()
                .map(|value| resolve_value(plan, value).map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .ok_or(ReplayActionFailure::StepFailed)?;
            semantic_action(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                tabs,
                BrowserAction::Select {
                    target: action_target(locator),
                    values,
                },
            )
            .await
        }
        BrowserRecipeAction::Keypress { locator, key } => {
            let key = resolve_value(plan, key)
                .map(str::to_string)
                .ok_or(ReplayActionFailure::StepFailed)?;
            semantic_action(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                tabs,
                BrowserAction::Keypress {
                    target: locator.as_ref().map(action_target),
                    key,
                },
            )
            .await
        }
        BrowserRecipeAction::Scroll {
            locator,
            delta_x,
            delta_y,
        } => {
            semantic_action(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                tabs,
                BrowserAction::Scroll {
                    target: locator.as_ref().map(action_target),
                    delta_x: *delta_x,
                    delta_y: *delta_y,
                },
            )
            .await
        }
        BrowserRecipeAction::DragDrop {
            source,
            destination,
        } => {
            semantic_action(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                tabs,
                BrowserAction::DragDrop {
                    source: action_target(source),
                    destination: action_target(destination),
                },
            )
            .await
        }
        BrowserRecipeAction::Upload { locator, file } => {
            let BrowserRecipeValue::Input { name } = file else {
                return Err(ReplayActionFailure::StepFailed);
            };
            if plan.input_kind(name) != Some(BrowserRecipeInputKind::File) {
                return Err(ReplayActionFailure::StepFailed);
            }
            let value = resolve_value(plan, file).ok_or(ReplayActionFailure::StepFailed)?;
            let candidate = PathBuf::from(value);
            let candidate = if candidate.is_absolute() {
                candidate
            } else {
                local_project_root.join(candidate)
            };
            let (canonical, risk) = classify_upload_path(local_project_root, candidate)
                .map_err(|_| ReplayActionFailure::StepFailed)?;
            let response = checked_request_with_options(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                "replay step upload file",
                risk,
                Some(local_project_root),
                BrowserCommand::Upload {
                    tab_id: tabs.current.clone(),
                    target: action_target(locator),
                    paths: vec![canonical.clone()],
                },
            )
            .await?;
            match response {
                BrowserResponse::Upload { result } if result.files == vec![canonical] => Ok(()),
                _ => Err(ReplayActionFailure::StepFailed),
            }
        }
        BrowserRecipeAction::Download { locator } => {
            semantic_action(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                tabs,
                BrowserAction::Click {
                    target: action_target(locator),
                },
            )
            .await
        }
        BrowserRecipeAction::Wait { condition } => {
            let (condition, timeout_ms) =
                compile_wait(plan, condition).ok_or(ReplayActionFailure::StepFailed)?;
            let response = checked_request(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                "replay step wait",
                BrowserCommand::Wait {
                    tab_id: tabs.current.clone(),
                    condition,
                    timeout_ms,
                },
            )
            .await?;
            match response {
                BrowserResponse::Wait { result } if result.matched => Ok(()),
                _ => Err(ReplayActionFailure::StepFailed),
            }
        }
        BrowserRecipeAction::Screenshot { full_page } => {
            let response = checked_request(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                "replay step screenshot",
                BrowserCommand::Screenshot {
                    tab_id: tabs.current.clone(),
                    mode: if *full_page {
                        BrowserScreenshotMode::FullPage
                    } else {
                        BrowserScreenshotMode::Viewport
                    },
                },
            )
            .await?;
            matches!(response, BrowserResponse::Screenshot { .. })
                .then_some(())
                .ok_or(ReplayActionFailure::StepFailed)
        }
    }
}

struct BrowserReplaySecretStoreCloseGuard<'a>(&'a BrowserReplayExecutionHandle);

impl Drop for BrowserReplaySecretStoreCloseGuard<'_> {
    fn drop(&mut self) {
        self.0.close_secret_store();
    }
}

async fn acknowledged_command(
    controller: &BrowserController,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    execution: &BrowserReplayExecutionHandle,
    actor: BrowserInvocationActor,
    intent: &'static str,
    command: BrowserCommand,
) -> Result<(), ReplayActionFailure> {
    matches!(
        checked_request(
            controller,
            coordinator,
            instance,
            execution,
            actor,
            intent,
            command,
        )
        .await?,
        BrowserResponse::Acknowledged
    )
    .then_some(())
    .ok_or(ReplayActionFailure::StepFailed)
}

#[allow(clippy::too_many_arguments)]
async fn execute_step_wait(
    controller: &BrowserController,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    execution: &BrowserReplayExecutionHandle,
    actor: BrowserInvocationActor,
    plan: &BrowserReplayPlan,
    tabs: &ReplayTabState,
    wait: &BrowserRecipeWait,
) -> Result<(), ReplayActionFailure> {
    let (condition, timeout_ms) =
        compile_wait(plan, wait).ok_or(ReplayActionFailure::StepFailed)?;
    let response = checked_request(
        controller,
        coordinator,
        instance,
        execution,
        actor,
        "replay step wait",
        BrowserCommand::Wait {
            tab_id: tabs.current.clone(),
            condition,
            timeout_ms,
        },
    )
    .await?;
    match response {
        BrowserResponse::Wait { result } if result.matched => Ok(()),
        _ => Err(ReplayActionFailure::StepFailed),
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_assertion(
    controller: &BrowserController,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    execution: &BrowserReplayExecutionHandle,
    actor: BrowserInvocationActor,
    plan: &BrowserReplayPlan,
    tabs: &ReplayTabState,
    assertion: &BrowserRecipeAssertion,
) -> Result<(), ReplayActionFailure> {
    let condition = match assertion {
        BrowserRecipeAssertion::Url { value, exact } => BrowserWaitCondition::Url {
            value: resolve_value(plan, value)
                .map(str::to_string)
                .ok_or(ReplayActionFailure::StepFailed)?,
            exact: *exact,
        },
        BrowserRecipeAssertion::Title { value, exact } => BrowserWaitCondition::Title {
            value: resolve_value(plan, value)
                .map(str::to_string)
                .ok_or(ReplayActionFailure::StepFailed)?,
            exact: *exact,
        },
        BrowserRecipeAssertion::Text { value, present } => {
            let text = resolve_value(plan, value)
                .map(str::to_string)
                .ok_or(ReplayActionFailure::StepFailed)?;
            if *present {
                BrowserWaitCondition::TextPresent { text }
            } else {
                BrowserWaitCondition::TextAbsent { text }
            }
        }
        BrowserRecipeAssertion::Element { locator, state } => match state {
            BrowserRecipeElementState::Present => BrowserWaitCondition::ElementPresent {
                target: action_target(locator),
            },
            BrowserRecipeElementState::Absent => BrowserWaitCondition::ElementAbsent {
                target: action_target(locator),
            },
            BrowserRecipeElementState::Visible => BrowserWaitCondition::ElementVisible {
                target: action_target(locator),
            },
            BrowserRecipeElementState::Hidden => BrowserWaitCondition::ElementHidden {
                target: action_target(locator),
            },
        },
        BrowserRecipeAssertion::Value { locator, value } => BrowserWaitCondition::ElementValue {
            target: action_target(locator),
            value: resolve_value(plan, value)
                .map(str::to_string)
                .ok_or(ReplayActionFailure::StepFailed)?,
        },
    };
    let response = match checked_request(
        controller,
        coordinator,
        instance,
        execution,
        actor,
        "replay step assertion",
        BrowserCommand::Wait {
            tab_id: tabs.current.clone(),
            condition,
            timeout_ms: ASSERTION_TIMEOUT_MS,
        },
    )
    .await
    {
        Err(ReplayActionFailure::PageConditionTimeout) => {
            return Err(ReplayActionFailure::AssertionFailed)
        }
        result => result?,
    };
    match response {
        BrowserResponse::Wait { result } if result.matched => Ok(()),
        BrowserResponse::Wait { .. } => Err(ReplayActionFailure::AssertionFailed),
        _ => Err(ReplayActionFailure::StepFailed),
    }
}

async fn semantic_action(
    controller: &BrowserController,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    execution: &BrowserReplayExecutionHandle,
    actor: BrowserInvocationActor,
    tabs: &ReplayTabState,
    action: BrowserAction,
) -> Result<(), ReplayActionFailure> {
    let response = checked_request(
        controller,
        coordinator,
        instance,
        execution,
        actor,
        "replay step semantic action",
        BrowserCommand::Act {
            tab_id: tabs.current.clone(),
            actions: vec![action],
        },
    )
    .await?;
    match response {
        BrowserResponse::Action { result } if result.completed_actions == 1 => Ok(()),
        _ => Err(ReplayActionFailure::StepFailed),
    }
}

#[allow(clippy::too_many_arguments)]
async fn secret_semantic_action(
    controller: &BrowserController,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    execution: &BrowserReplayExecutionHandle,
    actor: BrowserInvocationActor,
    tabs: &ReplayTabState,
    locator: &BrowserRecipeLocator,
    input_name: &str,
) -> Result<(), ReplayActionFailure> {
    if let Some(projection) = terminal_projection(execution, coordinator, instance)? {
        return Err(ReplayActionFailure::Terminal(projection));
    }
    let lease = execution
        .secret_lease(input_name)
        .map_err(|_| ReplayActionFailure::StepFailed)?;
    let context = BrowserInvocationContext::for_actor(
        actor,
        "replay step secret type",
        BrowserRisk::AccountSecurity,
    )
    .map_err(|_| ReplayActionFailure::StepFailed)?;
    let response = controller
        .request_replay_secret_type(
            BrowserCommand::SecretType {
                tab_id: tabs.current.clone(),
                target: action_target(locator),
                input_name: input_name.to_string(),
            },
            context,
            instance.clone(),
            lease,
        )
        .await;
    if let Some(projection) = terminal_projection(execution, coordinator, instance)? {
        return Err(ReplayActionFailure::Terminal(projection));
    }
    let response = match response {
        Ok(response) => response,
        Err(BrowserError::Interrupted) => {
            return match coordinator.cancel(instance) {
                Ok(projection) => Err(ReplayActionFailure::Terminal(projection)),
                Err(error) => {
                    match retained_terminal_after_transition_error(coordinator, instance, error) {
                        Ok(projection) => Err(ReplayActionFailure::Terminal(projection)),
                        Err(error) => Err(ReplayActionFailure::Replay(error)),
                    }
                }
            };
        }
        Err(_) => return Err(ReplayActionFailure::StepFailed),
    };
    match response {
        BrowserResponse::Action { result } if result.completed_actions == 1 => Ok(()),
        _ => Err(ReplayActionFailure::StepFailed),
    }
}

async fn checked_request(
    controller: &BrowserController,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    execution: &BrowserReplayExecutionHandle,
    actor: BrowserInvocationActor,
    intent: &'static str,
    command: BrowserCommand,
) -> Result<BrowserResponse, ReplayActionFailure> {
    checked_request_with_options(
        controller,
        coordinator,
        instance,
        execution,
        actor,
        intent,
        BrowserRisk::Normal,
        None,
        command,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn checked_request_with_options(
    controller: &BrowserController,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    execution: &BrowserReplayExecutionHandle,
    actor: BrowserInvocationActor,
    intent: &'static str,
    risk: BrowserRisk,
    local_project_root: Option<&Path>,
    command: BrowserCommand,
) -> Result<BrowserResponse, ReplayActionFailure> {
    if let Some(projection) = terminal_projection(execution, coordinator, instance)? {
        return Err(ReplayActionFailure::Terminal(projection));
    }
    let response =
        request_with_options(controller, actor, intent, risk, local_project_root, command).await;
    if let Some(projection) = terminal_projection(execution, coordinator, instance)? {
        return Err(ReplayActionFailure::Terminal(projection));
    }
    match response {
        Ok(response) => Ok(response),
        Err(BrowserError::Interrupted) => match coordinator.cancel(instance) {
            Ok(projection) => Err(ReplayActionFailure::Terminal(projection)),
            Err(error) => {
                match retained_terminal_after_transition_error(coordinator, instance, error) {
                    Ok(projection) => Err(ReplayActionFailure::Terminal(projection)),
                    Err(error) => Err(ReplayActionFailure::Replay(error)),
                }
            }
        },
        Err(BrowserError::Timeout { operation }) if operation == "pageCondition" => {
            Err(ReplayActionFailure::PageConditionTimeout)
        }
        Err(_) => Err(ReplayActionFailure::StepFailed),
    }
}

fn terminal_projection(
    execution: &BrowserReplayExecutionHandle,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
) -> Result<Option<BrowserReplayProjection>, ReplayActionFailure> {
    if execution.is_cancelled() {
        return coordinator
            .status(instance)
            .map(Some)
            .map_err(ReplayActionFailure::Replay);
    }
    let projection = coordinator
        .status(instance)
        .map_err(ReplayActionFailure::Replay)?;
    if projection.status != BrowserReplayStatus::Running {
        return Ok(Some(projection));
    }
    if execution.is_cancelled() {
        return coordinator
            .status(instance)
            .map(Some)
            .map_err(ReplayActionFailure::Replay);
    }
    Ok(None)
}

fn resolve_value<'a>(
    plan: &'a BrowserReplayPlan,
    value: &'a BrowserRecipeValue,
) -> Option<&'a str> {
    plan.resolve_value(value)
}

fn action_target(locator: &BrowserRecipeLocator) -> BrowserActionTarget {
    BrowserActionTarget {
        element_ref: None,
        locator: BrowserLocator::from(locator.clone()),
        coordinates: None,
    }
}

fn compile_wait(
    plan: &BrowserReplayPlan,
    wait: &BrowserRecipeWait,
) -> Option<(BrowserWaitCondition, u64)> {
    match wait {
        BrowserRecipeWait::Duration { duration_ms } => Some((
            BrowserWaitCondition::Duration {
                duration_ms: *duration_ms,
            },
            *duration_ms,
        )),
        BrowserRecipeWait::Url {
            value,
            exact,
            timeout_ms,
        } => Some((
            BrowserWaitCondition::Url {
                value: resolve_value(plan, value)?.to_string(),
                exact: *exact,
            },
            *timeout_ms,
        )),
        BrowserRecipeWait::Load { timeout_ms } => Some((BrowserWaitCondition::Load, *timeout_ms)),
        BrowserRecipeWait::NetworkIdle { timeout_ms } => {
            Some((BrowserWaitCondition::NetworkIdle, *timeout_ms))
        }
        BrowserRecipeWait::ElementPresent {
            locator,
            timeout_ms,
        } => Some((
            BrowserWaitCondition::ElementPresent {
                target: action_target(locator),
            },
            *timeout_ms,
        )),
        BrowserRecipeWait::ElementVisible {
            locator,
            timeout_ms,
        } => Some((
            BrowserWaitCondition::ElementVisible {
                target: action_target(locator),
            },
            *timeout_ms,
        )),
        BrowserRecipeWait::ElementHidden {
            locator,
            timeout_ms,
        } => Some((
            BrowserWaitCondition::ElementHidden {
                target: action_target(locator),
            },
            *timeout_ms,
        )),
        BrowserRecipeWait::TextPresent { value, timeout_ms } => Some((
            BrowserWaitCondition::TextPresent {
                text: resolve_value(plan, value)?.to_string(),
            },
            *timeout_ms,
        )),
        BrowserRecipeWait::TextAbsent { value, timeout_ms } => Some((
            BrowserWaitCondition::TextAbsent {
                text: resolve_value(plan, value)?.to_string(),
            },
            *timeout_ms,
        )),
    }
}

async fn request_with_options(
    controller: &BrowserController,
    actor: BrowserInvocationActor,
    intent: &'static str,
    risk: BrowserRisk,
    local_project_root: Option<&Path>,
    command: BrowserCommand,
) -> Result<BrowserResponse, BrowserError> {
    let context = BrowserInvocationContext::for_actor(actor, intent, risk)?;
    match local_project_root {
        Some(root) => {
            controller
                .request_with_local_project_root(command, context, root)
                .await
        }
        None => controller.request_with_context(command, context).await,
    }
}

fn exact_selected_tab(snapshot: &BrowserWorkspaceSnapshot) -> Option<&str> {
    let selected = snapshot.selected_tab_id.as_deref()?;
    if selected.trim().is_empty()
        || snapshot
            .tabs
            .iter()
            .filter(|tab| tab.id == selected)
            .count()
            != 1
    {
        return None;
    }
    Some(selected)
}

fn snapshot_proves_selected_tab(
    snapshot: &BrowserWorkspaceSnapshot,
    expected_tab_id: &str,
    expected_viewport: Option<&BrowserViewport>,
    expected_url: Option<&str>,
) -> bool {
    if exact_selected_tab(snapshot) != Some(expected_tab_id) {
        return false;
    }
    let Some(tab) = exact_tab(snapshot, expected_tab_id) else {
        return false;
    };
    expected_viewport.is_none_or(|viewport| &tab.viewport == viewport)
        && expected_url.is_none_or(|url| tab.url == url)
}

fn exact_tab<'a>(
    snapshot: &'a BrowserWorkspaceSnapshot,
    expected_tab_id: &str,
) -> Option<&'a BrowserTabSnapshot> {
    let mut matches = snapshot.tabs.iter().filter(|tab| tab.id == expected_tab_id);
    let tab = matches.next()?;
    matches.next().is_none().then_some(tab)
}

fn validated_runtime_tabs(snapshot: &BrowserWorkspaceSnapshot) -> Option<HashSet<String>> {
    if snapshot.tabs.is_empty() || snapshot.tabs.len() > MAX_REPLAY_RUNTIME_TABS {
        return None;
    }
    let mut runtime_tabs = HashSet::with_capacity(snapshot.tabs.len());
    for tab in &snapshot.tabs {
        if tab.id.trim().is_empty() || !runtime_tabs.insert(tab.id.clone()) {
            return None;
        }
    }
    Some(runtime_tabs)
}

fn fail_step(
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
) -> Result<BrowserReplayProjection, BrowserReplayError> {
    match coordinator.fail(instance, BrowserReplayFailureCode::StepFailed) {
        Ok(projection) => Ok(projection),
        Err(error) => retained_terminal_after_transition_error(coordinator, instance, error),
    }
}

fn fail_assertion(
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
) -> Result<BrowserReplayProjection, BrowserReplayError> {
    match coordinator.fail(instance, BrowserReplayFailureCode::AssertionFailed) {
        Ok(projection) => Ok(projection),
        Err(error) => retained_terminal_after_transition_error(coordinator, instance, error),
    }
}

fn finish_failure(
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    failure: ReplayActionFailure,
) -> Result<BrowserReplayProjection, BrowserReplayError> {
    match failure {
        ReplayActionFailure::StepFailed => fail_step(coordinator, instance),
        ReplayActionFailure::AssertionFailed => fail_assertion(coordinator, instance),
        ReplayActionFailure::PageConditionTimeout => fail_step(coordinator, instance),
        ReplayActionFailure::Terminal(projection) => Ok(projection),
        ReplayActionFailure::Replay(error) => Err(error),
    }
}

fn retained_terminal_after_transition_error(
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    transition_error: BrowserReplayError,
) -> Result<BrowserReplayProjection, BrowserReplayError> {
    match coordinator.status(instance) {
        Ok(projection)
            if matches!(
                projection.status,
                BrowserReplayStatus::Completed
                    | BrowserReplayStatus::Failed
                    | BrowserReplayStatus::Cancelled
            ) =>
        {
            Ok(projection)
        }
        _ => Err(transition_error),
    }
}

fn cancelled_projection(
    execution: &BrowserReplayExecutionHandle,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
) -> Result<Option<BrowserReplayProjection>, BrowserReplayError> {
    if !execution.is_cancelled() {
        return Ok(None);
    }
    coordinator.status(instance).map(Some)
}
