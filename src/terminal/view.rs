use crate::models::TabType;
use crate::state::{AiActivity, SessionStatus};
use crate::terminal::session::{
    TerminalCellSnapshot, TerminalCursorSnapshot, TerminalIndexedCellSnapshot, TerminalSessionView,
};
use crate::theme;
use alacritty_terminal::vte::ansi::CursorShape;
use gpui::{
    canvas, div, fill, img, point, px, rgb, size, AnyElement, App, Bounds, Hsla, ImageSource,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ObjectFit, ParentElement,
    SharedString, StrikethroughStyle, Styled, StyledImage, TextRun, UnderlineStyle, Window,
};

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
    pub blocking_notice: Option<String>,
    pub debug_enabled: bool,
    pub font_size: f32,
    pub cell_width: f32,
    pub line_height: f32,
    pub selection: Option<TerminalSelectionSnapshot>,
    pub runtime_controls: Option<TerminalRuntimeControlsModel>,
    pub splash_image: Option<std::sync::Arc<gpui::RenderImage>>,
}

pub struct TerminalPaneActions {
    pub on_start_server: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_stop_server: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_restart_server: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_clear_output: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_kill_port: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_open_local_url: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_prompt_action: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
}

#[derive(Debug, Clone)]
pub struct TerminalRuntimeControlsModel {
    pub port_label: Option<String>,
    pub port_color: u32,
    pub can_start: bool,
    pub can_stop: bool,
    pub can_restart: bool,
    pub can_clear: bool,
    pub can_kill_port: bool,
    pub can_open_url: bool,
    pub kill_label: &'static str,
    pub kill_color: u32,
    pub prompt_action_label: Option<String>,
    pub prompt_action_color: u32,
}

pub fn render_terminal_surface(
    model: &TerminalPaneModel,
    actions: Option<TerminalPaneActions>,
) -> impl IntoElement {
    let notice = model.startup_notice.as_ref().map(|message| {
        div()
            .px_2()
            .py_1()
            .bg(rgb(theme::PANEL_HEADER_BG))
            .text_xs()
            .text_color(rgb(theme::TEXT_MUTED))
            .child(SharedString::from(message.clone()))
    });
    let blocking_notice = model.blocking_notice.as_ref().map(|message| {
        div()
            .mx_2()
            .my_1()
            .py_2()
            .border_t_1()
            .border_b_1()
            .border_color(rgb(theme::BORDER_PRIMARY))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(SharedString::from(message.clone())),
            )
    });

    let is_ai_tab = matches!(
        model.active_tab_type,
        Some(TabType::Claude) | Some(TabType::Codex)
    );
    let status_text = model
        .session
        .as_ref()
        .map(|s| session_status_label(s, is_ai_tab))
        .unwrap_or(if is_ai_tab { "saved" } else { "" });
    let status_color = model
        .session
        .as_ref()
        .map(session_status_color)
        .unwrap_or(theme::TEXT_MUTED);
    let session_title = model
        .session
        .as_ref()
        .and_then(|session| session.runtime.title.clone())
        .filter(|title| is_meaningful_title(title))
        .unwrap_or_else(|| model.session_label.clone());
    let header_title = if model.active_project.is_empty() || session_title == model.active_project {
        session_title
    } else {
        format!("{} • {}", model.active_project, session_title)
    };
    let header_detail = surface_header_detail(model);
    let runtime_controls = model.runtime_controls.clone();
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
            model.cell_width,
            model.line_height,
        )
        .into_any_element()
    } else {
        render_empty_body(empty_surface_message(model), model.splash_image.clone())
            .into_any_element()
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
                .overflow_hidden()
                .child(
                    div()
                        .flex_shrink_0()
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
                        .overflow_hidden()
                        .min_w(px(0.0))
                        .children(
                            runtime_controls
                                .as_ref()
                                .and_then(|controls| controls.port_label.as_ref())
                                .map(|detail| {
                                    div()
                                        .text_xs()
                                        .text_color(rgb(runtime_controls
                                            .as_ref()
                                            .map(|controls| controls.port_color)
                                            .unwrap_or(theme::TEXT_DIM)))
                                        .child(SharedString::from(detail.clone()))
                                }),
                        )
                        .children(header_detail.map(|detail| {
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_DIM))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(SharedString::from(detail))
                        }))
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(rgb(status_color))
                                .child(status_text),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_DIM))
                                .child(SharedString::from(format!(
                                    "font {}",
                                    model.font_size.round() as u32
                                ))),
                        ),
                )
                .children(
                    actions
                        .zip(runtime_controls.clone())
                        .map(|(actions, controls)| {
                            render_runtime_actions(actions, controls).into_any_element()
                        }),
                ),
        )
        .child(
            div().flex_1().pb(px(2.0)).child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .bg(rgb(theme::TERMINAL_BG))
                    .children(notice)
                    .children(blocking_notice)
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
    cell_width: f32,
    line_height: f32,
) -> impl IntoElement {
    let (background_runs, text_runs, cursor_overlay) = collect_grid_paint_runs(session, selection);

    div()
        .flex_1()
        .flex()
        .flex_col()
        .bg(rgb(theme::TERMINAL_BG))
        .overflow_hidden()
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .px_1()
                .py(px(2.0))
                .bg(rgb(theme::TERMINAL_BG))
                .child(render_grid_canvas(
                    background_runs,
                    text_runs,
                    cursor_overlay,
                    font_size,
                    cell_width,
                    line_height,
                )),
        )
}

fn render_empty_body(
    message: String,
    splash_image: Option<std::sync::Arc<gpui::RenderImage>>,
) -> impl IntoElement {
    div()
        .flex_1()
        .bg(rgb(theme::TERMINAL_BG))
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden()
        .children(splash_image.map(|image| {
            img(ImageSource::Render(image))
                .size_full()
                .object_fit(ObjectFit::Cover)
        }))
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
    let has_live_terminal = session.runtime.status.is_live() || session.runtime.interactive_shell;

    if session.runtime.status.is_live() && session.runtime.resources.last_sample_at.is_some() {
        let mem_mb = session.runtime.resources.memory_bytes / 1024 / 1024;
        let cpu = session.runtime.resources.cpu_percent;
        let procs = session.runtime.resources.process_count;
        let uptime = session
            .runtime
            .started_at
            .map(|started| {
                let elapsed = started.elapsed();
                let total_secs = elapsed.as_secs();
                if total_secs >= 3600 {
                    format!("Up: {}h {}m", total_secs / 3600, (total_secs % 3600) / 60)
                } else if total_secs >= 60 {
                    format!("Up: {}m {}s", total_secs / 60, total_secs % 60)
                } else {
                    format!("Up: {}s", total_secs)
                }
            })
            .unwrap_or_default();
        let uptime_part = if uptime.is_empty() {
            String::new()
        } else {
            format!(" • {uptime}")
        };
        return Some(format!(
            "{mem_mb} MB • {cpu:.1}% • {procs} proc{}{uptime_part}",
            if procs == 1 { "" } else { "s" }
        ));
    }

    has_live_terminal.then(|| match model.active_tab_type.as_ref() {
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
        None => String::new(),
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

fn collect_grid_paint_runs(
    session: &TerminalSessionView,
    selection: Option<&TerminalSelectionSnapshot>,
) -> (
    Vec<TerminalBackgroundRect>,
    Vec<TerminalTextRun>,
    Option<TerminalCursorOverlay>,
) {
    let cursor = session.screen.cursor;
    let mut background_regions: Vec<BackgroundRegion> = Vec::new();
    let mut text_runs = Vec::new();
    let mut current_run: Option<TerminalTextRun> = None;
    let mut current_row = None;
    let mut previous_cell_had_extras = false;

    for indexed in &session.screen.cells {
        if current_row != Some(indexed.row) {
            if let Some(run) = current_run.take() {
                text_runs.push(run);
            }
            current_row = Some(indexed.row);
            previous_cell_had_extras = false;
        }

        let selected = line_selection_range(selection, indexed.row, session.screen.cols)
            .map(|(start, end)| indexed.column >= start && indexed.column < end)
            .unwrap_or(false);
        let cursor_cell = cursor.filter(|cursor| {
            cursor.row == indexed.row
                && cursor.column == indexed.column
                && matches!(cursor.shape, CursorShape::Block)
        });
        let style = effective_cell_style(&indexed.cell, selected, cursor_cell);

        if style.paint_background {
            let col = indexed.column;
            match background_regions.last_mut() {
                Some(region)
                    if region.color == style.background
                        && region.start_row == indexed.row
                        && region.end_row == indexed.row
                        && region.end_col + 1 == col =>
                {
                    region.end_col = col;
                }
                _ => {
                    background_regions.push(BackgroundRegion::new(
                        indexed.row,
                        col,
                        style.background,
                    ));
                }
            }
        }

        if indexed.cell.character == ' ' && previous_cell_had_extras {
            previous_cell_had_extras = false;
            continue;
        }
        previous_cell_had_extras = !indexed.cell.zero_width.is_empty();

        if is_blank_cell(&indexed.cell, &style) {
            continue;
        }

        let text_style = text_run_style(&style, indexed.cell.character);
        if let Some(run) = current_run.as_mut() {
            if run.can_append(&text_style, indexed.row, indexed.column) {
                run.append_cell(indexed.cell.character, &indexed.cell.zero_width);
                continue;
            }
        }

        if let Some(run) = current_run.take() {
            text_runs.push(run);
        }

        current_run = Some(TerminalTextRun::new(
            indexed,
            text_style,
            &indexed.cell.zero_width,
        ));
    }

    if let Some(run) = current_run.take() {
        text_runs.push(run);
    }

    let background_runs = merge_background_regions(background_regions)
        .into_iter()
        .flat_map(|region| {
            (region.start_row..=region.end_row).map(move |row| TerminalBackgroundRect {
                row,
                start_column: region.start_col,
                cell_count: region.end_col - region.start_col + 1,
                color: region.color,
            })
        })
        .collect();

    let cursor_overlay = cursor.and_then(|cursor| match cursor.shape {
        CursorShape::Underline | CursorShape::Beam => Some(TerminalCursorOverlay {
            row: cursor.row,
            column: cursor.column,
            shape: cursor.shape,
            color: theme::SUCCESS_TEXT,
        }),
        _ => None,
    });

    (background_runs, text_runs, cursor_overlay)
}

fn render_grid_canvas(
    background_runs: Vec<TerminalBackgroundRect>,
    text_runs: Vec<TerminalTextRun>,
    cursor_overlay: Option<TerminalCursorOverlay>,
    font_size: f32,
    cell_width: f32,
    line_height: f32,
) -> impl IntoElement {
    canvas(
        move |_bounds, _window, _cx| (background_runs, text_runs, cursor_overlay),
        move |bounds: Bounds<_>, (background_runs, text_runs, cursor_overlay), window, cx| {
            for run in background_runs {
                let position = point(
                    bounds.origin.x + px(run.start_column as f32 * cell_width),
                    bounds.origin.y + px(run.row as f32 * line_height),
                );
                let run_size = size(px(cell_width * run.cell_count as f32), px(line_height));
                window.paint_quad(fill(Bounds::new(position, run_size), rgb(run.color)));
            }

            for run in text_runs {
                let shaped_line = window.text_system().shape_line(
                    SharedString::from(run.text),
                    px(font_size),
                    &[run.style.clone()],
                    None,
                );
                let position = point(
                    bounds.origin.x + px(run.start_column as f32 * cell_width),
                    bounds.origin.y + px(run.row as f32 * line_height),
                );
                let _ = shaped_line.paint(position, px(line_height), window, cx);
            }

            if let Some(cursor) = cursor_overlay {
                let position = point(
                    bounds.origin.x + px(cursor.column as f32 * cell_width),
                    bounds.origin.y + px(cursor.row as f32 * line_height),
                );
                let cursor_bounds = match cursor.shape {
                    CursorShape::Underline => Bounds::new(
                        point(position.x, position.y + px((line_height - 2.0).max(0.0))),
                        size(px(cell_width.max(1.0)), px(2.0)),
                    ),
                    CursorShape::Beam => {
                        Bounds::new(position, size(px(2.0), px(line_height.max(1.0))))
                    }
                    _ => Bounds::new(position, size(px(cell_width), px(line_height))),
                };
                window.paint_quad(fill(cursor_bounds, rgb(cursor.color)));
            }
        },
    )
    .size_full()
}

#[derive(Clone)]
struct TerminalTextRun {
    row: usize,
    start_column: usize,
    cell_count: usize,
    text: String,
    style: TextRun,
}

impl TerminalTextRun {
    fn new(indexed: &TerminalIndexedCellSnapshot, style: TextRun, zero_width: &[char]) -> Self {
        let mut text = String::with_capacity(8);
        text.push(indexed.cell.character);
        for &character in zero_width {
            text.push(character);
        }

        let mut style = style;
        style.len = text.len();

        Self {
            row: indexed.row,
            start_column: indexed.column,
            cell_count: 1,
            text,
            style,
        }
    }

    fn can_append(&self, other_style: &TextRun, row: usize, column: usize) -> bool {
        self.row == row
            && self.start_column + self.cell_count == column
            && self.style.font == other_style.font
            && self.style.color == other_style.color
            && self.style.background_color == other_style.background_color
            && self.style.underline == other_style.underline
            && self.style.strikethrough == other_style.strikethrough
    }

    fn append_cell(&mut self, character: char, zero_width: &[char]) {
        self.text.push(character);
        self.cell_count += 1;
        self.style.len += character.len_utf8();
        for &extra in zero_width {
            self.text.push(extra);
            self.style.len += extra.len_utf8();
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TerminalBackgroundRect {
    row: usize,
    start_column: usize,
    cell_count: usize,
    color: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EffectiveCellStyle {
    foreground: u32,
    background: u32,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    undercurl: bool,
    strike: bool,
    paint_background: bool,
}

#[derive(Debug, Clone, Copy)]
struct TerminalCursorOverlay {
    row: usize,
    column: usize,
    shape: CursorShape,
    color: u32,
}

#[derive(Debug, Clone, Copy)]
struct BackgroundRegion {
    start_row: usize,
    start_col: usize,
    end_row: usize,
    end_col: usize,
    color: u32,
}

impl BackgroundRegion {
    fn new(row: usize, col: usize, color: u32) -> Self {
        Self {
            start_row: row,
            start_col: col,
            end_row: row,
            end_col: col,
            color,
        }
    }

    fn can_merge_with(&self, other: &BackgroundRegion) -> bool {
        if self.color != other.color {
            return false;
        }

        if self.start_row == other.start_row && self.end_row == other.end_row {
            return self.end_col + 1 == other.start_col || other.end_col + 1 == self.start_col;
        }

        if self.start_col == other.start_col && self.end_col == other.end_col {
            return self.end_row + 1 == other.start_row || other.end_row + 1 == self.start_row;
        }

        false
    }

    fn merge_with(&mut self, other: &BackgroundRegion) {
        self.start_row = self.start_row.min(other.start_row);
        self.start_col = self.start_col.min(other.start_col);
        self.end_row = self.end_row.max(other.end_row);
        self.end_col = self.end_col.max(other.end_col);
    }
}

fn merge_background_regions(regions: Vec<BackgroundRegion>) -> Vec<BackgroundRegion> {
    if regions.is_empty() {
        return regions;
    }

    let mut merged = regions;
    let mut changed = true;
    while changed {
        changed = false;
        let mut i = 0;
        while i < merged.len() {
            let mut j = i + 1;
            while j < merged.len() {
                if merged[i].can_merge_with(&merged[j]) {
                    let other = merged.remove(j);
                    merged[i].merge_with(&other);
                    changed = true;
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
    }

    merged
}

fn text_run_style(style: &EffectiveCellStyle, character: char) -> TextRun {
    let mut color: Hsla = rgb(style.foreground).into();
    if style.dim {
        color.a *= 0.7;
    }

    let underline = style.underline.then_some(UnderlineStyle {
        color: Some(color),
        thickness: px(1.0),
        wavy: style.undercurl,
    });
    let strikethrough = style.strike.then_some(StrikethroughStyle {
        color: Some(color),
        thickness: px(1.0),
    });

    let mut terminal_font = crate::terminal::terminal_font();
    if style.bold {
        terminal_font = terminal_font.bold();
    }
    if style.italic {
        terminal_font = terminal_font.italic();
    }

    TextRun {
        len: character.len_utf8(),
        font: terminal_font,
        color,
        background_color: None,
        underline,
        strikethrough,
    }
}

fn is_blank_cell(cell: &TerminalCellSnapshot, style: &EffectiveCellStyle) -> bool {
    cell.character == ' '
        && cell.zero_width.is_empty()
        && !style.paint_background
        && !cell.has_hyperlink
        && !cell.underline
        && !cell.strike
}

fn effective_cell_style(
    cell: &TerminalCellSnapshot,
    selected: bool,
    cursor: Option<TerminalCursorSnapshot>,
) -> EffectiveCellStyle {
    let mut foreground = cell.foreground;
    let mut background = cell.background;
    let mut bold = cell.bold;
    let mut dim = cell.dim;
    let italic = cell.italic;
    let underline = cell.underline;
    let undercurl = cell.undercurl;
    let strike = cell.strike;
    let mut paint_background = !cell.default_background;

    if selected {
        foreground = theme::SELECTION_TEXT;
        background = theme::SELECTION_BG;
        paint_background = true;
    }

    if let Some(cursor) = cursor {
        match cursor.shape {
            CursorShape::Block => {
                foreground = theme::PANEL_BG;
                background = theme::SUCCESS_TEXT;
                bold = true;
                dim = false;
                paint_background = true;
            }
            CursorShape::Underline | CursorShape::Beam => {}
            _ => {}
        }
    }

    EffectiveCellStyle {
        foreground,
        background,
        bold,
        dim,
        italic,
        underline,
        undercurl,
        strike,
        paint_background,
    }
}

fn render_runtime_actions(
    actions: TerminalPaneActions,
    controls: TerminalRuntimeControlsModel,
) -> impl IntoElement {
    let TerminalPaneActions {
        on_start_server,
        on_stop_server,
        on_restart_server,
        on_clear_output,
        on_kill_port,
        on_open_local_url,
        on_prompt_action,
    } = actions;

    div()
        .flex()
        .items_center()
        .gap(px(4.0))
        .children(
            controls
                .can_start
                .then_some(on_start_server)
                .flatten()
                .map(|on_click| runtime_action_button("start", theme::SUCCESS_TEXT, on_click)),
        )
        .children(
            controls
                .can_stop
                .then_some(on_stop_server)
                .flatten()
                .map(|on_click| runtime_action_button("stop", theme::DANGER_TEXT, on_click)),
        )
        .children(
            controls
                .can_restart
                .then_some(on_restart_server)
                .flatten()
                .map(|on_click| runtime_action_button("restart", theme::WARNING_TEXT, on_click)),
        )
        .children(
            controls
                .can_clear
                .then_some(on_clear_output)
                .flatten()
                .map(|on_click| runtime_action_button("clear", theme::TEXT_MUTED, on_click)),
        )
        .children(
            controls
                .can_kill_port
                .then_some(on_kill_port)
                .flatten()
                .map(|on_click| {
                    runtime_action_button(controls.kill_label, controls.kill_color, on_click)
                }),
        )
        .children(
            controls
                .can_open_url
                .then_some(on_open_local_url)
                .flatten()
                .map(|on_click| runtime_action_button("open", theme::PRIMARY, on_click)),
        )
        .children(
            controls
                .prompt_action_label
                .zip(on_prompt_action)
                .map(|(label, on_click)| {
                    runtime_action_button(label.as_str(), controls.prompt_action_color, on_click)
                }),
        )
}

fn runtime_action_button(
    label: &str,
    color: u32,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .px(px(5.0))
        .py(px(1.0))
        .border_1()
        .border_color(rgb(theme::BORDER_PRIMARY))
        .bg(rgb(theme::PANEL_HEADER_BG))
        .rounded_sm()
        .text_xs()
        .text_color(rgb(color))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::BUTTON_HOVER_BG)))
        .child(SharedString::from(label.to_string()))
        .on_mouse_down(MouseButton::Left, on_click)
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

fn is_meaningful_title(title: &str) -> bool {
    let t = title.trim();
    if t.is_empty() {
        return false;
    }
    if t.contains("\\system32\\") || t.contains("/bin/") || t.contains("/usr/") {
        return false;
    }
    if t.ends_with(".exe") && (t.contains('\\') || t.contains('/')) {
        return false;
    }
    true
}

fn session_status_label(session: &TerminalSessionView, is_ai_tab: bool) -> &'static str {
    if session.runtime.unseen_ready {
        return "ready";
    }
    if matches!(session.runtime.ai_activity, Some(AiActivity::Thinking)) {
        return "thinking";
    }

    match session.runtime.status {
        SessionStatus::Starting => "starting",
        SessionStatus::Running => {
            if is_ai_tab {
                "idle"
            } else {
                "Live Terminal"
            }
        }
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
