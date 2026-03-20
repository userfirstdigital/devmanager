use crate::state::RuntimeState;
use crate::updater::{UpdaterSnapshot, UpdaterStage};
use crate::{icons, theme};
use gpui::{
    div, px, rgb, App, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    SharedString, Styled, Window,
};
use time::{format_description, OffsetDateTime};

pub const STATUS_BAR_HEIGHT_PX: f32 = 22.0;

pub struct StatusBarActions<'a> {
    pub on_install_update: &'a dyn Fn() -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
}

pub fn render_status_bar(
    runtime: &RuntimeState,
    updater: &UpdaterSnapshot,
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
            let label = if let Some(total) = updater.total_bytes {
                let percent = if total == 0 {
                    0
                } else {
                    ((updater.downloaded_bytes as f64 / total as f64) * 100.0).round() as u64
                };
                format!("Downloading update {percent}%")
            } else {
                "Downloading update...".to_string()
            };
            div()
                .text_xs()
                .text_color(rgb(theme::AI_DOT))
                .child(SharedString::from(label))
        }
        UpdaterStage::ReadyToInstall => {
            let version = updater.target_version.as_deref().unwrap_or("new");
            div()
                .text_xs()
                .text_color(rgb(theme::PROJECT_DOT))
                .cursor_pointer()
                .hover(|s| s.text_color(rgb(theme::PRIMARY_HOVER)))
                .child(SharedString::from(format!("⊞ Restart to update {version}")))
                .on_mouse_down(MouseButton::Left, (actions.on_install_update)())
        }
        UpdaterStage::UpdateAvailable => {
            div()
                .text_xs()
                .text_color(rgb(theme::PROJECT_DOT))
                .child(SharedString::from(format!(
                    "Update {} ready in Settings",
                    updater.target_version.as_deref().unwrap_or("ready")
                )))
        }
        UpdaterStage::Checking => div()
            .text_xs()
            .text_color(rgb(theme::TEXT_SUBTLE))
            .child("Checking for updates"),
        UpdaterStage::Error => div()
            .text_xs()
            .text_color(rgb(theme::DANGER_TEXT))
            .child("Update failed"),
        UpdaterStage::UpToDate => div()
            .text_xs()
            .text_color(rgb(theme::TEXT_SUBTLE))
            .child(SharedString::from(format!("v{}", updater.current_version))),
        _ => div().text_xs().text_color(rgb(theme::TEXT_DIM)).child(""),
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
    let format = format_description::parse("[hour]:[minute]").expect("valid time format");
    now.format(&format).unwrap_or_else(|_| "00:00".to_string())
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
    use super::running_terminal_metrics;
    use crate::state::{
        ResourceSnapshot, RuntimeState, ServerLaunchSpec, SessionDimensions, SessionRuntimeState,
        SessionStatus,
    };
    use crate::terminal::session::TerminalBackend;
    use std::collections::HashMap;
    use std::path::PathBuf;

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
}
