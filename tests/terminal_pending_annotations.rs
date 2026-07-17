use devmanager::browser::{
    BrowserAnnotation, BrowserAnnotationKind, BrowserBounds, BrowserLocator, BrowserResourceId,
    BrowserRevision, BrowserTabSnapshot, BrowserViewport, BrowserWorkspaceKey,
    BrowserWorkspaceSnapshot, REDACTED_VALUE,
};
use devmanager::models::TabType;
use devmanager::terminal::view::pending_annotation_chip_models;
use std::collections::BTreeMap;

fn annotation(
    id: &str,
    tab_id: &str,
    revision: BrowserRevision,
    comment: &str,
    url: &str,
) -> BrowserAnnotation {
    BrowserAnnotation {
        id: id.to_string(),
        kind: BrowserAnnotationKind::Element,
        tab_id: tab_id.to_string(),
        anchor_revision: revision,
        comment: comment.to_string(),
        url: url.to_string(),
        locator: BrowserLocator::default(),
        bounds: BrowserBounds {
            x: 1,
            y: 2,
            width: 30,
            height: 40,
        },
        viewport: BrowserViewport::default(),
        screenshot_resource: BrowserResourceId(format!("shot-{id}")),
        computed_styles: BTreeMap::new(),
        resolved: false,
    }
}

fn snapshot() -> BrowserWorkspaceSnapshot {
    BrowserWorkspaceSnapshot {
        revision: BrowserRevision(7),
        tabs: vec![BrowserTabSnapshot {
            id: "page-a".to_string(),
            title: "Page A".to_string(),
            url: "https://alice:credential@example.test/path?token=url-secret#fragment-secret"
                .to_string(),
            viewport: BrowserViewport::default(),
        }],
        selected_tab_id: Some("page-a".to_string()),
        ..BrowserWorkspaceSnapshot::default()
    }
}

#[test]
fn pending_annotation_chips_are_ordered_bounded_redacted_and_ai_only() {
    let workspace_key = BrowserWorkspaceKey::new("project-a", "conversation-a").unwrap();
    let current = annotation(
        "ann-current",
        "page-a",
        BrowserRevision(7),
        &format!("password=hunter2 review this {}", "x".repeat(500)),
        "https://alice:credential@example.test/path?token=url-secret#fragment-secret",
    );
    let stale = annotation(
        "ann-stale",
        "missing-page",
        BrowserRevision(6),
        "second annotation",
        "not a safe URL oauth-code-value",
    );
    let authoritative_order = vec![stale, current];

    for tab_type in [TabType::Claude, TabType::Codex] {
        let chips = pending_annotation_chip_models(
            Some(&tab_type),
            &workspace_key,
            &snapshot(),
            &authoritative_order,
        );
        assert_eq!(
            chips
                .iter()
                .map(|chip| chip.action.annotation_id.as_str())
                .collect::<Vec<_>>(),
            vec!["ann-stale", "ann-current"]
        );
        assert_eq!(chips[0].stable_id, "ann-stale");
        assert!(chips[0].stale);
        assert!(!chips[1].stale);
        assert!(chips[1].comment.chars().count() <= 96);
        assert!(chips[1].comment.contains(REDACTED_VALUE));
        assert!(!chips[1].comment.contains("hunter2"));
        assert_eq!(chips[1].url, "https://example.test");
        for forbidden in [
            "alice",
            "credential",
            "url-secret",
            "fragment-secret",
            "oauth-code-value",
        ] {
            assert!(
                chips
                    .iter()
                    .all(|chip| !chip.comment.contains(forbidden) && !chip.url.contains(forbidden)),
                "chip leaked {forbidden}"
            );
        }
    }

    for tab_type in [TabType::Server, TabType::Ssh] {
        assert!(pending_annotation_chip_models(
            Some(&tab_type),
            &workspace_key,
            &snapshot(),
            &authoritative_order,
        )
        .is_empty());
    }
    assert!(pending_annotation_chip_models(
        None,
        &workspace_key,
        &snapshot(),
        &authoritative_order,
    )
    .is_empty());
}

#[test]
fn pending_annotation_strip_lives_in_terminal_surface_independent_of_browser_collapse() {
    let source = include_str!("../src/terminal/view.rs");
    let start = source.find("pub fn render_terminal_surface(").unwrap();
    let end = source[start..]
        .find("fn render_grid(")
        .map(|offset| start + offset)
        .unwrap();
    let surface = &source[start..end];
    let notice = surface.find(".children(blocking_notice)").unwrap();
    let chips = surface
        .find("render_pending_annotation_chips")
        .expect("terminal surface must render pending annotation chips");
    let search = surface.find(".children(model.search.as_ref()").unwrap();

    assert!(notice < chips && chips < search);
    assert!(!surface.contains("pane_open"));
}

#[test]
fn pending_annotation_chip_actions_preview_and_nested_remove_stops_propagation() {
    let view_source = include_str!("../src/terminal/view.rs");
    let actions_start = view_source.find("pub struct TerminalPaneActions").unwrap();
    let actions_end = view_source[actions_start..]
        .find("pub struct TerminalScrollbarActions")
        .map(|offset| actions_start + offset)
        .unwrap();
    let actions = &view_source[actions_start..actions_end];
    assert!(actions.contains("on_preview_annotation"));
    assert!(actions.contains("on_remove_annotation"));

    let render_start = view_source
        .find("fn render_pending_annotation_chips")
        .unwrap();
    let render_end = view_source[render_start..]
        .find("fn render_grid(")
        .map(|offset| render_start + offset)
        .unwrap();
    let render = &view_source[render_start..render_end];
    assert!(render.contains("preview(action"));
    let nested_remove = render
        .find("remove(action")
        .expect("nested remove must call its dedicated action");
    let stop = render[..nested_remove]
        .rfind("stop_propagation()")
        .expect("nested remove must stop the parent preview click");
    assert!(stop < nested_remove);
}

#[test]
fn native_chip_actions_reuse_authoritative_sources_and_the_existing_host_barrier() {
    let source = include_str!("../src/app/mod.rs");
    let source_start = source
        .find("fn pending_annotation_source_for_tab(")
        .expect("shared authoritative pending source");
    let source_end = source[source_start..]
        .find("fn pending_annotation_chip_models_for_tab(")
        .map(|offset| source_start + offset)
        .unwrap();
    let authoritative = &source[source_start..source_end];
    let remote = authoritative.find("self.remote_mode.is_some()").unwrap();
    let remote_ids = authoritative[remote..]
        .find("pending_annotation_ids")
        .map(|offset| remote + offset)
        .unwrap();
    let local_broker = authoritative[remote_ids..]
        .find("browser_attachment_broker")
        .map(|offset| remote_ids + offset)
        .unwrap();
    assert!(remote < remote_ids && remote_ids < local_broker);

    let remove_start = source
        .find("fn remove_pending_annotation_action(")
        .expect("native remove handler");
    let preview_start = source[remove_start..]
        .find("fn preview_pending_annotation_action(")
        .map(|offset| remove_start + offset)
        .expect("native preview handler");
    let remove = &source[remove_start..preview_start];
    assert!(
        remove.find("self.remote_mode.is_some()").unwrap()
            < remove
                .find("remove_pending_annotation_projection_transaction")
                .unwrap()
    );
    assert!(!remove.contains("BrowserCommand::Annotations"));
    assert!(!remove.contains("BrowserAnnotationOperation::Delete"));

    let preview_end = source[preview_start..]
        .find("fn capture_browser_split_bounds(")
        .map(|offset| preview_start + offset)
        .unwrap();
    let preview = &source[preview_start..preview_end];
    assert!(preview.contains("browser_annotation_preview_plan"));
    assert!(preview.contains("dispatch_browser_command"));
    assert!(!preview.contains(".detach("));
    assert!(!preview.contains("acknowledge_dirty_projection"));
    assert!(!preview.contains("reserve_for_input"));
    assert!(!preview.contains(".commit("));

    let terminal_actions_start = source.find("let terminal_actions =").unwrap();
    let terminal_actions_end = source[terminal_actions_start..]
        .find("div()\n            .size_full()")
        .map(|offset| terminal_actions_start + offset)
        .unwrap();
    let terminal_actions = &source[terminal_actions_start..terminal_actions_end];
    assert!(terminal_actions.contains("on_preview_annotation:"));
    assert!(terminal_actions.contains("on_remove_annotation:"));
}

#[test]
fn pending_annotation_action_failures_reach_the_visible_terminal_notice_without_raw_details() {
    let source = include_str!("../src/app/mod.rs");
    let helper_start = source
        .find("fn show_pending_annotation_action_failure(")
        .expect("shared terminal-visible chip action failure helper");
    let remove_start = source[helper_start..]
        .find("fn remove_pending_annotation_action(")
        .map(|offset| helper_start + offset)
        .unwrap();
    let helper = &source[helper_start..remove_start];
    assert!(helper.contains("self.pending_annotation_action_notice = Some(notice.clone())"));
    assert!(helper.contains("diagnostic = Some(message.to_string())"));

    let preview_start = source[remove_start..]
        .find("fn preview_pending_annotation_action(")
        .map(|offset| remove_start + offset)
        .unwrap();
    let preview_end = source[preview_start..]
        .find("fn capture_browser_split_bounds(")
        .map(|offset| preview_start + offset)
        .unwrap();
    let remove = &source[remove_start..preview_start];
    let preview = &source[preview_start..preview_end];

    assert_eq!(
        remove
            .matches("show_pending_annotation_action_failure")
            .count(),
        4,
        "each local or remote remove failure must reach the visible terminal notice"
    );
    assert_eq!(
        preview
            .matches("show_pending_annotation_action_failure")
            .count(),
        3,
        "each preview failure must reach the visible terminal notice"
    );
    assert!(!remove.contains("error.to_string()"));
    assert!(!preview.contains("error.to_string()"));

    let model_start = source.find("fn sync_terminal_session(").unwrap();
    let model_end = source[model_start..]
        .find("fn focus_terminal(")
        .map(|offset| model_start + offset)
        .unwrap();
    assert_eq!(
        source[model_start..model_end]
            .matches("refresh_terminal_pane_model_notice")
            .count(),
        2,
        "local and remote AI model refreshes must project action feedback"
    );
}

#[test]
fn terminal_viewport_reserves_space_for_visible_pending_annotation_chips() {
    let source = include_str!("../src/app/mod.rs");
    assert!(source.contains("const PENDING_ANNOTATION_STRIP_HEIGHT_PX"));
    let start = source.find("fn terminal_viewport_layout(").unwrap();
    let end = source[start..]
        .find("fn copy_terminal_selection_to_clipboard(")
        .map(|offset| start + offset)
        .unwrap();
    let layout = &source[start..end];
    assert!(layout.contains("pending_annotation_action_notice_message"));
    assert!(layout.contains("pending_annotation_source_for_tab"));
    assert!(layout.contains("PENDING_ANNOTATION_STRIP_HEIGHT_PX + STACK_GAP_PX"));
}
