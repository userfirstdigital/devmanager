use crate::state::RuntimeState;
use crate::updater::{UpdaterSnapshot, UpdaterStage};
use crate::{icons, theme};
use gpui::{
    div, px, rgb, App, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    SharedString, Styled, Window,
};
use time::{format_description, OffsetDateTime};

pub const STATUS_BAR_HEIGHT_PX: f32 = 22.0;

#[derive(Clone, Copy)]
pub enum StatusBarTone {
    Muted,
    Accent,
    Success,
    Warning,
    Danger,
}

pub struct StatusBarQuickAction {
    pub label: String,
    pub tone: StatusBarTone,
}

pub struct RemoteStatusBarModel {
    pub label: String,
    pub tone: StatusBarTone,
    pub primary_action: Option<StatusBarQuickAction>,
    pub secondary_action: Option<StatusBarQuickAction>,
    pub tertiary_action: Option<StatusBarQuickAction>,
}

pub struct StatusBarActions<'a> {
    pub on_install_update: &'a dyn Fn() -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_open_remote: &'a dyn Fn() -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_remote_primary:
        Option<&'a dyn Fn() -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_remote_secondary:
        Option<&'a dyn Fn() -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_remote_tertiary:
        Option<&'a dyn Fn() -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
}

pub fn render_status_bar(
    runtime: &RuntimeState,
    updater: &UpdaterSnapshot,
    remote: Option<&RemoteStatusBarModel>,
    actions: StatusBarActions<'_>,
) -> impl IntoElement {
    let (open_terminals, total_memory_bytes) = running_terminal_metrics(runtime);
    let time_label = current_time_label();
    let update_content = render_updater_status(updater, &actions);

    div()
        .h(px(STATUS_BAR_HEIGHT_PX))
        .flex_none()
        .flex()
        .items_center()
        .justify_between()
        .px_2()
        .bg(rgb(theme::STATUS_BAR_BG))
        .border_t_1()
        .border_color(rgb(theme::BORDER_PRIMARY))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .text_xs()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(icons::app_icon(icons::SERVER, 10.0, theme::SUCCESS_TEXT))
                        .child(SharedString::from(format!(
                            "{open_terminals} terminal{} open",
                            plural(open_terminals)
                        ))),
                )
                .children((open_terminals > 0).then(|| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child(icons::app_icon(icons::ACTIVITY, 10.0, theme::TEXT_DIM))
                        .child(SharedString::from(format!(
                            "{} total memory",
                            format_memory(total_memory_bytes)
                        )))
                })),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .children(
                    remote.map(|remote| render_remote_status(remote, &actions).into_any_element()),
                )
                .child(update_content)
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child(SharedString::from(time_label)),
                ),
        )
}

fn render_updater_status(
    updater: &UpdaterSnapshot,
    actions: &StatusBarActions<'_>,
) -> impl IntoElement {
    match updater.stage {
        UpdaterStage::Downloading => {
            let label = updater_status_label(updater)
                .unwrap_or_else(|| "Downloading update...".to_string());
            div()
                .text_xs()
                .text_color(rgb(theme::AI_DOT))
                .child(SharedString::from(label))
        }
        UpdaterStage::ReadyToInstall => {
            let label =
                updater_status_label(updater).unwrap_or_else(|| "Restart to update".to_string());
            div()
                .text_xs()
                .text_color(rgb(theme::PROJECT_DOT))
                .cursor_pointer()
                .hover(|s| s.text_color(rgb(theme::PRIMARY_HOVER)))
                .child(SharedString::from(label))
                .on_mouse_down(MouseButton::Left, (actions.on_install_update)())
        }
        UpdaterStage::UpdateAvailable => {
            div()
                .text_xs()
                .text_color(rgb(theme::PROJECT_DOT))
                .child(SharedString::from(
                    updater_status_label(updater)
                        .unwrap_or_else(|| "Update found. Starting download...".to_string()),
                ))
        }
        UpdaterStage::Checking => {
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_SUBTLE))
                .child(SharedString::from(
                    updater_status_label(updater)
                        .unwrap_or_else(|| "Checking for updates".to_string()),
                ))
        }
        UpdaterStage::Error => {
            div()
                .text_xs()
                .text_color(rgb(theme::DANGER_TEXT))
                .child(SharedString::from(
                    updater_status_label(updater).unwrap_or_else(|| "Update failed".to_string()),
                ))
        }
        UpdaterStage::Installing => {
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_SUBTLE))
                .child(SharedString::from(
                    updater_status_label(updater)
                        .unwrap_or_else(|| "Installing update...".to_string()),
                ))
        }
        UpdaterStage::UpToDate => {
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_SUBTLE))
                .child(SharedString::from(
                    updater_status_label(updater)
                        .unwrap_or_else(|| format!("v{}", updater.current_version)),
                ))
        }
        _ => div().text_xs().text_color(rgb(theme::TEXT_DIM)).child(""),
    }
}

fn render_remote_status(
    remote: &RemoteStatusBarModel,
    actions: &StatusBarActions<'_>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .px(px(6.0))
                .h(px(16.0))
                .rounded_full()
                .bg(rgb(status_bar_tone_bg(remote.tone)))
                .border_1()
                .border_color(rgb(status_bar_tone_border(remote.tone)))
                .text_xs()
                .text_color(rgb(status_bar_tone_text(remote.tone)))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(status_bar_tone_hover_bg(remote.tone))))
                .child(icons::app_icon(
                    icons::SERVER,
                    10.0,
                    status_bar_tone_text(remote.tone),
                ))
                .child(SharedString::from(remote.label.clone()))
                .on_mouse_down(MouseButton::Left, (actions.on_open_remote)()),
        )
        .children(
            remote
                .primary_action
                .as_ref()
                .zip(actions.on_remote_primary)
                .map(|(action, handler)| {
                    render_status_bar_action(action, handler).into_any_element()
                }),
        )
        .children(
            remote
                .secondary_action
                .as_ref()
                .zip(actions.on_remote_secondary)
                .map(|(action, handler)| {
                    render_status_bar_action(action, handler).into_any_element()
                }),
        )
        .children(
            remote
                .tertiary_action
                .as_ref()
                .zip(actions.on_remote_tertiary)
                .map(|(action, handler)| {
                    render_status_bar_action(action, handler).into_any_element()
                }),
        )
}

fn render_status_bar_action(
    action: &StatusBarQuickAction,
    handler: &dyn Fn() -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .px(px(6.0))
        .h(px(16.0))
        .rounded_full()
        .bg(rgb(status_bar_tone_bg(action.tone)))
        .border_1()
        .border_color(rgb(status_bar_tone_border(action.tone)))
        .text_xs()
        .text_color(rgb(status_bar_tone_text(action.tone)))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(status_bar_tone_hover_bg(action.tone))))
        .child(SharedString::from(action.label.clone()))
        .on_mouse_down(MouseButton::Left, handler())
}

fn status_bar_tone_bg(tone: StatusBarTone) -> u32 {
    match tone {
        StatusBarTone::Muted => theme::TOPBAR_BG,
        StatusBarTone::Accent => theme::PRIMARY_MUTED,
        StatusBarTone::Success => theme::SUCCESS_BG,
        StatusBarTone::Warning => 0x2a2211,
        StatusBarTone::Danger => 0x2b161c,
    }
}

fn status_bar_tone_hover_bg(tone: StatusBarTone) -> u32 {
    match tone {
        StatusBarTone::Muted => theme::ROW_HOVER_BG,
        StatusBarTone::Accent => 0x3730a3,
        StatusBarTone::Success => 0x19301f,
        StatusBarTone::Warning => 0x3a2d11,
        StatusBarTone::Danger => 0x3a1d25,
    }
}

fn status_bar_tone_border(tone: StatusBarTone) -> u32 {
    match tone {
        StatusBarTone::Muted => theme::BORDER_PRIMARY,
        StatusBarTone::Accent => theme::PRIMARY,
        StatusBarTone::Success => 0x1c3b27,
        StatusBarTone::Warning => 0x4f3b0d,
        StatusBarTone::Danger => 0x5a2630,
    }
}

fn status_bar_tone_text(tone: StatusBarTone) -> u32 {
    match tone {
        StatusBarTone::Muted => theme::TEXT_MUTED,
        StatusBarTone::Accent => 0xc7d2fe,
        StatusBarTone::Success => theme::SUCCESS_TEXT,
        StatusBarTone::Warning => theme::WARNING_TEXT,
        StatusBarTone::Danger => theme::DANGER_TEXT,
    }
}

fn updater_status_label(updater: &UpdaterSnapshot) -> Option<String> {
    match updater.stage {
        UpdaterStage::Checking => Some("Checking for updates".to_string()),
        UpdaterStage::UpdateAvailable => Some(format!(
            "Update {} found. Starting download...",
            updater.target_version.as_deref().unwrap_or("ready")
        )),
        UpdaterStage::Downloading => Some(if let Some(total) = updater.total_bytes {
            let percent = if total == 0 {
                0
            } else {
                ((updater.downloaded_bytes as f64 / total as f64) * 100.0).round() as u64
            };
            format!("Downloading update {percent}%")
        } else {
            "Downloading update...".to_string()
        }),
        UpdaterStage::ReadyToInstall => Some(format!(
            "Restart to update {}",
            updater.target_version.as_deref().unwrap_or("new")
        )),
        UpdaterStage::Installing => Some("Installing update...".to_string()),
        UpdaterStage::Error => Some("Update failed".to_string()),
        UpdaterStage::UpToDate => Some(format!("v{}", updater.current_version)),
        UpdaterStage::Disabled | UpdaterStage::Idle => None,
    }
}

fn running_terminal_metrics(runtime: &RuntimeState) -> (usize, u64) {
    runtime
        .sessions
        .values()
        .filter(|session| session.status.is_live())
        .fold((0, 0), |(count, memory), session| {
            (
                count + 1,
                memory.saturating_add(session.resources.memory_bytes),
            )
        })
}

fn format_memory(bytes: u64) -> String {
    let mb = bytes as f64 / 1024.0 / 1024.0;
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{:.0} MB", mb)
    }
}

fn current_time_label() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let format = format_description::parse("[hour repr:12]:[minute] [period case:lower]")
        .expect("valid time format");
    now.format(&format)
        .unwrap_or_else(|_| "12:00 am".to_string())
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::{running_terminal_metrics, updater_status_label};
    use crate::state::{
        ResourceSnapshot, RuntimeState, ServerLaunchSpec, SessionDimensions, SessionRuntimeState,
        SessionStatus,
    };
    use crate::terminal::session::TerminalBackend;
    use crate::updater::{UpdaterSnapshot, UpdaterStage};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::SystemTime;

    #[test]
    fn running_terminal_metrics_counts_all_live_sessions() {
        let mut runtime = RuntimeState::new(false);

        let mut shell = SessionRuntimeState::new(
            "shell-1",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        shell.status = SessionStatus::Running;
        shell.resources = ResourceSnapshot {
            memory_bytes: 32,
            ..Default::default()
        };

        let mut server = SessionRuntimeState::new(
            "server-1",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        server.status = SessionStatus::Running;
        server.configure_server(ServerLaunchSpec {
            command_id: "cmd-1".to_string(),
            project_id: "project-1".to_string(),
            cwd: PathBuf::from("."),
            program: "cmd".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            auto_restart: false,
            log_file_path: None,
        });
        server.resources = ResourceSnapshot {
            memory_bytes: 64,
            ..Default::default()
        };

        let mut stopped = SessionRuntimeState::new(
            "shell-2",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        stopped.status = SessionStatus::Stopped;
        stopped.resources = ResourceSnapshot {
            memory_bytes: 128,
            ..Default::default()
        };

        runtime.sessions.insert(shell.session_id.clone(), shell);
        runtime.sessions.insert(server.session_id.clone(), server);
        runtime.sessions.insert(stopped.session_id.clone(), stopped);

        assert_eq!(running_terminal_metrics(&runtime), (2, 96));
    }

    #[test]
    fn updater_status_label_matches_background_download_flow() {
        let mut snapshot = UpdaterSnapshot {
            configured: true,
            current_version: "0.2.1".to_string(),
            endpoints: vec!["https://example.com/latest.json".to_string()],
            stage: UpdaterStage::UpdateAvailable,
            target_version: Some("0.2.2".to_string()),
            detail: String::new(),
            release_notes: None,
            last_checked_at: Some(SystemTime::now()),
            downloaded_bytes: 0,
            total_bytes: None,
        };

        assert_eq!(
            updater_status_label(&snapshot).as_deref(),
            Some("Update 0.2.2 found. Starting download...")
        );

        snapshot.stage = UpdaterStage::ReadyToInstall;
        assert_eq!(
            updater_status_label(&snapshot).as_deref(),
            Some("Restart to update 0.2.2")
        );
    }
}
