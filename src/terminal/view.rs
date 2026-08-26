use crate::browser::{
    compact_browser_attachment_text, compact_browser_attachment_url, redact_browser_text,
    BrowserAnnotation, BrowserWorkspaceKey, BrowserWorkspaceSnapshot,
};
use crate::models::TabType;
use crate::state::{AiActivity, ResourceMetricValueState, ResourceSnapshot, SessionStatus};
use crate::terminal::session::{
    TerminalCellSnapshot, TerminalCursorSnapshot, TerminalIndexedCellSnapshot, TerminalSessionView,
};
use crate::theme;
use crate::ui::tokens::ThemeTokens;
use alacritty_terminal::vte::ansi::CursorShape;
use gpui::{
    canvas, div, fill, img, point, px, rgb, size, AnyElement, App, Bounds, Hsla, ImageSource,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ObjectFit, ParentElement,
    SharedString, StrikethroughStyle, Styled, StyledImage, TextRun, UnderlineStyle, Window,
};
use std::sync::Arc;

pub const TERMINAL_FONT_SIZE: f32 = 13.0;
pub const TERMINAL_LINE_HEIGHT: f32 = 18.0;
pub const TERMINAL_SCROLLBAR_WIDTH_PX: f32 = 10.0;
pub const TERMINAL_SCROLLBAR_TRACK_INSET_X_PX: f32 = 2.0;
pub const TERMINAL_SCROLLBAR_TRACK_INSET_Y_PX: f32 = 6.0;
pub const TERMINAL_SCROLLBAR_TRACK_WIDTH_PX: f32 = 6.0;
pub const TERMINAL_SCROLLBAR_MIN_THUMB_HEIGHT_PX: f32 = 18.0;

pub fn terminal_line_height(font_size: f32) -> f32 {
    (font_size + 5.0).max(TERMINAL_LINE_HEIGHT)
}

/// Explicit Copy palette for terminal chrome and default cell named colors.
/// Built from [`ThemeTokens`] for live themed paint, or from legacy theme
/// constants for the default wrapper used by older callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalRenderPalette {
    pub canvas: u32,
    pub panel: u32,
    pub panel_header: u32,
    pub row: u32,
    pub row_hover: u32,
    pub button_hover: u32,
    pub border: u32,
    pub text_primary: u32,
    pub text_muted: u32,
    pub text_subtle: u32,
    pub text_dim: u32,
    pub selection_bg: u32,
    pub selection_text: u32,
    pub primary: u32,
    pub primary_muted: u32,
    pub danger: u32,
    pub danger_bg: u32,
    pub warning: u32,
    pub success: u32,
    pub terminal_bg: u32,
    pub terminal_fg: u32,
    pub terminal_cursor: u32,
    pub terminal_selection: u32,
    pub scrollbar_track: u32,
    pub scrollbar_thumb: u32,
}

impl TerminalRenderPalette {
    /// Map every chrome / named default color from the caller's active tokens.
    pub fn from_tokens(tokens: ThemeTokens) -> Self {
        Self {
            canvas: tokens.surfaces.canvas.to_u32(),
            panel: tokens.surfaces.sunken.to_u32(),
            panel_header: tokens.surfaces.raised.to_u32(),
            row: tokens.surfaces.overlay.to_u32(),
            row_hover: tokens.surfaces.hover.to_u32(),
            button_hover: tokens.surfaces.hover.to_u32(),
            border: tokens.borders.default.to_u32(),
            text_primary: tokens.text.primary.to_u32(),
            text_muted: tokens.text.muted.to_u32(),
            text_subtle: tokens.text.disabled.to_u32(),
            text_dim: tokens.text.disabled.to_u32(),
            selection_bg: tokens.terminal.selection.to_u32(),
            selection_text: tokens.text.on_selection.to_u32(),
            primary: tokens.actions.primary.default.background.to_u32(),
            primary_muted: tokens.actions.primary.selected.background.to_u32(),
            danger: tokens.status.destructive.to_u32(),
            danger_bg: tokens.status.destructive_surface.to_u32(),
            warning: tokens.status.warning.to_u32(),
            success: tokens.status.success.to_u32(),
            terminal_bg: tokens.terminal.background.to_u32(),
            terminal_fg: tokens.terminal.foreground.to_u32(),
            terminal_cursor: tokens.terminal.cursor.to_u32(),
            terminal_selection: tokens.terminal.selection.to_u32(),
            scrollbar_track: tokens.surfaces.raised.to_u32(),
            scrollbar_thumb: tokens.text.muted.to_u32(),
        }
    }

    /// Preserve the pre-token hard-coded visual defaults for legacy callers.
    pub fn legacy_default() -> Self {
        Self {
            canvas: theme::APP_BG,
            panel: theme::PANEL_BG,
            panel_header: theme::PANEL_HEADER_BG,
            row: theme::PROJECT_ROW_BG,
            row_hover: theme::ROW_HOVER_BG,
            button_hover: theme::BUTTON_HOVER_BG,
            border: theme::BORDER_PRIMARY,
            text_primary: theme::TEXT_PRIMARY,
            text_muted: theme::TEXT_MUTED,
            text_subtle: theme::TEXT_SUBTLE,
            text_dim: theme::TEXT_DIM,
            selection_bg: theme::SELECTION_BG,
            selection_text: theme::SELECTION_TEXT,
            primary: theme::PRIMARY,
            primary_muted: theme::PRIMARY_MUTED,
            danger: theme::DANGER_TEXT,
            danger_bg: theme::DANGER_BG_SUBTLE,
            warning: theme::WARNING_TEXT,
            success: theme::SUCCESS_TEXT,
            terminal_bg: theme::TERMINAL_BG,
            terminal_fg: theme::TEXT_PRIMARY,
            terminal_cursor: theme::SUCCESS_TEXT,
            terminal_selection: theme::SELECTION_BG,
            scrollbar_track: theme::PANEL_HEADER_BG,
            scrollbar_thumb: theme::TEXT_DIM,
        }
    }
}

/// Public mapping seam used by themed render entrypoints and contract tests.
pub fn terminal_render_palette_from_tokens(tokens: ThemeTokens) -> TerminalRenderPalette {
    TerminalRenderPalette::from_tokens(tokens)
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
    pub actionable_notice: Option<TerminalActionableNotice>,
    pub pending_annotations: Vec<PendingAnnotationChipModel>,
    pub debug_enabled: bool,
    pub font_size: f32,
    pub cell_width: f32,
    pub line_height: f32,
    pub selection: Option<TerminalSelectionSnapshot>,
    pub search: Option<TerminalSearchUiModel>,
    pub search_highlight: Option<TerminalSearchHighlight>,
    pub scrollbar: Option<TerminalScrollbarModel>,
    pub runtime_controls: Option<TerminalRuntimeControlsModel>,
    pub splash_image: Option<std::sync::Arc<gpui::RenderImage>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAnnotationAction {
    pub workspace_key: BrowserWorkspaceKey,
    pub annotation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAnnotationChipModel {
    pub action: PendingAnnotationAction,
    pub stable_id: String,
    pub comment: String,
    pub url: String,
    pub stale: bool,
}

pub fn pending_annotation_chip_models(
    active_tab_type: Option<&TabType>,
    workspace_key: &BrowserWorkspaceKey,
    snapshot: &BrowserWorkspaceSnapshot,
    pending_annotations: &[BrowserAnnotation],
) -> Vec<PendingAnnotationChipModel> {
    if !matches!(active_tab_type, Some(TabType::Claude | TabType::Codex)) {
        return Vec::new();
    }

    pending_annotations
        .iter()
        .map(|annotation| PendingAnnotationChipModel {
            action: PendingAnnotationAction {
                workspace_key: workspace_key.clone(),
                annotation_id: annotation.id.clone(),
            },
            stable_id: compact_browser_attachment_text(&annotation.id, 64),
            comment: compact_browser_attachment_text(&annotation.comment, 96),
            url: compact_browser_attachment_url(&annotation.url, 96),
            stale: pending_annotation_is_stale(snapshot, annotation),
        })
        .collect()
}

fn pending_annotation_is_stale(
    snapshot: &BrowserWorkspaceSnapshot,
    annotation: &BrowserAnnotation,
) -> bool {
    annotation.tab_id.is_empty()
        || annotation.anchor_revision != snapshot.revision
        || snapshot
            .tabs
            .iter()
            .find(|tab| tab.id == annotation.tab_id)
            .is_none_or(|tab| redact_browser_text(&tab.url) != redact_browser_text(&annotation.url))
}

#[derive(Debug, Clone)]
pub struct TerminalActionableNotice {
    pub message: String,
    pub action_label: &'static str,
    pub action_color: u32,
}

pub struct TerminalPaneActions {
    pub on_open_browser: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_preview_annotation: Option<PendingAnnotationActionHandler>,
    pub on_remove_annotation: Option<PendingAnnotationActionHandler>,
    pub on_start_server: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_stop_server: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_restart_server: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_clear_output: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_actionable_notice_action: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_open_local_url: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_prompt_action: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_toggle_search: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_search_prev: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_search_next: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_toggle_search_case: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_close_search: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_jump_prev_prompt: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_jump_next_prompt: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_export_screen: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_export_scrollback: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_export_selection: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_take_remote_control: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_release_remote_control: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_toggle_mouse_override: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_toggle_read_only: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub scrollbar: Option<TerminalScrollbarActions>,
}

pub type PendingAnnotationActionHandler =
    Arc<dyn Fn(PendingAnnotationAction, &MouseDownEvent, &mut Window, &mut App)>;

#[derive(Clone)]
pub struct TerminalScrollbarActions {
    pub on_mouse_down: Arc<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_mouse_move: Arc<dyn Fn(&gpui::MouseMoveEvent, &mut Window, &mut App)>,
    pub on_mouse_up: Arc<dyn Fn(&gpui::MouseUpEvent, &mut Window, &mut App)>,
}

#[derive(Debug, Clone)]
pub struct TerminalRuntimeControlsModel {
    pub port_label: Option<String>,
    pub port_color: u32,
    pub can_start: bool,
    pub can_stop: bool,
    pub can_restart: bool,
    pub can_clear: bool,
    pub can_open_url: bool,
    pub prompt_action_label: Option<String>,
    pub prompt_action_color: u32,
    pub search_active: bool,
    pub search_case_sensitive: bool,
    pub search_summary: Option<String>,
    pub can_search: bool,
    pub can_jump_prev_prompt: bool,
    pub can_jump_next_prompt: bool,
    pub can_export_screen: bool,
    pub can_export_scrollback: bool,
    pub can_export_selection: bool,
    pub remote_control: Option<TerminalRemoteControlModel>,
    pub mouse_override_enabled: bool,
    pub read_only_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct TerminalRemoteControlModel {
    pub label: String,
    pub color: u32,
    pub can_take: bool,
    pub can_release: bool,
}

#[derive(Debug, Clone)]
pub struct TerminalSearchUiModel {
    pub query: String,
    pub summary: String,
    pub case_sensitive: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct TerminalSearchHighlight {
    pub row: usize,
    pub start_column: usize,
    pub end_column: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct TerminalScrollbarModel {
    pub thumb_top_ratio: f32,
    pub thumb_height_ratio: f32,
}

/// Overlay painted over the last valid replica grid. The cockpit never owns a
/// `TerminalSession`; it only projects a `TerminalReplica` snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalReplicaOverlay {
    None,
    Reconnecting,
    Resyncing,
    Exited { summary: String },
}

/// Borrowed replica snapshot plus client-local viewport/overlay state.
pub struct ReplicaPaneRequest<'a> {
    pub active_project: &'a str,
    pub session_label: &'a str,
    pub replica_view: Option<&'a TerminalSessionView>,
    pub last_valid_view: Option<&'a TerminalSessionView>,
    pub overlay: TerminalReplicaOverlay,
    pub selection: Option<TerminalSelectionSnapshot>,
    pub search: Option<TerminalSearchUiModel>,
    pub search_highlight: Option<TerminalSearchHighlight>,
    pub scrollbar: Option<TerminalScrollbarModel>,
}

/// Map a Phase 3 replica snapshot onto the existing native terminal surface.
///
/// Overlay states use only the last valid grid. They never fall back to an
/// uncorrelated optional replica view.
pub fn terminal_pane_from_replica(request: ReplicaPaneRequest<'_>) -> TerminalPaneModel {
    let session = match &request.overlay {
        TerminalReplicaOverlay::None => request.replica_view.cloned(),
        _ => request.last_valid_view.cloned(),
    };
    let blocking_notice = match &request.overlay {
        TerminalReplicaOverlay::Reconnecting => Some(String::from("Reconnecting to terminal")),
        TerminalReplicaOverlay::Resyncing => Some(String::from("Resynchronizing terminal")),
        TerminalReplicaOverlay::Exited { summary } => Some(bound_overlay_summary(summary)),
        TerminalReplicaOverlay::None => None,
    };
    let cell_width = session
        .as_ref()
        .map(|view| f32::from(view.runtime.dimensions.cell_width))
        .filter(|width| *width > 0.0)
        .unwrap_or(8.0);
    let line_height = session
        .as_ref()
        .map(|view| f32::from(view.runtime.dimensions.cell_height))
        .filter(|height| *height > 0.0)
        .unwrap_or_else(|| terminal_line_height(TERMINAL_FONT_SIZE));
    TerminalPaneModel {
        active_project: request.active_project.to_string(),
        session_label: request.session_label.to_string(),
        active_tab_type: None,
        session,
        startup_notice: None,
        blocking_notice,
        actionable_notice: None,
        pending_annotations: Vec::new(),
        debug_enabled: false,
        font_size: TERMINAL_FONT_SIZE,
        cell_width,
        line_height,
        selection: request.selection,
        search: request.search,
        search_highlight: request.search_highlight,
        scrollbar: request.scrollbar,
        runtime_controls: None,
        splash_image: None,
    }
}

fn bound_overlay_summary(summary: &str) -> String {
    crate::ui::components::interaction::redacted_bounded_text(
        "terminal exit summary",
        summary,
        160,
        640,
    )
    .unwrap_or_else(|_| String::from("Terminal exited"))
}

pub fn render_terminal_surface(
    model: &TerminalPaneModel,
    actions: Option<TerminalPaneActions>,
) -> impl IntoElement {
    render_terminal_surface_with_palette(model, actions, TerminalRenderPalette::legacy_default())
}

/// Themed terminal surface: every non-ANSI chrome color comes from `tokens`.
pub fn render_terminal_surface_with_tokens(
    model: &TerminalPaneModel,
    actions: Option<TerminalPaneActions>,
    tokens: ThemeTokens,
) -> impl IntoElement {
    render_terminal_surface_with_palette(model, actions, TerminalRenderPalette::from_tokens(tokens))
}

fn render_terminal_surface_with_palette(
    model: &TerminalPaneModel,
    actions: Option<TerminalPaneActions>,
    palette: TerminalRenderPalette,
) -> impl IntoElement {
    let mut actions = actions;
    let actionable_notice_action = actions
        .as_mut()
        .and_then(|actions| actions.on_actionable_notice_action.take());
    let scrollbar_actions = actions
        .as_ref()
        .and_then(|actions| actions.scrollbar.clone());
    let open_browser_action = actions
        .as_mut()
        .and_then(|actions| actions.on_open_browser.take());
    let preview_annotation_action = actions
        .as_ref()
        .and_then(|actions| actions.on_preview_annotation.clone());
    let remove_annotation_action = actions
        .as_ref()
        .and_then(|actions| actions.on_remove_annotation.clone());
    // Hide the plain muted startup_notice when an actionable banner is taking over,
    // so we don't show the same text twice.
    let notice = if model.actionable_notice.is_some() {
        None
    } else {
        model.startup_notice.as_ref().map(|message| {
            div()
                .px_2()
                .py_1()
                .bg(rgb(palette.panel_header))
                .text_xs()
                .text_color(rgb(palette.text_muted))
                .child(SharedString::from(message.clone()))
        })
    };
    let actionable_banner = model
        .actionable_notice
        .as_ref()
        .map(|banner| render_actionable_notice(banner, actionable_notice_action, palette));
    let blocking_notice = model.blocking_notice.as_ref().map(|message| {
        div()
            .mx_2()
            .my_1()
            .py_2()
            .border_t_1()
            .border_b_1()
            .border_color(rgb(palette.border))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(palette.text_muted))
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
        .map(|session| session_status_color(session, palette))
        .unwrap_or(palette.text_muted);
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
                .bg(rgb(palette.row))
                .text_xs()
                .text_color(rgb(palette.text_muted))
                .child(SharedString::from(exit.summary.clone()))
        });
    let terminal_body: AnyElement = if let Some(session) = model.session.as_ref() {
        render_grid(
            session,
            model.selection.as_ref(),
            model.search_highlight,
            model.scrollbar,
            scrollbar_actions,
            model.font_size,
            model.cell_width,
            model.line_height,
            palette,
        )
        .into_any_element()
    } else {
        render_empty_body(
            empty_surface_message(model),
            model.splash_image.clone(),
            palette,
        )
        .into_any_element()
    };

    div()
        .flex_1()
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(palette.canvas))
        .child(
            div()
                .h(px(22.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .px(px(6.0))
                .bg(rgb(palette.panel_header))
                .border_b_1()
                .border_color(rgb(palette.border))
                .overflow_hidden()
                .child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(palette.text_primary))
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
                                            .unwrap_or(palette.text_dim)))
                                        .child(SharedString::from(detail.clone()))
                                }),
                        )
                        .children(header_detail.map(|detail| {
                            div()
                                .text_xs()
                                .text_color(rgb(palette.text_dim))
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
                                .text_color(rgb(palette.text_dim))
                                .child(SharedString::from(format!(
                                    "font {}",
                                    model.font_size.round() as u32
                                ))),
                        ),
                )
                .children(open_browser_action.map(|on_click| {
                    runtime_action_button("Browser", palette.primary, on_click, palette)
                        .into_any_element()
                }))
                .children(
                    actions
                        .zip(runtime_controls.clone())
                        .map(|(actions, controls)| {
                            render_runtime_actions(actions, controls, palette).into_any_element()
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
                    .bg(rgb(palette.terminal_bg))
                    .children(notice)
                    .children(actionable_banner)
                    .children(blocking_notice)
                    .children(render_pending_annotation_chips(
                        &model.pending_annotations,
                        preview_annotation_action,
                        remove_annotation_action,
                        palette,
                    ))
                    .children(
                        model
                            .search
                            .as_ref()
                            .map(|search| render_search_bar(search, palette)),
                    )
                    .children(exit_banner)
                    .child(terminal_body)
                    .children(model.debug_enabled.then(|| {
                        div()
                            .px_2()
                            .pb_1()
                            .text_xs()
                            .text_color(rgb(palette.text_subtle))
                            .child(SharedString::from(
                                metrics.unwrap_or_else(|| "No metrics yet".to_string()),
                            ))
                    })),
            ),
        )
}

fn render_pending_annotation_chips(
    models: &[PendingAnnotationChipModel],
    preview: Option<PendingAnnotationActionHandler>,
    remove: Option<PendingAnnotationActionHandler>,
    palette: TerminalRenderPalette,
) -> Option<AnyElement> {
    (!models.is_empty()).then(|| {
        div()
            .mx_2()
            .mt_1()
            .px_2()
            .py_1()
            .bg(rgb(palette.panel_header))
            .border_1()
            .border_color(rgb(palette.border))
            .rounded_sm()
            .flex()
            .items_center()
            .gap(px(6.0))
            .overflow_hidden()
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(rgb(palette.text_subtle))
                    .child("Pending"),
            )
            .children(models.iter().cloned().map(|model| {
                let action = model.action.clone();
                let mut chip = div()
                    .max_w(px(260.0))
                    .px_2()
                    .py(px(3.0))
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(if model.stale {
                        palette.warning
                    } else {
                        palette.border
                    }))
                    .bg(rgb(palette.row))
                    .text_xs()
                    .text_color(rgb(palette.text_primary))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(
                        div()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(SharedString::from(format!(
                                "{}{} · {} · {}",
                                if model.stale { "stale " } else { "" },
                                model.stable_id,
                                model.comment,
                                model.url
                            ))),
                    )
                    .children(remove.clone().map(|remove| {
                        let action = action.clone();
                        div()
                            .ml_1()
                            .px_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_color(rgb(palette.text_muted))
                            .hover(|style| style.bg(rgb(palette.row_hover)))
                            .child("×")
                            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                                cx.stop_propagation();
                                remove(action.clone(), event, window, cx);
                            })
                    }));
                if let Some(preview) = preview.clone() {
                    chip = chip
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(palette.row_hover)));
                    chip = chip.on_mouse_down(MouseButton::Left, move |event, window, cx| {
                        cx.stop_propagation();
                        preview(action.clone(), event, window, cx);
                    });
                }
                chip
            }))
            .into_any_element()
    })
}

fn render_grid(
    session: &TerminalSessionView,
    selection: Option<&TerminalSelectionSnapshot>,
    search_highlight: Option<TerminalSearchHighlight>,
    scrollbar: Option<TerminalScrollbarModel>,
    scrollbar_actions: Option<TerminalScrollbarActions>,
    font_size: f32,
    cell_width: f32,
    line_height: f32,
    palette: TerminalRenderPalette,
) -> impl IntoElement {
    let (background_runs, text_runs, cursor_overlay) =
        collect_grid_paint_runs(session, selection, palette);
    let search_highlight = search_highlight.map(|highlight| {
        let start_column = highlight.start_column.min(session.screen.cols);
        let end_column = highlight
            .end_column
            .min(session.screen.cols)
            .max(start_column + 1);
        TerminalBackgroundRect {
            row: highlight.row.min(session.screen.rows.saturating_sub(1)),
            start_column,
            cell_count: end_column.saturating_sub(start_column),
            color: palette.primary_muted,
        }
    });

    div()
        .flex_1()
        .flex()
        .flex_row()
        .bg(rgb(palette.terminal_bg))
        .overflow_hidden()
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .px_1()
                .py(px(2.0))
                .bg(rgb(palette.terminal_bg))
                .child(render_grid_canvas(
                    background_runs,
                    search_highlight,
                    text_runs,
                    cursor_overlay,
                    font_size,
                    cell_width,
                    line_height,
                )),
        )
        .children(
            scrollbar.map(|scrollbar| render_scrollbar(scrollbar, scrollbar_actions, palette)),
        )
}

fn render_empty_body(
    message: String,
    splash_image: Option<std::sync::Arc<gpui::RenderImage>>,
    palette: TerminalRenderPalette,
) -> impl IntoElement {
    div()
        .flex_1()
        .bg(rgb(palette.terminal_bg))
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
                .text_color(rgb(palette.text_subtle))
                .child(SharedString::from(message))
        }))
}

fn render_search_bar(
    model: &TerminalSearchUiModel,
    palette: TerminalRenderPalette,
) -> impl IntoElement {
    div()
        .mx_2()
        .mt_1()
        .px_2()
        .py(px(6.0))
        .bg(rgb(palette.panel_header))
        .border_1()
        .border_color(rgb(palette.border))
        .rounded_sm()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .min_w(px(0.0))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(palette.text_subtle))
                        .child("Search"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(palette.text_primary))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(SharedString::from(model.query.clone())),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(if model.case_sensitive {
                            palette.primary
                        } else {
                            palette.text_dim
                        }))
                        .child(if model.case_sensitive { "Aa" } else { "aa" }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(palette.text_subtle))
                        .child(SharedString::from(model.summary.clone())),
                ),
        )
}

fn render_scrollbar(
    scrollbar: TerminalScrollbarModel,
    actions: Option<TerminalScrollbarActions>,
    palette: TerminalRenderPalette,
) -> impl IntoElement {
    canvas(
        move |_bounds, _window, _cx| (scrollbar, actions.clone()),
        move |bounds: Bounds<_>, state, window, _cx| {
            let (scrollbar, actions) = state;
            let track = Bounds::new(
                point(
                    bounds.origin.x + px(TERMINAL_SCROLLBAR_TRACK_INSET_X_PX),
                    bounds.origin.y + px(TERMINAL_SCROLLBAR_TRACK_INSET_Y_PX),
                ),
                size(
                    px(TERMINAL_SCROLLBAR_TRACK_WIDTH_PX),
                    (bounds.size.height - px(TERMINAL_SCROLLBAR_TRACK_INSET_Y_PX * 2.0))
                        .max(px(12.0)),
                ),
            );
            window.paint_quad(fill(track, rgb(palette.scrollbar_track)));

            let thumb_height = (track.size.height * scrollbar.thumb_height_ratio.clamp(0.08, 1.0))
                .max(px(TERMINAL_SCROLLBAR_MIN_THUMB_HEIGHT_PX));
            let thumb_range = (track.size.height - thumb_height).max(px(0.0));
            let thumb_top =
                track.origin.y + thumb_range * scrollbar.thumb_top_ratio.clamp(0.0, 1.0);
            let thumb = Bounds::new(
                point(track.origin.x, thumb_top),
                size(track.size.width, thumb_height),
            );
            window.paint_quad(fill(thumb, rgb(palette.scrollbar_thumb)));

            if let Some(actions) = actions.as_ref() {
                let on_mouse_down = actions.on_mouse_down.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, _, window, cx| {
                    if bounds.contains(&event.position) {
                        (on_mouse_down)(event, window, cx);
                    }
                });

                let on_mouse_move = actions.on_mouse_move.clone();
                window.on_mouse_event(move |event: &gpui::MouseMoveEvent, _, window, cx| {
                    (on_mouse_move)(event, window, cx);
                });

                let on_mouse_up = actions.on_mouse_up.clone();
                window.on_mouse_event(move |event: &gpui::MouseUpEvent, _, window, cx| {
                    (on_mouse_up)(event, window, cx);
                });
            }
        },
    )
    .w(px(TERMINAL_SCROLLBAR_WIDTH_PX))
    .flex_none()
    .h_full()
}

fn surface_header_detail(model: &TerminalPaneModel) -> Option<String> {
    let session = model.session.as_ref()?;
    let has_live_terminal = session.runtime.status.is_live() || session.runtime.interactive_shell;

    if session.runtime.status.is_live() && session.runtime.resources.last_sample_at.is_some() {
        let resource_metrics = format_compact_resource_metrics(&session.runtime.resources);
        let process_count = format_compact_process_count(&session.runtime.resources);
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
        return Some(format!("{resource_metrics} • {process_count}{uptime_part}"));
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
    palette: TerminalRenderPalette,
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
        let style = effective_cell_style(&indexed.cell, selected, cursor_cell, palette);

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
            color: palette.terminal_cursor,
        }),
        _ => None,
    });

    (background_runs, text_runs, cursor_overlay)
}

fn render_grid_canvas(
    background_runs: Vec<TerminalBackgroundRect>,
    search_highlight: Option<TerminalBackgroundRect>,
    text_runs: Vec<TerminalTextRun>,
    cursor_overlay: Option<TerminalCursorOverlay>,
    font_size: f32,
    cell_width: f32,
    line_height: f32,
) -> impl IntoElement {
    canvas(
        move |_bounds, _window, _cx| (background_runs, search_highlight, text_runs, cursor_overlay),
        move |bounds: Bounds<_>,
              (background_runs, search_highlight, text_runs, cursor_overlay),
              window,
              cx| {
            for run in background_runs {
                let position = point(
                    bounds.origin.x + px(run.start_column as f32 * cell_width),
                    bounds.origin.y + px(run.row as f32 * line_height),
                );
                let run_size = size(px(cell_width * run.cell_count as f32), px(line_height));
                window.paint_quad(fill(Bounds::new(position, run_size), rgb(run.color)));
            }

            if let Some(run) = search_highlight {
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
    palette: TerminalRenderPalette,
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

    // Apply themed default foreground before selection/cursor overrides so
    // custom/T3 tokens replace the stale NamedColor::Foreground static.
    if cell.default_foreground {
        foreground = palette.terminal_fg;
    }

    if selected {
        foreground = palette.selection_text;
        background = palette.selection_bg;
        paint_background = true;
    }

    if let Some(cursor) = cursor {
        match cursor.shape {
            CursorShape::Block => {
                foreground = palette.panel;
                background = palette.terminal_cursor;
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
    palette: TerminalRenderPalette,
) -> impl IntoElement {
    let TerminalPaneActions {
        on_open_browser: _,
        on_preview_annotation: _,
        on_remove_annotation: _,
        on_start_server,
        on_stop_server,
        on_restart_server,
        on_clear_output: _,
        on_actionable_notice_action: _,
        on_open_local_url,
        on_prompt_action,
        on_toggle_search,
        on_search_prev,
        on_search_next,
        on_toggle_search_case,
        on_close_search,
        on_jump_prev_prompt: _,
        on_jump_next_prompt: _,
        on_export_screen: _,
        on_export_scrollback,
        on_export_selection,
        on_take_remote_control,
        on_release_remote_control,
        on_toggle_mouse_override: _,
        on_toggle_read_only: _,
        scrollbar: _,
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
                .map(|on_click| runtime_action_button("start", palette.success, on_click, palette)),
        )
        .children(
            controls
                .can_stop
                .then_some(on_stop_server)
                .flatten()
                .map(|on_click| runtime_action_button("stop", palette.danger, on_click, palette)),
        )
        .children(
            controls
                .can_restart
                .then_some(on_restart_server)
                .flatten()
                .map(|on_click| {
                    runtime_action_button("restart", palette.warning, on_click, palette)
                }),
        )
        .children(
            controls
                .can_open_url
                .then_some(on_open_local_url)
                .flatten()
                .map(|on_click| runtime_action_button("open", palette.primary, on_click, palette)),
        )
        .children(
            controls
                .prompt_action_label
                .zip(on_prompt_action)
                .map(|(label, on_click)| {
                    runtime_action_button(
                        label.as_str(),
                        controls.prompt_action_color,
                        on_click,
                        palette,
                    )
                }),
        )
        .children(controls.remote_control.map(|control| {
            remote_control_button(
                control,
                on_take_remote_control,
                on_release_remote_control,
                palette,
            )
        }))
        .children(
            controls
                .can_search
                .then_some(on_toggle_search)
                .flatten()
                .map(|on_click| {
                    runtime_action_button(
                        if controls.search_active {
                            "find"
                        } else {
                            "search"
                        },
                        if controls.search_active {
                            palette.primary
                        } else {
                            palette.text_muted
                        },
                        on_click,
                        palette,
                    )
                }),
        )
        .children(
            controls
                .search_active
                .then_some(on_search_prev)
                .flatten()
                .map(|on_click| {
                    runtime_action_button("prev", palette.text_muted, on_click, palette)
                }),
        )
        .children(
            controls
                .search_active
                .then_some(on_search_next)
                .flatten()
                .map(|on_click| {
                    runtime_action_button("next", palette.text_muted, on_click, palette)
                }),
        )
        .children(
            controls
                .search_active
                .then_some(on_toggle_search_case)
                .flatten()
                .map(|on_click| {
                    runtime_action_button(
                        if controls.search_case_sensitive {
                            "Aa"
                        } else {
                            "aa"
                        },
                        if controls.search_case_sensitive {
                            palette.primary
                        } else {
                            palette.text_muted
                        },
                        on_click,
                        palette,
                    )
                }),
        )
        .children(
            controls
                .search_active
                .then_some(on_close_search)
                .flatten()
                .map(|on_click| {
                    runtime_action_button("close", palette.text_muted, on_click, palette)
                }),
        )
        .children(
            controls
                .can_export_scrollback
                .then_some(on_export_scrollback)
                .flatten()
                .map(|on_click| {
                    runtime_action_button("export", palette.text_muted, on_click, palette)
                }),
        )
        .children(
            controls
                .can_export_selection
                .then_some(on_export_selection)
                .flatten()
                .map(|on_click| {
                    runtime_action_button("selection", palette.text_muted, on_click, palette)
                }),
        )
}

fn remote_control_button(
    control: TerminalRemoteControlModel,
    on_take_remote_control: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    on_release_remote_control: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    palette: TerminalRenderPalette,
) -> impl IntoElement {
    let action = if control.can_release {
        on_release_remote_control.map(|on_click| ("release", on_click))
    } else if control.can_take {
        on_take_remote_control.map(|on_click| ("take control", on_click))
    } else {
        None
    };

    div()
        .flex()
        .items_center()
        .gap(px(4.0))
        .px(px(5.0))
        .py(px(1.0))
        .border_1()
        .border_color(rgb(palette.border))
        .bg(rgb(palette.panel_header))
        .rounded_sm()
        .text_xs()
        .text_color(rgb(control.color))
        .child(SharedString::from(control.label))
        .children(action.map(|(label, on_click)| {
            div()
                .px(px(4.0))
                .bg(rgb(palette.button_hover))
                .rounded_sm()
                .text_color(rgb(palette.text_primary))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(palette.row_hover)))
                .child(SharedString::from(label.to_string()))
                .on_mouse_down(MouseButton::Left, on_click)
        }))
}

fn render_actionable_notice(
    banner: &TerminalActionableNotice,
    on_click: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    palette: TerminalRenderPalette,
) -> impl IntoElement {
    div()
        .mx(px(8.0))
        .my(px(4.0))
        .px(px(10.0))
        .py(px(6.0))
        .border_1()
        .border_color(rgb(palette.danger))
        .bg(rgb(palette.danger_bg))
        .rounded_sm()
        .flex()
        .items_center()
        .gap(px(10.0))
        .child(
            div()
                .flex_1()
                .text_sm()
                .text_color(rgb(palette.danger))
                .child(SharedString::from(banner.message.clone())),
        )
        .children(on_click.map(|handler| {
            runtime_action_button(banner.action_label, banner.action_color, handler, palette)
        }))
}

fn runtime_action_button(
    label: &str,
    color: u32,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    palette: TerminalRenderPalette,
) -> impl IntoElement {
    div()
        .px(px(5.0))
        .py(px(1.0))
        .border_1()
        .border_color(rgb(palette.border))
        .bg(rgb(palette.panel_header))
        .rounded_sm()
        .text_xs()
        .text_color(rgb(color))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(palette.button_hover)))
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

fn session_status_color(session: &TerminalSessionView, palette: TerminalRenderPalette) -> u32 {
    if session.runtime.unseen_ready {
        return palette.success;
    }
    if matches!(session.runtime.ai_activity, Some(AiActivity::Thinking)) {
        return palette.warning;
    }

    match session.runtime.status {
        SessionStatus::Running => palette.text_subtle,
        SessionStatus::Starting | SessionStatus::Stopping => palette.warning,
        SessionStatus::Crashed | SessionStatus::Failed => palette.danger,
        _ => palette.text_muted,
    }
}

fn format_compact_resource_metrics(resources: &ResourceSnapshot) -> String {
    let cpu = match resources.cpu_value_state {
        ResourceMetricValueState::Observed => format!("{:.1}% CPU", resources.cpu_percent),
        ResourceMetricValueState::Partial => {
            format!("{:.1}% CPU (partial)", resources.cpu_percent)
        }
        ResourceMetricValueState::LastKnown => {
            format!("{:.1}% CPU (last known)", resources.cpu_percent)
        }
        ResourceMetricValueState::Unavailable => "CPU unavailable".to_string(),
    };
    let memory_mb = resources.memory_bytes / 1024 / 1024;
    let memory = match resources.memory_value_state {
        ResourceMetricValueState::Observed => {
            format!("{} {memory_mb} MB", resources.memory_metric.label())
        }
        ResourceMetricValueState::Partial => format!(
            "{} {memory_mb} MB (partial)",
            resources.memory_metric.label()
        ),
        ResourceMetricValueState::LastKnown => format!(
            "{} {memory_mb} MB (last known)",
            resources.memory_metric.label()
        ),
        ResourceMetricValueState::Unavailable => {
            format!("{} unavailable", resources.memory_metric.label())
        }
    };
    format!("{cpu} • {memory}")
}

fn format_compact_process_count(resources: &ResourceSnapshot) -> String {
    let count = resources.process_count;
    let noun = if count == 1 { "proc" } else { "procs" };
    match resources.process_count_value_state {
        ResourceMetricValueState::Observed => format!("{count} {noun}"),
        ResourceMetricValueState::Partial => format!("{count} {noun} (partial)"),
        ResourceMetricValueState::LastKnown => format!("{count} {noun} (last known)"),
        ResourceMetricValueState::Unavailable => "process count unavailable".to_string(),
    }
}

#[cfg(test)]
mod resource_metric_tests {
    use super::{format_compact_process_count, format_compact_resource_metrics};
    use crate::state::{ResourceMemoryMetric, ResourceMetricValueState, ResourceSnapshot};

    #[test]
    fn compact_terminal_metrics_do_not_render_unavailable_zero_as_idle() {
        let unavailable = ResourceSnapshot {
            cpu_percent: 0.0,
            memory_bytes: 0,
            memory_metric: ResourceMemoryMetric::PrivateCommitted,
            cpu_value_state: ResourceMetricValueState::Unavailable,
            memory_value_state: ResourceMetricValueState::Unavailable,
            ..ResourceSnapshot::default()
        };
        let label = format_compact_resource_metrics(&unavailable);
        assert!(label.contains("CPU unavailable"));
        assert!(label.contains("private committed unavailable"));
        assert!(!label.contains("0.0%"));

        let partial = ResourceSnapshot {
            cpu_percent: 6.25,
            memory_bytes: 4 * 1024 * 1024,
            memory_metric: ResourceMemoryMetric::PrivateCommitted,
            cpu_value_state: ResourceMetricValueState::Partial,
            memory_value_state: ResourceMetricValueState::Observed,
            ..ResourceSnapshot::default()
        };
        let label = format_compact_resource_metrics(&partial);
        assert!(label.contains("6.2% CPU (partial)"));
        assert!(label.contains("private committed 4 MB"));

        let retained_count = ResourceSnapshot {
            process_count: 3,
            process_count_value_state: ResourceMetricValueState::LastKnown,
            ..ResourceSnapshot::default()
        };
        assert_eq!(
            format_compact_process_count(&retained_count),
            "3 procs (last known)"
        );
        assert_eq!(
            format_compact_process_count(&ResourceSnapshot::default()),
            "process count unavailable"
        );
    }
}

#[cfg(test)]
mod theme_palette_tests {
    use super::{effective_cell_style, terminal_render_palette_from_tokens, TerminalRenderPalette};
    use crate::terminal::session::TerminalCellSnapshot;
    use crate::theme;
    use crate::ui::tokens::{dark, Color, Density, Scale, ThemeMode, PREVIEW_SENTINEL};

    fn sentinel_tokens() -> crate::ui::tokens::ThemeTokens {
        let mut tokens = dark(Density::Comfortable, Scale::Scale100);
        tokens.mode = ThemeMode::Dark;
        tokens.surfaces.canvas = Color::from_u32(0x010101);
        tokens.surfaces.raised = Color::from_u32(0x020202);
        tokens.surfaces.overlay = Color::from_u32(0x030303);
        tokens.surfaces.hover = Color::from_u32(0x040404);
        tokens.surfaces.disabled = Color::from_u32(0x050505);
        tokens.surfaces.sunken = Color::from_u32(0x161616);
        tokens.borders.default = Color::from_u32(0x060606);
        tokens.text.primary = Color::from_u32(0x070707);
        tokens.text.muted = Color::from_u32(0x080808);
        tokens.text.disabled = Color::from_u32(0x090909);
        tokens.text.on_selection = Color::from_u32(0x0a0a0a);
        tokens.actions.primary.default.background = Color::from_u32(0x0b0b0b);
        tokens.actions.primary.selected.background = Color::from_u32(0x0c0c0c);
        tokens.status.destructive = Color::from_u32(0x0d0d0d);
        tokens.status.destructive_surface = Color::from_u32(0x0e0e0e);
        tokens.status.warning = Color::from_u32(0x0f0f0f);
        tokens.status.success = Color::from_u32(0x101010);
        tokens.terminal.background = PREVIEW_SENTINEL;
        tokens.terminal.foreground = Color::from_u32(0x121212);
        tokens.terminal.cursor = Color::from_u32(0x131313);
        tokens.terminal.selection = Color::from_u32(0x141414);
        tokens
    }

    #[test]
    fn sentinel_theme_tokens_map_into_terminal_render_palette() {
        let tokens = sentinel_tokens();
        let palette = terminal_render_palette_from_tokens(tokens);
        assert_eq!(palette.canvas, 0x010101);
        assert_eq!(palette.panel, 0x161616);
        assert_eq!(palette.panel_header, 0x020202);
        assert_eq!(palette.row, 0x030303);
        assert_eq!(palette.row_hover, 0x040404);
        assert_eq!(palette.button_hover, 0x040404);
        assert_eq!(palette.border, 0x060606);
        assert_eq!(palette.text_primary, 0x070707);
        assert_eq!(palette.text_muted, 0x080808);
        assert_eq!(palette.text_subtle, 0x090909);
        assert_eq!(palette.text_dim, 0x090909);
        assert_eq!(palette.selection_text, 0x0a0a0a);
        assert_eq!(palette.primary, 0x0b0b0b);
        assert_eq!(palette.primary_muted, 0x0c0c0c);
        assert_eq!(palette.danger, 0x0d0d0d);
        assert_eq!(palette.danger_bg, 0x0e0e0e);
        assert_eq!(palette.warning, 0x0f0f0f);
        assert_eq!(palette.success, 0x101010);
        assert_eq!(palette.terminal_bg, PREVIEW_SENTINEL.to_u32());
        assert_eq!(palette.terminal_fg, 0x121212);
        assert_eq!(palette.terminal_cursor, 0x131313);
        assert_eq!(palette.terminal_selection, 0x141414);
        assert_eq!(palette.selection_bg, 0x141414);
        assert_eq!(palette.scrollbar_track, 0x020202);
        assert_eq!(palette.scrollbar_thumb, 0x080808);
    }

    #[test]
    fn legacy_default_palette_matches_pre_token_theme_constants() {
        let palette = TerminalRenderPalette::legacy_default();
        assert_eq!(palette.canvas, theme::APP_BG);
        assert_eq!(palette.panel, theme::PANEL_BG);
        assert_eq!(palette.panel_header, theme::PANEL_HEADER_BG);
        assert_eq!(palette.button_hover, theme::BUTTON_HOVER_BG);
        assert_eq!(palette.text_dim, theme::TEXT_DIM);
        assert_eq!(palette.terminal_bg, theme::TERMINAL_BG);
        assert_eq!(palette.terminal_fg, theme::TEXT_PRIMARY);
        assert_eq!(palette.terminal_cursor, theme::SUCCESS_TEXT);
        assert_eq!(palette.selection_bg, theme::SELECTION_BG);
        assert_eq!(palette.scrollbar_track, theme::PANEL_HEADER_BG);
        assert_eq!(palette.scrollbar_thumb, theme::TEXT_DIM);
    }

    #[test]
    fn effective_style_replaces_default_foreground_with_palette_terminal_fg() {
        let palette = terminal_render_palette_from_tokens(sentinel_tokens());
        let cell = TerminalCellSnapshot {
            character: 'a',
            zero_width: Vec::new(),
            foreground: 0xe4e4e7,
            background: 0x09090b,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            undercurl: false,
            strike: false,
            hidden: false,
            has_hyperlink: false,
            default_background: true,
            default_foreground: true,
        };
        let style = effective_cell_style(&cell, false, None, palette);
        assert_eq!(style.foreground, palette.terminal_fg);
        assert_eq!(style.foreground, 0x121212);
        assert!(!style.paint_background);
    }

    #[test]
    fn effective_style_keeps_explicit_ansi_foreground() {
        let palette = terminal_render_palette_from_tokens(sentinel_tokens());
        let cell = TerminalCellSnapshot {
            character: 'x',
            zero_width: Vec::new(),
            foreground: 0xef4444,
            background: 0x09090b,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            undercurl: false,
            strike: false,
            hidden: false,
            has_hyperlink: false,
            default_background: true,
            default_foreground: false,
        };
        let style = effective_cell_style(&cell, false, None, palette);
        assert_eq!(style.foreground, 0xef4444);
        assert_ne!(style.foreground, palette.terminal_fg);
    }

    #[test]
    fn effective_style_selection_overrides_themed_default_foreground() {
        let palette = terminal_render_palette_from_tokens(sentinel_tokens());
        let cell = TerminalCellSnapshot {
            character: 's',
            zero_width: Vec::new(),
            foreground: 0xe4e4e7,
            background: 0x09090b,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            undercurl: false,
            strike: false,
            hidden: false,
            has_hyperlink: false,
            default_background: true,
            default_foreground: true,
        };
        let style = effective_cell_style(&cell, true, None, palette);
        assert_eq!(style.foreground, palette.selection_text);
        assert_eq!(style.background, palette.selection_bg);
        assert!(style.paint_background);
    }
}
