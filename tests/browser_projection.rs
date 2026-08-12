use devmanager::browser::{
    projection_meta, BrowserProjectionError, BrowserProjectionEvent, BrowserProjectionSession,
};
use devmanager::domain::browser::BrowserTabKind;
use devmanager::domain::id::{BrowserContextId, BrowserTabId, ClientId, TaskId};
use devmanager::protocol::{
    BrowserFocusEpoch, BrowserFrameKind, BrowserInteractionMode, BrowserProjectionEnvelope,
    BrowserRemoteInput, BrowserRemoteInputKind, BrowserRuntimeGeneration, BrowserSecurityState,
    BrowserTabProjection, StreamPayloadKind,
};

fn tab(url: &str) -> BrowserTabProjection {
    let tab = BrowserTabProjection {
        tab_id: BrowserTabId::new(),
        title: "Fixture".to_string(),
        url: url.to_string(),
        kind: BrowserTabKind::Page,
        security: BrowserProjectionSession::tab_security(url),
        loading: false,
        error: None,
    };
    tab.validate().unwrap();
    tab
}

fn session() -> (BrowserProjectionSession, BrowserTabId) {
    let tabs = vec![tab("https://example.test/")];
    let selected = tabs[0].tab_id;
    let meta = projection_meta(
        TaskId::new(),
        BrowserContextId::new(),
        1,
        tabs,
        Some(selected),
    )
    .unwrap();
    (BrowserProjectionSession::new(meta).unwrap(), selected)
}

#[test]
fn projection_navigation_tab_and_progress_are_immediate() {
    let (mut session, selected) = session();
    let event = session
        .emit_metadata(1, Some("navigating".to_string()))
        .unwrap();
    match event {
        BrowserProjectionEvent::Metadata(meta) => {
            assert_eq!(meta.progress.as_deref(), Some("navigating"));
            assert_eq!(meta.selected_tab_id, Some(selected));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn projection_frames_require_subscriber_and_honor_budget() {
    let (mut session, _) = session();
    assert!(!BrowserProjectionSession::pixels_are_local_dom());
    assert_eq!(
        BrowserProjectionSession::frame_payload_kind(),
        StreamPayloadKind::BROWSER_FRAME
    );
    assert_eq!(
        session.maybe_emit_frame(1, BrowserFrameKind::Full, vec![1, 2, 3]),
        Err(BrowserProjectionError::NotSubscribed)
    );
    session.subscribe(ClientId::new());
    let first = session
        .maybe_emit_frame(1, BrowserFrameKind::Full, vec![1, 2, 3])
        .unwrap();
    assert!(matches!(first, BrowserProjectionEvent::Frame(_)));
    assert_eq!(
        session.maybe_emit_frame(1, BrowserFrameKind::Full, vec![1, 2, 3]),
        Err(BrowserProjectionError::BudgetExceeded)
    );
    let tile = session
        .maybe_emit_frame(1, BrowserFrameKind::Tile, vec![9])
        .unwrap();
    assert!(matches!(tile, BrowserProjectionEvent::Frame(_)));
}

#[test]
fn projection_rejects_stale_frame_bounds_focus_and_generation() {
    let (mut session, _) = session();
    session.subscribe(ClientId::new());
    session
        .maybe_emit_frame(1, BrowserFrameKind::Tile, vec![1])
        .unwrap();
    let mut input = BrowserRemoteInput {
        frame_id: session.meta().frame_id,
        generation: session.meta().generation,
        bounds_epoch: session.meta().bounds_epoch,
        focus_epoch: session.meta().focus_epoch,
        kind: BrowserRemoteInputKind::Pointer,
        x: 10,
        y: 10,
        content_width: 320,
        content_height: 200,
        scale: 96,
    };
    session.meta();
    // observe mode rejects input
    assert_eq!(
        session.map_input(&input),
        Err(BrowserProjectionError::InvalidRequest)
    );
    let mut interact = session.meta().clone();
    interact.interaction_mode = BrowserInteractionMode::Interact;
    let mut session = BrowserProjectionSession::new(interact).unwrap();
    session.subscribe(ClientId::new());
    input.frame_id = 99;
    assert_eq!(
        session.map_input(&input),
        Err(BrowserProjectionError::StaleFrame)
    );
    input.frame_id = session.meta().frame_id;
    input.generation = BrowserRuntimeGeneration::new(2).unwrap();
    assert_eq!(
        session.map_input(&input),
        Err(BrowserProjectionError::StaleGeneration)
    );
}

#[test]
fn projection_maps_coordinates_through_scale_and_bounds() {
    let tabs = vec![tab("https://example.test/")];
    let selected = tabs[0].tab_id;
    let mut meta = projection_meta(
        TaskId::new(),
        BrowserContextId::new(),
        1,
        tabs,
        Some(selected),
    )
    .unwrap();
    meta.interaction_mode = BrowserInteractionMode::Interact;
    let session = BrowserProjectionSession::new(meta).unwrap();
    let mapped = session
        .map_input(&BrowserRemoteInput {
            frame_id: 1,
            generation: BrowserRuntimeGeneration::new(1).unwrap(),
            bounds_epoch: session.meta().bounds_epoch,
            focus_epoch: session.meta().focus_epoch,
            kind: BrowserRemoteInputKind::Touch,
            x: 24,
            y: 48,
            content_width: 320,
            content_height: 200,
            scale: 96,
        })
        .unwrap();
    assert_eq!(mapped, (24, 48));
}

#[test]
fn projection_approvals_are_first_answer_wins() {
    let (mut session, _) = session();
    session.offer_approval("perm-1").unwrap();
    let first = session.decide_approval("perm-1", true).unwrap();
    assert_eq!(
        first,
        BrowserProjectionEvent::Approval {
            request_id: "perm-1".into(),
            allowed: true
        }
    );
    assert_eq!(
        session.decide_approval("perm-1", false),
        Err(BrowserProjectionError::ApprovalConsumed)
    );
}

#[test]
fn projection_resync_clears_budget_without_manual_refresh_token() {
    let (mut session, _) = session();
    session.subscribe(ClientId::new());
    session
        .maybe_emit_frame(1, BrowserFrameKind::Tile, vec![1])
        .unwrap();
    match session.resync(1).unwrap() {
        BrowserProjectionEvent::Resync(meta) => assert_eq!(meta.generation.value(), 1),
        other => panic!("{other:?}"),
    }
    session
        .maybe_emit_frame(1, BrowserFrameKind::Full, vec![2, 3])
        .unwrap();
}

#[test]
fn projection_envelope_fails_closed_on_zero_generation() {
    assert!(BrowserProjectionEnvelope::new(0, 1, 1, 1, 8, &[1]).is_err());
    let envelope = BrowserProjectionEnvelope::new(1, 1, 1, 1, StreamPayloadKind::BROWSER_FRAME.get(), &[1])
        .unwrap();
    assert!(envelope.matches_generation(2).is_err());
    assert!(envelope.matches_generation(1).is_ok());
}

#[test]
fn projection_zero_focus_epoch_input_is_rejected() {
    let (session, _) = session();
    let input = BrowserRemoteInput {
        frame_id: 1,
        generation: BrowserRuntimeGeneration::new(1).unwrap(),
        bounds_epoch: session.meta().bounds_epoch,
        focus_epoch: BrowserFocusEpoch::initial(),
        kind: BrowserRemoteInputKind::Keyboard,
        x: 0,
        y: 0,
        content_width: 0,
        content_height: 200,
        scale: 96,
    };
    assert!(input.validate().is_err());
}
