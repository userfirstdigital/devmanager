#[test]
fn native_terminal_user_input_routes_only_explicit_user_origins() {
    let app = include_str!("../src/app/mod.rs");
    let process_manager = include_str!("../src/services/process_manager.rs");
    let image_paste = include_str!("../src/remote/web/image_paste.rs");

    let remote_start = app
        .find("remote_host_service.set_terminal_input_handler")
        .expect("remote terminal input handler");
    let remote_end = app[remote_start..]
        .find("let resize_manager = process_manager.clone()")
        .map(|offset| remote_start + offset)
        .expect("end of remote terminal input handler region");
    let remote_handler = &app[remote_start..remote_end];
    assert!(remote_handler.contains("input_manager.write_user_text_to_session(&session_id, &text)"));
    assert!(
        remote_handler.contains("input_manager.write_user_bytes_to_session(&session_id, &bytes)")
    );
    assert!(remote_handler.contains("input_manager.paste_user_text_to_session(&session_id, &text)"));
    assert!(remote_handler.contains("RemoteTerminalInput::Control"));
    assert!(remote_handler.contains("input_manager.write_bytes_to_session(&session_id, &bytes)"));
    assert!(!remote_handler.contains("input_manager.write_to_session(&session_id, &text)"));
    assert!(!remote_handler.contains("input_manager.paste_to_session(&session_id, &text)"));
    assert!(remote_handler.contains("web_mutation_authority_is_current(authority)"));

    let local_start = app
        .find("TerminalKeyAction::Paste =>")
        .expect("local terminal paste handler");
    let local_end = app[local_start..]
        .find("fn handle_terminal_scroll(")
        .map(|offset| local_start + offset)
        .expect("end of local terminal keyboard handler region");
    let local_handler = &app[local_start..local_end];
    assert_eq!(
        local_handler.matches("paste_user_text_to_session").count(),
        1
    );
    assert_eq!(
        local_handler.matches("write_user_bytes_to_session").count(),
        1
    );
    assert_eq!(
        local_handler.matches("write_user_text_to_session").count(),
        1
    );

    let generic_text = source_region(
        process_manager,
        "pub fn write_to_session",
        "pub fn write_bytes_to_session",
    );
    assert!(generic_text.contains("session.write_text(text)"));
    assert!(!generic_text.contains("browser_attachment_broker"));
    let generic_bytes = source_region(
        process_manager,
        "pub fn write_bytes_to_session",
        "pub fn paste_to_session",
    );
    assert!(generic_bytes.contains("session.write_bytes(bytes)"));
    assert!(!generic_bytes.contains("browser_attachment_broker"));
    let generic_paste = source_region(
        process_manager,
        "pub fn paste_to_session",
        "pub fn write_user_text_to_session",
    );
    assert!(generic_paste.contains("session.paste_text(text)"));
    assert!(!generic_paste.contains("browser_attachment_broker"));

    assert!(app.contains("write_to_session(session_id, \"yes\\r\")"));
    assert!(app.contains("write_bytes_to_session(&session_id, &sequence)"));
    assert_eq!(
        app.matches("remote_send_terminal_input(RemoteTerminalInput::Bytes {")
            .count(),
        1,
        "only the explicit native user RawBytes boundary may use Bytes"
    );
    assert_eq!(
        app.matches("remote_send_terminal_input(RemoteTerminalInput::Control {")
            .count(),
        6,
        "mouse and scroll protocol sequences must stay generic"
    );
    assert!(image_paste.contains("process_manager.paste_user_text_to_session"));
    assert!(!image_paste.contains("process_manager.paste_to_session"));
    let composer = source_region(
        image_paste,
        "pub(crate) fn handle_web_composer_batch",
        "fn execute_web_composer_batch",
    );
    assert!(composer.contains("process_manager.write_user_text_to_session"));
    assert!(composer.contains("process_manager.write_to_session"));
    assert!(!composer.contains("write_user_bytes_to_session"));
}

fn source_region<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let start = source.find(start).expect("source region start");
    let end = source[start..]
        .find(end)
        .map(|offset| start + offset)
        .expect("source region end");
    &source[start..end]
}

#[test]
fn replacement_binding_is_installed_before_the_old_session_is_forgotten() {
    let source = include_str!("../src/services/process_manager.rs");
    let ensure_start = source
        .find("pub fn ensure_ai_session_for_tab_with_response")
        .expect("ensure AI session method");
    let restart_start = source[ensure_start..]
        .find("pub fn restart_ai_session")
        .map(|offset| ensure_start + offset)
        .expect("restart AI session method");
    let ensure = &source[ensure_start..restart_start];
    let bind = ensure
        .find("prepare_browser_launch_for_session")
        .expect("replacement binding preparation");
    let forget = ensure
        .find("forget_session(existing_session_id)")
        .expect("old session cleanup");

    assert!(
        bind < forget,
        "replacement must bind before old PTY cleanup"
    );
}

#[test]
fn reset_and_clear_drop_state_only_while_local_close_fully_retires_the_workspace() {
    let app = include_str!("../src/app/mod.rs");
    let process_manager = include_str!("../src/services/process_manager.rs");

    let sync = source_region(
        app,
        "fn synchronize_browser_response",
        "fn project_local_browser_snapshot",
    );
    assert!(sync.contains("BrowserCommand::ResetWorkspace"));
    assert!(sync.contains("BrowserCommand::ClearProjectProfile"));
    assert!(sync.contains("reset_workspace_state"));
    assert!(!sync.contains("retire_workspace"));

    let close = source_region(
        process_manager,
        "pub fn close_ai_session_with_response",
        "pub fn reconcile_saved_ai_tabs",
    );
    assert!(close.contains("retire_workspace(&workspace_key)"));
}
