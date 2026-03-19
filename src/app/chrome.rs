use crate::state::RuntimeState;
use crate::theme;
use crate::updater::{UpdaterSnapshot, UpdaterStage};
use gpui::{
    div, px, rgb, App, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    SharedString, Styled, Window,
};
use time::{format_description, OffsetDateTime};

pub const STATUS_BAR_HEIGHT_PX: f32 = 18.0;

pub struct StatusBarActions<'a> {
    pub on_install_update: &'a dyn Fn() -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
}

pub fn render_status_bar(
    runtime: &RuntimeState,
    updater: &UpdaterSnapshot,
    actions: StatusBarActions<'_>,
) -> impl IntoElement {
    let (running_servers, total_memory_bytes) = running_server_metrics(runtime);
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
                        .child(
                            div()
                                .size(px(4.0))
                                .rounded_full()
                                .bg(rgb(theme::SUCCESS_TEXT)),
                        )
                        .child(SharedString::from(format!(
                            "{running_servers} server{} running",
                            plural(running_servers)
                        ))),
                )
                .children((running_servers > 0).then(|| {
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
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
                .child(SharedString::from(format!("Restart to update {version}")))
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

fn running_server_metrics(runtime: &RuntimeState) -> (usize, u64) {
    runtime
        .sessions
        .values()
        .filter(|session| session.command_id.is_some() && session.status.is_live())
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
