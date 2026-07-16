use devmanager::browser::{
    browser_action_plan, browser_content_bounds, browser_event_plan, browser_host_reconcile_plan,
    browser_host_visibility, browser_pane_eligible, browser_pane_open_fallback,
    browser_response_sync, browser_settings_plan, calculate_browser_split,
    normalize_browser_address, BrowserApprovalRequest, BrowserBounds, BrowserCommand, BrowserError,
    BrowserHostEvent, BrowserHostState, BrowserHostVisibility, BrowserInvocationActor,
    BrowserJournalActor, BrowserJournalEntry, BrowserPaneAction, BrowserPaneContext,
    BrowserPaneEventPlan, BrowserPaneModel, BrowserPaneSurface, BrowserPaneTransient,
    BrowserResponse, BrowserRisk, BrowserSettingsAction, BrowserTabSnapshot, BrowserUserInputKind,
    BrowserViewport, BrowserViewportPreset, BrowserWorkspaceKey, BrowserWorkspaceMutation,
    BrowserWorkspaceSnapshot,
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
    assert!(!handler.contains("dispatch_browser_command"));
}

#[test]
fn native_shell_pumps_async_browser_completions_before_host_events() {
    let source = include_str!("../src/app/mod.rs");
    let pump = source
        .find("fn pump_browser_events")
        .expect("browser event pump should exist");
    let body = &source[pump..];
    let completions = body
        .find("browser_host.pump_async_completions(window)")
        .expect("the GPUI pump should drain async WebView2 completions");
    let events = body
        .find("browser_host.drain_events()")
        .expect("the GPUI pump should drain host events");

    assert!(completions < events);
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
    assert!(opened.revision.0 > initial.revision.0);

    let collapsed = host.set_pane_open(&key, false).unwrap();
    assert!(!collapsed.snapshot.pane_open);
    assert!(collapsed.revision.0 > opened.revision.0);

    let actions = [
        BrowserPaneAction::Open,
        BrowserPaneAction::Collapse,
        BrowserPaneAction::CreateTab,
        BrowserPaneAction::Back,
        BrowserPaneAction::Forward,
        BrowserPaneAction::Reload,
        BrowserPaneAction::FocusAddress,
        BrowserPaneAction::SubmitAddress,
        BrowserPaneAction::SetViewport(BrowserViewportPreset::Desktop),
        BrowserPaneAction::ToggleAnnotation,
        BrowserPaneAction::ToggleRecording,
        BrowserPaneAction::OpenDevTools,
        BrowserPaneAction::OpenDownloads,
        BrowserPaneAction::Stop,
        BrowserPaneAction::ResetWorkspace,
        BrowserPaneAction::ClearProjectProfile,
    ];
    assert_eq!(actions.len(), 16);
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
