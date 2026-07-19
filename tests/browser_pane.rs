use devmanager::browser::{
    browser_action_plan, browser_annotation_preview_plan, browser_content_bounds,
    browser_event_plan, browser_host_reconcile_plan, browser_host_visibility,
    browser_pane_eligible, browser_pane_open_fallback, browser_response_sync,
    browser_settings_plan, calculate_browser_split, normalize_browser_address, BrowserAnnotation,
    BrowserApprovalRequest, BrowserBounds, BrowserCommand, BrowserError, BrowserHostEvent,
    BrowserHostState, BrowserHostVisibility, BrowserInvocationActor, BrowserJournalActor,
    BrowserJournalEntry, BrowserPaneAction, BrowserPaneContext, BrowserPaneEventPlan,
    BrowserPaneModel, BrowserPaneSurface, BrowserPaneTransient, BrowserResponse, BrowserRisk,
    BrowserSettingsAction, BrowserTabSnapshot, BrowserUserInputKind, BrowserViewport,
    BrowserViewportPreset, BrowserWorkspaceKey, BrowserWorkspaceMutation, BrowserWorkspaceSnapshot,
};
use std::path::PathBuf;

fn context(surface: BrowserPaneSurface) -> BrowserPaneContext {
    BrowserPaneContext {
        browser_enabled: true,
        platform_supported: true,
        active_surface: Some(surface),
        editor_open: false,
        modal_open: false,
    }
}

fn pending_annotation(id: &str, tab_id: &str, url: &str) -> BrowserAnnotation {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "kind": "element",
        "tabId": tab_id,
        "anchorRevision": 1,
        "comment": "Review this",
        "url": url,
        "locator": {},
        "bounds": { "x": 1, "y": 2, "width": 3, "height": 4 },
        "viewport": {},
        "screenshotResource": format!("shot-{id}"),
        "computedStyles": {},
        "resolved": false
    }))
    .expect("pending annotation fixture")
}

#[test]
fn annotation_preview_selects_and_conditionally_navigates_an_existing_tab_without_consuming() {
    let key = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let saved_url = "https://example.test/saved?token=redacted";
    let pending = vec![pending_annotation("ann-a", "tab-a", saved_url)];
    let snapshot = BrowserWorkspaceSnapshot {
        pane_open: false,
        tabs: vec![
            BrowserTabSnapshot {
                id: "tab-a".to_string(),
                title: "A".to_string(),
                url: "https://example.test/current".to_string(),
                viewport: BrowserViewport::default(),
            },
            BrowserTabSnapshot {
                id: "tab-b".to_string(),
                title: "B".to_string(),
                url: "https://other.test".to_string(),
                viewport: BrowserViewport::default(),
            },
        ],
        selected_tab_id: Some("tab-b".to_string()),
        ..BrowserWorkspaceSnapshot::default()
    };
    let before = pending.clone();

    let plan =
        browser_annotation_preview_plan(Some(&key), &key, Some(&snapshot), &pending, "ann-a")
            .unwrap();

    assert!(matches!(plan.commands[0], BrowserCommand::Ensure { .. }));
    assert_eq!(plan.commands[1], BrowserCommand::SetPaneOpen { open: true });
    assert_eq!(
        plan.commands[2],
        BrowserCommand::SelectTab {
            tab_id: "tab-a".to_string()
        }
    );
    assert_eq!(
        plan.commands[3],
        BrowserCommand::Navigate {
            tab_id: "tab-a".to_string(),
            url: saved_url.to_string(),
        }
    );
    assert_eq!(pending, before, "preview must not consume pending context");

    let already_selected = BrowserWorkspaceSnapshot {
        pane_open: true,
        tabs: vec![BrowserTabSnapshot {
            id: "tab-a".to_string(),
            title: "A".to_string(),
            url: saved_url.to_string(),
            viewport: BrowserViewport::default(),
        }],
        selected_tab_id: Some("tab-a".to_string()),
        ..BrowserWorkspaceSnapshot::default()
    };
    let plan = browser_annotation_preview_plan(
        Some(&key),
        &key,
        Some(&already_selected),
        &pending,
        "ann-a",
    )
    .unwrap();
    assert_eq!(plan.commands.len(), 1);
    assert!(matches!(plan.commands[0], BrowserCommand::Ensure { .. }));
}

#[test]
fn annotation_preview_treats_a_persisted_redacted_url_as_the_current_live_url() {
    let key = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let live_url = "https://example.test/form?token=super-secret&view=review";
    let saved_url = devmanager::browser::redact_browser_text(live_url);
    assert_ne!(
        saved_url, live_url,
        "fixture must model persistence redaction"
    );
    assert!(!saved_url.contains("super-secret"));
    let pending = vec![pending_annotation("ann-a", "tab-a", &saved_url)];
    let snapshot = BrowserWorkspaceSnapshot {
        pane_open: true,
        tabs: vec![
            BrowserTabSnapshot {
                id: "tab-a".to_string(),
                title: "Annotated page".to_string(),
                url: live_url.to_string(),
                viewport: BrowserViewport::default(),
            },
            BrowserTabSnapshot {
                id: "tab-b".to_string(),
                title: "Selected page".to_string(),
                url: "https://other.test".to_string(),
                viewport: BrowserViewport::default(),
            },
        ],
        selected_tab_id: Some("tab-b".to_string()),
        ..BrowserWorkspaceSnapshot::default()
    };

    let plan =
        browser_annotation_preview_plan(Some(&key), &key, Some(&snapshot), &pending, "ann-a")
            .unwrap();

    assert!(matches!(plan.commands[0], BrowserCommand::Ensure { .. }));
    assert_eq!(
        plan.commands[1],
        BrowserCommand::SelectTab {
            tab_id: "tab-a".to_string()
        }
    );
    assert!(!plan
        .commands
        .iter()
        .any(|command| matches!(command, BrowserCommand::Navigate { .. })));
}

#[test]
fn annotation_preview_creates_a_missing_tab_at_the_saved_url() {
    let key = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let saved_url = "https://example.test/saved";
    let pending = vec![pending_annotation("ann-a", "missing-tab", saved_url)];
    let snapshot = BrowserWorkspaceSnapshot::default();

    let plan =
        browser_annotation_preview_plan(Some(&key), &key, Some(&snapshot), &pending, "ann-a")
            .unwrap();

    assert!(matches!(plan.commands[0], BrowserCommand::Ensure { .. }));
    assert_eq!(plan.commands[1], BrowserCommand::SetPaneOpen { open: true });
    assert_eq!(
        plan.commands[2],
        BrowserCommand::CreateTab {
            url: Some(saved_url.to_string())
        }
    );
    assert!(!plan.commands.iter().any(|command| matches!(
        command,
        BrowserCommand::SelectTab { .. } | BrowserCommand::Navigate { .. }
    )));
}

#[test]
fn annotation_preview_rejects_cross_workspace_and_no_longer_pending_actions() {
    let key = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let other = BrowserWorkspaceKey::new("project-b", "conversation-b").unwrap();
    let pending = vec![pending_annotation("ann-a", "tab-a", "https://example.test")];

    for (action_key, annotation_id) in [(&other, "ann-a"), (&key, "ann-stale")] {
        assert!(matches!(
            browser_annotation_preview_plan(
                Some(&key),
                action_key,
                Some(&BrowserWorkspaceSnapshot::default()),
                &pending,
                annotation_id,
            ),
            Err(BrowserError::MissingAnnotation { .. })
        ));
    }
}

#[test]
fn pane_is_eligible_only_for_enabled_unobscured_ai_conversations() {
    assert!(browser_pane_eligible(&context(BrowserPaneSurface::Claude)));
    assert!(browser_pane_eligible(&context(BrowserPaneSurface::Codex)));

    for surface in [BrowserPaneSurface::Server, BrowserPaneSurface::Ssh] {
        assert!(!browser_pane_eligible(&context(surface)));
    }

    let mut disabled = context(BrowserPaneSurface::Claude);
    disabled.browser_enabled = false;
    assert!(!browser_pane_eligible(&disabled));

    let mut unsupported = context(BrowserPaneSurface::Codex);
    unsupported.platform_supported = false;
    assert!(!browser_pane_eligible(&unsupported));

    let mut editor = context(BrowserPaneSurface::Claude);
    editor.editor_open = true;
    assert!(!browser_pane_eligible(&editor));

    let mut modal = context(BrowserPaneSurface::Codex);
    modal.modal_open = true;
    assert!(!browser_pane_eligible(&modal));

    let mut no_tab = context(BrowserPaneSurface::Claude);
    no_tab.active_surface = None;
    assert!(!browser_pane_eligible(&no_tab));
}

#[test]
fn address_normalization_accepts_only_allowed_browser_destinations() {
    let accepted = [
        (" about:blank ", "about:blank"),
        ("http://example.test/path", "http://example.test/path"),
        ("https://example.test/path", "https://example.test/path"),
        ("localhost:3000/app", "http://localhost:3000/app"),
        ("127.0.0.1:5173", "http://127.0.0.1:5173"),
        ("[::1]:8080/health", "http://[::1]:8080/health"),
        ("::1", "http://[::1]"),
        ("devbox.local/path", "http://devbox.local/path"),
        ("example.com/path", "https://example.com/path"),
    ];
    for (input, expected) in accepted {
        assert_eq!(normalize_browser_address(input).unwrap(), expected);
    }

    for rejected in [
        "",
        "   ",
        "file:///C:/secret.txt",
        "javascript:alert(1)",
        "ftp://example.com/file",
        "words that are not a host",
    ] {
        assert!(matches!(
            normalize_browser_address(rejected),
            Err(BrowserError::NavigationFailure { .. })
        ));
    }
}

#[test]
fn split_and_content_geometry_stay_bounded_at_normal_and_narrow_widths() {
    let centered = calculate_browser_split(1000.0, 50, 300.0, 320.0, 6.0);
    assert_eq!(centered.total_width, 1000.0);
    assert!(centered.terminal_width >= 300.0);
    assert!(centered.pane_width >= 320.0);
    assert_eq!(
        centered.terminal_width + centered.divider_width + centered.pane_width,
        centered.total_width
    );

    let pane_heavy = calculate_browser_split(1000.0, 99, 300.0, 320.0, 6.0);
    assert_eq!(pane_heavy.split_percent, 75);
    assert_eq!(pane_heavy.terminal_width, 300.0);

    let too_narrow = calculate_browser_split(200.0, 50, 300.0, 320.0, 6.0);
    assert!(too_narrow.terminal_width >= 0.0);
    assert!(too_narrow.pane_width >= 0.0);
    assert!(too_narrow.divider_width >= 0.0);
    assert_eq!(
        too_narrow.terminal_width + too_narrow.divider_width + too_narrow.pane_width,
        too_narrow.total_width
    );

    let pane = BrowserBounds {
        x: 400,
        y: 20,
        width: 600,
        height: 500,
    };
    assert_eq!(
        browser_content_bounds(pane, 84),
        Some(BrowserBounds {
            x: 400,
            y: 104,
            width: 600,
            height: 416,
        })
    );
    assert_eq!(browser_content_bounds(pane, 500), None);
}

#[test]
fn native_shell_awaits_browser_commands_in_a_window_local_main_thread_task() {
    let source = include_str!("../src/app/mod.rs");

    assert!(source.contains("browser_host: BrowserWebViewHost"));
    assert!(source.contains("browser_bridge: BrowserCommandBridge"));
    assert!(source.contains("browser_inbox: Option<BrowserCommandInbox>"));
    assert!(source.contains(".spawn(cx, move |cx: &mut gpui::AsyncWindowContext|"));
    assert!(source.contains("inbox.recv().await"));
    assert!(source.contains("this.update_in("));
    assert!(!source.contains("Arc<Mutex<BrowserWebViewHost>>"));
}

#[test]
fn native_browser_storage_never_treats_the_process_cwd_as_trusted_config() {
    let source = include_str!("../src/app/mod.rs");
    assert!(!source.contains(
        "crate::persistence::app_config_dir().unwrap_or_else(|_| std::path::PathBuf::from(\".\"))"
    ));
    assert!(source.contains("BrowserWebViewHost::unavailable"));
    assert!(source.contains("Browser configuration is unavailable"));
    assert!(source.contains("browser_app_config_dir: Option<std::path::PathBuf>"));
    let reconcile = source.find("fn reconcile_browser_gateway").unwrap();
    let reconcile_end = source[reconcile..]
        .find("fn open_browser_workspace_keys")
        .unwrap()
        + reconcile;
    let reconcile = &source[reconcile..reconcile_end];
    assert!(reconcile.contains("self.browser_app_config_dir.as_ref()"));
    assert!(!reconcile.contains("persistence::app_config_dir()"));
}

#[test]
fn native_shell_routes_mcp_requests_through_the_async_host_queue() {
    let source = include_str!("../src/app/mod.rs");
    let start = source
        .find("fn handle_browser_request(")
        .expect("browser request handler should exist");
    let end = source[start..]
        .find("fn pump_browser_events(")
        .map(|offset| start + offset)
        .expect("browser event pump should follow request handler");
    let handler = &source[start..end];

    assert!(handler.contains("route_browser_request("));
    assert!(handler.contains("browser_host.handle_request(window, request)"));
    assert!(handler.contains("with_browser_host_control_barrier"));
    assert!(!handler.contains("dispatch_browser_command"));
}

#[test]
fn native_shell_applies_host_input_before_async_browser_completions() {
    let source = include_str!("../src/app/mod.rs");
    let pump_start = source
        .find("fn pump_browser_events")
        .expect("browser event pump should exist");
    let pump_end = source[pump_start..]
        .find("fn with_browser_host_control_barrier")
        .map(|offset| pump_start + offset)
        .expect("host-control barrier should follow the event pump");
    let body = &source[pump_start..pump_end];
    let completions = body
        .find("browser_host.pump_async_completions(window)")
        .expect("the GPUI pump should drain async WebView2 completions");
    let events_before = body
        .find("browser_host.drain_events_with_pre_apply_observer")
        .expect("the GPUI pump should drain host events");
    let events_after = body
        .rfind("browser_host.drain_events_with_pre_apply_observer")
        .expect("the GPUI pump should also drain completion-generated events");

    assert!(events_before < completions);
    assert!(completions < events_after);
    assert!(
        body[events_before..completions].contains("observe_host_event_under_host_control_barrier")
    );
    assert_eq!(
        body.matches("observe_host_event_under_host_control_barrier")
            .count(),
        3,
        "both pump drains and the post-dialog drain already hold the host-control barrier"
    );
}

#[test]
fn native_shell_drains_priority_host_controls_before_async_completions() {
    let source = include_str!("../src/app/mod.rs");
    let pump = source.find("fn pump_browser_events").unwrap();
    let body = &source[pump..];
    let controls = body
        .find("with_browser_host_control_barrier")
        .expect("host-control barrier must cover the GPUI pump");
    let completions = body
        .find("browser_host.pump_async_completions(window)")
        .expect("async completions remain on the GPUI pump");
    assert!(controls < completions);
}

#[test]
fn native_shell_projects_attachments_at_every_local_snapshot_ingress_before_replacement() {
    let source = include_str!("../src/app/mod.rs");

    let response_start = source.find("fn synchronize_browser_response(").unwrap();
    let response_end = source[response_start..]
        .find("fn handle_browser_request(")
        .map(|offset| response_start + offset)
        .unwrap();
    let response = &source[response_start..response_end];
    assert!(
        response.find("project_local_browser_snapshot").unwrap()
            < response.find("update_browser_workspace").unwrap()
    );

    let pump_start = source.find("fn pump_browser_events(").unwrap();
    let pump_end = source[pump_start..]
        .find("fn with_browser_host_control_barrier")
        .map(|offset| pump_start + offset)
        .unwrap();
    let pump = &source[pump_start..pump_end];
    assert!(
        pump.find("project_local_browser_snapshot").unwrap()
            < pump.find("move |current| *current = snapshot").unwrap()
    );

    let constructor =
        &source[source.find("fn new(cx:").unwrap()..source.find("fn start_browser_tasks").unwrap()];
    assert!(
        constructor
            .find("reconcile_restored_browser_attachment_state")
            .unwrap()
            < constructor.find("restore_saved_tabs(").unwrap()
    );
}

#[test]
fn empty_browser_event_pump_reconciles_retryable_dirty_projections_before_return() {
    let source = include_str!("../src/app/mod.rs");
    let start = source.find("fn pump_browser_events(").unwrap();
    let end = source[start..]
        .find("fn with_browser_host_control_barrier")
        .map(|offset| start + offset)
        .unwrap();
    let pump = &source[start..end];
    let reconcile = pump
        .find("reconcile_browser_attachment_projections")
        .unwrap();
    let empty_return = pump.find("if events.is_empty()").unwrap();
    assert!(reconcile < empty_return);

    let transaction_start = source
        .find("fn reconcile_browser_attachment_projection_transaction(")
        .unwrap();
    let transaction_end = source[transaction_start..]
        .find("struct NativeShellBrowserAttachmentProjectionSink")
        .map(|offset| transaction_start + offset)
        .unwrap();
    let transaction = &source[transaction_start..transaction_end];
    let host = transaction.find("sink.acknowledge_host").unwrap();
    let persist = transaction.find("sink.persist_snapshot").unwrap();
    let acknowledge = transaction.find("acknowledge_dirty_projection").unwrap();
    assert!(host < persist && persist < acknowledge);

    let sink_start = source
        .find("impl BrowserAttachmentProjectionSink for NativeShellBrowserAttachmentProjectionSink")
        .unwrap();
    let sink_end = source[sink_start..]
        .find("impl NativeShell {")
        .map(|offset| sink_start + offset)
        .unwrap();
    let sink = &source[sink_start..sink_end];
    assert!(sink.contains("with_browser_host_control_barrier"));
    assert!(sink.contains("save_session_state()"));
}

#[test]
fn remote_client_snapshot_merge_never_overlays_the_local_attachment_broker() {
    let source = include_str!("../src/app/mod.rs");
    let start = source.find("fn merge_remote_snapshot_into_state(").unwrap();
    let end = source[start..]
        .find("fn remote_has_control(")
        .map(|offset| start + offset)
        .unwrap();
    let merge = &source[start..end];
    assert!(!merge.contains("browser_attachment_broker"));
    assert!(!merge.contains("project_local_browser_snapshot"));
    assert!(!merge.contains("overlay_snapshot"));
}

#[test]
fn remote_disconnect_reconciles_the_local_backup_after_leaving_remote_mode_before_restore() {
    let source = include_str!("../src/app/mod.rs");
    let start = source.find("fn disconnect_remote_host(").unwrap();
    let end = source[start..]
        .find("fn current_runtime_snapshot(")
        .map(|offset| start + offset)
        .unwrap();
    let disconnect = &source[start..end];
    let leave_remote = disconnect.find("self.remote_mode.take()").unwrap();
    let reconcile = disconnect
        .find("reconcile_restored_browser_attachment_state")
        .expect("local backup must be projected through the broker");
    let restore = disconnect.find("self.state = local_state").unwrap();
    assert!(leave_remote < reconcile && reconcile < restore);
}

#[test]
fn synchronous_ui_commands_enter_the_host_inside_the_control_barrier() {
    let source = include_str!("../src/app/mod.rs");
    let start = source.find("fn dispatch_browser_command").unwrap();
    let end = source[start..]
        .find("fn synchronize_browser_response")
        .unwrap()
        + start;
    let dispatch = &source[start..end];
    let barrier = dispatch.find("with_locked_host_work_for_command").unwrap();
    let host_entry = dispatch.find("browser_host.handle_command").unwrap();
    assert!(barrier < host_entry);
}

#[test]
fn synchronous_ui_lifecycle_commands_do_not_leave_duplicate_deferred_controls() {
    let source = include_str!("../src/app/mod.rs");
    let start = source.find("fn apply_browser_pane_action").unwrap();
    let end = source[start..]
        .find("fn apply_browser_settings_action")
        .unwrap()
        + start;
    let handler = &source[start..end];

    assert!(handler.contains("dispatch_browser_command"));
    assert!(!handler.contains("controller.interrupt_tab"));
    assert!(!handler.contains("controller.interrupt_workspace"));
    assert!(!handler.contains("snapshot.advance_revision()"));
}

#[test]
fn native_approval_rechecks_priority_work_and_user_input_after_dialog_before_resume() {
    let source = include_str!("../src/app/mod.rs");
    let start = source
        .find("BrowserPaneEventPlan::ConfirmApproval")
        .unwrap();
    let body = &source[start..];
    let dialog = body.find(".show()").unwrap();
    let barrier = body[dialog..]
        .find("with_browser_host_control_barrier")
        .map(|offset| dialog + offset)
        .expect("priority lifecycle work received during the dialog must be applied");
    let input = body[barrier..]
        .find("browser_host.drain_events_with_pre_apply_observer")
        .map(|offset| barrier + offset)
        .expect("trusted user input received during the dialog must cancel host work");
    let observe = body[input..]
        .find("observe_host_event_under_host_control_barrier(event)")
        .map(|offset| input + offset)
        .unwrap();
    let resume = body.find("browser_host.resolve_approval(").unwrap();
    let resolution_match = body[resume..]
        .find("match resolution")
        .map(|offset| resume + offset)
        .unwrap();
    let atomic_post_dialog = &body[dialog..resolution_match];

    assert!(dialog < barrier);
    assert!(barrier < input);
    assert!(input < observe);
    assert!(observe < resume);
    assert_eq!(
        atomic_post_dialog
            .matches("with_browser_host_control_barrier")
            .count(),
        1,
        "trusted input publication must not fit between event observation and approval resolution"
    );
    assert!(!body[observe..resume].contains("events.extend("));
}

#[test]
fn canceled_buffered_approval_is_filtered_before_the_native_dialog() {
    let source = include_str!("../src/app/mod.rs");
    let start = source
        .find("BrowserPaneEventPlan::ConfirmApproval")
        .unwrap();
    let body = &source[start..];
    let pending = body
        .find("browser_host.is_pending_approval(")
        .expect("stale buffered approvals must be validated against live host state");
    let dialog = body.find("MessageDialog::new()").unwrap();
    assert!(pending < dialog);
    assert!(body[..pending].contains("with_browser_host_control_barrier"));
}

#[test]
fn elevated_agent_work_routes_to_a_redacted_devmanager_confirmation() {
    let key = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let request = BrowserApprovalRequest {
        operation_id: "op-approval".to_string(),
        actor: BrowserInvocationActor::Agent,
        intent: "delete the test account".to_string(),
        effective_risk: BrowserRisk::Destructive,
        action_summary: "click delete account".to_string(),
        origin_url: "https://fixture.test".to_string(),
    };
    let event = BrowserHostEvent::ApprovalRequested {
        workspace_key: key.clone(),
        tab_id: "tab-a".to_string(),
        request: request.clone(),
    };

    assert_eq!(
        browser_event_plan(&[key], &event),
        Some(BrowserPaneEventPlan::ConfirmApproval {
            workspace_key: BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap(),
            tab_id: "tab-a".to_string(),
            request,
        })
    );
}

#[test]
fn pane_model_projects_the_latest_bounded_agent_journal_entries() {
    let key = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let mut snapshot = BrowserWorkspaceSnapshot::default();
    for index in 0..5 {
        snapshot.append_journal_entry(BrowserJournalEntry {
            id: format!("op-{index}"),
            actor: BrowserJournalActor::Agent,
            intent: format!("inspect {index}"),
            url: "https://fixture.test".to_string(),
            started_at: "2026-07-16T00:00:00Z".to_string(),
            duration_ms: 1,
            result: "ok".to_string(),
            resource_ids: Vec::new(),
        });
    }

    let model = BrowserPaneModel::new(
        key,
        &context(BrowserPaneSurface::Codex),
        &snapshot,
        BrowserPaneTransient::default(),
    );
    assert_eq!(
        model
            .journal_entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["op-2", "op-3", "op-4"]
    );
}

#[test]
fn pane_model_tracks_default_open_collapse_and_control_vocabulary() {
    let key = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let mut host = BrowserHostState::new(PathBuf::from("pane-model-test"));
    let initial = host
        .ensure_workspace(key.clone(), BrowserWorkspaceSnapshot::default())
        .unwrap();
    let context = context(BrowserPaneSurface::Claude);

    let closed = BrowserPaneModel::new(
        key.clone(),
        &context,
        &initial.snapshot,
        BrowserPaneTransient {
            address_draft: Some("example.test".to_string()),
            address_cursor: 7,
            address_focused: true,
            ..BrowserPaneTransient::default()
        },
    );
    assert!(closed.eligible);
    assert!(!closed.pane_open);
    assert_eq!(closed.split_percent, 50);
    assert!(closed.selected_tab_id.is_some());
    assert_eq!(closed.address_cursor, 7);

    let opened = host.set_pane_open(&key, true).unwrap();
    assert!(
        BrowserPaneModel::new(
            key.clone(),
            &context,
            &opened.snapshot,
            BrowserPaneTransient::default(),
        )
        .pane_open
    );
    assert_eq!(opened.revision, initial.revision);

    let collapsed = host.set_pane_open(&key, false).unwrap();
    assert!(!collapsed.snapshot.pane_open);
    assert_eq!(collapsed.revision, opened.revision);

    let actions = [
        BrowserPaneAction::Open,
        BrowserPaneAction::Collapse,
        BrowserPaneAction::CreateTab,
        BrowserPaneAction::Back,
        BrowserPaneAction::Forward,
        BrowserPaneAction::Reload,
        BrowserPaneAction::FocusAddress,
        BrowserPaneAction::FocusAnnotation,
        BrowserPaneAction::SubmitAddress,
        BrowserPaneAction::SetViewport(BrowserViewportPreset::Desktop),
        BrowserPaneAction::ToggleAnnotation,
        BrowserPaneAction::StartRecording,
        BrowserPaneAction::OpenDevTools,
        BrowserPaneAction::OpenDownloads,
        BrowserPaneAction::Stop,
        BrowserPaneAction::ResetWorkspace,
        BrowserPaneAction::ClearProjectProfile,
    ];
    assert_eq!(actions.len(), 17);
    assert_eq!(
        browser_pane_open_fallback(&BrowserPaneAction::Open),
        Some(true)
    );
    assert_eq!(
        browser_pane_open_fallback(&BrowserPaneAction::Collapse),
        Some(false)
    );
    assert_eq!(
        browser_pane_open_fallback(&BrowserPaneAction::CreateTab),
        None
    );
}

#[test]
fn annotation_editor_has_an_explicit_native_focus_action() {
    let key = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let snapshot = BrowserWorkspaceSnapshot::default();
    let plan = browser_action_plan(
        Some(&key),
        Some(&snapshot),
        "",
        BrowserPaneAction::FocusAnnotation,
    )
    .unwrap();
    assert!(plan.commands.is_empty());

    let model = BrowserPaneModel::new(
        key.clone(),
        &context(BrowserPaneSurface::Claude),
        &snapshot,
        BrowserPaneTransient {
            annotation_focused: true,
            ..BrowserPaneTransient::default()
        },
    );
    assert!(model.annotation_focused);

    let pane_source = include_str!("../src/browser/pane.rs");
    let editor_start = pane_source.find("let annotation_editor").unwrap();
    let editor_end = pane_source[editor_start..]
        .find(".children(annotation_editor)")
        .unwrap()
        + editor_start;
    let editor = &pane_source[editor_start..editor_end];
    assert!(editor.contains(".on_mouse_down("));
    assert!(editor.contains("action(BrowserPaneAction::FocusAnnotation)"));

    let app_source = include_str!("../src/app/mod.rs");
    let start = app_source
        .find("BrowserPaneAction::FocusAnnotation =>")
        .unwrap();
    let body = &app_source[start..];
    assert!(body.contains("ui.annotation_focused = true"));
    assert!(body.contains("window.focus(&self.browser_annotation_focus)"));
}

#[test]
fn annotation_control_starts_capture_for_the_selected_tab() {
    let key = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let snapshot = BrowserWorkspaceSnapshot {
        tabs: vec![BrowserTabSnapshot {
            id: "tab-a".to_string(),
            title: "Fixture".to_string(),
            url: "https://fixture.test".to_string(),
            viewport: BrowserViewport::default(),
        }],
        selected_tab_id: Some("tab-a".to_string()),
        ..BrowserWorkspaceSnapshot::default()
    };

    let plan = browser_action_plan(
        Some(&key),
        Some(&snapshot),
        "",
        BrowserPaneAction::ToggleAnnotation,
    )
    .unwrap();
    assert!(plan.diagnostic.is_none());
    assert_eq!(plan.commands.len(), 1);
    let encoded = serde_json::to_value(&plan.commands[0]).unwrap();
    assert_eq!(encoded["type"], "setAnnotationMode");
    assert_eq!(encoded["tabId"], "tab-a");
    assert_eq!(encoded["enabled"], true);
}

#[test]
fn annotation_mode_changes_and_route_cancellation_have_distinct_ui_plans() {
    let key = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let open = [key.clone()];

    assert!(matches!(
        browser_event_plan(
            &open,
            &BrowserHostEvent::AnnotationModeChanged {
                workspace_key: key.clone(),
                tab_id: "tab-a".to_string(),
                enabled: false,
            },
        ),
        Some(BrowserPaneEventPlan::AnnotationModeChanged {
            workspace_key,
            enabled: false,
        }) if workspace_key == key
    ));

    assert!(matches!(
        browser_event_plan(
            &open,
            &BrowserHostEvent::AnnotationCanceled {
                workspace_key: key.clone(),
                tab_id: "tab-a".to_string(),
            },
        ),
        Some(BrowserPaneEventPlan::ClearAnnotation { workspace_key }) if workspace_key == key
    ));
}

#[test]
fn native_editor_keeps_ready_drafts_on_mode_off_and_clears_only_on_cancellation() {
    let source = include_str!("../src/app/mod.rs");
    let pump = &source[source.find("fn pump_browser_events").unwrap()..];
    let mode_start = pump
        .find("BrowserPaneEventPlan::AnnotationModeChanged")
        .unwrap();
    let clear_start = pump.find("BrowserPaneEventPlan::ClearAnnotation").unwrap();
    let approval_start = pump.find("BrowserPaneEventPlan::ConfirmApproval").unwrap();
    let mode = &pump[mode_start..clear_start];
    let clear = &pump[clear_start..approval_start];

    assert!(!mode.contains("annotation_draft"));
    assert!(clear.contains("ui.annotation_draft = None"));
    assert!(clear.contains("ui.annotation_comment.clear()"));
}

#[test]
fn host_visibility_is_selected_only_for_an_open_eligible_pane() {
    let key = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let mut snapshot = BrowserWorkspaceSnapshot {
        pane_open: true,
        tabs: vec![
            BrowserTabSnapshot {
                id: "tab-a".to_string(),
                title: "A".to_string(),
                url: "https://a.example".to_string(),
                viewport: BrowserViewport::default(),
            },
            BrowserTabSnapshot {
                id: "tab-b".to_string(),
                title: "B".to_string(),
                url: "https://b.example".to_string(),
                viewport: BrowserViewport::default(),
            },
        ],
        selected_tab_id: Some("tab-b".to_string()),
        ..BrowserWorkspaceSnapshot::default()
    };
    let eligible = context(BrowserPaneSurface::Codex);

    assert_eq!(
        browser_host_visibility(&eligible, &key, &snapshot, false),
        BrowserHostVisibility::Selected {
            workspace_key: key.clone(),
            tab_id: "tab-b".to_string(),
        }
    );

    snapshot.pane_open = false;
    assert_eq!(
        browser_host_visibility(&eligible, &key, &snapshot, false),
        BrowserHostVisibility::Hidden
    );
    snapshot.pane_open = true;

    for mut hidden in [
        context(BrowserPaneSurface::Server),
        context(BrowserPaneSurface::Ssh),
        BrowserPaneContext {
            browser_enabled: false,
            ..eligible
        },
        BrowserPaneContext {
            editor_open: true,
            ..eligible
        },
        BrowserPaneContext {
            modal_open: true,
            ..eligible
        },
    ] {
        assert_eq!(
            browser_host_visibility(&hidden, &key, &snapshot, false),
            BrowserHostVisibility::Hidden
        );
        hidden.active_surface = None;
        assert_eq!(
            browser_host_visibility(&hidden, &key, &snapshot, false),
            BrowserHostVisibility::Hidden
        );
    }

    assert_eq!(
        browser_host_visibility(&eligible, &key, &snapshot, true),
        BrowserHostVisibility::Hidden
    );
    snapshot.selected_tab_id = Some("missing".to_string());
    assert_eq!(
        browser_host_visibility(&eligible, &key, &snapshot, false),
        BrowserHostVisibility::Selected {
            workspace_key: key,
            tab_id: "tab-a".to_string(),
        }
    );
}

#[test]
fn restored_open_workspace_is_ensured_once_without_overwriting_live_host_state() {
    let key = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let persisted = BrowserWorkspaceSnapshot {
        pane_open: true,
        tabs: vec![BrowserTabSnapshot {
            id: "persisted-tab".to_string(),
            title: "Persisted".to_string(),
            url: "https://persisted.example".to_string(),
            viewport: BrowserViewport::default(),
        }],
        selected_tab_id: Some("persisted-tab".to_string()),
        ..BrowserWorkspaceSnapshot::default()
    };

    let restored = browser_host_reconcile_plan(
        &context(BrowserPaneSurface::Claude),
        &key,
        &persisted,
        false,
        None,
    );
    assert_eq!(restored.ensure_snapshot, Some(persisted.clone()));
    assert!(matches!(
        restored.visibility,
        BrowserHostVisibility::Selected { workspace_key, .. } if workspace_key == key
    ));

    let mut newer_live = persisted.clone();
    newer_live.tabs[0].url = "https://newer-live.example".to_string();
    newer_live.advance_revision();
    let routine_sync = browser_host_reconcile_plan(
        &context(BrowserPaneSurface::Claude),
        &key,
        &persisted,
        false,
        Some(&newer_live),
    );
    assert_eq!(routine_sync.ensure_snapshot, None);

    let app_source = include_str!("../src/app/mod.rs");
    assert!(app_source.contains("browser_host_reconcile_plan("));
    assert!(app_source.contains("BrowserCommand::Ensure { snapshot }"));
}

#[test]
fn ui_actions_and_workspace_responses_cannot_cross_route() {
    let active = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let other = BrowserWorkspaceKey::new("project-a", "conversation-b").unwrap();
    let snapshot = BrowserWorkspaceSnapshot {
        tabs: vec![BrowserTabSnapshot {
            id: "tab-a".to_string(),
            title: String::new(),
            url: "https://example.test".to_string(),
            viewport: BrowserViewport::default(),
        }],
        selected_tab_id: Some("tab-a".to_string()),
        ..BrowserWorkspaceSnapshot::default()
    };

    let plan = browser_action_plan(
        Some(&active),
        Some(&snapshot),
        "https://example.test",
        BrowserPaneAction::Back,
    )
    .unwrap();
    assert_eq!(plan.workspace_key, active);
    assert_eq!(
        plan.commands,
        vec![BrowserCommand::Back {
            tab_id: "tab-a".to_string(),
        }]
    );

    let response = BrowserResponse::Workspace {
        mutation: BrowserWorkspaceMutation {
            revision: snapshot.revision,
            snapshot: snapshot.clone(),
        },
    };
    assert!(browser_response_sync(std::slice::from_ref(&active), &other, &response).is_none());
    let sync = browser_response_sync(std::slice::from_ref(&active), &active, &response).unwrap();
    assert_eq!(sync.workspace_key, active);
    assert_eq!(sync.snapshot, snapshot);
}

#[test]
fn tab_selection_plan_rejects_unknown_and_elides_already_selected_tabs() {
    let workspace = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let snapshot = BrowserWorkspaceSnapshot {
        tabs: vec![
            BrowserTabSnapshot {
                id: "tab-a".to_string(),
                title: "A".to_string(),
                url: "https://a.test".to_string(),
                viewport: BrowserViewport::default(),
            },
            BrowserTabSnapshot {
                id: "tab-b".to_string(),
                title: "B".to_string(),
                url: "https://b.test".to_string(),
                viewport: BrowserViewport::default(),
            },
        ],
        selected_tab_id: Some("tab-a".to_string()),
        ..BrowserWorkspaceSnapshot::default()
    };

    assert!(matches!(
        browser_action_plan(
            Some(&workspace),
            Some(&snapshot),
            "",
            BrowserPaneAction::SelectTab("missing-tab".to_string()),
        ),
        Err(BrowserError::InvalidInvocation { field }) if field == "tabId"
    ));

    let selected = browser_action_plan(
        Some(&workspace),
        Some(&snapshot),
        "",
        BrowserPaneAction::SelectTab("tab-a".to_string()),
    )
    .unwrap();
    assert!(selected.commands.is_empty());

    let different = browser_action_plan(
        Some(&workspace),
        Some(&snapshot),
        "",
        BrowserPaneAction::SelectTab("tab-b".to_string()),
    )
    .unwrap();
    assert_eq!(
        different.commands,
        vec![BrowserCommand::SelectTab {
            tab_id: "tab-b".to_string()
        }]
    );
}

#[test]
fn user_input_and_new_window_events_stay_in_the_matching_conversation() {
    let active = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let other = BrowserWorkspaceKey::new("project-a", "conversation-b").unwrap();
    let open = vec![active.clone(), other.clone()];

    let user_input = BrowserHostEvent::UserInput {
        workspace_key: active.clone(),
        tab_id: "tab-a".to_string(),
        kind: BrowserUserInputKind::Keyboard,
    };
    assert_eq!(
        browser_event_plan(&open, &user_input),
        Some(BrowserPaneEventPlan::SyncSnapshot {
            workspace_key: active.clone(),
            tab_id: "tab-a".to_string(),
            interrupt_agent: true,
            loading: None,
        })
    );

    let popup = BrowserHostEvent::NewWindow {
        workspace_key: active.clone(),
        tab_id: "tab-a".to_string(),
        url: "https://example.test/popup".to_string(),
    };
    assert_eq!(
        browser_event_plan(&open, &popup),
        Some(BrowserPaneEventPlan::OpenLogicalTab {
            workspace_key: active,
            url: "https://example.test/popup".to_string(),
        })
    );

    let orphan = BrowserHostEvent::NewWindow {
        workspace_key: BrowserWorkspaceKey::new("project-z", "closed").unwrap(),
        tab_id: "tab-z".to_string(),
        url: "https://example.test/ignored".to_string(),
    };
    assert_eq!(browser_event_plan(&open, &orphan), None);
}

#[test]
fn settings_plans_scope_profile_and_workspace_resets_without_deleting_artifacts() {
    let active = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let same_project = BrowserWorkspaceKey::new("project-a", "conversation-b").unwrap();
    let other_project = BrowserWorkspaceKey::new("project-b", "conversation-c").unwrap();
    let open = vec![active.clone(), same_project.clone(), other_project];

    let clear = browser_settings_plan(
        BrowserSettingsAction::ClearActiveProjectProfile,
        Some(&active),
        &open,
    )
    .unwrap();
    assert_eq!(clear.route_key, active);
    assert_eq!(clear.command, BrowserCommand::ClearProjectProfile);
    assert_eq!(
        clear.reset_workspaces,
        vec![clear.route_key.clone(), same_project]
    );
    assert!(clear.preserve_downloads);
    assert!(clear.preserve_resources);

    let reset = browser_settings_plan(
        BrowserSettingsAction::ResetActiveConversation,
        Some(&clear.route_key),
        &open,
    )
    .unwrap();
    assert_eq!(reset.command, BrowserCommand::ResetWorkspace);
    assert_eq!(reset.reset_workspaces, vec![clear.route_key.clone()]);
    assert!(reset.preserve_downloads);
    assert!(reset.preserve_resources);

    let downloads = browser_settings_plan(
        BrowserSettingsAction::RevealActiveDownloads,
        Some(&clear.route_key),
        &open,
    )
    .unwrap();
    assert_eq!(downloads.command, BrowserCommand::DownloadDirectory);
    assert!(downloads.reset_workspaces.is_empty());
    assert!(downloads.preserve_downloads);
    assert!(downloads.preserve_resources);

    assert!(
        browser_settings_plan(BrowserSettingsAction::ResetActiveConversation, None, &open,)
            .is_err()
    );
}
