mod commands;
mod models;
mod services;
mod state;

use state::AppState;
use std::sync::Mutex;
use std::collections::HashMap;
use tauri::Manager;
use tauri::tray::{TrayIconBuilder, MouseButton, MouseButtonState, TrayIconEvent};
use tauri::menu::{MenuBuilder, MenuItemBuilder};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app_state = AppState {
        config: Mutex::new(None),
        processes: Mutex::new(HashMap::new()),
        resource_monitors: Mutex::new(HashMap::new()),
        pty_sessions: Mutex::new(HashMap::new()),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_window_state::Builder::new().build())
        .plugin({
            let mut builder = tauri_plugin_updater::Builder::new();
            if let Some(pubkey) = option_env!("TAURI_UPDATER_PUBKEY") {
                builder = builder.pubkey(pubkey);
            }
            builder.build()
        })
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            commands::config::get_config,
            commands::config::save_full_config,
            commands::config::add_project,
            commands::config::update_project,
            commands::config::remove_project,
            commands::config::update_settings,
            commands::scanner::scan_project,
            commands::scanner::check_dependencies,
            commands::scanner::get_git_branch,
            commands::process::register_process,
            commands::process::unregister_process,
            commands::process::kill_process_tree,
            commands::process::get_running_processes,
            commands::ports::check_port_in_use,
            commands::ports::kill_port,
            commands::ports::get_port_conflicts,
            commands::ports::update_env_port,
            commands::resources::get_process_resources,
            commands::resources::start_resource_monitor,
            commands::resources::stop_resource_monitor,
            commands::session::get_session,
            commands::session::save_session,
            commands::terminal::open_terminal,
            commands::env::read_env_file,
            commands::env::write_env_file,
            commands::pty::create_pty_session,
            commands::pty::write_pty,
            commands::pty::resize_pty,
            commands::pty::close_pty,
            commands::scanner::scan_root,
        ])
        .setup(|app| {
            // Kill any orphaned processes from a previous crash
            services::pid_file::cleanup_orphaned_processes();

            let show = MenuItemBuilder::with_id("show", "Show DevManager").build(app)?;
            let stop_all = MenuItemBuilder::with_id("stop_all", "Stop All Servers").build(app)?;
            let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;

            let menu = MenuBuilder::new(app)
                .item(&show)
                .separator()
                .item(&stop_all)
                .separator()
                .item(&quit)
                .build()?;

            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .tooltip("DevManager")
                .on_menu_event(move |app, event| {
                    match event.id().as_ref() {
                        "show" => {
                            if let Some(window) = app.get_webview_window("main") {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                        "stop_all" => {
                            let state = app.state::<AppState>();
                            let processes = state.processes.lock().unwrap();
                            for (_, info) in processes.iter() {
                                let _ = std::process::Command::new("taskkill")
                                    .args(["/T", "/F", "/PID", &info.pid.to_string()])
                                    .output();
                            }
                        }
                        "quit" => {
                            let state = app.state::<AppState>();
                            let processes = state.processes.lock().unwrap();
                            for (_, info) in processes.iter() {
                                let _ = std::process::Command::new("taskkill")
                                    .args(["/T", "/F", "/PID", &info.pid.to_string()])
                                    .output();
                            }
                            drop(processes);
                            app.exit(0);
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let app_handle = window.app_handle();
                let state = app_handle.state::<AppState>();

                // Check minimize_to_tray setting
                let config = state.config.lock().unwrap();
                let minimize = config.as_ref().map(|c| c.settings.minimize_to_tray).unwrap_or(false);
                drop(config);

                if minimize {
                    api.prevent_close();
                    let _ = window.hide();
                } else {
                    let processes = state.processes.lock().unwrap();
                    for (_, info) in processes.iter() {
                        let _ = std::process::Command::new("taskkill")
                            .args(["/T", "/F", "/PID", &info.pid.to_string()])
                            .output();
                    }
                    drop(processes);
                    // Kill PTY sessions
                    let mut pty_sessions = state.pty_sessions.lock().unwrap();
                    for (_, mut session) in pty_sessions.drain() {
                        let _ = session.child.kill();
                    }
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
