//! Dependency-safe Task 5.10 quality-gate contracts.
//!
//! This module projects isolated preview/fake-host fixtures. It does not
//! launch the installed app, attach a production host, or claim pixel
//! approval for the missing canonical Task Cockpit shell.

use crate::client::action::{
    catalog, task_create_command, task_rename_command, ActionArgumentSchema, ActionRequest,
    ServiceControlArguments, TaskCreateArguments, TaskRenameArguments, ACTION_HOST_ACTIONS,
    ACTION_HOST_STATUS, ACTION_SERVICE_RESTART, ACTION_SERVICE_START, ACTION_SERVICE_STOP,
    ACTION_TASK_LIST, ACTION_TASK_SHOW,
};
use crate::client::model::{ClientModel, ClientModelBuilder, ClientModelError};
use crate::domain::command::ServiceControlAction;
use crate::domain::event::{DomainEvent, Event};
use crate::domain::id::{
    ClientId, CommandId, EnvironmentId, EventId, ProjectId, SnapshotId, TaskId,
};
use crate::domain::snapshot::{
    EventPage, PageLimits, SnapshotPage, SnapshotSection, MAX_SNAPSHOT_PAGE_ENCODED_BYTES,
    MAX_SNAPSHOT_PAGE_ITEMS,
};
use crate::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
    TaskLifecycle, WorkspaceRef,
};
use crate::host::{HostCleanupProgress, HostCleanupWorker};
use crate::ui::components::empty_state::EmptyState;
use crate::ui::components::error_boundary::{ErrorBoundary, SafeErrorCode, SafeErrorProjection};
use crate::ui::components::interaction::{AccessibilityMetadata, FocusCoordinator, FocusEpoch};
use crate::ui::components::status_light::StatusLight;
use crate::ui::components::text_field::TextField;
use crate::ui::components::Button;
use crate::ui::preview::{is_sensitive_path, is_within, PreviewPathPolicy, MAX_FIXTURE_BYTES};
use crate::ui::tokens::{
    theme, Density, PhysicalDensityMetrics, Scale, StatusMeaning, ThemeMode, ThemeTokens,
};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

pub const QUALITY_SCHEMA: &str = "devmanager.ui.quality/v1";
pub const INBOX_VIRTUALIZATION_LIMIT: usize = 5_000;
pub const TIMELINE_VIRTUALIZATION_LIMIT: usize = 20_000;
pub const VIRTUALIZATION_WINDOW: usize = 24;
pub const MAX_QUALITY_CONTROLS: usize = 32;
pub const MAX_QUALITY_STRING_SCALARS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogInput {
    None,
    TaskId(TaskId),
    Create(TaskCreateArguments),
    Rename {
        args: TaskRenameArguments,
        expected_revision: u64,
    },
    Service(ServiceControlArguments),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisualGate {
    RequiresCanonicalShell {
        reason: String,
        missing: Vec<String>,
    },
}

impl VisualGate {
    pub fn approved_for_pixel_inspection(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaleContract {
    pub percent: u16,
    pub scale: Scale,
    pub physical: PhysicalDensityMetrics,
    pub reduced_motion_ms: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualRow {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualizationProjection {
    pub total: usize,
    pub projected_count: usize,
    pub work_units: usize,
    pub cancelled: bool,
    pub rows: Vec<VirtualRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualityControlView {
    pub id: String,
    pub kind: QualityControlKind,
    pub accessibility: AccessibilityMetadata,
    pub status_meaning: Option<StatusMeaning>,
    pub interactive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityControlKind {
    Button,
    Status,
    TextField,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualitySamples {
    pub long_text: String,
    pub unicode: String,
    pub empty_title: String,
    pub empty_description: String,
    pub error_title: String,
    pub error_message: String,
    pub partial_title: String,
    pub partial_available: Vec<String>,
    pub partial_missing: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualityDocument {
    fixture: QualityFixture,
    visual_gate: VisualGate,
    source_path: PathBuf,
}

impl QualityDocument {
    pub fn schema(&self) -> &str {
        &self.fixture.schema
    }

    pub fn surface_kind(&self) -> &str {
        &self.fixture.surface
    }

    pub fn visual_gate(&self) -> &VisualGate {
        &self.visual_gate
    }

    pub fn fixture_id(&self) -> &str {
        &self.fixture.id
    }
}

pub struct QualitySurface {
    fixture_id: String,
    visual_gate: VisualGate,
    theme_mode: ThemeMode,
    density: Density,
    scale: Scale,
    samples: Option<QualitySamples>,
    virtualization: Option<QualityVirtualization>,
    views: Vec<QualityControlView>,
    empty_state: Option<EmptyState>,
    error_boundary: Option<ErrorBoundary>,
}

impl QualitySurface {
    fn from_document(
        document: QualityDocument,
        focus: &mut FocusCoordinator,
    ) -> Result<Self, QualityError> {
        let QualityDocument {
            fixture,
            visual_gate,
            source_path,
        } = document;
        admit_collection_len(fixture.controls.len(), MAX_QUALITY_CONTROLS, "controls")?;
        let epoch = focus.current();
        let theme_mode = fixture.theme.into();
        let density = fixture.density.into();
        let scale = scale_from_percent(fixture.scale, &source_path)?;

        let mut views = Vec::with_capacity(fixture.controls.len());
        for control in &fixture.controls {
            views.push(build_control(control, epoch)?);
        }

        let (empty_state, error_boundary) = if let Some(samples) = &fixture.samples {
            validate_samples(samples, &source_path)?;
            let empty = EmptyState::new(&samples.empty_title, &samples.empty_description)
                .map_err(|error| QualityError::InvalidControl(error.to_string()))?;
            let error = ErrorBoundary::new(
                SafeErrorProjection::new(
                    SafeErrorCode::HostUnavailable,
                    &samples.error_title,
                    &samples.error_message,
                )
                .map_err(|error| QualityError::InvalidControl(error.to_string()))?,
            )
            .map_err(|error| QualityError::InvalidControl(error.to_string()))?;
            (Some(empty), Some(error))
        } else {
            (None, None)
        };

        if let Some(virtualization) = &fixture.virtualization {
            if virtualization.inbox_rows > INBOX_VIRTUALIZATION_LIMIT {
                return Err(QualityError::VirtualizationBudgetExceeded {
                    kind: "inbox".into(),
                    total: virtualization.inbox_rows,
                    limit: INBOX_VIRTUALIZATION_LIMIT,
                });
            }
            if virtualization.timeline_items > TIMELINE_VIRTUALIZATION_LIMIT {
                return Err(QualityError::VirtualizationBudgetExceeded {
                    kind: "timeline".into(),
                    total: virtualization.timeline_items,
                    limit: TIMELINE_VIRTUALIZATION_LIMIT,
                });
            }
            if virtualization.window_len == 0 || virtualization.window_len > VIRTUALIZATION_WINDOW {
                return Err(QualityError::MalformedFixture {
                    path: source_path,
                    message: "virtualization window is missing or oversized".into(),
                });
            }
        }

        Ok(Self {
            fixture_id: fixture.id,
            visual_gate,
            theme_mode,
            density,
            scale,
            samples: fixture.samples,
            virtualization: fixture.virtualization,
            views,
            empty_state,
            error_boundary,
        })
    }

    pub fn fixture_id(&self) -> &str {
        &self.fixture_id
    }

    pub fn visual_gate(&self) -> &VisualGate {
        &self.visual_gate
    }

    pub fn theme_tokens(&self) -> ThemeTokens {
        theme(self.theme_mode, self.density, self.scale)
    }

    pub fn control(&self, id: &str) -> Option<&QualityControlView> {
        self.views.iter().find(|view| view.id == id)
    }

    pub fn samples(&self) -> Option<&QualitySamples> {
        self.samples.as_ref()
    }

    pub fn empty_state(&self) -> Option<&EmptyState> {
        self.empty_state.as_ref()
    }

    pub fn error_boundary(&self) -> Option<&ErrorBoundary> {
        self.error_boundary.as_ref()
    }

    pub fn host_started_evidence(&self) -> Result<bool, QualityError> {
        Err(QualityError::Hold {
            missing: "host start evidence requires HostLock or PreviewInitReport".into(),
        })
    }

    pub fn pixel_readback(&self) -> Result<(), QualityError> {
        Err(QualityError::Hold {
            missing: "gpui_png_readback".into(),
        })
    }

    pub fn shutdown_evidence(&self) -> Result<HostCleanupProgress, QualityError> {
        Err(QualityError::Hold {
            missing:
                "HostCleanupWorker requires CommandBus; private shutdown bool is not host lifecycle evidence"
                    .into(),
        })
    }

    pub fn anatomy_evidence(&self) -> Result<(), QualityError> {
        Err(QualityError::Hold {
            missing: "desktop_mobile_anatomy".into(),
        })
    }

    pub fn performance_evidence(&self) -> Result<(), QualityError> {
        Err(QualityError::Hold {
            missing: "performance_budgets".into(),
        })
    }

    pub fn partial_projection(&self) -> Result<(), QualityError> {
        Err(QualityError::Hold {
            missing: "parent PartialState component".into(),
        })
    }

    pub fn scale_contracts(&self) -> Vec<ScaleContract> {
        [
            Scale::Scale100,
            Scale::Scale125,
            Scale::Scale150,
            Scale::Scale200,
        ]
        .into_iter()
        .map(|scale| {
            let tokens = theme(self.theme_mode, self.density, scale);
            ScaleContract {
                percent: scale.percent(),
                scale,
                physical: tokens.density.physical(),
                reduced_motion_ms: tokens.density.motion.reduced_motion_ms,
            }
        })
        .collect()
    }

    pub fn project_inbox_window(
        &self,
        model: &ClientModel,
        start: usize,
    ) -> Result<VirtualizationProjection, QualityError> {
        let virtualization = self
            .virtualization
            .as_ref()
            .ok_or(QualityError::MissingSection("virtualization"))?;
        let total = model.tasks().len();
        if total > INBOX_VIRTUALIZATION_LIMIT {
            return Err(QualityError::VirtualizationBudgetExceeded {
                kind: "inbox".into(),
                total,
                limit: INBOX_VIRTUALIZATION_LIMIT,
            });
        }
        let window_len = virtualization.window_len;
        if window_len == 0 || window_len > VIRTUALIZATION_WINDOW {
            return Err(QualityError::InvalidControl(
                "virtualization window is missing or oversized".into(),
            ));
        }
        admit_collection_len(window_len, VIRTUALIZATION_WINDOW, "inbox-window")?;
        let mut rows = Vec::with_capacity(window_len);
        for (task_id, snapshot) in model.tasks().iter().skip(start).take(window_len) {
            rows.push(VirtualRow {
                id: task_id.to_string(),
                title: snapshot.task.title.clone(),
            });
        }
        Ok(VirtualizationProjection {
            total,
            projected_count: rows.len(),
            work_units: rows.len(),
            cancelled: false,
            rows,
        })
    }

    pub fn project_timeline_window(
        &self,
        _start: usize,
    ) -> Result<VirtualizationProjection, QualityError> {
        Err(QualityError::Hold {
            missing: "timeline 20k projection requires a semantic DomainEvent journal and Task 5.7 renderer; synthesizing timeline-NNNN ids is forbidden".into(),
        })
    }

    pub fn bind_host_cleanup_worker(
        &self,
        _worker: HostCleanupWorker,
    ) -> Result<HostCleanupProgress, QualityError> {
        Err(QualityError::Hold {
            missing:
                "HostCleanupWorker requires CommandBus; isolated preview cannot run host cleanup"
                    .into(),
        })
    }
}

pub fn load_quality_fixture(
    path: impl AsRef<Path>,
    policy: &PreviewPathPolicy,
) -> Result<QualityDocument, QualityError> {
    let path = approved_quality_path(path.as_ref(), policy)?;
    let bytes = read_quality_bytes(&path)?;
    let fixture: QualityFixture =
        serde_json::from_slice(&bytes).map_err(|error| QualityError::MalformedFixture {
            path: path.clone(),
            message: error.to_string(),
        })?;
    fixture.validate(&path)?;
    let visual_gate = VisualGate::RequiresCanonicalShell {
        reason: fixture.visual_gate.reason.clone(),
        missing: fixture.visual_gate.missing.clone(),
    };
    Ok(QualityDocument {
        fixture,
        visual_gate,
        source_path: path,
    })
}

pub fn load_quality_surface(
    path: impl AsRef<Path>,
    policy: &PreviewPathPolicy,
    focus: &mut FocusCoordinator,
) -> Result<QualitySurface, QualityError> {
    QualitySurface::from_document(load_quality_fixture(path, policy)?, focus)
}

pub fn request_from_catalog(id: &str, input: CatalogInput) -> Result<ActionRequest, QualityError> {
    let descriptor = catalog().iter().find(|descriptor| descriptor.id == id);
    let Some(descriptor) = descriptor else {
        return Err(QualityError::InvalidControl(format!(
            "unknown catalog action {id}"
        )));
    };
    match descriptor.argument_schema {
        ActionArgumentSchema::None => match (id, input) {
            (ACTION_HOST_ACTIONS, CatalogInput::None) => Ok(ActionRequest::HostActions),
            (ACTION_HOST_STATUS, CatalogInput::None) => Ok(ActionRequest::HostStatus),
            (ACTION_TASK_LIST, CatalogInput::None) => Ok(ActionRequest::TaskList),
            (other, _) => Err(QualityError::InvalidControl(format!(
                "catalog action {other} requires CatalogInput::None"
            ))),
        },
        ActionArgumentSchema::TaskId => match input {
            CatalogInput::TaskId(task_id) => Ok(ActionRequest::TaskShow { task_id }),
            _ => Err(QualityError::InvalidControl(format!(
                "{ACTION_TASK_SHOW} requires TaskId"
            ))),
        },
        ActionArgumentSchema::TaskCreateV1 => match input {
            CatalogInput::Create(args) => {
                task_create_command(
                    CommandId::new(),
                    ClientId::new(),
                    1_725_000_000_100,
                    args.clone(),
                )
                .map_err(|error| QualityError::InvalidControl(error.to_string()))?;
                Ok(ActionRequest::TaskCreate(args))
            }
            _ => Err(QualityError::InvalidControl(
                "task.create requires TaskCreateArguments".into(),
            )),
        },
        ActionArgumentSchema::TaskRenameV1 => match input {
            CatalogInput::Rename {
                args,
                expected_revision,
            } => {
                task_rename_command(
                    CommandId::new(),
                    ClientId::new(),
                    1,
                    expected_revision,
                    args.clone(),
                )
                .map_err(|error| QualityError::InvalidControl(error.to_string()))?;
                Ok(ActionRequest::TaskRename(args))
            }
            _ => Err(QualityError::InvalidControl(
                "task.rename requires TaskRenameArguments".into(),
            )),
        },
        ActionArgumentSchema::TaskCreateV2
        | ActionArgumentSchema::ProviderInputV1
        | ActionArgumentSchema::PromptMetadataPageV1
        | ActionArgumentSchema::PromptVersionPageV1
        | ActionArgumentSchema::PromptDiffV1
        | ActionArgumentSchema::PromptChainPageV1
        | ActionArgumentSchema::TaskCockpitV1 => Err(QualityError::InvalidControl(format!(
            "catalog action {id} requires an unsupported argument schema"
        ))),
        ActionArgumentSchema::ServiceControlV1 => match input {
            CatalogInput::Service(arguments) => {
                let action = match id {
                    ACTION_SERVICE_START => ServiceControlAction::Start,
                    ACTION_SERVICE_STOP => ServiceControlAction::Stop,
                    ACTION_SERVICE_RESTART => ServiceControlAction::Restart,
                    _ => {
                        return Err(QualityError::InvalidControl(
                            "unknown service control action".into(),
                        ))
                    }
                };
                Ok(ActionRequest::ServiceControl { action, arguments })
            }
            _ => Err(QualityError::InvalidControl(
                "service control requires ServiceControlArguments".into(),
            )),
        },
    }
}

pub fn admit_collection_len(len: usize, limit: usize, kind: &str) -> Result<(), QualityError> {
    if len > limit {
        return Err(QualityError::CollectionBoundExceeded {
            kind: kind.to_string(),
            len,
            limit,
        });
    }
    Ok(())
}

pub fn assemble_replayed_inbox(count: usize) -> Result<ClientModel, QualityError> {
    if count == 0 || count > INBOX_VIRTUALIZATION_LIMIT {
        return Err(QualityError::VirtualizationBudgetExceeded {
            kind: "inbox".into(),
            total: count,
            limit: INBOX_VIRTUALIZATION_LIMIT,
        });
    }
    let limits = PageLimits::new(MAX_SNAPSHOT_PAGE_ITEMS, MAX_SNAPSHOT_PAGE_ENCODED_BYTES)
        .map_err(|error| QualityError::InvalidControl(error.to_string()))?;
    let snapshot = SnapshotId::from_bytes(quality_id_bytes(0x01, 0)).map_err(id_err)?;
    let environment = EnvironmentId::from_bytes(quality_id_bytes(0x10, 0)).map_err(id_err)?;
    let project = ProjectId::from_bytes(quality_id_bytes(0x11, 0)).map_err(id_err)?;
    let mut builder = ClientModelBuilder::new();
    for section in [
        SnapshotSection::Tasks,
        SnapshotSection::AgentSessions,
        SnapshotSection::Artifacts,
        SnapshotSection::Resources,
        SnapshotSection::Operations,
    ] {
        builder
            .ingest_page(bounded_snapshot_page(
                SnapshotPage {
                    snapshot_id: snapshot,
                    through_sequence: 0,
                    section,
                    after_item: None,
                    items: Vec::new(),
                    encoded_bytes: 1,
                    next_cursor: None,
                },
                &limits,
            )?)
            .map_err(model_err)?;
    }
    let mut model = builder.finish().map_err(model_err)?;
    let page_size = limits.max_items as usize;
    let through_sequence = count as u64;
    let mut seq = 0_u64;
    while seq < through_sequence {
        let remaining = through_sequence - seq;
        let take = remaining.min(page_size as u64) as usize;
        admit_collection_len(take, page_size, "replay-page-events")?;
        let after_sequence = seq;
        let mut events = Vec::with_capacity(take);
        for _ in 0..take {
            seq += 1;
            let index = u16::try_from(seq).map_err(|_| QualityError::CollectionBoundExceeded {
                kind: "replay-sequence".into(),
                len: seq as usize,
                limit: u16::MAX as usize,
            })?;
            let task_id = TaskId::from_bytes(quality_id_bytes(0x21, index)).map_err(id_err)?;
            let event_id = EventId::from_bytes(quality_id_bytes(0x52, index)).map_err(id_err)?;
            events.push(DomainEvent {
                id: event_id,
                task_id: Some(task_id),
                sequence: seq,
                task_revision: Some(1),
                occurred_at_ms: seq as i64,
                payload: Event::TaskCreated {
                    task: TaskFacts {
                        id: task_id,
                        environment_id: environment,
                        title: format!("Inbox task {index}"),
                        description: None,
                        project_id: project,
                        workspace: WorkspaceRef::Main,
                        assignment: TaskAssignment::LocalOwner,
                        lifecycle: TaskLifecycle::Open,
                        action_epoch: 0,
                        revision: 1,
                        created_at_ms: 1_725_000_000_000,
                    },
                    connectivity: TaskConnectivity::Connected,
                    attention: TaskAttention::None,
                    activity: TaskActivity::Idle,
                    review_readiness: ReviewReadiness::NotReady,
                },
            });
        }
        let next_cursor = (seq < through_sequence).then(|| seq.to_be_bytes().to_vec());
        model
            .apply_replay_page(&EventPage {
                after_sequence,
                through_sequence,
                events,
                next_cursor,
            })
            .map_err(model_err)?;
    }
    Ok(model)
}

fn quality_id_bytes(kind: u8, index: u16) -> [u8; 16] {
    [
        0x01,
        0x8f,
        0x60,
        0xb0,
        0x9c,
        0x1a,
        0x70,
        0x01,
        0x80,
        kind,
        0x00,
        0x00,
        0x00,
        0x00,
        (index >> 8) as u8,
        index as u8,
    ]
}

fn bounded_snapshot_page(
    mut page: SnapshotPage,
    limits: &PageLimits,
) -> Result<SnapshotPage, QualityError> {
    admit_collection_len(
        page.items.len(),
        limits.max_items as usize,
        "snapshot-page-items",
    )?;
    page.encoded_bytes = 1;
    let encoded = rmp_serde::to_vec_named(&page)
        .map_err(|error| QualityError::InvalidControl(error.to_string()))?;
    if encoded.is_empty() || encoded.len() as u32 > limits.max_encoded_bytes {
        return Err(QualityError::CollectionBoundExceeded {
            kind: "snapshot-page-bytes".into(),
            len: encoded.len(),
            limit: limits.max_encoded_bytes as usize,
        });
    }
    page.encoded_bytes = encoded.len() as u32;
    Ok(page)
}

fn model_err(error: ClientModelError) -> QualityError {
    QualityError::InvalidControl(error.to_string())
}

fn id_err(error: crate::domain::id::IdError) -> QualityError {
    QualityError::InvalidControl(error.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QualityFixture {
    schema: String,
    id: String,
    title: String,
    surface: String,
    visual_gate: VisualGateFixture,
    #[serde(default)]
    theme: QualityTheme,
    #[serde(default)]
    density: QualityDensity,
    #[serde(default = "default_scale")]
    scale: u16,
    #[serde(default)]
    scales: Vec<u16>,
    #[serde(default)]
    controls: Vec<QualityControlFixture>,
    #[serde(default)]
    samples: Option<QualitySamples>,
    #[serde(default)]
    virtualization: Option<QualityVirtualization>,
    #[serde(default)]
    keyboard: Option<QualityKeyboard>,
}

impl QualityFixture {
    fn validate(&self, path: &Path) -> Result<(), QualityError> {
        if self.schema != QUALITY_SCHEMA {
            return Err(QualityError::UnsupportedSchema {
                path: path.to_path_buf(),
                schema: self.schema.clone(),
            });
        }
        if self.id.trim().is_empty()
            || self.id.chars().count() > 128
            || self.title.trim().is_empty()
            || self.title.chars().count() > 256
        {
            return Err(QualityError::MalformedFixture {
                path: path.to_path_buf(),
                message: "fixture id or title is empty or oversized".into(),
            });
        }
        if self.surface != "isolated_preview" {
            return Err(QualityError::MalformedFixture {
                path: path.to_path_buf(),
                message: "quality fixtures must declare the isolated_preview surface".into(),
            });
        }
        if self.visual_gate.kind != "requires_canonical_shell" {
            return Err(QualityError::MalformedFixture {
                path: path.to_path_buf(),
                message: "quality fixtures must not claim pixel approval".into(),
            });
        }
        if self.visual_gate.reason.trim().is_empty()
            || self.visual_gate.reason.chars().count() > MAX_QUALITY_STRING_SCALARS
            || self.visual_gate.missing.is_empty()
        {
            return Err(QualityError::MalformedFixture {
                path: path.to_path_buf(),
                message: "canonical-shell visual gate must name the missing union".into(),
            });
        }
        admit_collection_len(
            self.visual_gate.missing.len(),
            MAX_QUALITY_CONTROLS,
            "visual-gate-missing",
        )?;
        for missing in &self.visual_gate.missing {
            if missing.trim().is_empty() || missing.chars().count() > MAX_QUALITY_STRING_SCALARS {
                return Err(QualityError::MalformedFixture {
                    path: path.to_path_buf(),
                    message: "visual-gate missing identifiers must be bounded and non-empty".into(),
                });
            }
        }
        if !self.scales.is_empty() && self.scales != [100, 125, 150, 200] {
            return Err(QualityError::MalformedFixture {
                path: path.to_path_buf(),
                message: "scale contracts must cover 100, 125, 150, and 200 percent".into(),
            });
        }
        let _ = scale_from_percent(self.scale, path)?;
        admit_collection_len(self.controls.len(), MAX_QUALITY_CONTROLS, "controls")?;
        let mut seen = Vec::with_capacity(self.controls.len());
        for control in &self.controls {
            if control.id.trim().is_empty()
                || control.id.chars().count() > MAX_QUALITY_STRING_SCALARS
                || control.name.trim().is_empty()
                || control.name.chars().count() > MAX_QUALITY_STRING_SCALARS
                || control.description.as_ref().is_some_and(|description| {
                    description.chars().count() > MAX_QUALITY_STRING_SCALARS
                })
                || control
                    .action
                    .as_ref()
                    .is_some_and(|action| action.chars().count() > MAX_QUALITY_STRING_SCALARS)
            {
                return Err(QualityError::InvalidControl(
                    "control text must be non-empty and bounded".into(),
                ));
            }
            if seen.contains(&control.id) {
                return Err(QualityError::InvalidControl(format!(
                    "duplicate control id {}",
                    control.id
                )));
            }
            seen.push(control.id.clone());
        }
        if let Some(keyboard) = &self.keyboard {
            if keyboard.order.is_empty() {
                return Err(QualityError::MalformedFixture {
                    path: path.to_path_buf(),
                    message: "keyboard order must not be empty when declared".into(),
                });
            }
            admit_collection_len(keyboard.order.len(), MAX_QUALITY_CONTROLS, "keyboard-order")?;
            let mut seen_keyboard = Vec::with_capacity(keyboard.order.len());
            for id in &keyboard.order {
                if !seen.contains(id) {
                    return Err(QualityError::MalformedFixture {
                        path: path.to_path_buf(),
                        message: format!("keyboard order references unknown control {id}"),
                    });
                }
                if seen_keyboard.contains(id) {
                    return Err(QualityError::MalformedFixture {
                        path: path.to_path_buf(),
                        message: format!("keyboard order repeats control {id}"),
                    });
                }
                seen_keyboard.push(id.clone());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VisualGateFixture {
    kind: String,
    reason: String,
    missing: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QualityTheme {
    #[default]
    Dark,
    Light,
}

impl From<QualityTheme> for ThemeMode {
    fn from(value: QualityTheme) -> Self {
        match value {
            QualityTheme::Dark => Self::Dark,
            QualityTheme::Light => Self::Light,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QualityDensity {
    Compact,
    #[default]
    Comfortable,
}

impl From<QualityDensity> for Density {
    fn from(value: QualityDensity) -> Self {
        match value {
            QualityDensity::Compact => Self::Compact,
            QualityDensity::Comfortable => Self::Comfortable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QualityControlFixture {
    id: String,
    kind: QualityControlKind,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    meaning: Option<QualityStatusMeaning>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QualityStatusMeaning {
    External,
    Attention,
    Success,
    Warning,
    Destructive,
    Inactive,
}

impl From<QualityStatusMeaning> for StatusMeaning {
    fn from(value: QualityStatusMeaning) -> Self {
        match value {
            QualityStatusMeaning::External => Self::External,
            QualityStatusMeaning::Attention => Self::Attention,
            QualityStatusMeaning::Success => Self::Success,
            QualityStatusMeaning::Warning => Self::Warning,
            QualityStatusMeaning::Destructive => Self::Destructive,
            QualityStatusMeaning::Inactive => Self::Inactive,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QualityVirtualization {
    inbox_rows: usize,
    timeline_items: usize,
    #[serde(default = "default_window")]
    window_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QualityKeyboard {
    order: Vec<String>,
}

fn default_scale() -> u16 {
    100
}

fn default_window() -> usize {
    VIRTUALIZATION_WINDOW
}

fn scale_from_percent(percent: u16, path: &Path) -> Result<Scale, QualityError> {
    match percent {
        100 => Ok(Scale::Scale100),
        125 => Ok(Scale::Scale125),
        150 => Ok(Scale::Scale150),
        200 => Ok(Scale::Scale200),
        _ => Err(QualityError::MalformedFixture {
            path: path.to_path_buf(),
            message: format!("unsupported scale {percent}"),
        }),
    }
}

fn validate_samples(samples: &QualitySamples, path: &Path) -> Result<(), QualityError> {
    for (name, value) in [
        ("long_text", &samples.long_text),
        ("unicode", &samples.unicode),
        ("empty_title", &samples.empty_title),
        ("empty_description", &samples.empty_description),
        ("error_title", &samples.error_title),
        ("error_message", &samples.error_message),
        ("partial_title", &samples.partial_title),
    ] {
        if value.chars().count() > MAX_QUALITY_STRING_SCALARS {
            return Err(QualityError::CollectionBoundExceeded {
                kind: name.into(),
                len: value.chars().count(),
                limit: MAX_QUALITY_STRING_SCALARS,
            });
        }
    }
    admit_collection_len(
        samples.partial_available.len(),
        MAX_QUALITY_CONTROLS,
        "partial_available",
    )?;
    admit_collection_len(
        samples.partial_missing.len(),
        MAX_QUALITY_CONTROLS,
        "partial_missing",
    )?;
    if samples.long_text.chars().count() <= 256 {
        return Err(QualityError::MalformedFixture {
            path: path.to_path_buf(),
            message: "long_text must exercise overflow wrapping".into(),
        });
    }
    if !samples
        .unicode
        .chars()
        .any(|character| !character.is_ascii())
    {
        return Err(QualityError::MalformedFixture {
            path: path.to_path_buf(),
            message: "unicode sample must contain non-ASCII text".into(),
        });
    }
    for (name, value) in [
        ("empty_title", &samples.empty_title),
        ("empty_description", &samples.empty_description),
        ("error_title", &samples.error_title),
        ("error_message", &samples.error_message),
        ("partial_title", &samples.partial_title),
    ] {
        if value.trim().is_empty() {
            return Err(QualityError::MalformedFixture {
                path: path.to_path_buf(),
                message: format!("{name} must not be blank"),
            });
        }
    }
    if samples.partial_available.is_empty() || samples.partial_missing.is_empty() {
        return Err(QualityError::MalformedFixture {
            path: path.to_path_buf(),
            message: "partial states must declare available and missing fields".into(),
        });
    }
    Ok(())
}

fn build_control(
    control: &QualityControlFixture,
    epoch: FocusEpoch,
) -> Result<QualityControlView, QualityError> {
    match control.kind {
        QualityControlKind::Button => {
            let action = request_from_catalog(
                control.action.as_deref().ok_or_else(|| {
                    QualityError::InvalidControl(format!(
                        "button {} requires an action",
                        control.id
                    ))
                })?,
                CatalogInput::None,
            )?;
            let mut button = Button::new(&control.name, action)
                .map_err(|error| QualityError::InvalidControl(error.to_string()))?;
            button.set_focus_epoch(epoch);
            if let Some(description) = &control.description {
                button
                    .set_accessibility_description(description)
                    .map_err(|error| QualityError::InvalidControl(error.to_string()))?;
            }
            Ok(QualityControlView {
                id: control.id.clone(),
                kind: control.kind.clone(),
                accessibility: button.accessibility().clone(),
                status_meaning: None,
                interactive: true,
            })
        }
        QualityControlKind::Status => {
            let meaning = control.meaning.ok_or_else(|| {
                QualityError::InvalidControl(format!("status {} requires a meaning", control.id))
            })?;
            let description = control.description.clone().ok_or_else(|| {
                QualityError::InvalidControl(format!(
                    "status {} requires a color-independent description",
                    control.id
                ))
            })?;
            let light = StatusLight::new(meaning.into(), &control.name, description)
                .map_err(|error| QualityError::InvalidControl(error.to_string()))?;
            Ok(QualityControlView {
                id: control.id.clone(),
                kind: control.kind.clone(),
                accessibility: light.accessibility().clone(),
                status_meaning: Some(light.meaning()),
                interactive: false,
            })
        }
        QualityControlKind::TextField => {
            let mut field = TextField::new(&control.name)
                .map_err(|error| QualityError::InvalidControl(error.to_string()))?;
            let _ = field.set_focus_epoch(epoch);
            if let Some(description) = &control.description {
                field
                    .set_description(description)
                    .map_err(|error| QualityError::InvalidControl(error.to_string()))?;
            }
            Ok(QualityControlView {
                id: control.id.clone(),
                kind: control.kind.clone(),
                accessibility: field.accessibility().clone(),
                status_meaning: None,
                interactive: true,
            })
        }
    }
}

fn approved_quality_path(path: &Path, policy: &PreviewPathPolicy) -> Result<PathBuf, QualityError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(|error| QualityError::MalformedFixture {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?
    };
    if is_sensitive_path(&absolute) {
        return Err(QualityError::SensitivePath { path: absolute });
    }
    let quality_root = policy.fixture_root().join("quality");
    if !is_within(&absolute, &quality_root) {
        return Err(QualityError::OutsideQualityRoot { path: absolute });
    }
    if absolute
        .extension()
        .and_then(|extension| extension.to_str())
        != Some("json")
    {
        return Err(QualityError::MalformedFixture {
            path: absolute,
            message: "quality fixtures must use the .json extension".into(),
        });
    }
    Ok(absolute)
}

fn read_quality_bytes(path: &Path) -> Result<Vec<u8>, QualityError> {
    let metadata = fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            QualityError::FixtureMissing {
                path: path.to_path_buf(),
            }
        } else {
            QualityError::FixtureIo {
                path: path.to_path_buf(),
                message: error.to_string(),
            }
        }
    })?;
    if !metadata.is_file() {
        return Err(QualityError::FixtureIo {
            path: path.to_path_buf(),
            message: "fixture is not a regular file".into(),
        });
    }
    if metadata.len() > MAX_FIXTURE_BYTES {
        return Err(QualityError::FixtureTooLarge {
            path: path.to_path_buf(),
            bytes: metadata.len(),
            max_bytes: MAX_FIXTURE_BYTES,
        });
    }
    let bytes = fs::read(path).map_err(|error| QualityError::FixtureIo {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    if bytes.len() as u64 > MAX_FIXTURE_BYTES {
        return Err(QualityError::FixtureTooLarge {
            path: path.to_path_buf(),
            bytes: bytes.len() as u64,
            max_bytes: MAX_FIXTURE_BYTES,
        });
    }
    Ok(bytes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QualityError {
    OutsideQualityRoot {
        path: PathBuf,
    },
    SensitivePath {
        path: PathBuf,
    },
    FixtureMissing {
        path: PathBuf,
    },
    FixtureTooLarge {
        path: PathBuf,
        bytes: u64,
        max_bytes: u64,
    },
    FixtureIo {
        path: PathBuf,
        message: String,
    },
    MalformedFixture {
        path: PathBuf,
        message: String,
    },
    UnsupportedSchema {
        path: PathBuf,
        schema: String,
    },
    VirtualizationBudgetExceeded {
        kind: String,
        total: usize,
        limit: usize,
    },
    Shutdown,
    MissingSection(&'static str),
    InvalidControl(String),
    Hold {
        missing: String,
    },
    CollectionBoundExceeded {
        kind: String,
        len: usize,
        limit: usize,
    },
}

impl Display for QualityError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutsideQualityRoot { path } => {
                write!(
                    f,
                    "quality fixture is outside tests/fixtures/ui/quality: {}",
                    path.display()
                )
            }
            Self::SensitivePath { path } => {
                write!(f, "sensitive production path refused: {}", path.display())
            }
            Self::FixtureMissing { path } => {
                write!(f, "quality fixture does not exist: {}", path.display())
            }
            Self::FixtureTooLarge {
                path,
                bytes,
                max_bytes,
            } => write!(
                f,
                "quality fixture is too large ({} bytes; max {}): {}",
                bytes,
                max_bytes,
                path.display()
            ),
            Self::FixtureIo { path, message } => {
                write!(
                    f,
                    "quality fixture I/O failed for {}: {message}",
                    path.display()
                )
            }
            Self::MalformedFixture { path, message } => {
                write!(f, "malformed quality fixture {}: {message}", path.display())
            }
            Self::UnsupportedSchema { path, schema } => write!(
                f,
                "unsupported quality fixture schema {schema} in {}",
                path.display()
            ),
            Self::VirtualizationBudgetExceeded { kind, total, limit } => {
                write!(
                    f,
                    "{kind} virtualization budget exceeded ({total} > {limit})"
                )
            }
            Self::Shutdown => f.write_str("quality surface has shut down"),
            Self::MissingSection(section) => write!(f, "quality fixture is missing {section}"),
            Self::InvalidControl(message) => write!(f, "invalid quality control: {message}"),
            Self::Hold { missing } => write!(f, "quality gate HOLD: {missing}"),
            Self::CollectionBoundExceeded { kind, len, limit } => {
                write!(f, "{kind} collection bound exceeded ({len} > {limit})")
            }
        }
    }
}

impl Error for QualityError {}
