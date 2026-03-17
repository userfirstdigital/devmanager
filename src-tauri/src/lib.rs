mod commands;
mod models;
mod services;
mod state;

use services::platform;
use state::AppState;
use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app_state = AppState {
        config: RwLock::new(None),
        runtime_platform: platform::detect_runtime_platform(),
        processes: Mutex::new(HashMap::new()),
        monitored_processes: Mutex::new(HashMap::new()),
        pty_sessions: Mutex::new(HashMap::new()),
        pty_buffers: Mutex::new(HashMap::new()),
        git_watcher: Mutex::new(None),
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
            commands::pty::check_pty_session,
            commands::pty::drain_pty_buffer,
            commands::pty::snapshot_pty_buffer,
            commands::pty::create_server_session,
            commands::pty::restore_sessions,
            commands::scanner::scan_root,
            commands::scanner::watch_git_branches,
            commands::scanner::unwatch_git_branches,
            commands::runtime::get_runtime_info,
            commands::runtime::quit_app,
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
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "stop_all" => {
                        let state = app.state::<AppState>();
                        platform::stop_all_tracked_processes(&state);
                    }
                    "quit" => {
                        let state = app.state::<AppState>();
                        platform::shutdown_managed_processes(&state);
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)?;

            // Spawn unified resource monitoring loop
            let app_handle = app.handle().clone();
            spawn_resource_monitor_loop(app_handle);

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let app_handle = window.app_handle();
                let state = app_handle.state::<AppState>();

                // Check settings (RwLock read)
                let config = state.config.read().unwrap();
                let minimize = config
                    .as_ref()
                    .map(|c| c.settings.minimize_to_tray)
                    .unwrap_or(false);
                let confirm = config
                    .as_ref()
                    .map(|c| c.settings.confirm_on_close)
                    .unwrap_or(true);
                drop(config);

                if minimize {
                    api.prevent_close();
                    let _ = window.hide();
                } else if confirm {
                    // Let the frontend handle the confirmation dialog.
                    // It will call stopAll() + window.destroy() if the user confirms,
                    // or do nothing and allow close if no processes are running.
                    // We must NOT kill anything here — that causes a crash when
                    // React subsequently prevents the close and shows the dialog.
                } else {
                    // No confirmation — kill everything and close
                    #[cfg(target_os = "macos")]
                    {
                        api.prevent_close();
                        platform::shutdown_managed_processes(&state);
                        app_handle.exit(0);
                    }

                    #[cfg(not(target_os = "macos"))]
                    {
                        platform::shutdown_managed_processes(&state);
                    }
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Collect process tree resource info for a given root PID using a shared sysinfo::System.
fn collect_process_tree(
    sys: &sysinfo::System,
    command_id: &str,
    root_pid: sysinfo::Pid,
) -> Option<models::config::ProcessTreeInfo> {
    // Check if root process still exists
    if sys.process(root_pid).is_none() {
        return None;
    }

    let mut tree_pids: Vec<sysinfo::Pid> = vec![root_pid];

    for (proc_pid, process) in sys.processes() {
        if *proc_pid == root_pid {
            continue;
        }
        let mut current_pid = process.parent();
        while let Some(parent_pid) = current_pid {
            if parent_pid == root_pid {
                tree_pids.push(*proc_pid);
                break;
            }
            match sys.process(parent_pid) {
                Some(parent_proc) => current_pid = parent_proc.parent(),
                None => break,
            }
        }
    }

    let mut processes = Vec::new();
    let mut total_memory_mb = 0.0;
    let mut total_cpu_percent: f32 = 0.0;

    for proc_pid in &tree_pids {
        if let Some(process) = sys.process(*proc_pid) {
            let memory_mb = process.memory() as f64 / (1024.0 * 1024.0);
            let cpu_percent = process.cpu_usage();
            total_memory_mb += memory_mb;
            total_cpu_percent += cpu_percent;
            processes.push(models::config::ChildProcessInfo {
                pid: proc_pid.as_u32(),
                name: process.name().to_string_lossy().to_string(),
                memory_mb,
                cpu_percent,
            });
        }
    }

    Some(models::config::ProcessTreeInfo {
        command_id: command_id.to_string(),
        processes,
        total_memory_mb,
        total_cpu_percent,
    })
}

/// Single background loop that refreshes sysinfo once per tick and emits
/// resource-update events for all monitored processes.
fn spawn_resource_monitor_loop(app: tauri::AppHandle) {
    use tauri::Emitter;

    tauri::async_runtime::spawn(async move {
        let mut sys = sysinfo::System::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;

            // Refresh all processes once
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

            let state = app.state::<AppState>();

            // Snapshot current monitors
            let monitors: Vec<(String, u32)> = {
                let m = state.monitored_processes.lock().unwrap();
                m.values().map(|e| (e.command_id.clone(), e.pid)).collect()
            };

            for (command_id, pid) in &monitors {
                let root_pid = sysinfo::Pid::from_u32(*pid);
                if let Some(info) = collect_process_tree(&sys, command_id, root_pid) {
                    let _ = app.emit("resource-update", &info);
                }
                // Dead root? Just skip this tick. User will stop monitoring when they close the tab.
            }
        }
    });
}
