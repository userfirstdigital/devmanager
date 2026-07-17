use super::{
    effective_browser_risk, verified_authenticated_local_project_root, BrowserError,
    BrowserRecordingInputSummary, BrowserRecordingOperation, BrowserRecordingResult,
    BrowserRecordingStatus, BrowserResourceKind, BrowserResourceStore, BrowserRisk,
    BrowserWorkflowCoordinator, BrowserWorkspaceKey,
};
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserRecordingReviewResource<'a> {
    version: u8,
    recording_id: u64,
    recipe: &'a super::BrowserRecipeV1,
}

pub fn browser_recording_status_result(
    coordinator: &BrowserWorkflowCoordinator,
    workspace_key: &BrowserWorkspaceKey,
    operation: BrowserRecordingOperation,
) -> BrowserRecordingResult {
    coordinator.with_recorder(|recorder| match recorder.status(workspace_key) {
        BrowserRecordingStatus::Inactive => {
            empty_result(operation, BrowserRecordingStatus::Inactive)
        }
        BrowserRecordingStatus::Recording => {
            let mut result = empty_result(operation, BrowserRecordingStatus::Recording);
            result.recording_id = recorder
                .active_instance(workspace_key)
                .map(|instance| instance.id());
            result
        }
        BrowserRecordingStatus::Review => {
            let Some(review) = recorder.review_for_workspace(workspace_key) else {
                return empty_result(operation, BrowserRecordingStatus::Inactive);
            };
            result_from_review(operation, &review, None)
        }
    })
}

pub fn browser_recording_review_result(
    coordinator: &BrowserWorkflowCoordinator,
    workspace_key: &BrowserWorkspaceKey,
    operation: BrowserRecordingOperation,
    resources: &BrowserResourceStore,
) -> Result<BrowserRecordingResult, BrowserError> {
    coordinator.with_recorder(|recorder| {
        let review = recorder
            .review_for_workspace(workspace_key)
            .ok_or_else(stale_recording_review)?;
        let recipe = recorder.recipe_for_save(review.instance())?;
        let mut bytes = serde_json::to_vec_pretty(&BrowserRecordingReviewResource {
            version: 1,
            recording_id: review.instance().id(),
            recipe: &recipe,
        })
        .map_err(|_| recording_resource_unavailable())?;
        bytes.push(b'\n');
        let resource = resources
            .put(
                workspace_key,
                BrowserResourceKind::WorkflowReview,
                "application/json",
                bytes,
                false,
            )
            .map_err(|_| recording_resource_unavailable())?;
        Ok(result_from_review(operation, &review, Some(resource)))
    })
}

pub fn effective_browser_recording_risk(
    declared_risk: BrowserRisk,
    operation: BrowserRecordingOperation,
    overwrites_existing: bool,
) -> BrowserRisk {
    let storage_risk = match operation {
        BrowserRecordingOperation::Discard => Some(BrowserRisk::Destructive),
        BrowserRecordingOperation::Save if overwrites_existing => Some(BrowserRisk::Destructive),
        _ => None,
    };
    effective_browser_risk(declared_risk, None, storage_risk)
}

pub fn browser_recording_save_would_overwrite(
    coordinator: &BrowserWorkflowCoordinator,
    workspace_key: &BrowserWorkspaceKey,
    expected_instance_id: u64,
    authenticated_local_project_root: impl AsRef<Path>,
) -> Result<bool, BrowserError> {
    let project_root =
        verified_authenticated_local_project_root(authenticated_local_project_root.as_ref())?;
    coordinator.with_recorder(|recorder| {
        let review = recorder
            .review_for_workspace(workspace_key)
            .ok_or_else(stale_recording_review)?;
        if review.instance().id() != expected_instance_id {
            return Err(stale_recording_review());
        }
        let recipe = recorder.recipe_for_save(review.instance())?;
        super::recipes::recipe_exists(&project_root, &recipe.id)
            .map_err(|_| recording_storage_error("inspect"))
    })
}

pub fn save_browser_recording_review(
    coordinator: &BrowserWorkflowCoordinator,
    workspace_key: &BrowserWorkspaceKey,
    expected_instance_id: u64,
    authenticated_local_project_root: impl AsRef<Path>,
    allow_overwrite: bool,
) -> Result<BrowserRecordingResult, BrowserError> {
    let project_root =
        verified_authenticated_local_project_root(authenticated_local_project_root.as_ref())?;
    coordinator.with_recorder(|recorder| {
        let review = recorder
            .review_for_workspace(workspace_key)
            .ok_or_else(stale_recording_review)?;
        if review.instance().id() != expected_instance_id {
            return Err(stale_recording_review());
        }
        let instance = review.instance().clone();
        let recipe = recorder.recipe_for_save(&instance)?;
        let mut result = result_from_review(BrowserRecordingOperation::Save, &review, None);
        let (_path, overwrote_existing) = super::recipes::save_recipe_with_overwrite_policy(
            &project_root,
            &recipe,
            allow_overwrite,
        )
        .map_err(|_| recording_storage_error("save"))?;
        recorder
            .discard(&instance)
            .map_err(|_| stale_recording_review())?;
        result.status = BrowserRecordingStatus::Inactive;
        result.overwrote_existing = Some(overwrote_existing);
        Ok(result)
    })
}

pub fn discard_browser_recording(
    coordinator: &BrowserWorkflowCoordinator,
    workspace_key: &BrowserWorkspaceKey,
    expected_instance_id: u64,
) -> Result<BrowserRecordingResult, BrowserError> {
    let instance = coordinator
        .current_instance(workspace_key)
        .ok_or_else(stale_recording_review)?;
    if instance.id() != expected_instance_id {
        return Err(stale_recording_review());
    }
    coordinator
        .discard(&instance)
        .map_err(|_| stale_recording_review())?;
    let mut result = empty_result(
        BrowserRecordingOperation::Discard,
        BrowserRecordingStatus::Inactive,
    );
    result.recording_id = Some(instance.id());
    Ok(result)
}

fn result_from_review(
    operation: BrowserRecordingOperation,
    review: &super::BrowserRecordingReview,
    resource: Option<super::BrowserResourceHandle>,
) -> BrowserRecordingResult {
    let recipe = review.recipe();
    BrowserRecordingResult {
        operation,
        status: BrowserRecordingStatus::Review,
        recording_id: Some(review.instance().id()),
        recipe_id: Some(recipe.id.clone()),
        step_count: recipe.steps.len(),
        inputs: recipe
            .inputs
            .iter()
            .map(|input| BrowserRecordingInputSummary {
                name: input.name.clone(),
                kind: input.kind,
            })
            .collect(),
        valid: recipe.validate().is_ok(),
        resource,
        overwrote_existing: None,
    }
}

fn empty_result(
    operation: BrowserRecordingOperation,
    status: BrowserRecordingStatus,
) -> BrowserRecordingResult {
    BrowserRecordingResult {
        operation,
        status,
        recording_id: None,
        recipe_id: None,
        step_count: 0,
        inputs: Vec::new(),
        valid: false,
        resource: None,
        overwrote_existing: None,
    }
}

fn stale_recording_review() -> BrowserError {
    BrowserError::InvalidRecipe {
        message: "recording review instance is not active".to_string(),
    }
}

pub(crate) fn recording_resource_unavailable() -> BrowserError {
    BrowserError::RecordingResourceUnavailable
}

fn recording_storage_error(operation: &str) -> BrowserError {
    BrowserError::InvalidRecipe {
        message: format!(
            "recording {operation} requires an available authenticated local project root"
        ),
    }
}
