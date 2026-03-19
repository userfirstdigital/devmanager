use crate::models::TabType;
use crate::state::{AiActivity, SessionStatus};
use crate::terminal::session::{TerminalCursorSnapshot, TerminalSessionView};
use crate::theme;
use gpui::{div, px, rgb, AnyElement, IntoElement, ParentElement, SharedString, Styled};

pub const TERMINAL_FONT_SIZE: f32 = 13.0;
pub const TERMINAL_LINE_HEIGHT: f32 = 18.0;

pub fn terminal_line_height(font_size: f32) -> f32 {
    (font_size + 5.0).max(TERMINAL_LINE_HEIGHT)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSelectionSnapshot {
    pub start_row: usize,
    pub start_column: usize,
    pub end_row: usize,
    pub end_column: usize,
}

#[derive(Debug, Clone)]
pub struct TerminalPaneModel {
    pub active_project: String,
    pub session_label: String,
    pub active_tab_type: Option<TabType>,
    pub session: Option<TerminalSessionView>,
    pub startup_notice: Option<String>,
    pub debug_enabled: bool,
    pub font_size: f32,
    pub line_height: f32,
    pub selection: Option<TerminalSelectionSnapshot>,
}

pub fn render_terminal_surface(model: &TerminalPaneModel) -> impl IntoElement {
    let notice = model.startup_notice.as_ref().map(|message| {
        div()
            .px_2()
            .py_1()
            .bg(rgb(theme::PANEL_HEADER_BG))
            .text_xs()
            .text_color(rgb(theme::TEXT_MUTED))
            .child(SharedString::from(message.clone()))
    });

    let status_text = model
        .session
        .as_ref()
        .map(session_status_label)
        .unwrap_or("saved");
    let status_color = model
        .session
        .as_ref()
        .map(session_status_color)
        .unwrap_or(theme::TEXT_MUTED);
    let session_title = model
        .session
        .as_ref()
        .and_then(|session| session.runtime.title.clone())
        .unwrap_or_else(|| model.session_label.clone());
    let header_title = if model.active_project.is_empty() || session_title == model.active_project {
        session_title
    } else {
        format!("{} • {}", model.active_project, session_title)
    };
    let header_detail = surface_header_detail(model);
    let metrics = model.session.as_ref().map(|session| {
        let metrics = &session.runtime.metrics;
        format!(
            "{} B/s • {} fps • {} us • resize {} • scroll {}",
            metrics.pty_bytes_per_second,
            metrics.frames_per_second,
            metrics.last_render_micros,
            metrics.resize_events,
            metrics.scroll_events
        )
    });
    let exit_banner = model
        .session
        .as_ref()
        .and_then(|session| session.runtime.exit.as_ref())
        .map(|exit| {
            div()
                .px_2()
                .py_1()
                .bg(rgb(theme::PROJECT_ROW_BG))
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(exit.summary.clone()))
        });
    let terminal_body: AnyElement = if let Some(session) = model.session.as_ref() {
        render_grid(
            session,
            model.selection.as_ref(),
            model.font_size,
            model.line_height,
        )
        .into_any_element()
    } else {
        render_empty_body(empty_surface_message(model)).into_any_element()
    };

    div()
        .flex_1()
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::APP_BG))
        .child(
            div()
                .h(px(22.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .px(px(6.0))
                .bg(rgb(theme::TOPBAR_BG))
                .border_b_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(SharedString::from(header_title)),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(12.0))
                        .children(header_detail.map(|detail| {
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_DIM))
                                .child(SharedString::from(detail))
                        }))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(status_color))
                                .child(status_text),
                        )
                        .child(div().text_xs().text_color(rgb(theme::TEXT_DIM)).child(
                            SharedString::from(format!("font {}", model.font_size.round() as u32)),
                        )),
                ),
        )
        .child(
            div().flex_1().pb(px(2.0)).child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .bg(rgb(theme::PANEL_BG))
                    .children(notice)
                    .children(exit_banner)
                    .child(terminal_body)
                    .children(model.debug_enabled.then(|| {
                        div()
                            .px_2()
                            .pb_1()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_SUBTLE))
                            .child(SharedString::from(
                                metrics.unwrap_or_else(|| "No metrics yet".to_string()),
                            ))
                    })),
            ),
        )
}

fn render_grid(
    session: &TerminalSessionView,
    selection: Option<&TerminalSelectionSnapshot>,
    font_size: f32,
    line_height: f32,
) -> impl IntoElement {
    let cursor = session.screen.cursor;
    let rows = session
        .screen
        .lines
        .iter()
        .enumerate()
        .map(|(row_index, line)| {
            let selection_range = line_selection_range(selection, row_index, line.chars().count());
            render_line(
                line,
                cursor.filter(|cursor| cursor.row == row_index),
                selection_range,
                font_size,
                line_height,
            )
        });

    div()
        .flex_1()
        .flex()
        .flex_col()
        .bg(rgb(theme::PANEL_BG))
        .overflow_hidden()
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .px_1()
                .py(px(2.0))
                .bg(rgb(theme::PANEL_BG))
                .children(rows),
        )
}

fn render_empty_body(message: String) -> impl IntoElement {
    div()
        .flex_1()
        .bg(rgb(theme::PANEL_BG))
        .flex()
        .items_start()
        .justify_start()
        .children((!message.is_empty()).then(|| {
            div()
                .px(px(10.0))
                .py(px(8.0))
                .text_xs()
                .text_color(rgb(theme::TEXT_SUBTLE))
                .child(SharedString::from(message))
        }))
}

fn surface_header_detail(model: &TerminalPaneModel) -> Option<String> {
    let session = model.session.as_ref()?;

    if let Some(_) = session.runtime.resources.last_sample_at {
        return Some(format!(
            "{} MB • {:.1}% • {} proc",
            session.runtime.resources.memory_bytes / 1024 / 1024,
            session.runtime.resources.cpu_percent,
            session.runtime.resources.child_count
        ));
    }

    session
        .runtime
        .status
        .is_live()
        .then(|| match model.active_tab_type.as_ref() {
            Some(TabType::Claude) | Some(TabType::Codex) => {
                format!(
                    "{} • {}",
                    tab_kind_label(model.active_tab_type.as_ref()),
                    session.runtime.shell_program
                )
            }
            _ => session.runtime.shell_program.clone(),
        })
}

fn empty_surface_message(model: &TerminalPaneModel) -> String {
    match model.active_tab_type.as_ref() {
        Some(TabType::Server) => String::new(),
        Some(TabType::Claude) | Some(TabType::Codex) => String::new(),
        Some(TabType::Ssh) => String::new(),
        None => "Select a command in the sidebar.".to_string(),
    }
}

fn tab_kind_label(tab_type: Option<&TabType>) -> &'static str {
    match tab_type {
        Some(TabType::Server) => "server log",
        Some(TabType::Claude) => "claude terminal",
        Some(TabType::Codex) => "codex terminal",
        Some(TabType::Ssh) => "ssh console",
        None => "local shell",
    }
}

fn render_line(
    line: &str,
    cursor: Option<TerminalCursorSnapshot>,
    selection: Option<(usize, usize)>,
    font_size: f32,
    line_height: f32,
) -> impl IntoElement {
    let base = div()
        .h(px(line_height))
        .flex()
        .items_center()
        .font_family(".ZedMono")
        .text_size(px(font_size))
        .line_height(px(line_height))
        .whitespace_nowrap()
        .text_color(rgb(theme::TEXT_PRIMARY));

    if let Some((start, end)) = selection {
        let characters: Vec<char> = line.chars().collect();
        let prefix: String = characters.iter().take(start).collect();
        let selected: String = characters.iter().skip(start).take(end - start).collect();
        let suffix: String = characters.iter().skip(end).collect();

        base.child(SharedString::from(prefix))
            .child(
                div()
                    .bg(rgb(theme::SELECTION_BG))
                    .text_color(rgb(theme::SELECTION_TEXT))
                    .child(SharedString::from(selected)),
            )
            .child(SharedString::from(suffix))
    } else if let Some(cursor) = cursor {
        let characters: Vec<char> = line.chars().collect();
        let prefix: String = characters.iter().take(cursor.column).collect();
        let cursor_char = characters
            .get(cursor.column)
            .copied()
            .unwrap_or('\u{00a0}')
            .to_string();
        let suffix: String = characters.iter().skip(cursor.column + 1).collect();

        base.child(SharedString::from(prefix))
            .child(
                div()
                    .px(px(2.0))
                    .bg(rgb(theme::SUCCESS_TEXT))
                    .text_color(rgb(theme::PANEL_BG))
                    .child(SharedString::from(cursor_char)),
            )
            .child(SharedString::from(suffix))
    } else {
        base.child(SharedString::from(line.to_string()))
    }
}

fn line_selection_range(
    selection: Option<&TerminalSelectionSnapshot>,
    row_index: usize,
    line_len: usize,
) -> Option<(usize, usize)> {
    let selection = selection?;
    if row_index < selection.start_row || row_index > selection.end_row {
        return None;
    }

    let start = if row_index == selection.start_row {
        selection.start_column.min(line_len)
    } else {
        0
    };
    let end = if row_index == selection.end_row {
        selection.end_column.min(line_len)
    } else {
        line_len
    };

    (start < end).then_some((start, end))
}

fn session_status_label(session: &TerminalSessionView) -> &'static str {
    if session.runtime.unseen_ready {
        return "ready";
    }
    if matches!(session.runtime.ai_activity, Some(AiActivity::Thinking)) {
        return "thinking";
    }

    match session.runtime.status {
        SessionStatus::Starting => "starting",
        SessionStatus::Running => "Live Terminal",
        SessionStatus::Stopping => "stopping",
        SessionStatus::Crashed => "crashed",
        SessionStatus::Exited => "exited",
        SessionStatus::Failed => "failed",
        SessionStatus::Stopped => "stopped",
    }
}

fn session_status_color(session: &TerminalSessionView) -> u32 {
    if session.runtime.unseen_ready {
        return theme::SUCCESS_TEXT;
    }
    if matches!(session.runtime.ai_activity, Some(AiActivity::Thinking)) {
        return theme::WARNING_TEXT;
    }

    match session.runtime.status {
        SessionStatus::Running => theme::TEXT_SUBTLE,
        SessionStatus::Starting | SessionStatus::Stopping => theme::WARNING_TEXT,
        SessionStatus::Crashed | SessionStatus::Failed => theme::DANGER_TEXT,
        _ => theme::TEXT_MUTED,
    }
}
