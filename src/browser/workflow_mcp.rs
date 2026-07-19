use super::{
    compile_browser_replay, execute_browser_replay, list_recipes, load_recipe,
    verified_authenticated_local_project_root, BrowserController, BrowserElementRef, BrowserError,
    BrowserInvocationActor, BrowserInvocationContext, BrowserRecipeInputKind, BrowserRecipeV1,
    BrowserRecipeViewport, BrowserReplayAdmission, BrowserReplayCoordinator, BrowserReplayError,
    BrowserReplayFailureCode, BrowserReplayProjection, BrowserReplayPublicInput,
    BrowserReplayRepairCandidate, BrowserReplayRepairProjection, BrowserResourceHandle,
    BrowserResourceKind, BrowserResourceStore, BrowserWorkspaceKey,
};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) enum BrowserWorkflowServiceError {
    InvalidRequest,
    InvalidRecipe,
    MissingRecipe,
    RepositoryUnavailable,
    ResourceUnavailable,
    StaleReference,
    InvalidState,
    InvalidProjectRoot,
    Browser(BrowserError),
}

#[derive(Clone)]
pub(crate) struct BrowserWorkflowMcpService {
    controller: BrowserController,
    coordinator: BrowserReplayCoordinator,
    resource_store: BrowserResourceStore,
    authenticated_local_project_root: PathBuf,
}

pub(crate) struct BrowserWorkflowReplayStatus {
    pub replay: BrowserReplayProjection,
    pub repair: Option<super::BrowserReplayRepairProjection>,
}

pub(crate) struct BrowserWorkflowRepairApplyResult {
    pub replay: BrowserReplayProjection,
    pub repair: BrowserReplayRepairProjection,
    pub recipe_written: bool,
}

impl BrowserWorkflowMcpService {
    pub(crate) fn new(
        controller: BrowserController,
        resource_store: BrowserResourceStore,
        authenticated_local_project_root: PathBuf,
    ) -> Self {
        let coordinator = controller.replay_coordinator();
        Self {
            controller,
            coordinator,
            resource_store,
            authenticated_local_project_root,
        }
    }

    pub(crate) fn verify_authenticated_root(&self) -> Result<PathBuf, BrowserWorkflowServiceError> {
        verified_authenticated_local_project_root(&self.authenticated_local_project_root)
            .map_err(|_| BrowserWorkflowServiceError::InvalidProjectRoot)
    }

    pub(crate) fn list(
        &self,
    ) -> Result<Vec<BrowserWorkflowRecipeSummary>, BrowserWorkflowServiceError> {
        list_browser_workflow_recipes(self.verify_authenticated_root()?)
            .map_err(map_repository_error)
    }

    pub(crate) fn get(
        &self,
        recipe_id: &str,
    ) -> Result<BrowserWorkflowRecipeGet, BrowserWorkflowServiceError> {
        let root = self.verify_authenticated_root()?;
        let recipe = load_recipe(&root, recipe_id).map_err(map_load_error)?;
        let mut bytes = serde_json::to_vec_pretty(&recipe)
            .map_err(|_| BrowserWorkflowServiceError::InvalidRecipe)?;
        bytes.push(b'\n');
        let resource = self
            .resource_store
            .put(
                self.controller.workspace_key(),
                BrowserResourceKind::WorkflowRecipe,
                "application/json",
                bytes,
                false,
            )
            .map_err(|_| BrowserWorkflowServiceError::ResourceUnavailable)?;
        Ok(BrowserWorkflowRecipeGet {
            recipe: BrowserWorkflowRecipeSummary::from(&recipe),
            resource,
        })
    }

    pub(crate) fn replay(
        &self,
        recipe_id: &str,
        inputs: Vec<BrowserReplayPublicInput>,
        admission: BrowserReplayAdmission,
    ) -> Result<BrowserReplayProjection, BrowserWorkflowServiceError> {
        let root = self.verify_authenticated_root()?;
        let recipe = load_recipe(&root, recipe_id).map_err(map_load_error)?;
        let plan = compile_browser_replay(&recipe, inputs).map_err(map_compile_error)?;
        let start = self
            .controller
            .replace_replay_if_admitted(admission, plan)
            .map_err(BrowserWorkflowServiceError::Browser)?
            .map_err(map_replay_error)?;
        let projection = start.projection.clone();
        let controller = self.controller.clone();
        let coordinator = self.coordinator.clone();
        let resource_store = self.resource_store.clone();
        let instance = start.instance.clone();
        tokio::spawn(async move {
            let result = execute_browser_replay(
                &controller,
                &coordinator,
                &instance,
                start.execution,
                BrowserInvocationActor::Agent,
                &resource_store,
                &root,
            )
            .await;
            if result.is_err()
                && coordinator
                    .status(&instance)
                    .is_ok_and(|projection| !replay_status_is_terminal(projection.status))
            {
                let _ =
                    coordinator.fail_nonterminal(&instance, BrowserReplayFailureCode::StepFailed);
            }
        });
        Ok(projection)
    }

    pub(crate) fn capture_replay_admission(
        &self,
    ) -> Result<BrowserReplayAdmission, BrowserWorkflowServiceError> {
        self.controller
            .capture_replay_admission()
            .map_err(BrowserWorkflowServiceError::Browser)
    }

    pub(crate) fn status(
        &self,
        replay_instance_id: u64,
    ) -> Result<BrowserWorkflowReplayStatus, BrowserWorkflowServiceError> {
        let instance = self
            .coordinator
            .exact_instance(self.controller.workspace_key(), replay_instance_id)
            .map_err(map_replay_error)?;
        let replay = self
            .coordinator
            .status(&instance)
            .map_err(map_replay_error)?;
        let repair = self
            .coordinator
            .active_state(self.controller.workspace_key())
            .filter(|active| active.instance.id() == replay_instance_id)
            .and_then(|active| active.repair);
        Ok(BrowserWorkflowReplayStatus { replay, repair })
    }

    pub(crate) fn cancel(
        &self,
        replay_instance_id: u64,
    ) -> Result<BrowserReplayProjection, BrowserWorkflowServiceError> {
        let instance = self
            .coordinator
            .exact_instance(self.controller.workspace_key(), replay_instance_id)
            .map_err(map_replay_error)?;
        self.coordinator.cancel(&instance).map_err(map_replay_error)
    }

    pub(crate) async fn repair_preview(
        &self,
        replay_instance_id: u64,
        repair_id: u64,
        element_ref: BrowserElementRef,
        context: BrowserInvocationContext,
    ) -> Result<BrowserWorkflowReplayStatus, BrowserWorkflowServiceError> {
        self.verify_authenticated_root()?;
        let repair = self
            .coordinator
            .exact_repair(
                self.controller.workspace_key(),
                replay_instance_id,
                repair_id,
            )
            .map_err(map_repair_lookup_error)?;
        let current = self
            .coordinator
            .locator_repair_status(&repair)
            .map_err(map_repair_lookup_error)?;
        if element_ref.revision != current.revision {
            return Err(BrowserWorkflowServiceError::Browser(
                BrowserError::StaleReference {
                    expected: current.revision,
                    actual: element_ref.revision,
                },
            ));
        }
        let candidate = BrowserReplayRepairCandidate::new(element_ref);
        self.controller
            .request_replay_repair_preview_with_context(
                &self.coordinator,
                &repair,
                candidate,
                context,
            )
            .await
            .map_err(BrowserWorkflowServiceError::Browser)?;
        self.status(replay_instance_id)
    }

    pub(crate) async fn repair_apply(
        &self,
        replay_instance_id: u64,
        repair_id: u64,
        confirmed: bool,
        resume: bool,
        context: BrowserInvocationContext,
    ) -> Result<BrowserWorkflowRepairApplyResult, BrowserWorkflowServiceError> {
        self.verify_authenticated_root()?;
        let repair = self
            .coordinator
            .exact_repair(
                self.controller.workspace_key(),
                replay_instance_id,
                repair_id,
            )
            .map_err(map_repair_lookup_error)?;
        let commit = self
            .controller
            .request_replay_repair_apply(&self.coordinator, &repair, confirmed, resume, context)
            .await
            .map_err(BrowserWorkflowServiceError::Browser)?;
        Ok(BrowserWorkflowRepairApplyResult {
            replay: commit.replay,
            repair: commit.repair,
            recipe_written: commit.recipe_written,
        })
    }
}

fn replay_status_is_terminal(status: super::BrowserReplayStatus) -> bool {
    matches!(
        status,
        super::BrowserReplayStatus::Completed
            | super::BrowserReplayStatus::Failed
            | super::BrowserReplayStatus::Cancelled
    )
}

fn map_load_error(error: BrowserError) -> BrowserWorkflowServiceError {
    match error {
        BrowserError::MissingFile { .. } => BrowserWorkflowServiceError::MissingRecipe,
        BrowserError::InvalidRecipe { .. } | BrowserError::UnsupportedRecipeVersion { .. } => {
            BrowserWorkflowServiceError::InvalidRecipe
        }
        _ => BrowserWorkflowServiceError::RepositoryUnavailable,
    }
}

fn map_repository_error(error: BrowserError) -> BrowserWorkflowServiceError {
    match error {
        BrowserError::InvalidRecipe { .. } | BrowserError::UnsupportedRecipeVersion { .. } => {
            BrowserWorkflowServiceError::InvalidRecipe
        }
        _ => BrowserWorkflowServiceError::RepositoryUnavailable,
    }
}

fn map_compile_error(error: BrowserReplayError) -> BrowserWorkflowServiceError {
    match error {
        BrowserReplayError::InvalidRecipe => BrowserWorkflowServiceError::InvalidRecipe,
        _ => BrowserWorkflowServiceError::InvalidRequest,
    }
}

fn map_replay_error(error: BrowserReplayError) -> BrowserWorkflowServiceError {
    match error {
        BrowserReplayError::StaleInstance => BrowserWorkflowServiceError::StaleReference,
        _ => BrowserWorkflowServiceError::InvalidState,
    }
}

fn map_repair_lookup_error(error: BrowserReplayError) -> BrowserWorkflowServiceError {
    match error {
        BrowserReplayError::StaleInstance | BrowserReplayError::InvalidRepairEvidence => {
            BrowserWorkflowServiceError::StaleReference
        }
        _ => BrowserWorkflowServiceError::InvalidState,
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserWorkflowRecipeInputSummary {
    pub name: String,
    pub kind: BrowserRecipeInputKind,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserWorkflowRecipeSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub start_url: String,
    pub viewport: BrowserRecipeViewport,
    pub inputs: Vec<BrowserWorkflowRecipeInputSummary>,
    pub step_count: usize,
}

impl From<&BrowserRecipeV1> for BrowserWorkflowRecipeSummary {
    fn from(recipe: &BrowserRecipeV1) -> Self {
        Self {
            id: recipe.id.clone(),
            name: recipe.name.clone(),
            description: recipe.description.clone(),
            start_url: recipe.start_url.clone(),
            viewport: recipe.viewport,
            inputs: recipe
                .inputs
                .iter()
                .map(|input| BrowserWorkflowRecipeInputSummary {
                    name: input.name.clone(),
                    kind: input.kind,
                })
                .collect(),
            step_count: recipe.steps.len(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserWorkflowRecipeGet {
    pub recipe: BrowserWorkflowRecipeSummary,
    pub resource: BrowserResourceHandle,
}

pub fn list_browser_workflow_recipes(
    authenticated_local_project_root: impl AsRef<Path>,
) -> Result<Vec<BrowserWorkflowRecipeSummary>, BrowserError> {
    let root =
        verified_authenticated_local_project_root(authenticated_local_project_root.as_ref())?;
    list_recipes(root).map(|recipes| {
        recipes
            .iter()
            .map(BrowserWorkflowRecipeSummary::from)
            .collect()
    })
}

pub fn get_browser_workflow_recipe(
    authenticated_local_project_root: impl AsRef<Path>,
    owner: &BrowserWorkspaceKey,
    resource_store: &BrowserResourceStore,
    recipe_id: &str,
) -> Result<BrowserWorkflowRecipeGet, BrowserError> {
    let root =
        verified_authenticated_local_project_root(authenticated_local_project_root.as_ref())?;
    let recipe = load_recipe(root, recipe_id)?;
    let mut bytes =
        serde_json::to_vec_pretty(&recipe).map_err(|_| BrowserError::InvalidRecipe {
            message: "browser workflow recipe could not be serialized".to_string(),
        })?;
    bytes.push(b'\n');
    let resource = resource_store.put(
        owner,
        BrowserResourceKind::WorkflowRecipe,
        "application/json",
        bytes,
        false,
    )?;
    Ok(BrowserWorkflowRecipeGet {
        recipe: BrowserWorkflowRecipeSummary::from(&recipe),
        resource,
    })
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use crate::browser::{
        browser_command_channel, save_recipe, BrowserCommand, BrowserElementRef, BrowserHostEvent,
        BrowserInvocationContext, BrowserLocator, BrowserRecipeAction, BrowserRecipeInput,
        BrowserRecipeInputKind, BrowserRecipeLocator, BrowserRecipeStep, BrowserRecipeValue,
        BrowserReplayLocatorSlot, BrowserReplayRepairPhase, BrowserReplayRepairResumeCursor,
        BrowserReplayStatus, BrowserResourceKind, BrowserResourceLimits, BrowserResponse,
        BrowserRevision, BrowserRisk, BrowserUserInputKind, BROWSER_RECIPE_SCHEMA_VERSION,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    fn repair_recipe() -> BrowserRecipeV1 {
        BrowserRecipeV1 {
            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
            id: "workflow-service-repair".to_string(),
            name: "Workflow service repair".to_string(),
            description: "Exercise exact MCP repair orchestration".to_string(),
            start_url: "https://example.test".to_string(),
            viewport: BrowserRecipeViewport {
                width: 1280,
                height: 720,
                scale_percent: 100,
            },
            inputs: Vec::new(),
            steps: vec![BrowserRecipeStep {
                id: "click-target".to_string(),
                action: BrowserRecipeAction::Click {
                    locator: BrowserRecipeLocator {
                        test_id: Some("old-target".to_string()),
                        ..BrowserRecipeLocator::default()
                    },
                },
                wait: None,
                assertions: Vec::new(),
            }],
        }
    }

    fn secret_replay_recipe() -> BrowserRecipeV1 {
        BrowserRecipeV1 {
            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
            id: "route-admission-secret-replay".to_string(),
            name: "Route admission secret replay".to_string(),
            description: "Reject replay creation after route loss".to_string(),
            start_url: "https://example.test".to_string(),
            viewport: BrowserRecipeViewport::default(),
            inputs: vec![BrowserRecipeInput {
                name: "password".to_string(),
                kind: BrowserRecipeInputKind::Secret,
                default_value: None,
            }],
            steps: vec![BrowserRecipeStep {
                id: "type-password".to_string(),
                action: BrowserRecipeAction::Type {
                    locator: BrowserRecipeLocator {
                        test_id: Some("password".to_string()),
                        ..BrowserRecipeLocator::default()
                    },
                    value: BrowserRecipeValue::Input {
                        name: "password".to_string(),
                    },
                },
                wait: None,
                assertions: Vec::new(),
            }],
        }
    }

    fn candidate(revision: u64) -> BrowserElementRef {
        BrowserElementRef {
            revision: BrowserRevision(revision),
            locator: BrowserLocator {
                test_id: Some("replacement".to_string()),
                ..BrowserLocator::default()
            },
            backend_node_id: Some(77),
        }
    }

    #[tokio::test]
    async fn exact_repair_preview_and_apply_preserve_context_approval_and_resume() {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "devmanager-workflow-service-repair-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let recipe = repair_recipe();
        let recipe_path = save_recipe(&root, &recipe).unwrap();
        let store = BrowserResourceStore::open(
            root.join("resources"),
            BrowserResourceLimits {
                max_temporary_count: 0,
                max_temporary_bytes: 1024 * 1024,
                max_resource_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        let workspace = BrowserWorkspaceKey::new("workflow-repair", "conversation").unwrap();
        let (bridge, mut inbox) = browser_command_channel(16);
        let coordinator = bridge.replay_coordinator();
        let controller = bridge.bind(workspace.clone(), Duration::from_secs(2));
        let service =
            BrowserWorkflowMcpService::new(controller.clone(), store.clone(), root.clone());
        let plan = compile_browser_replay(&recipe, Vec::new()).unwrap();
        let started = coordinator.start(workspace, plan).unwrap();
        started.execution.bind_canonical_recipe_root(&root).unwrap();
        coordinator.begin(&started.instance).unwrap();
        let repair = coordinator
            .reserve_locator_repair_capture(
                &started.instance,
                &store,
                0,
                BrowserReplayLocatorSlot::PrimaryAction,
                "tab-a",
                BrowserRevision(9),
                BrowserReplayRepairResumeCursor::Action,
            )
            .unwrap();
        let snapshot = coordinator
            .retain_locator_repair_evidence_for_test(
                &repair,
                BrowserResourceKind::ReplayRepairSnapshot,
                "application/json",
                b"{}",
            )
            .unwrap();
        let screenshot = coordinator
            .retain_locator_repair_evidence_for_test(
                &repair,
                BrowserResourceKind::ReplayRepairScreenshot,
                "image/png",
                b"png",
            )
            .unwrap();
        coordinator
            .publish_locator_repair(&repair, &snapshot, &screenshot)
            .unwrap();

        assert!(matches!(
            service
                .repair_preview(
                    started.instance.id() + 100,
                    repair.repair_id(),
                    candidate(9),
                    BrowserInvocationContext::agent("stale replay preview", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await,
            Err(BrowserWorkflowServiceError::StaleReference)
        ));
        assert!(matches!(
            service
                .repair_preview(
                    started.instance.id(),
                    repair.repair_id(),
                    candidate(10),
                    BrowserInvocationContext::agent("drifted preview", BrowserRisk::Normal)
                        .unwrap(),
                )
                .await,
            Err(BrowserWorkflowServiceError::Browser(
                BrowserError::StaleReference { .. }
            ))
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), inbox.recv())
                .await
                .is_err()
        );

        let preview_service = service.clone();
        let replay_id = started.instance.id();
        let repair_id = repair.repair_id();
        let preview = tokio::spawn(async move {
            preview_service
                .repair_preview(
                    replay_id,
                    repair_id,
                    candidate(9),
                    BrowserInvocationContext::agent(
                        "preview the MCP-selected replacement",
                        BrowserRisk::PermissionChange,
                    )
                    .unwrap(),
                )
                .await
        });
        let preview_request = inbox.recv().await.expect("preview request");
        assert_eq!(
            preview_request.context().intent,
            "preview the MCP-selected replacement"
        );
        assert_eq!(
            preview_request.context().declared_risk,
            BrowserRisk::PermissionChange
        );
        let preview_authority = preview_request
            .repair_preview_highlight_authority()
            .unwrap()
            .clone();
        assert!(preview_authority.acknowledge_for_test());
        preview_request.respond(Ok(BrowserResponse::Acknowledged));
        let previewed = preview.await.unwrap().unwrap();
        assert_eq!(
            previewed.repair.unwrap().phase,
            BrowserReplayRepairPhase::Previewed
        );

        let before = std::fs::read(&recipe_path).unwrap();
        let denied_service = service.clone();
        let denied = tokio::spawn(async move {
            denied_service
                .repair_apply(
                    replay_id,
                    repair_id,
                    true,
                    true,
                    BrowserInvocationContext::agent(
                        "apply the exact MCP repair",
                        BrowserRisk::Normal,
                    )
                    .unwrap(),
                )
                .await
        });
        let denied_request = inbox.recv().await.expect("denied apply request");
        assert!(matches!(
            denied_request.command(),
            BrowserCommand::RepairValidate { .. }
        ));
        assert_eq!(
            denied_request.context().intent,
            "apply the exact MCP repair"
        );
        assert_eq!(
            denied_request
                .repair_apply_authority()
                .unwrap()
                .effective_risk(),
            BrowserRisk::Destructive
        );
        denied_request.respond(Err(BrowserError::BlockedPermission {
            permission: "Destructive".to_string(),
        }));
        assert!(matches!(
            denied.await.unwrap(),
            Err(BrowserWorkflowServiceError::Browser(
                BrowserError::BlockedPermission { .. }
            ))
        ));
        assert_eq!(std::fs::read(&recipe_path).unwrap(), before);

        let apply_service = service.clone();
        let applied = tokio::spawn(async move {
            apply_service
                .repair_apply(
                    replay_id,
                    repair_id,
                    true,
                    true,
                    BrowserInvocationContext::agent(
                        "apply the exact MCP repair",
                        BrowserRisk::Normal,
                    )
                    .unwrap(),
                )
                .await
        });
        for _ in 0..2 {
            let request = inbox.recv().await.expect("apply validation request");
            let authority = request.repair_apply_authority().unwrap().clone();
            assert!(authority.acknowledge_for_test());
            request.respond(Ok(BrowserResponse::Acknowledged));
        }
        let applied = applied.await.unwrap().unwrap();
        assert!(applied.recipe_written);
        assert_eq!(
            applied.replay.status,
            super::super::BrowserReplayStatus::Running
        );
        let repaired = load_recipe(&root, "workflow-service-repair").unwrap();
        assert!(matches!(
            &repaired.steps[0].action,
            BrowserRecipeAction::Click { locator }
                if locator.test_id.as_deref() == Some("replacement")
        ));

        coordinator.cancel(&started.instance).unwrap();
        drop(applied);
        drop(service);
        drop(controller);
        drop(bridge);
        drop(inbox);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn later_user_input_during_validation_stales_replay_admission_but_older_input_does_not() {
        let workspace =
            BrowserWorkspaceKey::new("workflow-input-admission", "conversation").unwrap();
        let (bridge, mut inbox) = browser_command_channel(4);
        let controller = bridge.bind(workspace.clone(), Duration::from_secs(1));
        let plan = || compile_browser_replay(&secret_replay_recipe(), Vec::new()).unwrap();

        let admission = controller.capture_replay_admission().unwrap();
        let validating_controller = controller.clone();
        let validation = tokio::spawn(async move {
            validating_controller
                .request_with_context(
                    BrowserCommand::WorkspaceState,
                    BrowserInvocationContext::agent(
                        "validate replay before later input",
                        BrowserRisk::Normal,
                    )
                    .unwrap(),
                )
                .await
        });
        let validation_request = inbox.recv().await.expect("workspace validation request");
        let later_input = BrowserHostEvent::user_input(
            workspace.clone(),
            "tab-a",
            BrowserUserInputKind::Keyboard,
        );
        validation_request.respond(Ok(BrowserResponse::WorkspaceState {
            snapshot: super::super::BrowserWorkspaceSnapshot {
                pane_open: true,
                ..super::super::BrowserWorkspaceSnapshot::default()
            },
        }));
        assert!(matches!(
            validation.await.unwrap(),
            Ok(BrowserResponse::WorkspaceState { .. })
        ));
        bridge.observe_host_event(&later_input);

        assert!(matches!(
            controller.replace_replay_if_admitted(admission, plan()),
            Err(BrowserError::Interrupted)
        ));
        assert!(bridge
            .replay_coordinator()
            .active_state(&workspace)
            .is_none());

        let older_input =
            BrowserHostEvent::user_input(workspace.clone(), "tab-a", BrowserUserInputKind::Pointer);
        let newer_admission = controller.capture_replay_admission().unwrap();
        bridge.observe_host_event(&older_input);
        let started = controller
            .replace_replay_if_admitted(newer_admission, plan())
            .unwrap()
            .expect("input older than admission must not cancel newer replay");
        assert_eq!(
            started.projection.status,
            BrowserReplayStatus::NeedsUserSecret
        );
        bridge.interrupt_workspace(&workspace);
    }

    #[tokio::test]
    async fn route_loss_after_successful_validation_rejects_replay_before_secret_residue() {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "devmanager-workflow-route-admission-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let recipe = secret_replay_recipe();
        save_recipe(&root, &recipe).unwrap();
        let store = BrowserResourceStore::open(
            root.join("resources"),
            BrowserResourceLimits {
                max_temporary_count: 0,
                max_temporary_bytes: 1024 * 1024,
                max_resource_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        let workspace =
            BrowserWorkspaceKey::new("workflow-route-admission", "conversation").unwrap();
        let (bridge, mut inbox) = browser_command_channel(4);
        let controller = bridge.bind(workspace.clone(), Duration::from_secs(1));
        let service = BrowserWorkflowMcpService::new(controller.clone(), store, root.clone());
        let admission = service.capture_replay_admission().unwrap();

        let validating_controller = controller.clone();
        let validation = tokio::spawn(async move {
            validating_controller
                .request_with_context(
                    BrowserCommand::WorkspaceState,
                    BrowserInvocationContext::agent(
                        "validate workflow replay route",
                        BrowserRisk::Normal,
                    )
                    .unwrap(),
                )
                .await
        });
        let request = inbox.recv().await.expect("workspace validation request");
        request.respond(Ok(BrowserResponse::WorkspaceState {
            snapshot: super::super::BrowserWorkspaceSnapshot {
                pane_open: true,
                ..super::super::BrowserWorkspaceSnapshot::default()
            },
        }));
        assert!(matches!(
            validation.await.unwrap(),
            Ok(BrowserResponse::WorkspaceState { .. })
        ));

        bridge.interrupt_workspace(&workspace);
        assert!(matches!(
            service.replay(&recipe.id, Vec::new(), admission),
            Err(BrowserWorkflowServiceError::Browser(
                BrowserError::Interrupted
            ))
        ));
        assert!(
            bridge
                .replay_coordinator()
                .active_state(&workspace)
                .is_none(),
            "route loss must reject before a hidden NeedsUserSecret replay exists"
        );

        let admitted = service.capture_replay_admission().unwrap();
        let started = controller
            .replace_replay_if_admitted(
                admitted,
                compile_browser_replay(&recipe, Vec::new()).unwrap(),
            )
            .unwrap()
            .expect("replay start wins the opposite linearization order");
        assert_eq!(
            started.projection.status,
            BrowserReplayStatus::NeedsUserSecret
        );
        let coordinator = bridge.replay_coordinator();
        bridge.interrupt_workspace(&workspace);
        assert_eq!(
            coordinator.status(&started.instance).unwrap().status,
            BrowserReplayStatus::Cancelled
        );
        assert!(coordinator.active_state(&workspace).is_none());

        drop(service);
        drop(controller);
        drop(bridge);
        drop(inbox);
        std::fs::remove_dir_all(root).unwrap();
    }
}
