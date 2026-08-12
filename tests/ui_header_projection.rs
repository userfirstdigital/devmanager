use std::path::PathBuf;

use devmanager::client::action;
use devmanager::client::{ClientModel, ClientModelBuilder};
use devmanager::domain::agent::{
    AgentRole, AgentSessionFacts, AgentSessionLifecycle, ProviderSessionId,
};
use devmanager::domain::id::AgentSessionId;
use devmanager::domain::id::TaskId;
use devmanager::domain::snapshot::{SnapshotItem, SnapshotItemKey, SnapshotPage};
use devmanager::domain::task::{TaskActivity, VisibleTaskStatus};
use devmanager::providers::ProviderKind;
use devmanager::ui::actions::{KeyboardShortcut, ShortcutKey};
use devmanager::ui::components::AccessibleRole;
use devmanager::ui::shell::{HostEpochSnapshot, PointerButton, Shell, ShellAttachmentError};
use devmanager::ui::task_cockpit::header::{
    AgentRoleProjection, CpuInputUnit, HeaderFieldKey, HeaderHighWaterLedger, HighWaterDecision,
    PendingHeaderActionQueue, RemoteHealth, RemoteObservation, RemoteObservationIdentity,
    TitleLayout,
};
use devmanager::ui::task_cockpit::{
    HeaderField, PrimaryAgentProjection, TaskHeaderModel, TopBarModel, TopBarProjectionController,
    TopBarProjectionInput, WorkspaceProjection, MAX_HEADER_SPECIALISTS, PROVIDER_QUOTA_MAX_AGE_MS,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct HeaderFixture {
    snapshot_pages: Vec<SnapshotPage>,
    selected_task_id: TaskId,
    top_bar: TopBarProjectionInput,
}

fn fixture() -> HeaderFixture {
    serde_json::from_str(include_str!("fixtures/ui/header-states.json"))
        .expect("header fixture must be valid")
}

fn model_from_pages(pages: &[SnapshotPage]) -> ClientModel {
    let mut builder = ClientModelBuilder::new();
    for page in pages {
        builder
            .ingest_page(page.clone())
            .expect("fixture snapshot page must be admitted");
    }
    builder.finish().expect("fixture snapshot must finish")
}

fn host_epochs(client_epoch: u64) -> HostEpochSnapshot {
    HostEpochSnapshot::try_from_host(3, 5, 7, client_epoch, 11)
        .expect("test host attachment must issue non-zero epochs")
}

fn attached_shell(task_id: Option<TaskId>, client_epoch: u64) -> Shell {
    Shell::attach(task_id, Some(host_epochs(client_epoch))).expect("host attachment")
}

fn project_header(model: &ClientModel, task_id: TaskId) -> TaskHeaderModel {
    let shell = attached_shell(Some(task_id), model.last_applied_sequence());
    shell
        .task_header(model)
        .expect("selected task must project")
}

#[test]
fn header_projects_task_context_and_stable_agent_status_links() {
    let fixture = fixture();
    let model = model_from_pages(&fixture.snapshot_pages);

    let header = project_header(&model, fixture.selected_task_id);

    assert_eq!(header.identity.task_id, fixture.selected_task_id);
    assert_eq!(header.identity.revision, 9);
    assert_eq!(header.identity.action_epoch, 4);
    assert_eq!(header.title, "Ship the native cockpit");
    assert_eq!(
        header.project.id.to_string(),
        "018f60b0-9c1a-7001-8000-000000000011"
    );
    assert_eq!(header.project.label, "018f60b0-9c1a-7001-8000-000000000011");
    assert_eq!(
        header.workspace,
        WorkspaceProjection::Worktree {
            path: PathBuf::from("workspace"),
            branch: "codex/header".to_string(),
        }
    );

    let primary = match &header.primary {
        PrimaryAgentProjection::Present(primary) => primary,
        other => panic!("expected Primary projection, got {other:?}"),
    };
    assert_eq!(primary.provider, "Claude");
    assert_eq!(
        primary.identity.agent_id.to_string(),
        "018f60b0-9c1a-7001-8000-000000000022"
    );
    assert_eq!(primary.identity.task, header.identity);
    assert_eq!(primary.identity.resource_generation, 3);
    assert_eq!(primary.identity.revision, 2);
    assert_eq!(
        primary.identity.provider_session_id.as_deref(),
        Some("claude-session-1")
    );
    assert_eq!(header.specialists.len(), 1);
    assert_eq!(header.specialists[0].label, "reviewer");
    assert_eq!(header.specialists[0].provider, "Codex");
    assert_eq!(header.specialist_total, 1);
    assert_eq!(header.specialist_hidden_count, 0);

    assert_eq!(header.turn.activity, TaskActivity::Working);
    assert_eq!(header.turn.status, VisibleTaskStatus::Working);
    assert_eq!(header.status.action.id(), action::ACTION_TASK_SHOW);
    assert_eq!(header.status.role, AccessibleRole::Button);
    assert!(header.status.focusable);
    assert!(!header.status.tooltip.is_empty());
    assert!(matches!(
        header.status.action.target(),
        devmanager::ui::task_cockpit::header::ActionTarget::Task(_)
    ));
    assert!(header.status.description.contains("working"));
    assert!(header
        .accessible_description
        .contains("Ship the native cockpit"));
}

#[test]
fn shell_projects_only_its_captured_selected_task() {
    let fixture = fixture();
    let model = model_from_pages(&fixture.snapshot_pages);
    let mut shell = attached_shell(Some(fixture.selected_task_id), 1);
    assert!(shell.sync_client_epoch(model.last_applied_sequence()));

    let header = shell.task_header(&model).expect("selected task header");
    assert_eq!(header.identity.task_id, fixture.selected_task_id);
}

#[test]
fn top_bar_keeps_global_facts_hides_stale_quota_and_labels_cpu_diagnostics() {
    let fixture = fixture();
    let top_bar = TopBarModel::from_input(&fixture.top_bar);

    assert_eq!(top_bar.quotas.len(), 1);
    assert_eq!(top_bar.quotas[0].provider, "Claude");
    assert_eq!(top_bar.quotas[0].detail, "72% remaining");
    assert_eq!(top_bar.quotas[0].role, AccessibleRole::Button);
    assert!(top_bar.quotas[0].focusable);
    assert!(!top_bar.quotas[0].tooltip.is_empty());
    assert_eq!(top_bar.host.as_ref().unwrap().role, AccessibleRole::Button);
    assert!(top_bar.host.as_ref().unwrap().focusable);
    assert_eq!(
        top_bar.update.as_ref().unwrap().role,
        AccessibleRole::Button
    );
    assert_eq!(
        top_bar.resources.as_ref().unwrap().memory_bytes,
        Some(12_345_678)
    );

    let resources = top_bar.resources.as_ref().expect("host resources");
    let cpu = resources.cpu.as_ref().expect("aggregate CPU");
    assert!((cpu.whole_machine_percent - 1.953125).abs() < f64::EPSILON);
    assert_eq!(cpu.input_unit, CpuInputUnit::LegacyCoreTotalPercent);
    let diagnostic = cpu.diagnostic.as_ref().expect("core-equivalent diagnostic");
    assert_eq!(diagnostic.label, "Core-equivalent CPU (diagnostic)");
    assert!((diagnostic.core_equivalent - 1.25).abs() < f64::EPSILON);
    assert!(top_bar.accessible_description.contains("healthy"));
    assert!(top_bar.accessible_description.contains("connected"));
    assert!(top_bar
        .accessible_description
        .contains("Quota details unavailable"));
}

#[test]
fn narrow_header_uses_priority_overflow_with_accessible_text() {
    let fixture = fixture();
    let model = model_from_pages(&fixture.snapshot_pages);
    let header = project_header(&model, fixture.selected_task_id);

    let layout = header.responsive_layout(360);

    assert!(layout.inline.contains(&HeaderField::Title));
    assert!(layout.inline.contains(&HeaderField::TurnStatus));
    assert!(layout.overflow.contains(&HeaderField::Project));
    assert!(layout.overflow.contains(&HeaderField::Workspace));
    assert!(layout.overflow.contains(&HeaderField::Specialists));
    let overflow = layout.overflow_control.expect("overflow control");
    assert_eq!(overflow.label, "More task details");
    assert!(!overflow.description.is_empty());
    assert_eq!(overflow.role, AccessibleRole::Button);
    assert!(overflow.focusable);
    assert_eq!(overflow.pointer, PointerButton::Primary);
    assert_eq!(
        overflow.keyboard,
        KeyboardShortcut::ctrl(ShortcutKey::Character('m'))
    );
    assert_eq!(
        overflow.keyboard_action,
        devmanager::ui::actions::KeyboardAction::OpenTaskDetails
    );
    assert!(!overflow.tooltip.is_empty());
    assert_eq!(overflow.action.descriptor().id, action::ACTION_TASK_SHOW);
    assert!(layout.accessible_description.contains("More task details"));
}

#[test]
fn header_does_not_invent_a_primary_provider_when_the_reference_is_missing() {
    let fixture = fixture();
    let mut pages = fixture.snapshot_pages.clone();
    for page in &mut pages {
        for item in &mut page.items {
            if let SnapshotItem::Task(task) = item {
                task.primary_agent_id = None;
            }
        }
    }
    let model = model_from_pages(&pages);

    let header = project_header(&model, fixture.selected_task_id);

    assert!(matches!(
        header.primary,
        PrimaryAgentProjection::Unavailable { .. }
    ));
    assert!(header
        .accessible_description
        .contains("Primary provider unavailable"));
}

fn cpu_input(
    value: f64,
    unit: CpuInputUnit,
    logical_cpu_count: Option<u32>,
) -> TopBarProjectionInput {
    TopBarProjectionInput {
        now_ms: 1_000,
        generation: 7,
        host: None,
        connect: None,
        update: None,
        quotas: Vec::new(),
        resources: Some(devmanager::ui::task_cockpit::HostResourceObservation {
            cpu_percent: Some(value),
            cpu_input_unit: unit,
            memory_bytes: None,
            logical_cpu_count,
            cpu_observed_at_ms: Some(1_000),
            memory_observed_at_ms: Some(1_000),
            generation: Some(7),
            cpu_revision: 1,
            memory_revision: 1,
        }),
    }
}

#[test]
fn cpu_projection_normalizes_legacy_core_total_before_clamping_and_accepts_machine_percent() {
    let zero = TopBarModel::from_input(&cpu_input(
        0.0,
        CpuInputUnit::LegacyCoreTotalPercent,
        Some(64),
    ));
    assert_eq!(
        zero.resources
            .as_ref()
            .unwrap()
            .cpu
            .as_ref()
            .unwrap()
            .whole_machine_percent,
        0.0
    );

    let one_core = TopBarModel::from_input(&cpu_input(
        100.0,
        CpuInputUnit::LegacyCoreTotalPercent,
        Some(1),
    ));
    assert_eq!(
        one_core
            .resources
            .as_ref()
            .unwrap()
            .cpu
            .as_ref()
            .unwrap()
            .whole_machine_percent,
        100.0
    );

    let many_core = TopBarModel::from_input(&cpu_input(
        125.0,
        CpuInputUnit::LegacyCoreTotalPercent,
        Some(64),
    ));
    let cpu = many_core.resources.as_ref().unwrap().cpu.as_ref().unwrap();
    assert!((cpu.whole_machine_percent - 1.953125).abs() < f64::EPSILON);
    assert_eq!(cpu.diagnostic.as_ref().unwrap().core_equivalent, 1.25);

    let one_core_over_commitment = TopBarModel::from_input(&cpu_input(
        125.0,
        CpuInputUnit::LegacyCoreTotalPercent,
        Some(1),
    ));
    let cpu = one_core_over_commitment
        .resources
        .as_ref()
        .unwrap()
        .cpu
        .as_ref()
        .unwrap();
    assert_eq!(cpu.whole_machine_percent, 100.0);
    assert_eq!(cpu.diagnostic.as_ref().unwrap().core_equivalent, 1.25);

    let machine =
        TopBarModel::from_input(&cpu_input(100.0, CpuInputUnit::MachinePercent, Some(64)));
    let cpu = machine.resources.as_ref().unwrap().cpu.as_ref().unwrap();
    assert_eq!(cpu.whole_machine_percent, 100.0);
    assert!(
        cpu.diagnostic.is_none(),
        "machine input must not be divided twice"
    );
}

#[test]
fn cpu_projection_rejects_invalid_nonfinite_overflow_and_zero_core_inputs() {
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
        let model = TopBarModel::from_input(&cpu_input(
            value,
            CpuInputUnit::LegacyCoreTotalPercent,
            Some(64),
        ));
        assert!(model.resources.is_none(), "invalid CPU value {value:?}");
    }

    for logical_cpu_count in [None, Some(0)] {
        let model = TopBarModel::from_input(&cpu_input(
            125.0,
            CpuInputUnit::LegacyCoreTotalPercent,
            logical_cpu_count,
        ));
        assert!(
            model.resources.is_none(),
            "zero-core ambiguity must be hidden"
        );
    }

    let overflow = TopBarModel::from_input(&cpu_input(
        f64::MAX,
        CpuInputUnit::LegacyCoreTotalPercent,
        Some(1),
    ));
    assert!(overflow.resources.is_none(), "overflow must fail closed");
}

#[test]
fn quota_projection_rejects_future_and_exact_one_hour_boundary_observations() {
    let mut input = fixture().top_bar;
    let now = input.now_ms;
    input.quotas = vec![
        input.quotas[0].clone(),
        devmanager::ui::task_cockpit::QuotaObservation {
            identity: devmanager::ui::task_cockpit::QuotaObservationIdentity {
                provider: "claude".into(),
                provider_session_id: "claude-boundary".into(),
                observation_id: 99,
            },
            detail: Some("11% remaining".into()),
            observed_at_ms: Some(now - PROVIDER_QUOTA_MAX_AGE_MS),
            generation: Some(input.generation),
        },
        devmanager::ui::task_cockpit::QuotaObservation {
            identity: devmanager::ui::task_cockpit::QuotaObservationIdentity {
                provider: "codex".into(),
                provider_session_id: "codex-future".into(),
                observation_id: 100,
            },
            detail: Some("99% remaining".into()),
            observed_at_ms: Some(now + 1),
            generation: Some(input.generation),
        },
    ];

    let model = TopBarModel::from_input(&input);
    assert_eq!(model.quotas.len(), 1);
    assert_eq!(model.quotas[0].provider, "Claude");
    assert_eq!(model.quotas[0].detail, "72% remaining");
    assert!(model
        .quotas
        .iter()
        .all(|quota| quota.detail != "11% remaining"));
    assert!(model.quotas.iter().all(|quota| quota.age_ms >= 0));

    input.quotas = input
        .quotas
        .into_iter()
        .filter(|quota| quota.detail.as_deref() != Some("72% remaining"))
        .collect();
    let boundary_only = TopBarModel::from_input(&input);
    assert!(boundary_only.quotas.is_empty());

    input.quotas = vec![
        devmanager::ui::task_cockpit::QuotaObservation {
            identity: devmanager::ui::task_cockpit::QuotaObservationIdentity {
                provider: "claude".into(),
                provider_session_id: "claude-tied".into(),
                observation_id: 100,
            },
            detail: Some("10% remaining".into()),
            observed_at_ms: Some(now - 100),
            generation: Some(input.generation),
        },
        devmanager::ui::task_cockpit::QuotaObservation {
            identity: devmanager::ui::task_cockpit::QuotaObservationIdentity {
                provider: "claude".into(),
                provider_session_id: "claude-tied".into(),
                observation_id: 101,
            },
            detail: Some("20% remaining".into()),
            observed_at_ms: Some(now - 100),
            generation: Some(input.generation),
        },
    ];
    let tied_model = TopBarModel::from_input(&input);
    assert_eq!(tied_model.quotas.len(), 1);
    assert_eq!(tied_model.quotas[0].identity.observation_id, 101);
    assert_eq!(tied_model.quotas[0].detail, "20% remaining");
}

#[test]
fn top_bar_suppresses_missing_stale_future_and_wrong_generation_facts() {
    let fixture = fixture();
    let mut input = fixture.top_bar.clone();
    input.host.as_mut().unwrap().observed_at_ms = Some(input.now_ms + 1);
    input.connect.as_mut().unwrap().generation = Some(input.generation + 1);
    input.update.as_mut().unwrap().observed_at_ms =
        Some(input.now_ms - PROVIDER_QUOTA_MAX_AGE_MS - 1);
    input.resources.as_mut().unwrap().cpu_observed_at_ms = None;
    input.resources.as_mut().unwrap().memory_observed_at_ms = None;
    input.quotas[0].generation = None;

    let model = TopBarModel::from_input(&input);
    assert!(model.host.is_none());
    assert!(model.connect.is_none());
    assert!(model.update.is_none());
    assert!(model.resources.is_none());
    assert!(model.quotas.is_empty());
    assert!(model
        .accessible_description
        .contains("Host CPU unavailable"));
    assert!(model
        .accessible_description
        .contains("Host memory unavailable"));
}

#[test]
fn empty_top_bar_snapshot_exposes_typed_unavailable_state_instead_of_blank_text() {
    let input = TopBarProjectionInput {
        now_ms: 1,
        generation: 1,
        host: None,
        connect: None,
        update: None,
        quotas: Vec::new(),
        resources: None,
    };
    let model = TopBarModel::from_input(&input);
    assert!(!model.unavailable.is_empty());
    assert!(!model.accessible_description.is_empty());
    assert!(model
        .accessible_description
        .contains("observations are unavailable"));
}

#[test]
fn header_and_top_bar_actions_share_one_descriptor_and_reject_stale_identity() {
    let fixture = fixture();
    let model = model_from_pages(&fixture.snapshot_pages);
    let header = project_header(&model, fixture.selected_task_id);
    let primary = match &header.primary {
        PrimaryAgentProjection::Present(primary) => primary,
        _ => panic!("primary agent"),
    };
    let top_bar = TopBarModel::from_input(&fixture.top_bar);
    let overflow = header.responsive_layout(320).overflow_control.unwrap();

    let projected = [
        header.status.action.clone(),
        primary.action.clone(),
        overflow.action.clone(),
        top_bar.host.as_ref().unwrap().action.clone(),
        top_bar.connect.as_ref().unwrap().action.clone(),
        top_bar.update.as_ref().unwrap().action.clone(),
        top_bar.quotas[0].action.clone(),
    ];
    for action in &projected {
        let descriptors: Vec<_> = action::catalog()
            .iter()
            .filter(|descriptor| descriptor.id == action.id())
            .collect();
        assert_eq!(
            descriptors.len(),
            1,
            "{} must map exactly once",
            action.id()
        );
        assert!(std::ptr::eq(action.descriptor(), descriptors[0]));
    }

    assert!(header.accepts_action(&header.status.action));
    assert!(header.accepts_action(&primary.action));
    assert!(top_bar.accepts_action(&top_bar.quotas[0].action));

    let mut stale_top_bar = fixture.top_bar.clone();
    stale_top_bar.host.as_mut().unwrap().identity.revision += 1;
    let current_top_bar = TopBarModel::from_input(&fixture.top_bar);
    let newer_top_bar = TopBarModel::from_input(&stale_top_bar);
    assert!(!newer_top_bar.accepts_action(&current_top_bar.host.unwrap().action));
}

#[test]
fn presentation_is_bounded_redacted_and_does_not_leak_sensitive_labels() {
    let fixture = fixture();
    let mut pages = fixture.snapshot_pages.clone();
    for page in &mut pages {
        for item in &mut page.items {
            match item {
                SnapshotItem::Task(task) => {
                    task.task.title =
                        format!("{}\nAPI_KEY=HEADER_SECRET_SENTINEL", "界".repeat(500));
                    task.task.workspace = devmanager::domain::task::WorkspaceRef::Worktree {
                        path: PathBuf::from(r"C:\Users\HEADER_PATH_SECRET_SENTINEL\deep\worktree"),
                        branch: "feature\nHEADER_BRANCH_SECRET_SENTINEL".into(),
                    };
                }
                SnapshotItem::AgentSession(_agent) => {}
                _ => {}
            }
        }
    }
    let model = model_from_pages(&pages);
    let header = project_header(&model, fixture.selected_task_id);
    assert!(header.title.chars().count() <= 160);
    assert!(header
        .title
        .chars()
        .all(|character| !character.is_control()));
    assert!(!header.title.contains("HEADER_SECRET_SENTINEL"));
    assert!(!header
        .accessible_description
        .contains("HEADER_PATH_SECRET_SENTINEL"));

    let mut top_bar_input = fixture.top_bar;
    top_bar_input
        .update
        .as_mut()
        .unwrap()
        .identity
        .target_version = Some("API_KEY=UPDATE_SECRET_SENTINEL".into());
    top_bar_input.quotas[0].detail = Some("API_KEY=QUOTA_SECRET_SENTINEL".into());
    let top_bar = TopBarModel::from_input(&top_bar_input);
    assert!(!top_bar
        .accessible_description
        .contains("UPDATE_SECRET_SENTINEL"));
    assert!(!top_bar
        .accessible_description
        .contains("QUOTA_SECRET_SENTINEL"));
    assert!(!top_bar.quotas[0].detail.contains("QUOTA_SECRET_SENTINEL"));
    assert!(!top_bar
        .update
        .unwrap()
        .label
        .contains("UPDATE_SECRET_SENTINEL"));
}

#[test]
fn cpu_and_memory_have_independent_freshness_stamps() {
    let mut input = cpu_input(125.0, CpuInputUnit::LegacyCoreTotalPercent, Some(64));
    input.resources.as_mut().unwrap().memory_bytes = Some(42);
    input.resources.as_mut().unwrap().cpu_observed_at_ms = Some(
        input
            .now_ms
            .checked_sub(PROVIDER_QUOTA_MAX_AGE_MS + 1)
            .unwrap(),
    );

    let model = TopBarModel::from_input(&input);
    let resources = model.resources.as_ref().expect("fresh memory remains");
    assert!(resources.cpu.is_none());
    assert_eq!(resources.cpu_observed_at_ms, None);
    assert_eq!(resources.memory_bytes, Some(42));
    assert_eq!(resources.memory_observed_at_ms, Some(input.now_ms));

    input.resources.as_mut().unwrap().cpu_observed_at_ms = Some(input.now_ms);
    input.resources.as_mut().unwrap().memory_bytes = Some(42);
    input.resources.as_mut().unwrap().memory_observed_at_ms = Some(
        input
            .now_ms
            .checked_sub(PROVIDER_QUOTA_MAX_AGE_MS + 1)
            .unwrap(),
    );
    let model = TopBarModel::from_input(&input);
    let resources = model.resources.as_ref().expect("fresh CPU remains");
    assert!(resources.cpu.is_some());
    assert_eq!(resources.cpu_observed_at_ms, Some(input.now_ms));
    assert_eq!(resources.memory_bytes, None);
    assert_eq!(resources.memory_observed_at_ms, None);
}

#[test]
fn workspace_projection_does_not_retain_raw_path_or_branch_text() {
    let fixture = fixture();
    let mut pages = fixture.snapshot_pages.clone();
    for page in &mut pages {
        for item in &mut page.items {
            if let SnapshotItem::Task(task) = item {
                task.task.workspace = devmanager::domain::task::WorkspaceRef::Worktree {
                    path: PathBuf::from(r"C:\Users\ACCOUNT_ID_SECRET_SENTINEL\repo\worktree"),
                    branch: "feature\nCOMMAND_LINE_SECRET_SENTINEL".into(),
                };
            }
        }
    }

    let model = model_from_pages(&pages);
    let header = project_header(&model, fixture.selected_task_id);
    let WorkspaceProjection::Worktree { path, branch } = header.workspace else {
        panic!("expected worktree projection");
    };
    assert!(!path
        .to_string_lossy()
        .contains("ACCOUNT_ID_SECRET_SENTINEL"));
    assert!(!branch.contains("COMMAND_LINE_SECRET_SENTINEL"));
    assert!(!header
        .accessible_description
        .contains("ACCOUNT_ID_SECRET_SENTINEL"));
}

#[test]
fn stale_status_is_announced_as_unavailable() {
    let fixture = fixture();
    let mut input = fixture.top_bar;
    input.host.as_mut().unwrap().observed_at_ms = Some(
        input
            .now_ms
            .checked_sub(PROVIDER_QUOTA_MAX_AGE_MS + 1)
            .unwrap(),
    );

    let model = TopBarModel::from_input(&input);
    assert!(model.host.is_none());
    assert!(model
        .accessible_description
        .contains("Host status unavailable"));
}

#[test]
fn quota_truncation_is_announced_and_has_a_reachable_summary_action() {
    let fixture = fixture();
    let mut input = fixture.top_bar;
    input.quotas = (0..10)
        .map(|index| devmanager::ui::task_cockpit::QuotaObservation {
            identity: devmanager::ui::task_cockpit::QuotaObservationIdentity {
                provider: format!("provider-{index}"),
                provider_session_id: format!("session-{index}"),
                observation_id: index,
            },
            detail: Some(format!("{}% remaining", 90 - index)),
            observed_at_ms: Some(input.now_ms),
            generation: Some(input.generation),
        })
        .collect();

    let model = TopBarModel::from_input(&input);
    assert_eq!(model.quotas.len(), 8);
    assert_eq!(model.quota_hidden_count, 2);
    assert!(model.quotas_truncated);
    assert!(model
        .accessible_description
        .contains("8 quotas shown, 2 hidden"));
    assert!(model.quota_overflow_action.is_some());
    assert!(model.accepts_action(model.quota_overflow_action.as_ref().unwrap()));
}

#[test]
fn specialist_cap_announces_total_hidden_count_and_orders_by_agent_id() {
    let fixture = fixture();
    let mut pages = fixture.snapshot_pages.clone();
    let task_id = fixture.selected_task_id;
    let agents_page = pages
        .iter_mut()
        .find(|page| {
            matches!(
                page.section,
                devmanager::domain::snapshot::SnapshotSection::AgentSessions
            )
        })
        .unwrap();
    for index in 0..33u8 {
        let id = AgentSessionId::from_bytes([
            0x01,
            0x8f,
            0x60,
            0xb0,
            0x9c,
            0x1a,
            0x70,
            0x01,
            0x80,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x01,
            index.saturating_add(0x30),
        ])
        .unwrap();
        agents_page
            .items
            .push(SnapshotItem::AgentSession(AgentSessionFacts {
                id,
                task_id,
                role: AgentRole::Specialist {
                    name: format!("specialist-{index:02}"),
                },
                provider_kind: ProviderKind::Codex,
                provider_session_id: Some(
                    ProviderSessionId::new(format!("codex-specialist-{index:02}"))
                        .expect("provider session"),
                ),
                lifecycle: AgentSessionLifecycle::Open,
                runtime_generation: 1,
                revision: 1,
            }));
    }

    let model = model_from_pages(&pages);
    let header = project_header(&model, task_id);
    assert_eq!(header.specialist_total, 34);
    assert_eq!(header.specialists.len(), 34);
    assert_eq!(header.specialist_hidden_count, 0);
    assert!(!header.specialists_truncated);
    assert!(header
        .accessible_description
        .contains("34 specialists shown, 0 hidden"));

    let ids: Vec<_> = header
        .specialists
        .iter()
        .map(|agent| agent.identity.agent_id)
        .collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(
        ids, sorted,
        "specialists must have deterministic ID ordering"
    );
}

#[test]
fn title_layout_is_deterministic_at_320_and_360_pixels() {
    let fixture = fixture();
    let mut pages = fixture.snapshot_pages.clone();
    for page in &mut pages {
        for item in &mut page.items {
            if let SnapshotItem::Task(task) = item {
                task.task.title = "A very long deterministic title ".repeat(20);
            }
        }
    }
    let model = model_from_pages(&pages);
    let header = project_header(&model, fixture.selected_task_id);

    let compact = header.responsive_layout(320);
    let narrow = header.responsive_layout(360);
    assert!(matches!(compact.title, TitleLayout::Truncated(_)));
    let TitleLayout::Truncated(compact_title) = compact.title else {
        unreachable!();
    };
    assert!(compact_title.ends_with('…'));
    assert!(compact_title.chars().count() <= 28);

    let TitleLayout::Wrapped(lines) = narrow.title else {
        panic!("360px titles must wrap");
    };
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().all(|line| line.chars().count() <= 28));
    assert_ne!(compact_title, lines.join(" "));
}

#[test]
fn quota_projection_canonicalizes_claude_provider_aliases_to_one_summary() {
    let fixture = fixture();
    let mut input = fixture.top_bar;
    input.quotas = vec![
        devmanager::ui::task_cockpit::QuotaObservation {
            identity: devmanager::ui::task_cockpit::QuotaObservationIdentity {
                provider: "Claude".into(),
                provider_session_id: "claude-a".into(),
                observation_id: 1,
            },
            detail: Some("10% remaining".into()),
            observed_at_ms: Some(input.now_ms - 20),
            generation: Some(input.generation),
        },
        devmanager::ui::task_cockpit::QuotaObservation {
            identity: devmanager::ui::task_cockpit::QuotaObservationIdentity {
                provider: "claude_code".into(),
                provider_session_id: "claude-b".into(),
                observation_id: 2,
            },
            detail: Some("20% remaining".into()),
            observed_at_ms: Some(input.now_ms - 10),
            generation: Some(input.generation),
        },
    ];

    let model = TopBarModel::from_input(&input);
    assert_eq!(model.quotas.len(), 1);
    assert_eq!(model.quotas[0].provider, "Claude");
    assert_eq!(model.quotas[0].detail, "20% remaining");
}

#[test]
fn visible_and_accessible_text_redacts_key_name_separator_variants() {
    let fixture = fixture();
    let mut pages = fixture.snapshot_pages.clone();
    for page in &mut pages {
        for item in &mut page.items {
            if let SnapshotItem::Task(task) = item {
                task.task.title =
                    "API_KEY=ordinary ACCESS-KEY:ordinary Private.Key ordinary".into();
            }
        }
    }

    let header = project_header(&model_from_pages(&pages), fixture.selected_task_id);
    for output in [&header.title, &header.accessible_description] {
        assert!(!output.contains("API_KEY"));
        assert!(!output.contains("ACCESS-KEY"));
        assert!(!output.contains("Private.Key"));
    }
}

#[test]
fn separated_secret_key_and_token_forms_never_reach_public_header_text() {
    let fixture = fixture();
    for secret_form in [
        "API KEY ordinary_value",
        "ACCESS KEY ordinary_value",
        "PRIVATE KEY ordinary_value",
        "API - KEY ordinary_value",
        "ACCESS / PRIVATE / KEY ordinary_value",
        "TOKEN ordinary_value",
        "token: ordinary_value",
    ] {
        let mut pages = fixture.snapshot_pages.clone();
        for page in &mut pages {
            for item in &mut page.items {
                if let SnapshotItem::Task(task) = item {
                    task.task.title = secret_form.to_string();
                }
            }
        }
        let header = project_header(&model_from_pages(&pages), fixture.selected_task_id);
        let debug = format!("{header:?}");
        assert!(!header.title.contains("ordinary_value"), "{secret_form}");
        assert!(
            !header.accessible_description.contains("ordinary_value"),
            "{secret_form}"
        );
        assert!(!debug.contains("ordinary_value"), "{secret_form}");
    }
}

#[test]
fn agent_projection_exposes_only_a_bounded_sanitized_role() {
    let fixture = fixture();
    let mut pages = fixture.snapshot_pages.clone();
    for page in &mut pages {
        for item in &mut page.items {
            if let SnapshotItem::AgentSession(agent) = item {
                if matches!(agent.role, AgentRole::Specialist { .. }) {
                    agent.role = AgentRole::Specialist {
                        name: format!("SPECIALIST_API_KEY={}", "x".repeat(300)),
                    };
                }
            }
        }
    }

    let header = project_header(&model_from_pages(&pages), fixture.selected_task_id);
    let role = &header.specialists[0].role;
    let AgentRoleProjection::Specialist { label } = role else {
        panic!("expected specialist role projection");
    };
    assert!(label.chars().count() <= 64);
    assert!(!label.contains("API_KEY"));
    assert!(!format!("{role:?}").contains("SPECIALIST_API_KEY"));
}

#[test]
fn shell_owned_epochs_fence_a_projected_action_without_caller_context() {
    let fixture = fixture();
    let model = model_from_pages(&fixture.snapshot_pages);
    let mut shell = attached_shell(Some(fixture.selected_task_id), 1);
    assert!(shell.sync_client_epoch(model.last_applied_sequence()));

    let header = shell.task_header(&model).expect("selected task header");
    let action = header.status.action.clone();
    assert!(shell.dispatch_task_action(&model, &action));

    let cases: [(&str, fn(&mut Shell) -> bool); 5] = [
        ("resource", |shell: &mut Shell| {
            shell.advance_resource_generation()
        }),
        ("connection", |shell: &mut Shell| {
            shell.advance_connection_epoch()
        }),
        ("focus", |shell: &mut Shell| shell.advance_focus_epoch()),
        ("client", |shell: &mut Shell| shell.advance_client_epoch()),
        ("navigation", |shell: &mut Shell| shell.on_view_switch()),
    ];
    for (name, advance) in cases {
        let mut current = attached_shell(Some(fixture.selected_task_id), 1);
        assert!(current.sync_client_epoch(model.last_applied_sequence()));
        let projected = current
            .task_header(&model)
            .expect("selected task header")
            .status
            .action
            .clone();
        assert!(current.dispatch_task_action(&model, &projected));
        assert!(advance(&mut current), "advance {name} epoch");
        assert!(
            !current.dispatch_task_action(&model, &projected),
            "stale {name} epoch must be rejected by the Shell-owned dispatcher"
        );
    }
}

#[test]
fn separated_secret_key_values_are_redacted_from_public_projection_text() {
    let fixture = fixture();
    let mut pages = fixture.snapshot_pages.clone();
    for page in &mut pages {
        for item in &mut page.items {
            if let SnapshotItem::Task(task) = item {
                task.task.title = [
                    "API KEY ordinary_value",
                    "access-key ordinary_value_two",
                    "PRIVATE KEY ordinary_value_three",
                    "ToKeN ordinary_value_four",
                    "ACCESS/PRIVATE KEY ordinary_value_five",
                    "api\\private-key ordinary_value_six",
                    "ACCESS / PRIVATE KEY ordinary_value_seven",
                ]
                .join(" | ");
            }
        }
    }

    let model = model_from_pages(&pages);
    let mut shell = attached_shell(Some(fixture.selected_task_id), 1);
    assert!(shell.sync_client_epoch(model.last_applied_sequence()));
    let header = shell.task_header(&model).expect("selected task header");
    for output in [&header.title, &header.accessible_description] {
        for secret in [
            "ordinary_value",
            "ordinary_value_two",
            "ordinary_value_three",
            "ordinary_value_four",
            "ordinary_value_five",
            "ordinary_value_six",
            "ordinary_value_seven",
        ] {
            assert!(
                !output.contains(secret),
                "secret value leaked in {output:?}"
            );
        }
    }
}

#[test]
fn public_identity_input_and_action_debug_display_are_opaque() {
    let fixture = fixture();
    let mut input = fixture.top_bar.clone();
    input.host.as_mut().unwrap().identity.host_id = "HOST_SECRET_SENTINEL".into();
    input.update.as_mut().unwrap().identity.current_version = "UPDATE_SECRET_SENTINEL".into();
    input.quotas[0].identity.provider_session_id = "PROVIDER_SESSION_SECRET_SENTINEL".into();
    input.quotas[0].detail = Some("QUOTA_SECRET_SENTINEL".into());

    let debug = format!("{input:?}");
    let display = format!("{input}");
    for output in [&debug, &display] {
        assert!(!output.contains("HOST_SECRET_SENTINEL"), "{output}");
        assert!(!output.contains("UPDATE_SECRET_SENTINEL"), "{output}");
        assert!(
            !output.contains("PROVIDER_SESSION_SECRET_SENTINEL"),
            "{output}"
        );
        assert!(!output.contains("QUOTA_SECRET_SENTINEL"), "{output}");
    }

    let model = model_from_pages(&fixture.snapshot_pages);
    let mut shell = attached_shell(Some(fixture.selected_task_id), 1);
    assert!(shell.sync_client_epoch(model.last_applied_sequence()));
    let action = shell
        .task_header(&model)
        .expect("selected task header")
        .status
        .action
        .clone();
    assert!(!format!("{action:?}").contains(action.id()));
    assert!(!format!("{action}").contains(action.id()));
}

#[test]
fn header_high_water_tombstones_block_removed_revival_and_equal_generation_conflicts() {
    let task_id = fixture().selected_task_id;
    let agent_id =
        AgentSessionId::parse("018f60b0-9c1a-7001-8000-000000000023").expect("fixture agent id");
    let mut ledger = HeaderHighWaterLedger::new(32, 1_000);

    assert_eq!(
        ledger.observe(HeaderFieldKey::Task(task_id), 7, 1, 100, 11, false,),
        HighWaterDecision::Accepted
    );
    assert_eq!(
        ledger.observe(
            HeaderFieldKey::Agent { task_id, agent_id },
            7,
            1,
            101,
            21,
            false,
        ),
        HighWaterDecision::Accepted
    );
    assert_eq!(
        ledger.observe(HeaderFieldKey::Task(task_id), 7, 2, 102, 22, true,),
        HighWaterDecision::Accepted
    );
    assert_eq!(
        ledger.observe(HeaderFieldKey::Task(task_id), 7, 1, 103, 11, false,),
        HighWaterDecision::IgnoredStale
    );
    assert_eq!(
        ledger.observe(HeaderFieldKey::Task(task_id), 7, 2, 104, 99, false,),
        HighWaterDecision::RejectedConflict
    );
    assert_eq!(
        ledger.observe(HeaderFieldKey::Task(task_id), 7, 3, 105, 33, false,),
        HighWaterDecision::Accepted
    );
}

#[test]
fn header_high_water_fences_nested_provider_resource_quota_and_remote_fields() {
    let task_id = fixture().selected_task_id;
    let agent_id =
        AgentSessionId::parse("018f60b0-9c1a-7001-8000-000000000023").expect("fixture agent id");
    let mut ledger = HeaderHighWaterLedger::new(32, 1_000);
    let keys = [
        HeaderFieldKey::AgentProvider { task_id, agent_id },
        HeaderFieldKey::AgentResource {
            task_id,
            agent_id,
            field: devmanager::ui::task_cockpit::header::AgentResourceField::Cpu,
        },
        HeaderFieldKey::Quota {
            provider: "claude".to_string(),
            provider_session_id: "session".to_string(),
        },
        HeaderFieldKey::Remote {
            source_id: "remote".to_string(),
        },
    ];

    for (index, key) in keys.into_iter().enumerate() {
        assert_eq!(
            ledger.observe(key.clone(), 7, 4, 200 + index as i64, 40, false),
            HighWaterDecision::Accepted
        );
        assert_eq!(
            ledger.observe(key, 7, 4, 210 + index as i64, 41, false),
            HighWaterDecision::RejectedConflict
        );
    }
}

#[test]
fn header_high_water_expiry_and_capacity_rules_bound_tombstone_memory() {
    let task_id = fixture().selected_task_id;
    let agent_id =
        AgentSessionId::parse("018f60b0-9c1a-7001-8000-000000000023").expect("fixture agent id");
    let mut ledger = HeaderHighWaterLedger::new(2, 10);
    let task_key = HeaderFieldKey::Task(task_id);
    let agent_key = HeaderFieldKey::Agent { task_id, agent_id };
    let quota_key = HeaderFieldKey::Quota {
        provider: "claude".to_string(),
        provider_session_id: "session".to_string(),
    };

    assert_eq!(
        ledger.observe(task_key.clone(), 7, 1, 100, 1, false),
        HighWaterDecision::Accepted
    );
    assert_eq!(
        ledger.observe(agent_key, 7, 1, 101, 2, false),
        HighWaterDecision::Accepted
    );
    assert_eq!(
        ledger.observe(quota_key, 7, 1, 102, 3, false),
        HighWaterDecision::RejectedCapacity
    );
    assert_eq!(
        ledger.observe(task_key.clone(), 7, 2, 112, 4, true),
        HighWaterDecision::Accepted
    );
    assert_eq!(ledger.tombstone_count(), 1);
    assert!(ledger.contains_tombstone(&task_key));
    assert_eq!(ledger.len(), 1);
    assert_eq!(
        ledger.observe(task_key, 7, 1, 122, 1, false),
        HighWaterDecision::Accepted,
        "expired tombstones may be replaced only after the explicit TTL"
    );
    assert_eq!(ledger.tombstone_count(), 0);
}

#[test]
fn specialist_projection_retains_plan_cap_and_exposes_bounded_stable_keyed_window() {
    let fixture = fixture();
    let model = model_from_pages(&fixture.snapshot_pages);
    let header = project_header(&model, fixture.selected_task_id);

    assert!(MAX_HEADER_SPECIALISTS >= 5_000);
    assert_eq!(header.specialist_total, header.specialists.len());
    let window = header.specialist_window(0, usize::MAX);
    assert!(window.items.len() <= 128);
    assert_eq!(window.total, header.specialists.len());
    assert_eq!(
        window
            .items
            .iter()
            .map(|agent| agent.identity.agent_id)
            .collect::<Vec<_>>(),
        header
            .specialists
            .iter()
            .take(window.items.len())
            .map(|agent| agent.identity.agent_id)
            .collect::<Vec<_>>()
    );
}

#[test]
fn specialist_projection_retains_5000_and_stable_window_keys_across_page_reorder() {
    let fixture = fixture();
    let task_id = fixture.selected_task_id;
    let mut base_pages = fixture.snapshot_pages.clone();
    let agent_page_index = base_pages
        .iter()
        .position(|page| {
            matches!(
                page.section,
                devmanager::domain::snapshot::SnapshotSection::AgentSessions
            )
        })
        .expect("agent page");
    let agent_page = base_pages.remove(agent_page_index);
    let mut extra_items = Vec::new();
    for index in 0..5_001 {
        extra_items.push(SnapshotItem::AgentSession(AgentSessionFacts {
            id: AgentSessionId::new(),
            task_id,
            role: AgentRole::Specialist {
                name: format!("extra-{index:05}"),
            },
            provider_kind: ProviderKind::Codex,
            provider_session_id: Some(
                ProviderSessionId::new(format!("extra-session-{index:05}"))
                    .expect("provider session"),
            ),
            lifecycle: AgentSessionLifecycle::Open,
            runtime_generation: 1,
            revision: 1,
        }));
    }
    let mut all_items = agent_page.items.clone();
    all_items.extend(extra_items.clone());

    let make_pages = |items: Vec<SnapshotItem>| {
        let mut pages = base_pages.clone();
        let mut replacement = Vec::new();
        let mut previous_id = None;
        let chunks: Vec<Vec<SnapshotItem>> = items.chunks(900).map(ToOwned::to_owned).collect();
        for (index, chunk) in chunks.iter().enumerate() {
            let last_id = match chunk.last().expect("non-empty chunk") {
                SnapshotItem::AgentSession(agent) => agent.id,
                other => panic!("unexpected agent page item: {other:?}"),
            };
            replacement.push(SnapshotPage {
                snapshot_id: agent_page.snapshot_id,
                through_sequence: agent_page.through_sequence,
                section: agent_page.section,
                after_item: previous_id.map(SnapshotItemKey::AgentSession),
                items: chunk.clone(),
                encoded_bytes: agent_page.encoded_bytes,
                next_cursor: (index + 1 < chunks.len()).then(|| vec![index as u8]),
            });
            previous_id = Some(last_id);
        }
        pages.splice(agent_page_index..agent_page_index, replacement);
        pages
    };

    let first = model_from_pages(&make_pages(all_items.clone()));
    all_items.reverse();
    let reordered = model_from_pages(&make_pages(all_items));
    let first_header = project_header(&first, task_id);
    let reordered_header = project_header(&reordered, task_id);

    assert_eq!(first_header.specialist_total, 5_002);
    assert_eq!(first_header.specialists.len(), 5_000);
    assert_eq!(first_header.specialist_hidden_count, 2);
    assert!(first_header.specialists_truncated);
    let first_window = first_header.specialist_window(4_990, 200);
    let reordered_window = reordered_header.specialist_window(4_990, 200);
    assert_eq!(first_window.items.len(), 10);
    assert_eq!(first_window, reordered_window);
    assert!(first_window
        .items
        .windows(2)
        .all(|agents| agents[0].identity.agent_id != agents[1].identity.agent_id));
}

#[test]
fn top_bar_model_contains_explicit_remote_projection_and_one_quota_projection() {
    let fixture = fixture();
    let remote = RemoteObservation {
        identity: RemoteObservationIdentity {
            source_id: "remote-host".to_string(),
            revision: 2,
        },
        health: RemoteHealth::Healthy,
        label: "Remote host".to_string(),
        observed_at_ms: Some(fixture.top_bar.now_ms - 10),
        generation: Some(fixture.top_bar.generation),
    };
    let controller = TopBarProjectionController::new_with_remote(fixture.top_bar, Some(remote))
        .expect("remote projection");
    let model = controller.model();

    let remote = model.remote.as_ref().expect("remote must be rendered");
    assert_eq!(remote.health, RemoteHealth::Healthy);
    assert_eq!(remote.label, "Remote host");
    assert_eq!(model.quotas.len(), 1);
    assert!(model.accessible_description.contains("Remote host"));
}

#[test]
fn pending_header_actions_are_bounded_and_preserve_captured_focus_epoch() {
    let fixture = fixture();
    let model = model_from_pages(&fixture.snapshot_pages);
    let action = project_header(&model, fixture.selected_task_id)
        .status
        .action
        .clone();
    let mut queue = PendingHeaderActionQueue::new(1);

    queue.push(action.clone()).expect("first action fits");
    assert_eq!(
        queue.push(action.clone()),
        Err(devmanager::ui::task_cockpit::header::PendingHeaderActionError::Full)
    );
    assert_eq!(queue.drain_for_tick(1), vec![action]);
    assert!(queue.is_empty());
}

#[test]
fn quota_controller_rejects_late_observations_by_provider_generation_and_authority() {
    let fixture = fixture();
    let mut controller = TopBarProjectionController::new(fixture.top_bar.clone())
        .expect("fixture top bar must pass controller preflight");
    let current_detail = controller.model().quotas[0].detail.clone();
    let cached_before = controller.cached_quota_count();

    let mut stale_high_sequence = fixture.top_bar.clone();
    stale_high_sequence.quotas[0].identity.observation_id = u64::MAX;
    stale_high_sequence.quotas[0].observed_at_ms =
        Some(stale_high_sequence.now_ms - PROVIDER_QUOTA_MAX_AGE_MS);
    stale_high_sequence.quotas[0].detail = Some("0% remaining".into());
    assert!(!controller
        .apply(stale_high_sequence)
        .expect("stale observation must be handled"));
    assert_eq!(controller.cached_quota_count(), cached_before);
    assert_eq!(controller.model().quotas[0].detail, current_detail);

    let mut late_same_generation = fixture.top_bar.clone();
    late_same_generation.quotas[0].detail = Some("1% remaining".into());
    late_same_generation.quotas[0].identity.observation_id = late_same_generation.quotas[0]
        .identity
        .observation_id
        .saturating_sub(1);
    late_same_generation.quotas[0].observed_at_ms = Some(late_same_generation.now_ms);
    assert!(!controller
        .apply(late_same_generation)
        .expect("late same-generation event must be handled"));
    assert_eq!(controller.model().quotas[0].detail, current_detail);

    let mut newer_sequence = fixture.top_bar.clone();
    newer_sequence.quotas[0].identity.observation_id = 42;
    newer_sequence.quotas[0].observed_at_ms = Some(fixture.top_bar.now_ms - 100);
    newer_sequence.quotas[0].detail = Some("2% remaining".into());
    assert!(controller
        .apply(newer_sequence)
        .expect("newer same-generation sequence must be admitted"));
    assert_eq!(controller.model().quotas[0].detail, "2% remaining");

    let mut replacement_session = fixture.top_bar.clone();
    replacement_session.quotas[0].identity.provider_session_id = "new-provider-session".into();
    replacement_session.quotas[0].identity.observation_id = 43;
    replacement_session.quotas[0].observed_at_ms = Some(fixture.top_bar.now_ms - 50);
    replacement_session.quotas[0].detail = Some("4% remaining".into());
    assert!(controller
        .apply(replacement_session)
        .expect("newer provider session sequence must be admitted"));
    assert_eq!(controller.model().quotas[0].detail, "4% remaining");

    let mut late_previous_session = fixture.top_bar.clone();
    late_previous_session.quotas[0].identity.observation_id = 42;
    late_previous_session.quotas[0].observed_at_ms = Some(fixture.top_bar.now_ms);
    late_previous_session.quotas[0].detail = Some("1% remaining".into());
    assert!(!controller
        .apply(late_previous_session)
        .expect("late previous-session event must be rejected"));
    assert_eq!(controller.model().quotas[0].detail, "4% remaining");

    let mut same_sequence_older_timestamp = fixture.top_bar.clone();
    same_sequence_older_timestamp.quotas[0]
        .identity
        .provider_session_id = "new-provider-session".into();
    same_sequence_older_timestamp.quotas[0]
        .identity
        .observation_id = 43;
    same_sequence_older_timestamp.quotas[0].observed_at_ms = Some(fixture.top_bar.now_ms - 200);
    same_sequence_older_timestamp.quotas[0].detail = Some("1% remaining".into());
    assert!(!controller
        .apply(same_sequence_older_timestamp)
        .expect("older timestamp for the same sequence must be rejected"));
    assert_eq!(controller.model().quotas[0].detail, "4% remaining");

    let mut older_clock = fixture.top_bar.clone();
    older_clock.now_ms = fixture.top_bar.now_ms - 1;
    older_clock.quotas[0].identity.provider_session_id = "new-provider-session".into();
    older_clock.quotas[0].identity.observation_id = 43;
    older_clock.quotas[0].observed_at_ms = Some(older_clock.now_ms);
    older_clock.quotas[0].detail = Some("3% remaining".into());
    assert!(!controller
        .apply(older_clock)
        .expect("older host clock observation must be rejected"));
    assert_eq!(controller.model().quotas[0].detail, "4% remaining");

    let mut newer_generation = fixture.top_bar.clone();
    newer_generation.generation += 1;
    newer_generation.quotas[0].identity.observation_id = 1;
    newer_generation.quotas[0].observed_at_ms = Some(newer_generation.now_ms);
    newer_generation.quotas[0].generation = Some(newer_generation.generation);
    newer_generation.quotas[0].detail = Some("33% remaining".into());
    assert!(controller
        .apply(newer_generation.clone())
        .expect("new generation must be admitted"));
    assert_eq!(controller.model().quotas[0].detail, "33% remaining");

    let mut old_generation = newer_generation;
    old_generation.generation -= 1;
    old_generation.quotas[0].generation = Some(old_generation.generation);
    old_generation.quotas[0].identity.observation_id = u64::MAX;
    old_generation.quotas[0].detail = Some("99% remaining".into());
    assert!(!controller
        .apply(old_generation)
        .expect("old generation must be rejected without error"));
    assert_eq!(controller.model().quotas[0].detail, "33% remaining");
    assert_ne!(controller.generation(), 0);
}

#[test]
fn top_bar_controller_keeps_each_field_at_its_source_high_water_revision() {
    let fixture = fixture();
    let mut controller = TopBarProjectionController::new(fixture.top_bar.clone())
        .expect("fixture top bar must pass controller preflight");

    let mut newer = fixture.top_bar.clone();
    newer.now_ms += 1;
    newer.host.as_mut().unwrap().identity.revision += 1;
    newer.host.as_mut().unwrap().health = devmanager::ui::task_cockpit::HostHealth::Degraded;
    newer.connect.as_mut().unwrap().identity.connection_epoch += 1;
    newer.connect.as_mut().unwrap().state = devmanager::ui::task_cockpit::ConnectState::Connecting;
    newer.update.as_mut().unwrap().identity.revision = 2;
    newer.update.as_mut().unwrap().state = devmanager::ui::task_cockpit::UpdateState::Checking;
    newer.resources.as_mut().unwrap().cpu_revision = 2;
    newer.resources.as_mut().unwrap().cpu_percent = Some(25.0);
    newer.resources.as_mut().unwrap().memory_revision = 2;
    newer.resources.as_mut().unwrap().memory_bytes = Some(22_000_000);
    assert!(controller.apply(newer).expect("newer source fields"));

    let mut mixed = fixture.top_bar;
    mixed.now_ms += 2;
    mixed.host.as_mut().unwrap().identity.revision = 17;
    mixed.host.as_mut().unwrap().health = devmanager::ui::task_cockpit::HostHealth::Unavailable;
    mixed.connect.as_mut().unwrap().identity.connection_epoch = 5;
    mixed.connect.as_mut().unwrap().state =
        devmanager::ui::task_cockpit::ConnectState::Disconnected;
    mixed.update.as_mut().unwrap().identity.revision = 1;
    mixed.update.as_mut().unwrap().state = devmanager::ui::task_cockpit::UpdateState::Error;
    mixed.resources.as_mut().unwrap().cpu_revision = 1;
    mixed.resources.as_mut().unwrap().cpu_percent = Some(99.0);
    mixed.resources.as_mut().unwrap().memory_revision = 3;
    mixed.resources.as_mut().unwrap().memory_bytes = Some(33_000_000);
    assert!(controller.apply(mixed).expect("mixed source fields"));

    let model = controller.model();
    assert_eq!(
        model.host.as_ref().map(|link| link.status),
        Some(devmanager::ui::task_cockpit::TopBarStatus::Host(
            devmanager::ui::task_cockpit::HostHealth::Degraded
        ))
    );
    assert_eq!(
        model.connect.as_ref().map(|link| link.status),
        Some(devmanager::ui::task_cockpit::TopBarStatus::Connect(
            devmanager::ui::task_cockpit::ConnectState::Disconnected
        ))
    );
    assert_eq!(
        model.update.as_ref().map(|link| link.status),
        Some(devmanager::ui::task_cockpit::TopBarStatus::Update(
            devmanager::ui::task_cockpit::UpdateState::Checking
        ))
    );
    let resources = model.resources.expect("resource projection");
    assert_eq!(resources.cpu.unwrap().whole_machine_percent, 25.0 / 64.0);
    assert_eq!(resources.memory_bytes, Some(33_000_000));
}

#[test]
fn top_bar_controller_does_not_revive_resource_after_missing_bundle() {
    let fixture = fixture();
    let mut controller = TopBarProjectionController::new(fixture.top_bar.clone())
        .expect("fixture top bar must pass controller preflight");

    let mut missing = fixture.top_bar.clone();
    missing.now_ms += 1;
    missing.resources = None;
    assert!(controller.apply(missing).expect("missing resource bundle"));
    assert!(controller.model().resources.is_none());

    let mut delayed = fixture.top_bar;
    delayed.now_ms += 2;
    delayed.resources.as_mut().expect("resources").cpu_revision = 0;
    delayed
        .resources
        .as_mut()
        .expect("resources")
        .memory_revision = 0;
    controller
        .apply(delayed)
        .expect("delayed resource bundle must be ignored");
    assert!(controller.model().resources.is_none());
}

#[test]
fn top_bar_controller_keeps_remote_source_at_its_high_water_revision() {
    let fixture = fixture();
    let initial_remote = RemoteObservation {
        identity: RemoteObservationIdentity {
            source_id: "remote-host".to_string(),
            revision: 10,
        },
        health: RemoteHealth::Healthy,
        label: "Remote host healthy".to_string(),
        observed_at_ms: Some(fixture.top_bar.now_ms),
        generation: Some(fixture.top_bar.generation),
    };
    let mut controller =
        TopBarProjectionController::new_with_remote(fixture.top_bar.clone(), Some(initial_remote))
            .expect("fixture top bar must pass controller preflight");

    let mut mixed = fixture.top_bar;
    mixed.now_ms += 1;
    let stale_remote = RemoteObservation {
        identity: RemoteObservationIdentity {
            source_id: "remote-host".to_string(),
            revision: 9,
        },
        health: RemoteHealth::Unavailable,
        label: "Remote host unavailable".to_string(),
        observed_at_ms: Some(mixed.now_ms),
        generation: Some(mixed.generation),
    };
    assert!(controller
        .apply_with_remote(mixed, Some(stale_remote))
        .expect("mixed remote bundle"));
    assert_eq!(
        controller.remote().map(|remote| remote.health),
        Some(RemoteHealth::Healthy)
    );
}

#[test]
fn top_bar_input_is_preflight_bounded_before_projection_copy_or_truncation() {
    let fixture = fixture();
    let mut oversized = fixture.top_bar;
    oversized.update.as_mut().unwrap().identity.current_version = "x".repeat(16 * 1024);

    assert!(oversized.preflight().is_err());
    assert!(TopBarModel::try_from_input(&oversized).is_err());
    assert!(TopBarProjectionController::new(oversized).is_err());
}

#[test]
fn header_layout_contract_is_bounded_and_accessible_at_all_widths() {
    let fixture = fixture();
    let model = model_from_pages(&fixture.snapshot_pages);
    let header = project_header(&model, fixture.selected_task_id);

    let narrow_layout = header.responsive_layout(320);
    assert_eq!(
        narrow_layout.inline,
        vec![HeaderField::Title, HeaderField::TurnStatus]
    );
    assert_eq!(
        narrow_layout.overflow,
        vec![
            HeaderField::Project,
            HeaderField::Workspace,
            HeaderField::Primary,
            HeaderField::Specialists,
        ]
    );
    assert_eq!(
        narrow_layout.overflow_control.as_ref().unwrap().role,
        AccessibleRole::Button
    );
    assert!(narrow_layout.overflow_control.as_ref().unwrap().focusable);
    assert!(narrow_layout
        .accessible_description
        .contains("Ship the native cockpit"));

    let medium_layout = header.responsive_layout(480);
    assert_eq!(
        medium_layout.inline,
        vec![
            HeaderField::Title,
            HeaderField::TurnStatus,
            HeaderField::Project,
            HeaderField::Primary,
        ]
    );
    assert_eq!(
        medium_layout.overflow,
        vec![HeaderField::Workspace, HeaderField::Specialists]
    );

    let wide_layout = header.responsive_layout(720);
    assert!(wide_layout.overflow.is_empty());
    assert!(wide_layout
        .accessible_description
        .contains("Project 018f60b0-9c1a-7001-8000-000000000011"));
    assert!(wide_layout
        .accessible_description
        .contains("worktree codex/header"));
}

#[test]
fn shell_requires_nonzero_host_epochs_and_rejects_stale_action_epoch() {
    let fixture = fixture();
    assert_eq!(
        Shell::attach(Some(fixture.selected_task_id), None),
        Err(ShellAttachmentError::Unavailable)
    );
    for zero_index in 0..5 {
        let mut values = [3, 5, 7, 11, 13];
        values[zero_index] = 0;
        assert!(
            HostEpochSnapshot::try_from_host(values[0], values[1], values[2], values[3], values[4])
                .is_err(),
            "zero host epoch at index {zero_index} must be rejected"
        );
    }

    let model = model_from_pages(&fixture.snapshot_pages);
    let shell = attached_shell(
        Some(fixture.selected_task_id),
        model.last_applied_sequence(),
    );
    let action = shell
        .task_header(&model)
        .expect("attached shell projects current task")
        .status
        .action
        .clone();
    let mut changed_pages = fixture.snapshot_pages;
    for page in &mut changed_pages {
        for item in &mut page.items {
            if let SnapshotItem::Task(task) = item {
                task.task.action_epoch += 1;
            }
        }
    }
    let changed_model = model_from_pages(&changed_pages);
    assert!(!shell.dispatch_task_action(&changed_model, &action));
}

#[test]
fn quota_controller_bounds_cache_under_provider_churn_before_insertion() {
    let fixture = fixture();
    let mut controller = TopBarProjectionController::new(fixture.top_bar.clone())
        .expect("fixture top bar must pass controller preflight");
    for index in 0..128u64 {
        let mut input = fixture.top_bar.clone();
        let quota = input.quotas[0].clone();
        let mut quota = quota;
        quota.identity.provider = format!("provider-{index:03}");
        quota.identity.provider_session_id = format!("session-{index:03}");
        quota.identity.observation_id = 10_000 + index;
        quota.detail = Some(format!("{}% remaining", index % 100));
        input.quotas = vec![quota];
        assert!(controller.apply(input).is_ok());
    }
    assert!(controller.cached_quota_count() <= 64);
    let cached_count = controller.cached_quota_count();
    let mut rejected = fixture.top_bar;
    rejected.quotas[0].identity.provider = "provider-rejected".into();
    rejected.quotas[0].identity.provider_session_id = "session-rejected".into();
    rejected.quotas[0].identity.observation_id = 0;
    rejected.quotas[0].observed_at_ms = Some(rejected.now_ms);
    rejected.quotas[0].detail = Some("0% remaining".into());
    assert!(!controller
        .apply(rejected)
        .expect("older full-cache observation must be rejected"));
    assert_eq!(controller.cached_quota_count(), cached_count);
    assert!(controller.model().quotas.len() <= 8);
}

#[test]
fn quota_ordering_survives_bounded_cache_eviction_for_same_session() {
    let fixture = fixture();
    let mut controller = TopBarProjectionController::new(fixture.top_bar.clone())
        .expect("fixture top bar must pass controller preflight");

    let mut advanced_clock = fixture.top_bar.clone();
    advanced_clock.now_ms += PROVIDER_QUOTA_MAX_AGE_MS;
    assert!(controller
        .apply(advanced_clock.clone())
        .expect("clock advance must be admitted"));
    assert_eq!(controller.cached_quota_count(), 0);

    let mut delayed = advanced_clock;
    delayed.quotas[0].identity.observation_id = fixture.top_bar.quotas[0]
        .identity
        .observation_id
        .saturating_sub(1);
    delayed.quotas[0].observed_at_ms = Some(delayed.now_ms);
    delayed.quotas[0].detail = Some("99% remaining".into());
    assert!(!controller
        .apply(delayed)
        .expect("delayed same-session observation must be handled"));
    assert_eq!(controller.cached_quota_count(), 0);
    assert!(controller.model().quotas.is_empty());
}

#[test]
fn quota_session_replacement_accepts_reset_counter_only_from_current_host_snapshot() {
    let fixture = fixture();
    let mut controller = TopBarProjectionController::new(fixture.top_bar.clone())
        .expect("fixture top bar must pass controller preflight");
    let mut replacement = fixture.top_bar.clone();
    replacement.quotas[0].identity.provider_session_id = "replacement".into();
    replacement.quotas[0].identity.observation_id = 0;
    replacement.quotas[0].observed_at_ms = Some(replacement.now_ms);
    replacement.quotas[0].detail = Some("40% remaining".into());
    assert!(controller.apply(replacement.clone()).expect("replacement"));

    let mut delayed_old = replacement.clone();
    delayed_old.quotas[0].identity.provider_session_id = fixture.top_bar.quotas[0]
        .identity
        .provider_session_id
        .clone();
    delayed_old.quotas[0].identity.observation_id = u64::MAX;
    delayed_old.quotas[0].observed_at_ms = Some(delayed_old.now_ms);
    delayed_old.quotas[0].detail = Some("99% remaining".into());
    assert!(!controller.apply(delayed_old).expect("delayed old session"));
    assert_eq!(controller.model().quotas[0].detail, "40% remaining");
}
