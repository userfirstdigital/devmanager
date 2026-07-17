use super::commands::verified_authenticated_local_project_root;
use super::{
    BrowserCommand, BrowserController, BrowserError, BrowserInvocationActor,
    BrowserInvocationContext, BrowserReplayCoordinator, BrowserReplayError,
    BrowserReplayExecutionHandle, BrowserReplayFailureCode, BrowserReplayInstance,
    BrowserReplayProjection, BrowserResponse, BrowserRisk, BrowserTabSnapshot, BrowserViewport,
    BrowserWorkspaceSnapshot,
};
use std::path::Path;

pub async fn execute_browser_replay(
    controller: &BrowserController,
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
    execution: BrowserReplayExecutionHandle,
    actor: BrowserInvocationActor,
    authenticated_local_project_root: &Path,
) -> Result<BrowserReplayProjection, BrowserReplayError> {
    if !execution.same_instance(instance) {
        return Err(BrowserReplayError::StaleInstance);
    }
    if let Some(cancelled) = cancelled_projection(&execution, coordinator, instance)? {
        return Ok(cancelled);
    }
    coordinator.begin(instance)?;
    if verified_authenticated_local_project_root(authenticated_local_project_root).is_err() {
        return fail_step(coordinator, instance);
    }

    let plan = execution.plan();
    let create = request(
        controller,
        actor,
        "replay setup create tab",
        BrowserCommand::CreateTab { url: None },
    )
    .await;
    if let Some(cancelled) = cancelled_projection(&execution, coordinator, instance)? {
        return Ok(cancelled);
    }
    let BrowserResponse::Workspace { mutation } = (match create {
        Ok(response) => response,
        Err(_) => return fail_step(coordinator, instance),
    }) else {
        return fail_step(coordinator, instance);
    };
    let Some(tab_id) = exact_selected_tab(&mutation.snapshot).map(str::to_string) else {
        return fail_step(coordinator, instance);
    };

    let viewport = BrowserViewport::from(plan.viewport());
    let response = request(
        controller,
        actor,
        "replay setup apply viewport",
        BrowserCommand::UpdateViewport {
            tab_id: tab_id.clone(),
            viewport: viewport.clone(),
        },
    )
    .await;
    if let Some(cancelled) = cancelled_projection(&execution, coordinator, instance)? {
        return Ok(cancelled);
    }
    let Ok(BrowserResponse::Workspace { mutation }) = response else {
        return fail_step(coordinator, instance);
    };
    if !snapshot_proves_selected_tab(&mutation.snapshot, &tab_id, Some(&viewport), None) {
        return fail_step(coordinator, instance);
    }

    let response = request(
        controller,
        actor,
        "replay setup navigate start",
        BrowserCommand::Navigate {
            tab_id: tab_id.clone(),
            url: plan.start_url().to_string(),
        },
    )
    .await;
    if let Some(cancelled) = cancelled_projection(&execution, coordinator, instance)? {
        return Ok(cancelled);
    }
    let Ok(BrowserResponse::Workspace { mutation }) = response else {
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

    for (step_index, step) in plan.steps().iter().enumerate() {
        let command = match &step.action {
            super::BrowserRecipeAction::Reload => BrowserCommand::Reload {
                tab_id: tab_id.clone(),
            },
            _ => return fail_step(coordinator, instance),
        };
        let response = request(controller, actor, "replay step action", command).await;
        if let Some(cancelled) = cancelled_projection(&execution, coordinator, instance)? {
            return Ok(cancelled);
        }
        if !matches!(response, Ok(BrowserResponse::Acknowledged)) {
            return fail_step(coordinator, instance);
        }
        if let Some(cancelled) = cancelled_projection(&execution, coordinator, instance)? {
            return Ok(cancelled);
        }
        coordinator.advance_step(instance, step_index)?;
    }
    coordinator.complete(instance)
}

async fn request(
    controller: &BrowserController,
    actor: BrowserInvocationActor,
    intent: &'static str,
    command: BrowserCommand,
) -> Result<BrowserResponse, BrowserError> {
    let context = BrowserInvocationContext::for_actor(actor, intent, BrowserRisk::Normal)?;
    controller.request_with_context(command, context).await
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

fn fail_step(
    coordinator: &BrowserReplayCoordinator,
    instance: &BrowserReplayInstance,
) -> Result<BrowserReplayProjection, BrowserReplayError> {
    coordinator.fail(instance, BrowserReplayFailureCode::StepFailed)
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
