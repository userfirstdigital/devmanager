use super::commands::verified_authenticated_local_project_root;
use super::{
    browser_cdp_method_risk, classify_upload_path, BrowserAction, BrowserActionTarget,
    BrowserCommand, BrowserController, BrowserError, BrowserInvocationActor,
    BrowserInvocationContext, BrowserLocator, BrowserLocatorFailureTarget, BrowserRecipeAction,
    BrowserRecipeAssertion, BrowserRecipeElementState, BrowserRecipeInputKind,
    BrowserRecipeLocator, BrowserRecipeValue, BrowserRecipeWait, BrowserReplayCoordinator,
    BrowserReplayError, BrowserReplayExecutionHandle, BrowserReplayFailureCode,
    BrowserReplayInstance, BrowserReplayLocatorSlot, BrowserReplayPlan, BrowserReplayProjection,
    BrowserReplayRepairInstance, BrowserReplayRepairResumeCursor, BrowserReplayStatus,
    BrowserResourceHandle, BrowserResourceKind, BrowserResourceStore, BrowserResponse, BrowserRisk,
    BrowserScreenshotMode, BrowserTabSnapshot, BrowserViewport, BrowserWaitCondition,
    BrowserWorkspaceSnapshot,
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
    repair_resource_store: &BrowserResourceStore,
    authenticated_local_project_root: &Path,
) -> Result<BrowserReplayProjection, BrowserReplayError> {
    let _secret_store_close = BrowserReplaySecretStoreCloseGuard(&execution);
    let mut repair_watch = execution.repair_watch();
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
    execution.bind_canonical_recipe_root(&local_project_root)?;
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
        let mut cursor = if matches!(step.action, BrowserRecipeAction::Wait { .. }) {
            BrowserReplayRepairResumeCursor::ActionWait
        } else {
            BrowserReplayRepairResumeCursor::Action
        };
        loop {
            let phase_result = match cursor {
                BrowserReplayRepairResumeCursor::Action
                | BrowserReplayRepairResumeCursor::ActionWait => execute_action(
                    controller,
                    coordinator,
                    instance,
                    &execution,
                    actor,
                    plan,
                    &local_project_root,
                    &mut tabs,
                    step_index,
                    &step.action,
                )
                .await
                .map(|()| BrowserReplayRepairResumeCursor::StepWait),
                BrowserReplayRepairResumeCursor::StepWait => match &step.wait {
                    Some(wait) => execute_step_wait(
                        controller,
                        coordinator,
                        instance,
                        &execution,
                        actor,
                        plan,
                        &tabs,
                        step_index,
                        wait,
                    )
                    .await
                    .map(|()| BrowserReplayRepairResumeCursor::Assertion(0)),
                    None => Ok(BrowserReplayRepairResumeCursor::Assertion(0)),
                },
                BrowserReplayRepairResumeCursor::Assertion(index) => {
                    match step.assertions.get(index) {
                        Some(assertion) => execute_assertion(
                            controller,
                            coordinator,
                            instance,
                            &execution,
                            actor,
                            plan,
                            &tabs,
                            step_index,
                            index,
                            assertion,
                        )
                        .await
                        .map(|()| BrowserReplayRepairResumeCursor::Assertion(index + 1)),
                        None => break,
                    }
                }
            };
            match phase_result {
                Ok(next) => cursor = next,
                Err(ReplayActionFailure::Repairable(target)) => {
                    cursor = match capture_and_wait_for_locator_repair(
                        controller,
                        coordinator,
                        instance,
                        &execution,
                        actor,
                        repair_resource_store,
                        &mut repair_watch,
                        &tabs,
                        step_index,
                        target,
                    )
                    .await
                    {
                        Ok(cursor) => cursor,
                        Err(failure) => return finish_failure(coordinator, instance, failure),
                    };
                }
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
    LocatorNotFound(BrowserLocatorFailureTarget),
    Repairable(ReplayRepairTarget),
    Terminal(BrowserReplayProjection),
    Replay(BrowserReplayError),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ReplayRepairTarget {
    locator_slot: BrowserReplayLocatorSlot,
    resume_cursor: BrowserReplayRepairResumeCursor,
}

#[derive(Clone, Copy)]
enum ReplayLocatorFailureContext {
    Primary(ReplayRepairTarget),
    Drag {
        source: ReplayRepairTarget,
        destination: ReplayRepairTarget,
    },
}

impl ReplayLocatorFailureContext {
    fn resolve(self, target: BrowserLocatorFailureTarget) -> Option<ReplayRepairTarget> {
        match (self, target) {
            (Self::Primary(repair), BrowserLocatorFailureTarget::Primary) => Some(repair),
            (Self::Drag { source, .. }, BrowserLocatorFailureTarget::Source) => Some(source),
            (Self::Drag { destination, .. }, BrowserLocatorFailureTarget::Destination) => {
                Some(destination)
            }
            _ => None,
        }
    }
}

fn primary_locator_failure_context(
    locator_slot: BrowserReplayLocatorSlot,
    resume_cursor: BrowserReplayRepairResumeCursor,
) -> ReplayLocatorFailureContext {
    ReplayLocatorFailureContext::Primary(ReplayRepairTarget {
        locator_slot,
        resume_cursor,
    })
}

fn action_locator_failure_context(
    action: &BrowserRecipeAction,
) -> Option<ReplayLocatorFailureContext> {
    match action {
        BrowserRecipeAction::Click { .. }
        | BrowserRecipeAction::Hover { .. }
        | BrowserRecipeAction::Focus { .. }
        | BrowserRecipeAction::Type { .. }
        | BrowserRecipeAction::Clear { .. }
        | BrowserRecipeAction::Select { .. }
        | BrowserRecipeAction::Upload { .. }
        | BrowserRecipeAction::Download { .. } => Some(primary_locator_failure_context(
            BrowserReplayLocatorSlot::PrimaryAction,
            BrowserReplayRepairResumeCursor::Action,
        )),
        BrowserRecipeAction::Keypress {
            locator: Some(_), ..
        }
        | BrowserRecipeAction::Scroll {
            locator: Some(_), ..
        } => Some(primary_locator_failure_context(
            BrowserReplayLocatorSlot::OptionalAction,
            BrowserReplayRepairResumeCursor::Action,
        )),
        BrowserRecipeAction::DragDrop { .. } => Some(ReplayLocatorFailureContext::Drag {
            source: ReplayRepairTarget {
                locator_slot: BrowserReplayLocatorSlot::DragSource,
                resume_cursor: BrowserReplayRepairResumeCursor::Action,
            },
            destination: ReplayRepairTarget {
                locator_slot: BrowserReplayLocatorSlot::DragDestination,
                resume_cursor: BrowserReplayRepairResumeCursor::Action,
            },
        }),
        BrowserRecipeAction::Wait { condition } => wait_locator_failure_context(
            condition,
            BrowserReplayLocatorSlot::ActionWait,
            BrowserReplayRepairResumeCursor::ActionWait,
        ),
        _ => None,
    }
}

fn wait_locator_failure_context(
    wait: &BrowserRecipeWait,
    locator_slot: BrowserReplayLocatorSlot,
    resume_cursor: BrowserReplayRepairResumeCursor,
) -> Option<ReplayLocatorFailureContext> {
    matches!(
        wait,
        BrowserRecipeWait::ElementPresent { .. } | BrowserRecipeWait::ElementVisible { .. }
    )
    .then(|| primary_locator_failure_context(locator_slot, resume_cursor))
}

fn assertion_locator_failure_context(
    assertion: &BrowserRecipeAssertion,
    index: usize,
) -> Option<ReplayLocatorFailureContext> {
    matches!(
        assertion,
        BrowserRecipeAssertion::Element {
            state: BrowserRecipeElementState::Present | BrowserRecipeElementState::Visible,
            ..
        } | BrowserRecipeAssertion::Value { .. }
    )
    .then(|| {
        primary_locator_failure_context(
            BrowserReplayLocatorSlot::Assertion { index },
            BrowserReplayRepairResumeCursor::Assertion(index),
        )
    })
}

fn contextualize_locator_failure(
    locator_failure_context: Option<ReplayLocatorFailureContext>,
    target: BrowserLocatorFailureTarget,
) -> ReplayActionFailure {
    locator_failure_context
        .and_then(|context| context.resolve(target))
        .map(ReplayActionFailure::Repairable)
        .unwrap_or(ReplayActionFailure::StepFailed)
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
    step_index: usize,
    action: &BrowserRecipeAction,
) -> Result<(), ReplayActionFailure> {
    let locator_failure_context = action_locator_failure_context(action);
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
                locator_failure_context,
                BrowserAction::Click {
                    target: effective_action_target(
                        execution,
                        step_index,
                        BrowserReplayLocatorSlot::PrimaryAction,
                        locator,
                    ),
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
                locator_failure_context,
                BrowserAction::Hover {
                    target: effective_action_target(
                        execution,
                        step_index,
                        BrowserReplayLocatorSlot::PrimaryAction,
                        locator,
                    ),
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
                locator_failure_context,
                BrowserAction::Focus {
                    target: effective_action_target(
                        execution,
                        step_index,
                        BrowserReplayLocatorSlot::PrimaryAction,
                        locator,
                    ),
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
                        step_index,
                        locator,
                        name,
                        locator_failure_context,
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
                locator_failure_context,
                BrowserAction::Type {
                    target: effective_action_target(
                        execution,
                        step_index,
                        BrowserReplayLocatorSlot::PrimaryAction,
                        locator,
                    ),
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
                locator_failure_context,
                BrowserAction::Clear {
                    target: effective_action_target(
                        execution,
                        step_index,
                        BrowserReplayLocatorSlot::PrimaryAction,
                        locator,
                    ),
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
                locator_failure_context,
                BrowserAction::Select {
                    target: effective_action_target(
                        execution,
                        step_index,
                        BrowserReplayLocatorSlot::PrimaryAction,
                        locator,
                    ),
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
                locator_failure_context,
                BrowserAction::Keypress {
                    target: locator.as_ref().map(|locator| {
                        effective_action_target(
                            execution,
                            step_index,
                            BrowserReplayLocatorSlot::OptionalAction,
                            locator,
                        )
                    }),
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
                locator_failure_context,
                BrowserAction::Scroll {
                    target: locator.as_ref().map(|locator| {
                        effective_action_target(
                            execution,
                            step_index,
                            BrowserReplayLocatorSlot::OptionalAction,
                            locator,
                        )
                    }),
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
                locator_failure_context,
                BrowserAction::DragDrop {
                    source: effective_action_target(
                        execution,
                        step_index,
                        BrowserReplayLocatorSlot::DragSource,
                        source,
                    ),
                    destination: effective_action_target(
                        execution,
                        step_index,
                        BrowserReplayLocatorSlot::DragDestination,
                        destination,
                    ),
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
            let response = checked_locator_request_with_options(
                controller,
                coordinator,
                instance,
                execution,
                actor,
                "replay step upload file",
                risk,
                Some(local_project_root),
                locator_failure_context,
                BrowserCommand::Upload {
                    tab_id: tabs.current.clone(),
                    target: effective_action_target(
                        execution,
                        step_index,
                        BrowserReplayLocatorSlot::PrimaryAction,
                        locator,
                    ),
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
                locator_failure_context,
                BrowserAction::Click {
                    target: effective_action_target(
                        execution,
                        step_index,
                        BrowserReplayLocatorSlot::PrimaryAction,
                        locator,
                    ),
                },
            )
            .await
        }
        BrowserRecipeAction::Wait { condition } => {
            let locator_override =
                execution.locator_override(step_index, BrowserReplayLocatorSlot::ActionWait);
            let (condition, timeout_ms) = compile_wait(plan, condition, locator_override.as_ref())
                .ok_or(ReplayActionFailure::StepFailed)?;
            let response = repair_positive_timeout(
                checked_locator_request(
                    controller,
                    coordinator,
                    instance,
                    execution,
                    actor,
                    "replay step wait",
                    locator_failure_context,
                    BrowserCommand::Wait {
                        tab_id: tabs.current.clone(),
                        condition,
                        timeout_ms,
                    },
                )
                .await,
                locator_failure_context,
            )?;
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

struct BrowserReplayExecutorRepairGuard {
    coordinator: BrowserReplayCoordinator,
    repair: BrowserReplayRepairInstance,
    armed: bool,
}

impl BrowserReplayExecutorRepairGuard {
    fn new(coordinator: &BrowserReplayCoordinator, repair: &BrowserReplayRepairInstance) -> Self {
        Self {
            coordinator: coordinator.clone(),
            repair: repair.clone(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BrowserReplayExecutorRepairGuard {
    fn drop(&mut self) {
        if self.armed {
            self.coordinator.abort_locator_repair_capture(&self.repair);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn capture_and_wait_for_locator_repair(
    controller: &BrowserController,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    execution: &BrowserReplayExecutionHandle,
    actor: BrowserInvocationActor,
    resource_store: &BrowserResourceStore,
    repair_watch: &mut tokio::sync::watch::Receiver<u64>,
    tabs: &ReplayTabState,
    step_index: usize,
    target: ReplayRepairTarget,
) -> Result<BrowserReplayRepairResumeCursor, ReplayActionFailure> {
    let response = checked_request(
        controller,
        coordinator,
        instance,
        execution,
        actor,
        "replay repair inspect fresh workspace",
        BrowserCommand::WorkspaceState,
    )
    .await?;
    let BrowserResponse::WorkspaceState { snapshot } = response else {
        return Err(ReplayActionFailure::StepFailed);
    };
    if exact_selected_tab(&snapshot) != Some(tabs.current.as_str())
        || exact_tab(&snapshot, &tabs.current).is_none()
    {
        return Err(ReplayActionFailure::StepFailed);
    }
    let revision = snapshot.revision;
    let repair = match coordinator.reserve_locator_repair_capture(
        instance,
        resource_store,
        step_index,
        target.locator_slot,
        tabs.current.clone(),
        revision,
        target.resume_cursor,
    ) {
        Ok(repair) => repair,
        Err(_) => return terminal_or_step_failure(execution, coordinator, instance),
    };
    let mut repair_guard = BrowserReplayExecutorRepairGuard::new(coordinator, &repair);

    let snapshot_response = checked_repair_capture_request(
        controller,
        coordinator,
        instance,
        execution,
        actor,
        &repair,
        "replay repair capture semantic snapshot",
        BrowserCommand::Snapshot {
            tab_id: tabs.current.clone(),
        },
    )
    .await?;
    let BrowserResponse::Snapshot {
        summary,
        resource: snapshot_handle,
    } = snapshot_response
    else {
        return Err(ReplayActionFailure::StepFailed);
    };
    if summary.tab_id != tabs.current
        || summary.revision != revision
        || !exact_repair_resource(
            resource_store,
            instance,
            &snapshot_handle,
            BrowserResourceKind::ReplayRepairSnapshot,
            "application/json",
        )
    {
        return Err(ReplayActionFailure::StepFailed);
    }

    let screenshot_response = checked_repair_capture_request(
        controller,
        coordinator,
        instance,
        execution,
        actor,
        &repair,
        "replay repair capture viewport screenshot",
        BrowserCommand::Screenshot {
            tab_id: tabs.current.clone(),
            mode: BrowserScreenshotMode::Viewport,
        },
    )
    .await?;
    let BrowserResponse::Screenshot {
        resource: screenshot_handle,
    } = screenshot_response
    else {
        return Err(ReplayActionFailure::StepFailed);
    };
    if !exact_repair_resource(
        resource_store,
        instance,
        &screenshot_handle,
        BrowserResourceKind::ReplayRepairScreenshot,
        "image/png",
    ) {
        return Err(ReplayActionFailure::StepFailed);
    }

    if coordinator
        .publish_locator_repair(&repair, &snapshot_handle, &screenshot_handle)
        .is_err()
    {
        return terminal_or_step_failure(execution, coordinator, instance);
    }
    repair_guard.disarm();
    wait_for_locator_repair_resolution(
        coordinator,
        instance,
        execution,
        repair_watch,
        target.resume_cursor,
    )
    .await
}

fn exact_repair_resource(
    resource_store: &BrowserResourceStore,
    instance: &BrowserReplayInstance,
    handle: &BrowserResourceHandle,
    kind: BrowserResourceKind,
    mime_type: &str,
) -> bool {
    handle.kind == kind
        && handle.mime_type == mime_type
        && handle.pinned
        && resource_store
            .handle(instance.workspace_key(), &handle.id)
            .is_ok_and(|exact| exact == *handle)
}

#[allow(clippy::too_many_arguments)]
async fn checked_repair_capture_request(
    controller: &BrowserController,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    execution: &BrowserReplayExecutionHandle,
    actor: BrowserInvocationActor,
    repair: &BrowserReplayRepairInstance,
    intent: &'static str,
    command: BrowserCommand,
) -> Result<BrowserResponse, ReplayActionFailure> {
    if let Some(projection) = terminal_projection(execution, coordinator, instance)? {
        return Err(ReplayActionFailure::Terminal(projection));
    }
    let context = BrowserInvocationContext::for_actor(actor, intent, BrowserRisk::Normal)
        .map_err(|_| ReplayActionFailure::StepFailed)?;
    let response = controller
        .request_replay_repair_capture(coordinator, repair, command, context)
        .await;
    if let Some(projection) = terminal_projection(execution, coordinator, instance)? {
        return Err(ReplayActionFailure::Terminal(projection));
    }
    match response {
        Ok(response) => Ok(response),
        Err(BrowserError::Interrupted) => cancel_after_interrupted_request(coordinator, instance),
        Err(_) => Err(ReplayActionFailure::StepFailed),
    }
}

fn cancel_after_interrupted_request<T>(
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
) -> Result<T, ReplayActionFailure> {
    match coordinator.cancel(instance) {
        Ok(projection) => Err(ReplayActionFailure::Terminal(projection)),
        Err(error) => {
            match retained_terminal_after_transition_error(coordinator, instance, error) {
                Ok(projection) => Err(ReplayActionFailure::Terminal(projection)),
                Err(error) => Err(ReplayActionFailure::Replay(error)),
            }
        }
    }
}

fn terminal_or_step_failure<T>(
    execution: &BrowserReplayExecutionHandle,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
) -> Result<T, ReplayActionFailure> {
    match terminal_projection(execution, coordinator, instance)? {
        Some(projection) => Err(ReplayActionFailure::Terminal(projection)),
        None => Err(ReplayActionFailure::StepFailed),
    }
}

async fn wait_for_locator_repair_resolution(
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    execution: &BrowserReplayExecutionHandle,
    repair_watch: &mut tokio::sync::watch::Receiver<u64>,
    resume_cursor: BrowserReplayRepairResumeCursor,
) -> Result<BrowserReplayRepairResumeCursor, ReplayActionFailure> {
    loop {
        let projection = coordinator
            .status(instance)
            .map_err(ReplayActionFailure::Replay)?;
        match projection.status {
            BrowserReplayStatus::Running => return Ok(resume_cursor),
            BrowserReplayStatus::PausedLocatorRepair => {}
            BrowserReplayStatus::Completed
            | BrowserReplayStatus::Failed
            | BrowserReplayStatus::Cancelled => {
                return Err(ReplayActionFailure::Terminal(projection));
            }
            _ => {
                return Err(ReplayActionFailure::Replay(
                    BrowserReplayError::InvalidTransition,
                ))
            }
        }
        if execution.is_cancelled() {
            let projection = coordinator
                .status(instance)
                .map_err(ReplayActionFailure::Replay)?;
            if matches!(
                projection.status,
                BrowserReplayStatus::Completed
                    | BrowserReplayStatus::Failed
                    | BrowserReplayStatus::Cancelled
            ) {
                return Err(ReplayActionFailure::Terminal(projection));
            }
        }
        if repair_watch.changed().await.is_err() {
            let projection = coordinator
                .status(instance)
                .map_err(ReplayActionFailure::Replay)?;
            if matches!(
                projection.status,
                BrowserReplayStatus::Completed
                    | BrowserReplayStatus::Failed
                    | BrowserReplayStatus::Cancelled
            ) {
                return Err(ReplayActionFailure::Terminal(projection));
            }
            return Err(ReplayActionFailure::Replay(
                BrowserReplayError::InvalidTransition,
            ));
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
    step_index: usize,
    wait: &BrowserRecipeWait,
) -> Result<(), ReplayActionFailure> {
    let locator_failure_context = wait_locator_failure_context(
        wait,
        BrowserReplayLocatorSlot::StepWait,
        BrowserReplayRepairResumeCursor::StepWait,
    );
    let locator_override =
        execution.locator_override(step_index, BrowserReplayLocatorSlot::StepWait);
    let (condition, timeout_ms) = compile_wait(plan, wait, locator_override.as_ref())
        .ok_or(ReplayActionFailure::StepFailed)?;
    let response = repair_positive_timeout(
        checked_locator_request(
            controller,
            coordinator,
            instance,
            execution,
            actor,
            "replay step wait",
            locator_failure_context,
            BrowserCommand::Wait {
                tab_id: tabs.current.clone(),
                condition,
                timeout_ms,
            },
        )
        .await,
        locator_failure_context,
    )?;
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
    step_index: usize,
    assertion_index: usize,
    assertion: &BrowserRecipeAssertion,
) -> Result<(), ReplayActionFailure> {
    let locator_slot = BrowserReplayLocatorSlot::Assertion {
        index: assertion_index,
    };
    let locator_override = execution.locator_override(step_index, locator_slot);
    let locator_failure_context = assertion_locator_failure_context(assertion, assertion_index);
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
                target: action_target(locator_override.as_ref().unwrap_or(locator)),
            },
            BrowserRecipeElementState::Absent => BrowserWaitCondition::ElementAbsent {
                target: action_target(locator),
            },
            BrowserRecipeElementState::Visible => BrowserWaitCondition::ElementVisible {
                target: action_target(locator_override.as_ref().unwrap_or(locator)),
            },
            BrowserRecipeElementState::Hidden => BrowserWaitCondition::ElementHidden {
                target: action_target(locator),
            },
        },
        BrowserRecipeAssertion::Value { locator, value } => BrowserWaitCondition::ElementValue {
            target: action_target(locator_override.as_ref().unwrap_or(locator)),
            value: resolve_value(plan, value)
                .map(str::to_string)
                .ok_or(ReplayActionFailure::StepFailed)?,
        },
    };
    let response = match checked_locator_request(
        controller,
        coordinator,
        instance,
        execution,
        actor,
        "replay step assertion",
        locator_failure_context,
        BrowserCommand::Wait {
            tab_id: tabs.current.clone(),
            condition,
            timeout_ms: ASSERTION_TIMEOUT_MS,
        },
    )
    .await
    {
        Err(ReplayActionFailure::PageConditionTimeout) => match locator_failure_context
            .and_then(|context| context.resolve(BrowserLocatorFailureTarget::Primary))
        {
            Some(target) => return Err(ReplayActionFailure::Repairable(target)),
            None => return Err(ReplayActionFailure::AssertionFailed),
        },
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
    locator_failure_context: Option<ReplayLocatorFailureContext>,
    action: BrowserAction,
) -> Result<(), ReplayActionFailure> {
    let response = checked_locator_request(
        controller,
        coordinator,
        instance,
        execution,
        actor,
        "replay step semantic action",
        locator_failure_context,
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
    step_index: usize,
    locator: &BrowserRecipeLocator,
    input_name: &str,
    locator_failure_context: Option<ReplayLocatorFailureContext>,
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
                target: effective_action_target(
                    execution,
                    step_index,
                    BrowserReplayLocatorSlot::PrimaryAction,
                    locator,
                ),
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
        Err(BrowserError::LocatorNotFound { target }) => {
            return Err(contextualize_locator_failure(
                locator_failure_context,
                target,
            ));
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

async fn checked_locator_request(
    controller: &BrowserController,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    execution: &BrowserReplayExecutionHandle,
    actor: BrowserInvocationActor,
    intent: &'static str,
    locator_failure_context: Option<ReplayLocatorFailureContext>,
    command: BrowserCommand,
) -> Result<BrowserResponse, ReplayActionFailure> {
    checked_locator_request_with_options(
        controller,
        coordinator,
        instance,
        execution,
        actor,
        intent,
        BrowserRisk::Normal,
        None,
        locator_failure_context,
        command,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn checked_locator_request_with_options(
    controller: &BrowserController,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    execution: &BrowserReplayExecutionHandle,
    actor: BrowserInvocationActor,
    intent: &'static str,
    risk: BrowserRisk,
    local_project_root: Option<&Path>,
    locator_failure_context: Option<ReplayLocatorFailureContext>,
    command: BrowserCommand,
) -> Result<BrowserResponse, ReplayActionFailure> {
    match checked_request_with_options(
        controller,
        coordinator,
        instance,
        execution,
        actor,
        intent,
        risk,
        local_project_root,
        command,
    )
    .await
    {
        Err(ReplayActionFailure::LocatorNotFound(target)) => Err(contextualize_locator_failure(
            locator_failure_context,
            target,
        )),
        result => result,
    }
}

fn repair_positive_timeout<T>(
    result: Result<T, ReplayActionFailure>,
    locator_failure_context: Option<ReplayLocatorFailureContext>,
) -> Result<T, ReplayActionFailure> {
    match result {
        Err(ReplayActionFailure::PageConditionTimeout) => Err(locator_failure_context
            .and_then(|context| context.resolve(BrowserLocatorFailureTarget::Primary))
            .map(ReplayActionFailure::Repairable)
            .unwrap_or(ReplayActionFailure::PageConditionTimeout)),
        result => result,
    }
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
        Err(BrowserError::LocatorNotFound { target }) => {
            Err(ReplayActionFailure::LocatorNotFound(target))
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

fn effective_action_target(
    execution: &BrowserReplayExecutionHandle,
    step_index: usize,
    locator_slot: BrowserReplayLocatorSlot,
    locator: &BrowserRecipeLocator,
) -> BrowserActionTarget {
    let locator = execution
        .locator_override(step_index, locator_slot)
        .unwrap_or_else(|| locator.clone());
    action_target(&locator)
}

fn compile_wait(
    plan: &BrowserReplayPlan,
    wait: &BrowserRecipeWait,
    locator_override: Option<&BrowserRecipeLocator>,
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
                target: action_target(locator_override.unwrap_or(locator)),
            },
            *timeout_ms,
        )),
        BrowserRecipeWait::ElementVisible {
            locator,
            timeout_ms,
        } => Some((
            BrowserWaitCondition::ElementVisible {
                target: action_target(locator_override.unwrap_or(locator)),
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
        ReplayActionFailure::LocatorNotFound(_) | ReplayActionFailure::Repairable(_) => {
            fail_step(coordinator, instance)
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::replay::BrowserReplayRepairApplyCommit;
    use crate::browser::{
        browser_command_channel, compile_browser_replay, recipe_path, save_recipe,
        BrowserActionResult, BrowserElementRef, BrowserLocatorFailureTarget, BrowserRecipeInput,
        BrowserRecipeInputKind, BrowserRecipeStep, BrowserRecipeV1, BrowserRecipeViewport,
        BrowserReplayRepairCandidate, BrowserReplaySecretPromptVault, BrowserResourceKind,
        BrowserResourceLimits, BrowserSnapshotSummary, BrowserWaitResult, BrowserWorkspaceKey,
        BrowserWorkspaceMutation, BROWSER_RECIPE_SCHEMA_VERSION,
    };
    use std::time::Duration;

    fn locator(name: &str) -> BrowserRecipeLocator {
        BrowserRecipeLocator {
            test_id: Some(name.to_string()),
            ..BrowserRecipeLocator::default()
        }
    }

    fn literal(value: &str) -> BrowserRecipeValue {
        BrowserRecipeValue::Literal {
            value: value.to_string(),
        }
    }

    fn test_workspace() -> BrowserWorkspaceKey {
        BrowserWorkspaceKey::new("executor-repair", "secure-pause").unwrap()
    }

    fn workspace_response(
        tab_id: &str,
        url: &str,
        viewport: BrowserViewport,
        revision: u64,
    ) -> BrowserResponse {
        BrowserResponse::Workspace {
            mutation: BrowserWorkspaceMutation {
                revision: crate::browser::BrowserRevision(revision),
                snapshot: BrowserWorkspaceSnapshot {
                    revision: crate::browser::BrowserRevision(revision),
                    tabs: vec![BrowserTabSnapshot {
                        id: tab_id.to_string(),
                        title: "Repair".to_string(),
                        url: url.to_string(),
                        viewport,
                    }],
                    selected_tab_id: Some(tab_id.to_string()),
                    ..BrowserWorkspaceSnapshot::default()
                },
            },
        }
    }

    #[tokio::test]
    async fn execute_browser_replay_binds_recipe_root_exactly_once_before_first_command() {
        let owner = BrowserWorkspaceKey::new("executor-root-binding", "prebound").unwrap();
        let recipe = BrowserRecipeV1 {
            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
            id: "executor-root-binding".to_string(),
            name: "Root binding".to_string(),
            description: "Executor must bind before dispatch".to_string(),
            start_url: "https://example.test/".to_string(),
            viewport: BrowserRecipeViewport::default(),
            inputs: Vec::new(),
            steps: vec![BrowserRecipeStep {
                id: "reload".to_string(),
                action: BrowserRecipeAction::Reload,
                wait: None,
                assertions: Vec::new(),
            }],
        };
        let coordinator = BrowserReplayCoordinator::default();
        let started = coordinator
            .start(
                owner.clone(),
                compile_browser_replay(&recipe, Vec::new()).unwrap(),
            )
            .unwrap();
        let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .canonicalize()
            .unwrap();
        started
            .execution
            .bind_canonical_recipe_root(&project_root)
            .unwrap();
        let resource_root = std::env::temp_dir().join(format!(
            "devmanager-executor-root-binding-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&resource_root);
        let store = BrowserResourceStore::open(
            &resource_root,
            BrowserResourceLimits {
                max_temporary_count: 0,
                max_temporary_bytes: 1024,
                max_resource_bytes: 1024,
            },
        )
        .unwrap();
        let (bridge, mut inbox) = browser_command_channel(1);
        let controller = bridge.bind(owner, Duration::from_millis(50));
        let result = execute_browser_replay(
            &controller,
            &coordinator,
            &started.instance,
            started.execution,
            BrowserInvocationActor::Agent,
            &store,
            &project_root,
        )
        .await;
        assert!(matches!(
            result,
            Err(BrowserReplayError::RecipeRootAlreadyBound)
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), inbox.recv())
                .await
                .is_err()
        );

        coordinator.cancel(&started.instance).unwrap();
        drop(store);
        std::fs::remove_dir_all(resource_root).unwrap();
    }

    async fn next_test_request(
        inbox: &mut crate::browser::BrowserCommandInbox,
        label: &str,
    ) -> crate::browser::BrowserCommandRequest {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(request) = inbox.with_locked_host_work(|_controls, mut requests| {
                    assert!(requests.len() <= 1, "executor queued lifecycle commands");
                    requests.pop()
                }) {
                    return request;
                }
                if let Ok(Some(request)) =
                    tokio::time::timeout(Duration::from_millis(10), inbox.recv()).await
                {
                    return request;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
    }

    async fn respond_test_setup(inbox: &mut crate::browser::BrowserCommandInbox) {
        let create = next_test_request(inbox, "setup create").await;
        assert_eq!(create.command(), &BrowserCommand::CreateTab { url: None });
        create.respond(Ok(workspace_response(
            "repair-tab",
            "about:blank",
            BrowserViewport::default(),
            1,
        )));

        let viewport = next_test_request(inbox, "setup viewport").await;
        assert!(matches!(
            viewport.command(),
            BrowserCommand::UpdateViewport { tab_id, viewport }
                if tab_id == "repair-tab" && viewport == &BrowserViewport::default()
        ));
        viewport.respond(Ok(workspace_response(
            "repair-tab",
            "about:blank",
            BrowserViewport::default(),
            2,
        )));

        let navigate = next_test_request(inbox, "setup navigate").await;
        assert_eq!(
            navigate.command(),
            &BrowserCommand::Navigate {
                tab_id: "repair-tab".to_string(),
                url: "https://example.test/repair".to_string(),
            }
        );
        navigate.respond(Ok(workspace_response(
            "repair-tab",
            "https://example.test/repair",
            BrowserViewport::default(),
            3,
        )));
    }

    struct FailedClickFixture {
        owner: BrowserWorkspaceKey,
        store: BrowserResourceStore,
        resource_root: PathBuf,
        project_root: PathBuf,
        bridge: crate::browser::BrowserCommandBridge,
        coordinator: BrowserReplayCoordinator,
        instance: BrowserReplayInstance,
        run: tokio::task::JoinHandle<Result<BrowserReplayProjection, BrowserReplayError>>,
        inbox: crate::browser::BrowserCommandInbox,
    }

    async fn recipe_fixture(label: &str, recipe: BrowserRecipeV1) -> FailedClickFixture {
        let owner = BrowserWorkspaceKey::new("executor-repair", label).unwrap();
        let fixture_root = std::env::temp_dir().join(format!(
            "devmanager-executor-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&fixture_root);
        let project_root = fixture_root.join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        let project_root = project_root.canonicalize().unwrap();
        save_recipe(&project_root, &recipe).unwrap();
        let resource_root = fixture_root.join("resources");
        let store = BrowserResourceStore::open(
            &resource_root,
            BrowserResourceLimits {
                max_temporary_count: 0,
                max_temporary_bytes: 1024 * 1024,
                max_resource_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        let coordinator = BrowserReplayCoordinator::default();
        let started = coordinator
            .start(
                owner.clone(),
                compile_browser_replay(&recipe, Vec::new()).unwrap(),
            )
            .unwrap();
        let instance = started.instance.clone();
        let (bridge, mut inbox) = browser_command_channel(8);
        let controller = bridge.bind(owner.clone(), Duration::from_secs(1));
        let run_store = store.clone();
        let run_coordinator = coordinator.clone();
        let run_instance = instance.clone();
        let run_project_root = project_root.clone();
        let run = tokio::spawn(async move {
            execute_browser_replay(
                &controller,
                &run_coordinator,
                &run_instance,
                started.execution,
                BrowserInvocationActor::Agent,
                &run_store,
                &run_project_root,
            )
            .await
        });
        respond_test_setup(&mut inbox).await;
        FailedClickFixture {
            owner,
            store,
            resource_root: fixture_root,
            project_root,
            bridge,
            coordinator,
            instance,
            run,
            inbox,
        }
    }

    async fn failed_click_fixture(label: &str) -> FailedClickFixture {
        let recipe = BrowserRecipeV1 {
            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
            id: format!("repair-{label}"),
            name: "Repair fixture".to_string(),
            description: "Failure-boundary fixture".to_string(),
            start_url: "https://example.test/repair".to_string(),
            viewport: BrowserRecipeViewport::default(),
            inputs: Vec::new(),
            steps: vec![BrowserRecipeStep {
                id: "click-submit".to_string(),
                action: BrowserRecipeAction::Click {
                    locator: locator("submit"),
                },
                wait: None,
                assertions: Vec::new(),
            }],
        };
        let mut fixture = recipe_fixture(label, recipe).await;
        let action = next_test_request(&mut fixture.inbox, "repairable click").await;
        action.respond(Err(BrowserError::LocatorNotFound {
            target: BrowserLocatorFailureTarget::Primary,
        }));
        fixture
    }

    fn fresh_repair_snapshot() -> BrowserWorkspaceSnapshot {
        match workspace_response(
            "repair-tab",
            "https://example.test/repair",
            BrowserViewport::default(),
            9,
        ) {
            BrowserResponse::Workspace { mutation } => mutation.snapshot,
            _ => unreachable!(),
        }
    }

    fn replacement_plan(label: &str) -> BrowserReplayPlan {
        compile_browser_replay(
            &BrowserRecipeV1 {
                schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
                id: format!("replacement-{label}"),
                name: "Replacement".to_string(),
                description: "Terminal race replacement".to_string(),
                start_url: "https://example.test/replacement".to_string(),
                viewport: BrowserRecipeViewport::default(),
                inputs: Vec::new(),
                steps: vec![BrowserRecipeStep {
                    id: "reload".to_string(),
                    action: BrowserRecipeAction::Reload,
                    wait: None,
                    assertions: Vec::new(),
                }],
            },
            Vec::new(),
        )
        .unwrap()
    }

    fn completed_action_response() -> BrowserResponse {
        BrowserResponse::Action {
            result: BrowserActionResult {
                completed_actions: 1,
                revision: crate::browser::BrowserRevision(10),
            },
        }
    }

    fn matched_wait_response() -> BrowserResponse {
        BrowserResponse::Wait {
            result: BrowserWaitResult {
                matched: true,
                elapsed_ms: 1,
                revision: crate::browser::BrowserRevision(10),
            },
        }
    }

    fn command_targets_test_id(command: &BrowserCommand, expected: &str) -> bool {
        let target = match command {
            BrowserCommand::Act { actions, .. } => match actions.as_slice() {
                [BrowserAction::Click { target }] => Some(target),
                _ => None,
            },
            BrowserCommand::Wait { condition, .. } => match condition {
                BrowserWaitCondition::ElementPresent { target }
                | BrowserWaitCondition::ElementVisible { target }
                | BrowserWaitCondition::ElementValue { target, .. } => Some(target),
                _ => None,
            },
            _ => None,
        };
        target.and_then(|target| target.locator.test_id.as_deref()) == Some(expected)
    }

    async fn respond_fresh_repair_state(fixture: &mut FailedClickFixture) {
        let state = next_test_request(&mut fixture.inbox, "fresh repair workspace state").await;
        assert_eq!(state.command(), &BrowserCommand::WorkspaceState);
        state.respond(Ok(BrowserResponse::WorkspaceState {
            snapshot: fresh_repair_snapshot(),
        }));
    }

    async fn retain_snapshot(fixture: &mut FailedClickFixture) -> BrowserResourceHandle {
        let request = next_test_request(&mut fixture.inbox, "retained repair snapshot").await;
        assert!(request
            .validate_repair_retention_sidecar()
            .unwrap()
            .is_some());
        let handle = request
            .retain_repair_resource(
                &fixture.store,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            )
            .unwrap();
        request.respond(Ok(BrowserResponse::Snapshot {
            summary: BrowserSnapshotSummary {
                tab_id: "repair-tab".to_string(),
                url: "https://example.test/repair".to_string(),
                revision: crate::browser::BrowserRevision(9),
                element_count: 1,
            },
            resource: handle.clone(),
        }));
        handle
    }

    async fn drive_fixture_to_pause(
        mut fixture: FailedClickFixture,
    ) -> (
        FailedClickFixture,
        BrowserResourceHandle,
        BrowserResourceHandle,
    ) {
        respond_fresh_repair_state(&mut fixture).await;
        let snapshot = retain_snapshot(&mut fixture).await;
        let request = next_test_request(&mut fixture.inbox, "retained repair screenshot").await;
        assert_eq!(
            request.command(),
            &BrowserCommand::Screenshot {
                tab_id: "repair-tab".to_string(),
                mode: BrowserScreenshotMode::Viewport,
            }
        );
        assert!(request
            .validate_repair_retention_sidecar()
            .unwrap()
            .is_some());
        let screenshot = request
            .retain_repair_resource(
                &fixture.store,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                b"png",
            )
            .unwrap();
        request.respond(Ok(BrowserResponse::Screenshot {
            resource: screenshot.clone(),
        }));
        let paused = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if fixture
                    .coordinator
                    .status(&fixture.instance)
                    .unwrap()
                    .status
                    == BrowserReplayStatus::PausedLocatorRepair
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        if paused.is_err() {
            panic!(
                "stable paused repair: {:?}",
                fixture.coordinator.status(&fixture.instance)
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(25), fixture.inbox.recv())
                .await
                .is_err()
        );
        (fixture, snapshot, screenshot)
    }

    async fn preview_exact_repair(
        fixture: &mut FailedClickFixture,
        replacement_test_id: &str,
        revision: u64,
    ) -> BrowserReplayRepairInstance {
        let (repair, _, _) = fixture
            .coordinator
            .active_locator_repair_capture_for_test(&fixture.instance)
            .unwrap();
        let expected_phase = if fixture
            .coordinator
            .locator_repair_status(&repair)
            .unwrap()
            .phase
            == crate::browser::BrowserReplayRepairPhase::Applied
        {
            crate::browser::BrowserReplayRepairPhase::Applied
        } else {
            crate::browser::BrowserReplayRepairPhase::Previewed
        };
        let candidate = BrowserReplayRepairCandidate::new(BrowserElementRef {
            revision: crate::browser::BrowserRevision(revision),
            locator: BrowserLocator {
                test_id: Some(replacement_test_id.to_string()),
                ..BrowserLocator::default()
            },
            backend_node_id: Some(revision.saturating_add(100)),
        });
        let controller = fixture
            .bridge
            .bind(fixture.owner.clone(), Duration::from_secs(1));
        let preview_coordinator = fixture.coordinator.clone();
        let preview_repair = repair.clone();
        let preview = tokio::spawn(async move {
            controller
                .request_replay_repair_preview(
                    &preview_coordinator,
                    &preview_repair,
                    candidate,
                    BrowserInvocationActor::Agent,
                )
                .await
        });
        let preview_request = next_test_request(&mut fixture.inbox, "repair preview").await;
        assert!(matches!(
            preview_request.command(),
            BrowserCommand::RepairHighlight { tab_id } if tab_id == "repair-tab"
        ));
        let preview_authority = preview_request
            .repair_preview_highlight_authority()
            .expect("exact preview authority")
            .clone();
        assert!(preview_authority.acknowledge_for_test());
        preview_request.respond(Ok(BrowserResponse::Acknowledged));
        assert_eq!(preview.await.unwrap().unwrap().phase, expected_phase);
        repair
    }

    async fn apply_previewed_repair(
        fixture: &mut FailedClickFixture,
        repair: &BrowserReplayRepairInstance,
        resume: bool,
        expect_recipe_write: bool,
    ) -> BrowserReplayRepairApplyCommit {
        let apply_controller = fixture
            .bridge
            .bind(fixture.owner.clone(), Duration::from_secs(1));
        let apply_coordinator = fixture.coordinator.clone();
        let apply_repair = repair.clone();
        let apply = tokio::spawn(async move {
            apply_controller
                .request_replay_repair_apply(
                    &apply_coordinator,
                    &apply_repair,
                    true,
                    resume,
                    BrowserInvocationContext::agent(
                        "apply executor locator repair",
                        BrowserRisk::Normal,
                    )
                    .unwrap(),
                )
                .await
        });
        let validation_count = if expect_recipe_write { 2 } else { 1 };
        for index in 0..validation_count {
            let label = if index == 0 {
                "pre-commit repair validation"
            } else {
                "post-commit repair validation"
            };
            let request = next_test_request(&mut fixture.inbox, label).await;
            assert!(matches!(
                request.command(),
                BrowserCommand::RepairValidate { tab_id, .. } if tab_id == "repair-tab"
            ));
            let authority = request
                .repair_apply_authority()
                .expect("exact apply authority")
                .clone();
            assert!(authority.acknowledge_for_test());
            request.respond(Ok(BrowserResponse::Acknowledged));
        }
        let outcome = apply.await.unwrap().unwrap();
        assert_eq!(outcome.recipe_written, expect_recipe_write);
        outcome
    }

    async fn apply_exact_repair(
        fixture: &mut FailedClickFixture,
        replacement_test_id: &str,
        revision: u64,
    ) -> BrowserReplayRepairInstance {
        let repair = preview_exact_repair(fixture, replacement_test_id, revision).await;
        let outcome = apply_previewed_repair(fixture, &repair, true, true).await;
        assert_eq!(outcome.replay.status, BrowserReplayStatus::Running);
        drop(outcome);
        repair
    }

    async fn assert_failed_and_clean(mut fixture: FailedClickFixture) {
        let terminal = fixture.run.await.unwrap().unwrap();
        assert_eq!(terminal.status, BrowserReplayStatus::Failed);
        assert_eq!(terminal.failure, Some(BrowserReplayFailureCode::StepFailed));
        assert_eq!(terminal.current_step_index, 0);
        assert!(fixture.store.list(&fixture.owner).unwrap().is_empty());
        assert!(!matches!(
            tokio::time::timeout(Duration::from_millis(25), fixture.inbox.recv()).await,
            Ok(Some(_))
        ));
        drop(fixture.store);
        let _ = std::fs::remove_dir_all(fixture.resource_root);
    }

    fn assert_target(
        context: Option<ReplayLocatorFailureContext>,
        host_target: BrowserLocatorFailureTarget,
        expected_slot: BrowserReplayLocatorSlot,
        expected_cursor: BrowserReplayRepairResumeCursor,
    ) {
        let target = context
            .and_then(|context| context.resolve(host_target))
            .expect("repairable exact host target");
        assert_eq!(target.locator_slot, expected_slot);
        assert!(target.resume_cursor == expected_cursor);
    }

    #[test]
    fn locator_failure_context_maps_every_exact_action_slot_and_rejects_impossible_targets() {
        assert_target(
            action_locator_failure_context(&BrowserRecipeAction::Click {
                locator: locator("primary"),
            }),
            BrowserLocatorFailureTarget::Primary,
            BrowserReplayLocatorSlot::PrimaryAction,
            BrowserReplayRepairResumeCursor::Action,
        );
        assert_target(
            action_locator_failure_context(&BrowserRecipeAction::Keypress {
                locator: Some(locator("optional")),
                key: literal("Enter"),
            }),
            BrowserLocatorFailureTarget::Primary,
            BrowserReplayLocatorSlot::OptionalAction,
            BrowserReplayRepairResumeCursor::Action,
        );
        assert!(
            action_locator_failure_context(&BrowserRecipeAction::Keypress {
                locator: None,
                key: literal("Enter"),
            })
            .is_none()
        );

        let drag = action_locator_failure_context(&BrowserRecipeAction::DragDrop {
            source: locator("source"),
            destination: locator("destination"),
        })
        .expect("drag repair context");
        let source = drag
            .resolve(BrowserLocatorFailureTarget::Source)
            .expect("drag source");
        let destination = drag
            .resolve(BrowserLocatorFailureTarget::Destination)
            .expect("drag destination");
        assert_eq!(source.locator_slot, BrowserReplayLocatorSlot::DragSource);
        assert_eq!(
            destination.locator_slot,
            BrowserReplayLocatorSlot::DragDestination
        );
        assert!(drag.resolve(BrowserLocatorFailureTarget::Primary).is_none());
        assert!(action_locator_failure_context(&BrowserRecipeAction::Click {
            locator: locator("primary"),
        })
        .expect("primary context")
        .resolve(BrowserLocatorFailureTarget::Source)
        .is_none());
    }

    #[test]
    fn only_positive_locator_waits_and_assertions_are_repairable_at_their_exact_phase() {
        for wait in [
            BrowserRecipeWait::ElementPresent {
                locator: locator("present"),
                timeout_ms: 10,
            },
            BrowserRecipeWait::ElementVisible {
                locator: locator("visible"),
                timeout_ms: 10,
            },
        ] {
            assert_target(
                wait_locator_failure_context(
                    &wait,
                    BrowserReplayLocatorSlot::StepWait,
                    BrowserReplayRepairResumeCursor::StepWait,
                ),
                BrowserLocatorFailureTarget::Primary,
                BrowserReplayLocatorSlot::StepWait,
                BrowserReplayRepairResumeCursor::StepWait,
            );
        }
        assert!(wait_locator_failure_context(
            &BrowserRecipeWait::ElementHidden {
                locator: locator("hidden"),
                timeout_ms: 10,
            },
            BrowserReplayLocatorSlot::StepWait,
            BrowserReplayRepairResumeCursor::StepWait,
        )
        .is_none());

        for (index, assertion) in [
            BrowserRecipeAssertion::Element {
                locator: locator("present"),
                state: BrowserRecipeElementState::Present,
            },
            BrowserRecipeAssertion::Element {
                locator: locator("visible"),
                state: BrowserRecipeElementState::Visible,
            },
            BrowserRecipeAssertion::Value {
                locator: locator("value"),
                value: literal("expected"),
            },
        ]
        .into_iter()
        .enumerate()
        {
            assert_target(
                assertion_locator_failure_context(&assertion, index),
                BrowserLocatorFailureTarget::Primary,
                BrowserReplayLocatorSlot::Assertion { index },
                BrowserReplayRepairResumeCursor::Assertion(index),
            );
        }
        for assertion in [
            BrowserRecipeAssertion::Element {
                locator: locator("absent"),
                state: BrowserRecipeElementState::Absent,
            },
            BrowserRecipeAssertion::Element {
                locator: locator("hidden"),
                state: BrowserRecipeElementState::Hidden,
            },
        ] {
            assert!(assertion_locator_failure_context(&assertion, 0).is_none());
        }

        assert_target(
            wait_locator_failure_context(
                &BrowserRecipeWait::ElementVisible {
                    locator: locator("action-wait"),
                    timeout_ms: 10,
                },
                BrowserReplayLocatorSlot::ActionWait,
                BrowserReplayRepairResumeCursor::ActionWait,
            ),
            BrowserLocatorFailureTarget::Primary,
            BrowserReplayLocatorSlot::ActionWait,
            BrowserReplayRepairResumeCursor::ActionWait,
        );
    }

    #[test]
    fn impossible_host_targets_and_negative_timeouts_fall_back_without_repair() {
        let primary = action_locator_failure_context(&BrowserRecipeAction::Click {
            locator: locator("primary"),
        });
        assert!(matches!(
            contextualize_locator_failure(primary, BrowserLocatorFailureTarget::Source),
            ReplayActionFailure::StepFailed
        ));
        let drag = action_locator_failure_context(&BrowserRecipeAction::DragDrop {
            source: locator("source"),
            destination: locator("destination"),
        });
        assert!(matches!(
            contextualize_locator_failure(drag, BrowserLocatorFailureTarget::Primary),
            ReplayActionFailure::StepFailed
        ));
        assert!(matches!(
            contextualize_locator_failure(None, BrowserLocatorFailureTarget::Primary),
            ReplayActionFailure::StepFailed
        ));

        let positive = wait_locator_failure_context(
            &BrowserRecipeWait::ElementPresent {
                locator: locator("present"),
                timeout_ms: 10,
            },
            BrowserReplayLocatorSlot::StepWait,
            BrowserReplayRepairResumeCursor::StepWait,
        );
        assert!(matches!(
            repair_positive_timeout::<()>(Err(ReplayActionFailure::PageConditionTimeout), positive),
            Err(ReplayActionFailure::Repairable(_))
        ));
        assert!(matches!(
            repair_positive_timeout::<()>(Err(ReplayActionFailure::PageConditionTimeout), None),
            Err(ReplayActionFailure::PageConditionTimeout)
        ));
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn every_capture_failure_boundary_rolls_back_the_exact_lease_and_fails_the_step() {
        let mut workspace = failed_click_fixture("rollback-workspace").await;
        next_test_request(&mut workspace.inbox, "workspace-state rollback")
            .await
            .respond(Err(BrowserError::CrashedView {
                message: "private host detail".to_string(),
            }));
        assert_failed_and_clean(workspace).await;

        let mut snapshot = failed_click_fixture("rollback-snapshot").await;
        respond_fresh_repair_state(&mut snapshot).await;
        let snapshot_request =
            next_test_request(&mut snapshot.inbox, "snapshot capture rollback").await;
        assert!(snapshot_request
            .validate_repair_retention_sidecar()
            .unwrap()
            .is_some());
        snapshot_request.respond(Err(BrowserError::ResourceRootUnavailable));
        assert_failed_and_clean(snapshot).await;

        let mut screenshot = failed_click_fixture("rollback-screenshot").await;
        respond_fresh_repair_state(&mut screenshot).await;
        let snapshot_handle = retain_snapshot(&mut screenshot).await;
        let screenshot_request =
            next_test_request(&mut screenshot.inbox, "screenshot capture rollback").await;
        assert!(screenshot_request
            .validate_repair_retention_sidecar()
            .unwrap()
            .is_some());
        screenshot_request.respond(Err(BrowserError::ResourceRootUnavailable));
        let owner = screenshot.owner.clone();
        let store = screenshot.store.clone();
        assert_failed_and_clean(screenshot).await;
        assert!(matches!(
            store.handle(&owner, &snapshot_handle.id),
            Err(BrowserError::MissingResource { .. })
        ));

        let mut publish_validation = failed_click_fixture("rollback-publish").await;
        respond_fresh_repair_state(&mut publish_validation).await;
        let snapshot_handle = retain_snapshot(&mut publish_validation).await;
        let screenshot_request =
            next_test_request(&mut publish_validation.inbox, "publish validation rollback").await;
        let screenshot_handle = screenshot_request
            .retain_repair_resource(
                &publish_validation.store,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                b"png",
            )
            .unwrap();
        screenshot_request.respond(Ok(BrowserResponse::Screenshot {
            resource: snapshot_handle.clone(),
        }));
        let owner = publish_validation.owner.clone();
        let store = publish_validation.store.clone();
        assert_failed_and_clean(publish_validation).await;
        for id in [&snapshot_handle.id, &screenshot_handle.id] {
            assert!(matches!(
                store.handle(&owner, id),
                Err(BrowserError::MissingResource { .. })
            ));
        }
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn batch_b_paused_watch_returns_retained_terminal_for_cancel_replace_and_interrupt() {
        for path in ["cancel", "replace", "interrupt"] {
            let fixture = failed_click_fixture(&format!("paused-{path}")).await;
            let (fixture, snapshot, screenshot) = drive_fixture_to_pause(fixture).await;
            let replacement = match path {
                "cancel" => {
                    fixture.coordinator.cancel(&fixture.instance).unwrap();
                    None
                }
                "replace" => Some(
                    fixture
                        .coordinator
                        .replace(fixture.owner.clone(), replacement_plan(path))
                        .unwrap(),
                ),
                "interrupt" => {
                    fixture
                        .coordinator
                        .interrupt_workspace(&fixture.owner)
                        .unwrap();
                    None
                }
                _ => unreachable!(),
            };
            let terminal = fixture.run.await.unwrap().unwrap();
            assert_eq!(terminal.status, BrowserReplayStatus::Cancelled);
            assert_eq!(terminal.instance_id, fixture.instance.id());
            assert_eq!(terminal.current_step_index, 0);
            for id in [&snapshot.id, &screenshot.id] {
                assert!(matches!(
                    fixture.store.handle(&fixture.owner, id),
                    Err(BrowserError::MissingResource { .. })
                ));
            }
            if let Some(replacement) = replacement {
                fixture.coordinator.cancel(&replacement.instance).unwrap();
            }
            drop(fixture.store);
            let _ = std::fs::remove_dir_all(fixture.resource_root);
        }
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn batch_b_controller_interrupt_cancels_exact_replay_and_ignores_late_response() {
        let mut fixture = failed_click_fixture("controller-interrupt").await;
        respond_fresh_repair_state(&mut fixture).await;
        let snapshot = next_test_request(&mut fixture.inbox, "interrupted repair snapshot").await;
        assert!(snapshot
            .validate_repair_retention_sidecar()
            .unwrap()
            .is_some());
        fixture.bridge.interrupt_workspace(&fixture.owner);
        let terminal = tokio::time::timeout(Duration::from_secs(1), fixture.run)
            .await
            .expect("controller interruption wakes executor")
            .unwrap()
            .unwrap();
        assert_eq!(terminal.status, BrowserReplayStatus::Cancelled);
        assert_eq!(terminal.instance_id, fixture.instance.id());
        snapshot.respond(Ok(BrowserResponse::Action {
            result: crate::browser::BrowserActionResult {
                completed_actions: 1,
                revision: crate::browser::BrowserRevision(99),
            },
        }));
        assert!(fixture.store.list(&fixture.owner).unwrap().is_empty());
        drop(fixture.store);
        let _ = std::fs::remove_dir_all(fixture.resource_root);
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn batch_b_replacement_racing_publish_is_an_early_terminal_generation() {
        let mut fixture = failed_click_fixture("early-replacement").await;
        respond_fresh_repair_state(&mut fixture).await;
        let snapshot = retain_snapshot(&mut fixture).await;
        let request = next_test_request(&mut fixture.inbox, "racing repair screenshot").await;
        let screenshot = request
            .retain_repair_resource(
                &fixture.store,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                b"png",
            )
            .unwrap();
        request.respond(Ok(BrowserResponse::Screenshot {
            resource: screenshot.clone(),
        }));
        let replacement = fixture
            .coordinator
            .replace(fixture.owner.clone(), replacement_plan("early"))
            .unwrap();
        let terminal = fixture.run.await.unwrap().unwrap();
        assert_eq!(terminal.status, BrowserReplayStatus::Cancelled);
        assert_eq!(terminal.instance_id, fixture.instance.id());
        for id in [&snapshot.id, &screenshot.id] {
            assert!(matches!(
                fixture.store.handle(&fixture.owner, id),
                Err(BrowserError::MissingResource { .. })
            ));
        }
        fixture.coordinator.cancel(&replacement.instance).unwrap();
        drop(fixture.store);
        let _ = std::fs::remove_dir_all(fixture.resource_root);
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn batch_c_resume_retries_only_action_wait_step_wait_or_exact_assertion_phase() {
        let action = failed_click_fixture("phase-action").await;
        let (mut action, action_snapshot, action_screenshot) = drive_fixture_to_pause(action).await;
        let (_, action_slot, action_cursor) = action
            .coordinator
            .active_locator_repair_capture_for_test(&action.instance)
            .unwrap();
        assert_eq!(action_slot, BrowserReplayLocatorSlot::PrimaryAction);
        assert!(action_cursor == BrowserReplayRepairResumeCursor::Action);
        apply_exact_repair(&mut action, "repaired-submit", 9).await;
        assert_eq!(
            action
                .coordinator
                .status(&action.instance)
                .unwrap()
                .current_step_index,
            0
        );
        let retried_action = next_test_request(&mut action.inbox, "retried action").await;
        assert!(command_targets_test_id(
            retried_action.command(),
            "repaired-submit"
        ));
        retried_action.respond(Ok(completed_action_response()));
        assert_eq!(
            action.run.await.unwrap().unwrap().status,
            BrowserReplayStatus::Completed
        );
        for id in [&action_snapshot.id, &action_screenshot.id] {
            assert!(matches!(
                action.store.handle(&action.owner, id),
                Err(BrowserError::MissingResource { .. })
            ));
        }
        drop(action.store);
        let _ = std::fs::remove_dir_all(action.resource_root);

        let action_wait_recipe = BrowserRecipeV1 {
            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
            id: "phase-action-wait".to_string(),
            name: "Action wait".to_string(),
            description: "Action wait cursor fixture".to_string(),
            start_url: "https://example.test/repair".to_string(),
            viewport: BrowserRecipeViewport::default(),
            inputs: Vec::new(),
            steps: vec![BrowserRecipeStep {
                id: "action-wait".to_string(),
                action: BrowserRecipeAction::Wait {
                    condition: BrowserRecipeWait::ElementPresent {
                        locator: locator("action-ready"),
                        timeout_ms: 50,
                    },
                },
                wait: None,
                assertions: Vec::new(),
            }],
        };
        let mut action_wait = recipe_fixture("phase-action-wait", action_wait_recipe).await;
        let first_action_wait =
            next_test_request(&mut action_wait.inbox, "first action wait").await;
        assert!(matches!(
            first_action_wait.command(),
            BrowserCommand::Wait {
                condition: BrowserWaitCondition::ElementPresent { .. },
                ..
            }
        ));
        first_action_wait.respond(Err(BrowserError::Timeout {
            operation: "pageCondition".to_string(),
        }));
        let (mut action_wait, action_snapshot, action_screenshot) =
            drive_fixture_to_pause(action_wait).await;
        let (repair, slot, cursor) = action_wait
            .coordinator
            .active_locator_repair_capture_for_test(&action_wait.instance)
            .unwrap();
        assert_eq!(slot, BrowserReplayLocatorSlot::ActionWait);
        assert!(cursor == BrowserReplayRepairResumeCursor::ActionWait);
        assert_eq!(repair.replay(), &action_wait.instance);
        apply_exact_repair(&mut action_wait, "repaired-action-ready", 9).await;
        assert_eq!(
            action_wait
                .coordinator
                .status(&action_wait.instance)
                .unwrap()
                .current_step_index,
            0
        );
        let retry = next_test_request(&mut action_wait.inbox, "retried action wait").await;
        assert!(command_targets_test_id(
            retry.command(),
            "repaired-action-ready"
        ));
        retry.respond(Ok(matched_wait_response()));
        assert_eq!(
            action_wait.run.await.unwrap().unwrap().status,
            BrowserReplayStatus::Completed
        );
        for id in [&action_snapshot.id, &action_screenshot.id] {
            assert!(matches!(
                action_wait.store.handle(&action_wait.owner, id),
                Err(BrowserError::MissingResource { .. })
            ));
        }
        drop(action_wait.store);
        let _ = std::fs::remove_dir_all(action_wait.resource_root);

        let step_wait_recipe = BrowserRecipeV1 {
            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
            id: "phase-step-wait".to_string(),
            name: "Step wait".to_string(),
            description: "Step wait cursor fixture".to_string(),
            start_url: "https://example.test/repair".to_string(),
            viewport: BrowserRecipeViewport::default(),
            inputs: Vec::new(),
            steps: vec![BrowserRecipeStep {
                id: "click-then-wait".to_string(),
                action: BrowserRecipeAction::Click {
                    locator: locator("submit"),
                },
                wait: Some(BrowserRecipeWait::ElementVisible {
                    locator: locator("ready"),
                    timeout_ms: 50,
                }),
                assertions: Vec::new(),
            }],
        };
        let mut step_wait = recipe_fixture("phase-step-wait", step_wait_recipe).await;
        let action = next_test_request(&mut step_wait.inbox, "single mutating action").await;
        assert!(matches!(action.command(), BrowserCommand::Act { .. }));
        action.respond(Ok(completed_action_response()));
        let wait = next_test_request(&mut step_wait.inbox, "failing step wait").await;
        assert!(matches!(
            wait.command(),
            BrowserCommand::Wait {
                condition: BrowserWaitCondition::ElementVisible { .. },
                ..
            }
        ));
        wait.respond(Err(BrowserError::Timeout {
            operation: "pageCondition".to_string(),
        }));
        let (mut step_wait, step_snapshot, step_screenshot) =
            drive_fixture_to_pause(step_wait).await;
        let (repair, slot, cursor) = step_wait
            .coordinator
            .active_locator_repair_capture_for_test(&step_wait.instance)
            .unwrap();
        assert_eq!(slot, BrowserReplayLocatorSlot::StepWait);
        assert!(cursor == BrowserReplayRepairResumeCursor::StepWait);
        assert_eq!(repair.replay(), &step_wait.instance);
        apply_exact_repair(&mut step_wait, "repaired-step-ready", 9).await;
        assert_eq!(
            step_wait
                .coordinator
                .status(&step_wait.instance)
                .unwrap()
                .current_step_index,
            0
        );
        let retry = next_test_request(&mut step_wait.inbox, "step-wait-only retry").await;
        assert!(command_targets_test_id(
            retry.command(),
            "repaired-step-ready"
        ));
        retry.respond(Ok(matched_wait_response()));
        assert_eq!(
            step_wait.run.await.unwrap().unwrap().status,
            BrowserReplayStatus::Completed
        );
        for id in [&step_snapshot.id, &step_screenshot.id] {
            assert!(matches!(
                step_wait.store.handle(&step_wait.owner, id),
                Err(BrowserError::MissingResource { .. })
            ));
        }
        drop(step_wait.store);
        let _ = std::fs::remove_dir_all(step_wait.resource_root);

        let assertion_recipe = BrowserRecipeV1 {
            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
            id: "phase-assertion".to_string(),
            name: "Assertion".to_string(),
            description: "Assertion cursor fixture".to_string(),
            start_url: "https://example.test/repair".to_string(),
            viewport: BrowserRecipeViewport::default(),
            inputs: Vec::new(),
            steps: vec![BrowserRecipeStep {
                id: "click-then-assert".to_string(),
                action: BrowserRecipeAction::Click {
                    locator: locator("submit"),
                },
                wait: None,
                assertions: vec![
                    BrowserRecipeAssertion::Url {
                        value: literal("https://example.test/repair"),
                        exact: true,
                    },
                    BrowserRecipeAssertion::Value {
                        locator: locator("result"),
                        value: literal("42"),
                    },
                ],
            }],
        };
        let mut assertion = recipe_fixture("phase-assertion", assertion_recipe).await;
        next_test_request(&mut assertion.inbox, "single assertion action")
            .await
            .respond(Ok(completed_action_response()));
        let prior = next_test_request(&mut assertion.inbox, "successful prior assertion").await;
        assert!(matches!(
            prior.command(),
            BrowserCommand::Wait {
                condition: BrowserWaitCondition::Url { .. },
                ..
            }
        ));
        prior.respond(Ok(matched_wait_response()));
        let failed = next_test_request(&mut assertion.inbox, "failed value assertion").await;
        assert!(matches!(
            failed.command(),
            BrowserCommand::Wait {
                condition: BrowserWaitCondition::ElementValue { .. },
                ..
            }
        ));
        failed.respond(Err(BrowserError::Timeout {
            operation: "pageCondition".to_string(),
        }));
        let (mut assertion, assertion_snapshot, assertion_screenshot) =
            drive_fixture_to_pause(assertion).await;
        let (repair, slot, cursor) = assertion
            .coordinator
            .active_locator_repair_capture_for_test(&assertion.instance)
            .unwrap();
        assert_eq!(slot, BrowserReplayLocatorSlot::Assertion { index: 1 });
        assert!(cursor == BrowserReplayRepairResumeCursor::Assertion(1));
        assert_eq!(repair.replay(), &assertion.instance);
        apply_exact_repair(&mut assertion, "repaired-result", 9).await;
        assert_eq!(
            assertion
                .coordinator
                .status(&assertion.instance)
                .unwrap()
                .current_step_index,
            0
        );
        let retry = next_test_request(&mut assertion.inbox, "assertion-one-only retry").await;
        assert!(command_targets_test_id(retry.command(), "repaired-result"));
        retry.respond(Ok(matched_wait_response()));
        assert_eq!(
            assertion.run.await.unwrap().unwrap().status,
            BrowserReplayStatus::Completed
        );
        for id in [&assertion_snapshot.id, &assertion_screenshot.id] {
            assert!(matches!(
                assertion.store.handle(&assertion.owner, id),
                Err(BrowserError::MissingResource { .. })
            ));
        }
        drop(assertion.store);
        let _ = std::fs::remove_dir_all(assertion.resource_root);
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn applied_without_resume_requires_fresh_exact_preview_before_no_write_executor_retry() {
        let fixture = failed_click_fixture("applied-no-resume").await;
        let (mut fixture, snapshot, screenshot) = drive_fixture_to_pause(fixture).await;
        let repair = preview_exact_repair(&mut fixture, "repaired-submit", 9).await;
        let applied = apply_previewed_repair(&mut fixture, &repair, false, true).await;
        assert!(applied.recipe_written);
        assert_eq!(
            applied.repair.phase,
            crate::browser::BrowserReplayRepairPhase::Applied
        );
        assert_eq!(
            applied.replay.status,
            BrowserReplayStatus::PausedLocatorRepair
        );
        assert_eq!(applied.replay.current_step_index, 0);
        drop(applied);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), fixture.inbox.recv())
                .await
                .is_err()
        );

        let controller = fixture
            .bridge
            .bind(fixture.owner.clone(), Duration::from_secs(1));
        assert!(controller
            .request_replay_repair_apply(
                &fixture.coordinator,
                &repair,
                true,
                true,
                BrowserInvocationContext::agent(
                    "reject non-fresh executor resume",
                    BrowserRisk::Normal,
                )
                .unwrap(),
            )
            .await
            .is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(25), fixture.inbox.recv())
                .await
                .is_err()
        );

        let fresh = preview_exact_repair(&mut fixture, "repaired-submit", 10).await;
        assert!(fresh == repair);
        let recipe_file = recipe_path(&fixture.project_root, "repair-applied-no-resume").unwrap();
        let committed = std::fs::read(&recipe_file).unwrap();
        let mut permissions = std::fs::metadata(&recipe_file).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&recipe_file, permissions.clone()).unwrap();
        let resumed = apply_previewed_repair(&mut fixture, &repair, true, false).await;
        assert!(!resumed.recipe_written);
        assert_eq!(resumed.replay.status, BrowserReplayStatus::Running);
        assert_eq!(resumed.replay.current_step_index, 0);
        assert_eq!(std::fs::read(&recipe_file).unwrap(), committed);
        drop(resumed);

        let retry = next_test_request(&mut fixture.inbox, "fresh no-write action retry").await;
        assert!(command_targets_test_id(retry.command(), "repaired-submit"));
        permissions.set_readonly(false);
        std::fs::set_permissions(&recipe_file, permissions).unwrap();
        retry.respond(Ok(completed_action_response()));
        let completed = fixture.run.await.unwrap().unwrap();
        assert_eq!(completed.status, BrowserReplayStatus::Completed);
        assert_eq!(completed.current_step_index, 1);
        for id in [&snapshot.id, &screenshot.id] {
            assert!(matches!(
                fixture.store.handle(&fixture.owner, id),
                Err(BrowserError::MissingResource { .. })
            ));
        }
        drop(fixture.store);
        let _ = std::fs::remove_dir_all(fixture.resource_root);
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn stale_repair_cannot_resume_the_next_repair_in_the_same_executor() {
        let recipe = BrowserRecipeV1 {
            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
            id: "stale-repair-resume".to_string(),
            name: "Stale repair resume".to_string(),
            description: "Exact repair generation fixture".to_string(),
            start_url: "https://example.test/repair".to_string(),
            viewport: BrowserRecipeViewport::default(),
            inputs: Vec::new(),
            steps: vec![BrowserRecipeStep {
                id: "click-then-wait".to_string(),
                action: BrowserRecipeAction::Click {
                    locator: locator("submit"),
                },
                wait: Some(BrowserRecipeWait::ElementVisible {
                    locator: locator("ready"),
                    timeout_ms: 50,
                }),
                assertions: Vec::new(),
            }],
        };
        let mut fixture = recipe_fixture("stale-repair-resume", recipe).await;
        next_test_request(&mut fixture.inbox, "single mutating action")
            .await
            .respond(Ok(completed_action_response()));
        next_test_request(&mut fixture.inbox, "first failed step wait")
            .await
            .respond(Err(BrowserError::Timeout {
                operation: "pageCondition".to_string(),
            }));
        let (mut fixture, first_snapshot, first_screenshot) = drive_fixture_to_pause(fixture).await;
        let (stale_repair, stale_slot, stale_cursor) = fixture
            .coordinator
            .active_locator_repair_capture_for_test(&fixture.instance)
            .unwrap();
        assert_eq!(stale_slot, BrowserReplayLocatorSlot::StepWait);
        assert!(stale_cursor == BrowserReplayRepairResumeCursor::StepWait);
        apply_exact_repair(&mut fixture, "ready-first-repair", 9).await;

        next_test_request(&mut fixture.inbox, "second failed step wait")
            .await
            .respond(Err(BrowserError::Timeout {
                operation: "pageCondition".to_string(),
            }));
        let (mut fixture, second_snapshot, second_screenshot) =
            drive_fixture_to_pause(fixture).await;
        let (active_repair, active_slot, active_cursor) = fixture
            .coordinator
            .active_locator_repair_capture_for_test(&fixture.instance)
            .unwrap();
        assert_ne!(stale_repair.repair_id(), active_repair.repair_id());
        assert_eq!(active_slot, BrowserReplayLocatorSlot::StepWait);
        assert!(active_cursor == BrowserReplayRepairResumeCursor::StepWait);

        let stale_controller = fixture
            .bridge
            .bind(fixture.owner.clone(), Duration::from_secs(1));
        assert!(stale_controller
            .request_replay_repair_apply(
                &fixture.coordinator,
                &stale_repair,
                true,
                true,
                BrowserInvocationContext::agent(
                    "reject stale executor repair",
                    BrowserRisk::Normal,
                )
                .unwrap(),
            )
            .await
            .is_err());
        assert_eq!(
            fixture
                .coordinator
                .status(&fixture.instance)
                .unwrap()
                .status,
            BrowserReplayStatus::PausedLocatorRepair
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(25), fixture.inbox.recv())
                .await
                .is_err()
        );

        assert_eq!(active_repair.replay(), &fixture.instance);
        fixture.coordinator.cancel(&fixture.instance).unwrap();
        let terminal = fixture.run.await.unwrap().unwrap();
        assert_eq!(terminal.status, BrowserReplayStatus::Cancelled);
        for id in [
            &first_snapshot.id,
            &first_screenshot.id,
            &second_snapshot.id,
            &second_screenshot.id,
        ] {
            assert!(matches!(
                fixture.store.handle(&fixture.owner, id),
                Err(BrowserError::MissingResource { .. })
            ));
        }
        drop(fixture.store);
        let _ = std::fs::remove_dir_all(fixture.resource_root);
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn secret_type_locator_failure_enters_primary_action_repair_without_exposing_secret() {
        const SECRET: &str = "EXECUTOR_REPAIR_SECRET_9F2C";
        let owner = BrowserWorkspaceKey::new("executor-repair", "secret-locator").unwrap();
        let resource_root = std::env::temp_dir().join(format!(
            "devmanager-executor-secret-locator-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&resource_root);
        let store = BrowserResourceStore::open(
            &resource_root,
            BrowserResourceLimits {
                max_temporary_count: 0,
                max_temporary_bytes: 1024 * 1024,
                max_resource_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        let recipe = BrowserRecipeV1 {
            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
            id: "secret-locator".to_string(),
            name: "Secret locator".to_string(),
            description: "Secret locator failure fixture".to_string(),
            start_url: "https://example.test/repair".to_string(),
            viewport: BrowserRecipeViewport::default(),
            inputs: vec![BrowserRecipeInput {
                name: "credential".to_string(),
                kind: BrowserRecipeInputKind::Secret,
                default_value: None,
            }],
            steps: vec![BrowserRecipeStep {
                id: "type-secret".to_string(),
                action: BrowserRecipeAction::Type {
                    locator: locator("credential"),
                    value: BrowserRecipeValue::Input {
                        name: "credential".to_string(),
                    },
                },
                wait: None,
                assertions: Vec::new(),
            }],
        };
        let coordinator = BrowserReplayCoordinator::default();
        let started = coordinator
            .start(
                owner.clone(),
                compile_browser_replay(&recipe, Vec::new()).unwrap(),
            )
            .unwrap();
        let instance = started.instance.clone();
        let (mut prompt, _) = BrowserReplaySecretPromptVault::install(
            instance.clone(),
            started.projection.unresolved_secret_inputs.clone(),
        )
        .unwrap();
        prompt.edit(&instance, "credential", SECRET).unwrap();
        let (submission, _) = prompt.submit(&instance).unwrap();
        coordinator.submit_secrets(&instance, submission).unwrap();
        let (bridge, mut inbox) = browser_command_channel(8);
        let controller = bridge.bind(owner.clone(), Duration::from_secs(1));
        let run_store = store.clone();
        let run_coordinator = coordinator.clone();
        let run_instance = instance.clone();
        let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .canonicalize()
            .unwrap();
        let run = tokio::spawn(async move {
            execute_browser_replay(
                &controller,
                &run_coordinator,
                &run_instance,
                started.execution,
                BrowserInvocationActor::Agent,
                &run_store,
                &project_root,
            )
            .await
        });
        respond_test_setup(&mut inbox).await;
        let secret = next_test_request(&mut inbox, "secret type locator failure").await;
        assert!(secret.validate_secret_sidecar().unwrap().is_some());
        let safe = format!("{:?}\n{:?}", secret.command(), secret.context());
        assert!(!safe.contains(SECRET));
        secret.respond(Err(BrowserError::LocatorNotFound {
            target: BrowserLocatorFailureTarget::Primary,
        }));
        let state = next_test_request(&mut inbox, "secret repair workspace state").await;
        assert_eq!(state.command(), &BrowserCommand::WorkspaceState);
        state.respond(Ok(BrowserResponse::WorkspaceState {
            snapshot: fresh_repair_snapshot(),
        }));
        let snapshot = next_test_request(&mut inbox, "secret repair snapshot").await;
        assert!(snapshot
            .validate_repair_retention_sidecar()
            .unwrap()
            .is_some());
        let (_repair, slot, cursor) = coordinator
            .active_locator_repair_capture_for_test(&instance)
            .unwrap();
        assert_eq!(slot, BrowserReplayLocatorSlot::PrimaryAction);
        assert!(cursor == BrowserReplayRepairResumeCursor::Action);
        coordinator.cancel(&instance).unwrap();
        snapshot.respond(Err(BrowserError::Interrupted));
        let terminal = run.await.unwrap().unwrap();
        assert_eq!(terminal.status, BrowserReplayStatus::Cancelled);
        assert!(!format!("{terminal:?}").contains(SECRET));
        drop(store);
        let _ = std::fs::remove_dir_all(resource_root);
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn typed_locator_failure_captures_exact_evidence_then_waits_paused_until_cancelled() {
        let root = std::env::temp_dir().join(format!(
            "devmanager-executor-secure-pause-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let store = BrowserResourceStore::open(
            &root,
            BrowserResourceLimits {
                max_temporary_count: 0,
                max_temporary_bytes: 1024 * 1024,
                max_resource_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .canonicalize()
            .unwrap();
        assert_ne!(store.root(), project_root.as_path());

        let recipe = BrowserRecipeV1 {
            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
            id: "secure-pause".to_string(),
            name: "Secure pause".to_string(),
            description: "Exact evidence capture fixture".to_string(),
            start_url: "https://example.test/repair".to_string(),
            viewport: BrowserRecipeViewport::default(),
            inputs: Vec::new(),
            steps: vec![BrowserRecipeStep {
                id: "click-submit".to_string(),
                action: BrowserRecipeAction::Click {
                    locator: locator("submit"),
                },
                wait: None,
                assertions: Vec::new(),
            }],
        };
        let coordinator = BrowserReplayCoordinator::default();
        let started = coordinator
            .start(
                test_workspace(),
                compile_browser_replay(&recipe, Vec::new()).unwrap(),
            )
            .unwrap();
        let instance = started.instance.clone();
        let observed_instance = instance.clone();
        let (bridge, mut inbox) = browser_command_channel(8);
        let controller = bridge.bind(test_workspace(), Duration::from_secs(1));
        let run_store = store.clone();
        let run_coordinator = coordinator.clone();
        let run = tokio::spawn(async move {
            execute_browser_replay(
                &controller,
                &run_coordinator,
                &instance,
                started.execution,
                BrowserInvocationActor::Agent,
                &run_store,
                &project_root,
            )
            .await
        });

        respond_test_setup(&mut inbox).await;
        let action = next_test_request(&mut inbox, "failing action").await;
        assert_eq!(
            action.command(),
            &BrowserCommand::Act {
                tab_id: "repair-tab".to_string(),
                actions: vec![BrowserAction::Click {
                    target: BrowserActionTarget {
                        locator: BrowserLocator {
                            test_id: Some("submit".to_string()),
                            ..BrowserLocator::default()
                        },
                        ..BrowserActionTarget::default()
                    },
                }],
            }
        );
        action.respond(Err(BrowserError::LocatorNotFound {
            target: BrowserLocatorFailureTarget::Primary,
        }));

        let state = next_test_request(&mut inbox, "fresh workspace state after failure").await;
        assert_eq!(state.command(), &BrowserCommand::WorkspaceState);
        state.respond(Ok(BrowserResponse::WorkspaceState {
            snapshot: match workspace_response(
                "repair-tab",
                "https://example.test/repair",
                BrowserViewport::default(),
                9,
            ) {
                BrowserResponse::Workspace { mutation } => mutation.snapshot,
                _ => unreachable!(),
            },
        }));

        let snapshot = next_test_request(&mut inbox, "secure repair snapshot").await;
        assert_eq!(
            snapshot.command(),
            &BrowserCommand::Snapshot {
                tab_id: "repair-tab".to_string(),
            }
        );
        assert!(snapshot
            .validate_repair_retention_sidecar()
            .unwrap()
            .is_some());
        assert!(!snapshot.records_workflow_recipe_action());
        let snapshot_handle = snapshot
            .retain_repair_resource(
                &store,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            )
            .unwrap();
        snapshot.respond(Ok(BrowserResponse::Snapshot {
            summary: BrowserSnapshotSummary {
                tab_id: "repair-tab".to_string(),
                url: "https://example.test/repair".to_string(),
                revision: crate::browser::BrowserRevision(9),
                element_count: 1,
            },
            resource: snapshot_handle.clone(),
        }));

        let screenshot = next_test_request(&mut inbox, "secure repair screenshot").await;
        assert_eq!(
            screenshot.command(),
            &BrowserCommand::Screenshot {
                tab_id: "repair-tab".to_string(),
                mode: BrowserScreenshotMode::Viewport,
            }
        );
        let screenshot_handle = screenshot
            .retain_repair_resource(
                &store,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                b"png",
            )
            .unwrap();
        screenshot.respond(Ok(BrowserResponse::Screenshot {
            resource: screenshot_handle.clone(),
        }));

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if coordinator.status(&observed_instance).unwrap().status
                    == BrowserReplayStatus::PausedLocatorRepair
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("executor publishes a stable locator-repair pause");
        assert!(
            tokio::time::timeout(Duration::from_millis(25), inbox.recv())
                .await
                .is_err()
        );
        let resources = store.list(&test_workspace()).unwrap();
        assert_eq!(resources.len(), 2);
        assert_eq!(resources[0].kind, BrowserResourceKind::ReplayRepairSnapshot);
        assert_eq!(
            resources[1].kind,
            BrowserResourceKind::ReplayRepairScreenshot
        );
        assert!(
            !run.is_finished(),
            "executor must remain alive while paused"
        );

        coordinator.cancel(&observed_instance).unwrap();
        let terminal = run.await.unwrap().unwrap();
        assert_eq!(terminal.status, BrowserReplayStatus::Cancelled);
        for id in [&snapshot_handle.id, &screenshot_handle.id] {
            assert!(matches!(
                store.handle(&test_workspace(), id),
                Err(BrowserError::MissingResource { .. })
            ));
        }
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
