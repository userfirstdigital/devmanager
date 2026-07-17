#[test]
fn native_terminal_user_input_routes_only_explicit_user_origins() {
    let app = include_str!("../src/app/mod.rs");
    let process_manager = include_str!("../src/services/process_manager.rs");
    let image_paste = include_str!("../src/remote/web/image_paste.rs");

    assert!(app.contains("write_user_text_to_session(&session_id, &text)"));
    assert!(app.contains("paste_user_text_to_session(&session_id, &text)"));
    assert!(app.contains("write_user_bytes_to_session(&session_id, &bytes)"));

    assert!(process_manager.contains("pub fn write_to_session"));
    assert!(process_manager.contains("session.write_text(text)"));
    assert!(process_manager.contains("pub fn write_bytes_to_session"));
    assert!(process_manager.contains("session.write_bytes(bytes)"));
    assert!(process_manager.contains("pub fn paste_to_session"));
    assert!(process_manager.contains("session.paste_text(text)"));

    assert!(app.contains("write_to_session(session_id, \"yes\\r\")"));
    assert!(app.contains("write_bytes_to_session(&session_id, &sequence)"));
    assert!(image_paste.contains("process_manager.paste_to_session"));
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
