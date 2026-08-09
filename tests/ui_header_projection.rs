use std::path::PathBuf;

use devmanager::client::action;
use devmanager::client::{ClientModel, ClientModelBuilder};
use devmanager::domain::agent::{AgentRole, AgentSessionFacts, AgentSessionLifecycle};
use devmanager::domain::id::AgentSessionId;
use devmanager::domain::id::TaskId;
use devmanager::domain::snapshot::{SnapshotItem, SnapshotPage};
use devmanager::domain::task::{TaskActivity, VisibleTaskStatus};
use devmanager::ui::actions::{KeyboardShortcut, ShortcutKey};
use devmanager::ui::components::AccessibleRole;
use devmanager::ui::shell::{PointerButton, Shell};
use devmanager::ui::task_cockpit::header::{
    ActionTarget, CpuInputUnit, ProjectedAction, TaskActionContext, TitleLayout,
};
use devmanager::ui::task_cockpit::{
    HeaderAction, HeaderField, PrimaryAgentProjection, TaskHeaderModel, TopBarModel,
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

#[test]
fn header_projects_task_context_and_stable_agent_status_links() {
    let fixture = fixture();
    let model = model_from_pages(&fixture.snapshot_pages);

    let header = TaskHeaderModel::from_model(&model, fixture.selected_task_id)
        .expect("selected task must project");

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
    assert_eq!(
        header.status.action,
        HeaderAction::new(
            action::ACTION_TASK_SHOW,
            ActionTarget::Task(header.identity),
        )
    );
    assert!(header.status.description.contains("working"));
    assert!(header
        .accessible_description
        .contains("Ship the native cockpit"));
}

#[test]
fn shell_projects_only_its_captured_selected_task() {
    let fixture = fixture();
    let model = model_from_pages(&fixture.snapshot_pages);
    let shell = Shell::new(Some(fixture.selected_task_id));

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
    let header = TaskHeaderModel::from_model(&model, fixture.selected_task_id)
        .expect("selected task must project");

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

    let header = TaskHeaderModel::from_model(&model, fixture.selected_task_id)
        .expect("selected task must project");

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
fn quota_projection_rejects_future_observations_and_accepts_exact_one_hour_boundary() {
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
    assert!(model.quotas.iter().all(|quota| quota.age_ms >= 0));

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
fn header_and_top_bar_actions_share_one_descriptor_and_reject_stale_identity() {
    let fixture = fixture();
    let model = model_from_pages(&fixture.snapshot_pages);
    let header = TaskHeaderModel::from_model(&model, fixture.selected_task_id).unwrap();
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

    let stale_task = ProjectedAction::new(
        action::ACTION_TASK_SHOW,
        ActionTarget::Task(devmanager::ui::task_cockpit::TaskIdentity {
            revision: header.identity.revision + 1,
            ..header.identity
        }),
    );
    assert!(!header.accepts_action(&stale_task));

    let ActionTarget::Agent(mut stale_agent) = primary.action.target().clone() else {
        panic!("primary action target");
    };
    stale_agent.resource_generation += 1;
    assert!(!header.accepts_action(&ProjectedAction::new(
        action::ACTION_TASK_SHOW,
        ActionTarget::Agent(stale_agent),
    )));

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
                SnapshotItem::AgentSession(agent) => {
                    agent.provider_kind = "provider-secret-sentinel".into();
                }
                _ => {}
            }
        }
    }
    let model = model_from_pages(&pages);
    let header = TaskHeaderModel::from_model(&model, fixture.selected_task_id).unwrap();
    assert!(header.title.chars().count() <= 160);
    assert!(header
        .title
        .chars()
        .all(|character| !character.is_control()));
    assert!(!header.title.contains("HEADER_SECRET_SENTINEL"));
    assert!(!header
        .accessible_description
        .contains("HEADER_PATH_SECRET_SENTINEL"));
    assert!(!header
        .accessible_description
        .contains("provider-secret-sentinel"));

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
fn task_actions_capture_all_context_epochs_and_reject_stale_context() {
    let fixture = fixture();
    let model = model_from_pages(&fixture.snapshot_pages);
    let context = TaskActionContext {
        resource_generation: 12,
        connection_epoch: 4,
        focus_epoch: 8,
    };
    let header =
        TaskHeaderModel::from_model_with_context(&model, fixture.selected_task_id, context)
            .expect("selected task must project");

    assert_eq!(header.identity.task_id, fixture.selected_task_id);
    assert_eq!(header.identity.revision, 9);
    assert_eq!(header.identity.resource_generation, 12);
    assert_eq!(header.identity.connection_epoch, 4);
    assert_eq!(header.identity.focus_epoch, 8);
    assert_eq!(header.identity.action_epoch, 4);

    let stale = ProjectedAction::new(
        action::ACTION_TASK_SHOW,
        ActionTarget::Task(devmanager::ui::task_cockpit::TaskIdentity {
            focus_epoch: 9,
            ..header.identity
        }),
    );
    assert!(!header.accepts_action(&stale));
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
    let header = TaskHeaderModel::from_model(&model, fixture.selected_task_id).unwrap();
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
                provider_kind: "codex".into(),
                provider_session_id: Some(format!("codex-specialist-{index:02}")),
                lifecycle: AgentSessionLifecycle::Open,
                runtime_generation: 1,
                revision: 1,
            }));
    }

    let model = model_from_pages(&pages);
    let header = TaskHeaderModel::from_model(&model, task_id).unwrap();
    assert_eq!(header.specialist_total, 34);
    assert_eq!(header.specialists.len(), MAX_HEADER_SPECIALISTS);
    assert_eq!(header.specialist_hidden_count, 2);
    assert!(header.specialists_truncated);
    assert!(header
        .accessible_description
        .contains("34 specialists shown, 2 hidden"));

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
    let header = TaskHeaderModel::from_model(&model, fixture.selected_task_id).unwrap();

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
