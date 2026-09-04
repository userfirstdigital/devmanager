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
use crate::ui::scrollbar::{thumb_geometry, track_geometry};
use crate::ui::tokens::{ScrollbarTokens, ThemeTokens};
use alacritty_terminal::vte::ansi::CursorShape;
use gpui::{
    canvas, div, fill, img, point, px, rgb, size, AnyElement, App, Bounds, Hsla, ImageSource,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ObjectFit, ParentElement, Pixels, Point, SharedString, StrikethroughStyle, Styled, StyledImage,
    TextRun, UnderlineStyle, Window,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub const TERMINAL_FONT_SIZE: f32 = 13.0;
pub const TERMINAL_LINE_HEIGHT: f32 = 18.0;
/// The terminal's scrollbar carries no geometry of its own any more: every
/// width, inset, length and radius comes from `ThemeTokens::scrollbar`, the
/// same struct `crate::ui::scrollbar` paints every shell surface from, and the
/// two call the same geometry functions. See
/// `terminal_scrollbar_geometry_equals_the_shared_spec` for the assertion that
/// keeps them identical rather than merely similar.
///
/// Kept as a function rather than a constant so a caller cannot read a width
/// without saying which theme it belongs to.
pub fn terminal_scrollbar_gutter_width(spec: ScrollbarTokens) -> f32 {
    spec.gutter_width
}

/// The scrollbar geometry, for callers that have no `ThemeTokens` in hand.
///
/// Geometry is mode-independent by construction -- a pointer target does not
/// change size with the palette -- so reading it from the dark theme is not a
/// theme choice, it is the only geometry there is. Colours are NOT available
/// this way on purpose: those DO depend on the ground.
pub fn terminal_scrollbar_spec() -> ScrollbarTokens {
    crate::ui::tokens::dark(
        crate::ui::tokens::Density::Comfortable,
        crate::ui::tokens::Scale::Scale100,
    )
    .scrollbar
}

pub fn terminal_line_height(font_size: f32) -> f32 {
    (font_size + 5.0).max(TERMINAL_LINE_HEIGHT)
}

/// Cell pitch used only where there is no window or text system to ask:
/// headless hosts, unit tests, and the rounded `u16` grid dimensions the host
/// carries in [`crate::state::SessionDimensions`].
///
/// This is the FALLBACK, not the truth. The truth is the terminal font's own
/// horizontal advance, measured through GPUI's text system by
/// [`measure_terminal_cell_advance`]. Cascadia Mono at 13 px advances
/// 7.617 px, so painting on this constant places every glyph in a run
/// 0.383 px per column further right than the shaped run actually is.
pub const FALLBACK_TERMINAL_CELL_WIDTH: f32 = 8.0;

/// One source of truth for the terminal grid's horizontal pitch.
///
/// `measured_advance` is what GPUI's text system reports for the exact `Font`
/// the runs are shaped with (so a fallback substitution is measured rather
/// than assumed); `None` means nothing could be measured. The pitch MUST equal
/// that advance: background quads, the cursor quad and the shaped glyphs are
/// all positioned from it, and any other value makes the three disagree about
/// where a column starts, accumulating across a run and resetting at every run
/// boundary.
pub fn terminal_cell_pitch(measured_advance: Option<f32>) -> f32 {
    measured_advance
        .filter(|advance| advance.is_finite() && *advance > 0.0)
        .unwrap_or(FALLBACK_TERMINAL_CELL_WIDTH)
}

/// Window-space horizontal offset of a grid column, at the given pitch.
///
/// Every painted x in the grid goes through here so the quads and the glyphs
/// cannot drift apart.
pub fn terminal_column_offset(column: usize, cell_pitch: f32) -> f32 {
    column as f32 * cell_pitch
}

/// Measured advances, keyed by font size bits. Shaping metrics are constant
/// for a (font, size) pair, so the text system is asked once and never per
/// frame.
fn measured_cell_advances() -> &'static Mutex<HashMap<u32, f32>> {
    static CACHE: OnceLock<Mutex<HashMap<u32, f32>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record one measurement so window-free callers (the replica pane model, the
/// PTY cell-size report) can read the same number the painter uses.
pub(crate) fn record_measured_terminal_cell_advance(font_size: f32, advance: f32) {
    if !advance.is_finite() || advance <= 0.0 {
        return;
    }
    if let Ok(mut cache) = measured_cell_advances().lock() {
        cache.insert(font_size.to_bits(), advance);
    }
}

/// The advance last measured for `font_size`, or `None` when no window has
/// measured yet (headless hosts and tests).
pub fn measured_terminal_cell_advance(font_size: f32) -> Option<f32> {
    measured_cell_advances()
        .lock()
        .ok()
        .and_then(|cache| cache.get(&font_size.to_bits()).copied())
}

/// The pitch every window-free caller should use: the measured advance when
/// one exists, otherwise [`FALLBACK_TERMINAL_CELL_WIDTH`].
pub fn last_measured_terminal_cell_pitch(font_size: f32) -> f32 {
    terminal_cell_pitch(measured_terminal_cell_advance(font_size))
}

/// Measure the terminal font's advance through GPUI's text system, resolving
/// the same [`crate::terminal::terminal_font`] value the runs are shaped with
/// so a font substitution is measured rather than assumed. Cached per font
/// size; safe to call every frame. `None` only when the text system cannot
/// answer for this font.
pub fn measure_terminal_cell_advance(window: &Window, font_size: f32) -> Option<f32> {
    if let Some(cached) = measured_terminal_cell_advance(font_size) {
        return Some(cached);
    }
    let text_system = window.text_system();
    let font_id = text_system.resolve_font(&crate::terminal::terminal_font());
    let measured = text_system
        .ch_advance(font_id, px(font_size))
        .or_else(|_| text_system.ch_width(font_id, px(font_size)))
        .ok()
        .map(f32::from)
        .filter(|advance| advance.is_finite() && *advance > 0.0);
    if let Some(advance) = measured {
        record_measured_terminal_cell_advance(font_size, advance);
    }
    measured
}

/// Explicit Copy palette for terminal chrome and default cell named colors.
/// Built from [`ThemeTokens`] for live themed paint, or from legacy theme
/// constants for the default wrapper used by older callers.
// `Eq` is gone because the shared scrollbar spec carries f32 geometry.
#[derive(Debug, Clone, Copy, PartialEq)]
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
    pub scrollbar_thumb_hover: u32,
    /// Geometry for the gutter. Colours stay as `u32` beside the rest of the
    /// terminal chrome; the widths and lengths come straight from the shared
    /// token spec so there is nothing left here to drift.
    pub scrollbar_spec: ScrollbarTokens,
}

impl TerminalRenderPalette {
    /// Map every chrome / named default color from the caller's active tokens.
    pub fn from_tokens(tokens: ThemeTokens) -> Self {
        // v0.4.1 hierarchy: darkest terminal cell plane, readable chrome labels,
        // success-colored cursor visibility. ANSI cell colors stay process-owned
        // in `effective_cell_style`; only default/named chrome is remapped here.
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
            text_subtle: tokens.text.muted.to_u32(),
            text_dim: tokens.text.muted.to_u32(),
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
            terminal_cursor: tokens.status.success.to_u32(),
            terminal_selection: tokens.terminal.selection.to_u32(),
            // Resolved against the terminal plane, not the shell: in the
            // light theme those are opposite polarities and one colour cannot
            // serve both.
            scrollbar_track: tokens
                .scrollbar
                .colors_on(tokens.terminal.background)
                .track_active
                .to_u32(),
            scrollbar_thumb: tokens
                .scrollbar
                .colors_on(tokens.terminal.background)
                .thumb_idle
                .to_u32(),
            scrollbar_thumb_hover: tokens
                .scrollbar
                .colors_on(tokens.terminal.background)
                .thumb_hover
                .to_u32(),
            scrollbar_spec: tokens.scrollbar,
        }
    }

    /// The thumb colour for one pointer state, so the two call sites cannot
    /// pick different halves of the pair.
    pub fn scrollbar_thumb_color(&self, active: bool) -> u32 {
        if active {
            self.scrollbar_thumb_hover
        } else {
            self.scrollbar_thumb
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
            // The legacy palette keeps its pre-token colours, but not its
            // pre-token geometry: "one look" is the point, and the widths are
            // mode-independent.
            scrollbar_track: theme::PANEL_HEADER_BG,
            scrollbar_thumb: theme::TEXT_DIM,
            scrollbar_thumb_hover: theme::TEXT_PRIMARY,
            scrollbar_spec: crate::ui::tokens::dark(
                crate::ui::tokens::Density::Comfortable,
                crate::ui::tokens::Scale::Scale100,
            )
            .scrollbar,
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

/// Absolute buffer cell coordinates used by selection anchors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TerminalGridPosition {
    pub row: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TerminalCellSide {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TerminalSelectionEndpoint {
    pub position: TerminalGridPosition,
    pub side: TerminalCellSide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSelectionMode {
    Simple,
    Semantic,
    Lines,
}

/// Active drag/click selection for one terminal pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSelection {
    pub anchor: TerminalSelectionEndpoint,
    pub head: TerminalSelectionEndpoint,
    pub moved: bool,
    pub mode: TerminalSelectionMode,
}

/// Painted grid metrics used for hit-testing. Origin is window-space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TerminalTextBounds {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    pub cell_width: f32,
    pub row_height: f32,
    pub rows: usize,
    pub cols: usize,
}

/// Translate the actual painted terminal canvas into the bounded PTY grid.
/// This is deliberately based on the canvas bounds rather than the last host
/// projection, so the terminal follows free-form pane resizing in both axes.
pub fn terminal_grid_size_for_bounds(
    width: f32,
    height: f32,
    cell_width: f32,
    line_height: f32,
) -> (u16, u16) {
    let cols = (width.max(cell_width) / cell_width.max(1.0)).floor() as u16;
    let rows = (height.max(line_height) / line_height.max(1.0)).floor() as u16;
    (
        cols.clamp(1, crate::terminal::protocol::MAX_TERMINAL_COLS),
        rows.clamp(1, crate::terminal::protocol::MAX_TERMINAL_ROWS),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSelectionRange {
    pub start_row: usize,
    pub start_column: usize,
    pub end_row: usize,
    pub end_column: usize,
}

/// Grid pointer payload delivered from the painted cell plane.
#[derive(Debug, Clone, Copy)]
pub struct TerminalGridPointerEvent {
    pub endpoint: TerminalSelectionEndpoint,
    pub click_count: usize,
    pub shift: bool,
    pub dragging: bool,
}

/// Optional selection interaction registered against the actual painted grid bounds.
#[derive(Clone)]
pub struct TerminalGridInteraction {
    pub on_layout: Arc<dyn Fn((u16, u16), &mut Window, &mut App) + Send + Sync>,
    /// Paint-local platform input registration. Zed registers terminal input
    /// against the exact painted grid bounds, keeping mouse focus, IME, and the
    /// PTY cell plane under one focus owner.
    pub on_paint: Arc<dyn Fn(Bounds<Pixels>, &mut Window, &mut App) + Send + Sync>,
    pub on_mouse_down:
        Arc<dyn Fn(&MouseDownEvent, TerminalGridPointerEvent, &mut Window, &mut App) + Send + Sync>,
    pub on_mouse_move:
        Arc<dyn Fn(&MouseMoveEvent, TerminalGridPointerEvent, &mut Window, &mut App) + Send + Sync>,
    pub on_mouse_up:
        Arc<dyn Fn(&MouseUpEvent, TerminalGridPointerEvent, &mut Window, &mut App) + Send + Sync>,
}

impl std::fmt::Debug for TerminalGridInteraction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TerminalGridInteraction")
    }
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
    /// True while the pointer is anywhere in the gutter, or while the thumb is
    /// being dragged. Widening on gutter hover rather than thumb hover is what
    /// makes a 4 px bar grabbable, and it is why this is a model field: a
    /// `canvas` cannot answer a `group_hover`.
    pub hovered: bool,
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
    // `runtime.dimensions.cell_width` is the host's rounded `u16`, which has
    // always been the hardcoded fallback and cannot express a fractional
    // advance at all. Take the pitch from the one measured source instead, so
    // the model agrees with what the painter shapes.
    let cell_width = last_measured_terminal_cell_pitch(TERMINAL_FONT_SIZE);
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
    render_terminal_surface_with_palette(
        model,
        actions,
        None,
        TerminalRenderPalette::legacy_default(),
    )
}

/// Themed terminal surface: every non-ANSI chrome color comes from `tokens`.
pub fn render_terminal_surface_with_tokens(
    model: &TerminalPaneModel,
    actions: Option<TerminalPaneActions>,
    tokens: ThemeTokens,
) -> impl IntoElement {
    render_terminal_surface_with_palette(
        model,
        actions,
        None,
        TerminalRenderPalette::from_tokens(tokens),
    )
}

/// Themed terminal surface with grid-local selection hit-testing.
pub fn render_terminal_surface_with_tokens_and_grid(
    model: &TerminalPaneModel,
    actions: Option<TerminalPaneActions>,
    grid_selection: Option<TerminalGridInteraction>,
    tokens: ThemeTokens,
) -> impl IntoElement {
    render_terminal_surface_with_palette(
        model,
        actions,
        grid_selection,
        TerminalRenderPalette::from_tokens(tokens),
    )
}

fn render_terminal_surface_with_palette(
    model: &TerminalPaneModel,
    actions: Option<TerminalPaneActions>,
    grid_selection: Option<TerminalGridInteraction>,
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
            grid_selection,
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
    grid_selection: Option<TerminalGridInteraction>,
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
    let grid_rows = session.screen.rows.max(1);
    let grid_cols = session.screen.cols.max(1);

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
                    grid_selection,
                    grid_rows,
                    grid_cols,
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

/// The terminal gutter, painted from the same spec and the same geometry
/// functions as every other scrollbar in the app.
///
/// It stays hand-painted -- the gutter's pixel height is only known at paint
/// time and the `min_thumb_length` clamp is a pixel rule, so expressing it as
/// layout percentages would silently break on a deep scrollback. The hover
/// state therefore cannot come from `group_hover` and arrives on the model
/// instead, set by the same mouse-move listener that already drives the drag.
fn render_scrollbar(
    scrollbar: TerminalScrollbarModel,
    actions: Option<TerminalScrollbarActions>,
    palette: TerminalRenderPalette,
) -> impl IntoElement {
    let spec = palette.scrollbar_spec;
    canvas(
        move |_bounds, _window, _cx| (scrollbar, actions.clone()),
        move |bounds: Bounds<_>, state, window, _cx| {
            let (scrollbar, actions) = state;
            let gutter_height: f32 = bounds.size.height.into();
            let active = scrollbar.hovered;
            let visible_fraction = scrollbar.thumb_height_ratio.clamp(0.08, 1.0);

            if active {
                let track = track_geometry(spec, gutter_height);
                window.paint_quad(
                    fill(
                        Bounds::new(
                            point(
                                bounds.origin.x + px(track.left),
                                bounds.origin.y + px(track.top),
                            ),
                            size(px(track.width), px(track.height)),
                        ),
                        rgb(palette.scrollbar_track),
                    )
                    .corner_radii(px(track.radius)),
                );
            }

            if let Some(thumb) = thumb_geometry(
                spec,
                gutter_height,
                visible_fraction,
                scrollbar.thumb_top_ratio,
                active,
            ) {
                window.paint_quad(
                    fill(
                        Bounds::new(
                            point(
                                bounds.origin.x + px(thumb.left),
                                bounds.origin.y + px(thumb.top),
                            ),
                            size(px(thumb.width), px(thumb.height)),
                        ),
                        rgb(palette.scrollbar_thumb_color(active)),
                    )
                    .corner_radii(px(thumb.radius)),
                );
            }

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
    .w(px(terminal_scrollbar_gutter_width(spec)))
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
    grid_selection: Option<TerminalGridInteraction>,
    grid_rows: usize,
    grid_cols: usize,
    font_size: f32,
    cell_width: f32,
    line_height: f32,
) -> impl IntoElement {
    canvas(
        move |bounds, window, cx| {
            if let Some(interaction) = grid_selection.as_ref() {
                // Column arithmetic must use the same pitch the paint pass
                // below positions glyphs on, or the grid the PTY is told about
                // and the grid that is drawn are different grids.
                let cell_pitch = terminal_cell_pitch(
                    measure_terminal_cell_advance(window, font_size).or(Some(cell_width)),
                );
                let size = terminal_grid_size_for_bounds(
                    f32::from(bounds.size.width),
                    f32::from(bounds.size.height),
                    cell_pitch,
                    line_height,
                );
                (interaction.on_layout)(size, window, cx);
            }
            (
                background_runs,
                search_highlight,
                text_runs,
                cursor_overlay,
                grid_selection,
            )
        },
        move |bounds: Bounds<_>,
              (background_runs, search_highlight, text_runs, cursor_overlay, grid_selection),
              window,
              cx| {
            // The pitch is the advance of the font this very window shapes the
            // runs with — measured once per font size, never per frame. Any
            // other value (notably the historical hardcoded 8) drifts the
            // glyphs away from their own background quads by
            // (pitch - advance) per column.
            let cell_pitch = terminal_cell_pitch(
                measure_terminal_cell_advance(window, font_size).or(Some(cell_width)),
            );

            for run in &background_runs {
                let position = point(
                    bounds.origin.x + px(terminal_column_offset(run.start_column, cell_pitch)),
                    bounds.origin.y + px(run.row as f32 * line_height),
                );
                let run_size = size(
                    px(terminal_column_offset(run.cell_count, cell_pitch)),
                    px(line_height),
                );
                window.paint_quad(fill(Bounds::new(position, run_size), rgb(run.color)));
            }

            if let Some(run) = search_highlight {
                let position = point(
                    bounds.origin.x + px(terminal_column_offset(run.start_column, cell_pitch)),
                    bounds.origin.y + px(run.row as f32 * line_height),
                );
                let run_size = size(
                    px(terminal_column_offset(run.cell_count, cell_pitch)),
                    px(line_height),
                );
                window.paint_quad(fill(Bounds::new(position, run_size), rgb(run.color)));
            }

            for run in &text_runs {
                let shaped_line = window.text_system().shape_line(
                    SharedString::from(run.text.clone()),
                    px(font_size),
                    &[run.style.clone()],
                    None,
                );
                let position = point(
                    bounds.origin.x + px(terminal_column_offset(run.start_column, cell_pitch)),
                    bounds.origin.y + px(run.row as f32 * line_height),
                );
                let _ = shaped_line.paint(position, px(line_height), window, cx);
            }

            if let Some(cursor) = cursor_overlay {
                let position = point(
                    bounds.origin.x + px(terminal_column_offset(cursor.column, cell_pitch)),
                    bounds.origin.y + px(cursor.row as f32 * line_height),
                );
                let cursor_bounds = match cursor.shape {
                    CursorShape::Underline => Bounds::new(
                        point(position.x, position.y + px((line_height - 2.0).max(0.0))),
                        size(px(cell_pitch.max(1.0)), px(2.0)),
                    ),
                    CursorShape::Beam => {
                        Bounds::new(position, size(px(2.0), px(line_height.max(1.0))))
                    }
                    _ => Bounds::new(position, size(px(cell_pitch), px(line_height))),
                };
                window.paint_quad(fill(cursor_bounds, rgb(cursor.color)));
            }

            if let Some(interaction) = grid_selection.as_ref() {
                (interaction.on_paint)(bounds, window, cx);
                let text_bounds = TerminalTextBounds {
                    left: f32::from(bounds.origin.x),
                    top: f32::from(bounds.origin.y),
                    width: terminal_column_offset(grid_cols, cell_pitch)
                        .min(f32::from(bounds.size.width))
                        .max(cell_pitch),
                    height: (grid_rows as f32 * line_height)
                        .min(f32::from(bounds.size.height))
                        .max(line_height),
                    cell_width: cell_pitch,
                    row_height: line_height,
                    rows: grid_rows,
                    cols: grid_cols,
                };
                let on_mouse_down = interaction.on_mouse_down.clone();
                window.on_mouse_event({
                    let text_bounds = text_bounds;
                    move |event: &MouseDownEvent, _, window, cx| {
                        if event.button != MouseButton::Left {
                            return;
                        }
                        let Some(endpoint) =
                            terminal_endpoint_for_mouse(event.position, text_bounds, false)
                        else {
                            return;
                        };
                        cx.stop_propagation();
                        window.prevent_default();
                        (on_mouse_down)(
                            event,
                            TerminalGridPointerEvent {
                                endpoint,
                                click_count: event.click_count,
                                shift: event.modifiers.shift,
                                dragging: false,
                            },
                            window,
                            cx,
                        );
                    }
                });
                let on_mouse_move = interaction.on_mouse_move.clone();
                window.on_mouse_event({
                    let text_bounds = text_bounds;
                    move |event: &MouseMoveEvent, _, window, cx| {
                        if !event.dragging() {
                            return;
                        }
                        let Some(endpoint) =
                            terminal_endpoint_for_mouse(event.position, text_bounds, false)
                        else {
                            return;
                        };
                        cx.stop_propagation();
                        window.prevent_default();
                        (on_mouse_move)(
                            event,
                            TerminalGridPointerEvent {
                                endpoint,
                                click_count: 1,
                                shift: event.modifiers.shift,
                                dragging: true,
                            },
                            window,
                            cx,
                        );
                    }
                });
                let on_mouse_up = interaction.on_mouse_up.clone();
                window.on_mouse_event({
                    let text_bounds = text_bounds;
                    move |event: &MouseUpEvent, _, window, cx| {
                        let Some(endpoint) =
                            terminal_endpoint_for_mouse(event.position, text_bounds, false)
                        else {
                            (on_mouse_up)(
                                event,
                                TerminalGridPointerEvent {
                                    endpoint: TerminalSelectionEndpoint {
                                        position: TerminalGridPosition { row: 0, column: 0 },
                                        side: TerminalCellSide::Left,
                                    },
                                    click_count: 1,
                                    shift: event.modifiers.shift,
                                    dragging: false,
                                },
                                window,
                                cx,
                            );
                            return;
                        };
                        cx.stop_propagation();
                        window.prevent_default();
                        (on_mouse_up)(
                            event,
                            TerminalGridPointerEvent {
                                endpoint,
                                click_count: 1,
                                shift: event.modifiers.shift,
                                dragging: false,
                            },
                            window,
                            cx,
                        );
                    }
                });
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

pub fn selection_mode_for_click(click_count: usize) -> Option<TerminalSelectionMode> {
    match click_count {
        0 => None,
        1 => Some(TerminalSelectionMode::Simple),
        2 => Some(TerminalSelectionMode::Semantic),
        _ => Some(TerminalSelectionMode::Lines),
    }
}

pub fn ordered_selection_endpoints(
    anchor: TerminalSelectionEndpoint,
    head: TerminalSelectionEndpoint,
) -> (TerminalSelectionEndpoint, TerminalSelectionEndpoint) {
    if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    }
}

pub fn boundary_column(endpoint: TerminalSelectionEndpoint, screen_cols: usize) -> usize {
    match endpoint.side {
        TerminalCellSide::Left => endpoint.position.column.min(screen_cols),
        TerminalCellSide::Right => (endpoint.position.column + 1).min(screen_cols),
    }
}

fn endpoint_at_boundary(
    row: usize,
    boundary: usize,
    screen_cols: usize,
) -> TerminalSelectionEndpoint {
    if screen_cols == 0 || boundary == 0 {
        return TerminalSelectionEndpoint {
            position: TerminalGridPosition { row, column: 0 },
            side: TerminalCellSide::Left,
        };
    }

    TerminalSelectionEndpoint {
        position: TerminalGridPosition {
            row,
            column: boundary
                .saturating_sub(1)
                .min(screen_cols.saturating_sub(1)),
        },
        side: TerminalCellSide::Right,
    }
}

pub fn top_visible_buffer_line(screen: &crate::terminal::session::TerminalScreenSnapshot) -> usize {
    screen
        .total_lines
        .saturating_sub(screen.rows.max(1))
        .saturating_sub(screen.display_offset)
}

pub fn buffer_line_for_viewport_row(
    screen: &crate::terminal::session::TerminalScreenSnapshot,
    display_offset: usize,
    viewport_row: usize,
) -> usize {
    let top = screen
        .total_lines
        .saturating_sub(screen.rows.max(1))
        .saturating_sub(display_offset);
    top.saturating_add(viewport_row.min(screen.rows.saturating_sub(1)))
        .min(screen.total_lines.saturating_sub(1))
}

pub fn semantic_selection_bounds(
    line: &[TerminalCellSnapshot],
    column: usize,
    screen_cols: usize,
) -> (usize, usize) {
    let len = line.len().min(screen_cols);
    if len == 0 {
        return (0, 0);
    }

    let column = column.min(len.saturating_sub(1));
    let whitespace = line[column].character.is_whitespace();
    let mut start = column;
    while start > 0 && line[start - 1].character.is_whitespace() == whitespace {
        start -= 1;
    }

    let mut end = column + 1;
    while end < len && line[end].character.is_whitespace() == whitespace {
        end += 1;
    }

    (start, end)
}

pub fn terminal_selection_for_click(
    screen: &crate::terminal::session::TerminalScreenSnapshot,
    position: TerminalGridPosition,
    mode: TerminalSelectionMode,
) -> Option<TerminalSelection> {
    let visible_top = top_visible_buffer_line(screen);
    let viewport_row = position
        .row
        .saturating_sub(visible_top)
        .min(screen.lines.len().saturating_sub(1));
    match mode {
        TerminalSelectionMode::Simple => Some(TerminalSelection {
            anchor: TerminalSelectionEndpoint {
                position,
                side: TerminalCellSide::Left,
            },
            head: TerminalSelectionEndpoint {
                position,
                side: TerminalCellSide::Left,
            },
            moved: false,
            mode,
        }),
        TerminalSelectionMode::Semantic => {
            let line = screen.lines.get(viewport_row)?;
            let (start, end) = semantic_selection_bounds(line, position.column, screen.cols);
            Some(TerminalSelection {
                anchor: endpoint_at_boundary(position.row, start, screen.cols),
                head: endpoint_at_boundary(position.row, end, screen.cols),
                moved: start != end,
                mode,
            })
        }
        TerminalSelectionMode::Lines => Some(TerminalSelection {
            anchor: endpoint_at_boundary(position.row, 0, screen.cols),
            head: endpoint_at_boundary(position.row, screen.cols, screen.cols),
            moved: screen.cols > 0,
            mode,
        }),
    }
}

/// Hit-test a window-space point against the actual painted grid bounds.
pub fn terminal_endpoint_for_mouse(
    position: Point<Pixels>,
    bounds: TerminalTextBounds,
    clamp_to_terminal: bool,
) -> Option<TerminalSelectionEndpoint> {
    if bounds.cols == 0 || bounds.rows == 0 {
        return None;
    }

    let left = bounds.left;
    let top = bounds.top;
    let right = bounds.left + bounds.width;
    let bottom = bounds.top + bounds.height;
    let mut x: f32 = position.x.into();
    let mut y: f32 = position.y.into();

    if !clamp_to_terminal && (x < left || y < top || x >= right || y >= bottom) {
        return None;
    }

    if clamp_to_terminal {
        x = x.clamp(left, right);
        y = y.clamp(top, bottom);
    }

    let relative_x = (x - left).max(0.0);
    let relative_y = (y - top).max(0.0);
    let mut column = (relative_x / bounds.cell_width).floor() as usize;
    let mut row = (relative_y / bounds.row_height).floor() as usize;
    let mut side = if relative_x % bounds.cell_width > bounds.cell_width / 2.0 {
        TerminalCellSide::Right
    } else {
        TerminalCellSide::Left
    };

    if relative_x >= bounds.width {
        column = bounds.cols.saturating_sub(1);
        side = TerminalCellSide::Right;
    } else {
        column = column.min(bounds.cols.saturating_sub(1));
    }

    if y < top {
        row = 0;
        side = TerminalCellSide::Left;
    } else if relative_y >= bounds.height {
        row = bounds.rows.saturating_sub(1);
        side = TerminalCellSide::Right;
    } else {
        row = row.min(bounds.rows.saturating_sub(1));
    }

    Some(TerminalSelectionEndpoint {
        position: TerminalGridPosition { row, column },
        side,
    })
}

pub fn selection_range_from(
    selection: TerminalSelection,
    screen_cols: usize,
) -> Option<TerminalSelectionRange> {
    if !selection.moved {
        return None;
    }

    let (start, end) = ordered_selection_endpoints(selection.anchor, selection.head);
    let start_column = boundary_column(start, screen_cols);
    let end_column = boundary_column(end, screen_cols);
    if start.position.row == end.position.row && start_column == end_column {
        return None;
    }

    Some(TerminalSelectionRange {
        start_row: start.position.row,
        start_column,
        end_row: end.position.row,
        end_column,
    })
}

pub fn selection_snapshot_for_viewport(
    range: TerminalSelectionRange,
    screen: &crate::terminal::session::TerminalScreenSnapshot,
) -> Option<TerminalSelectionSnapshot> {
    let visible_top = top_visible_buffer_line(screen);
    let visible_bottom = visible_top.saturating_add(screen.rows.saturating_sub(1));
    if range.end_row < visible_top || range.start_row > visible_bottom {
        return None;
    }

    let start_row = range.start_row.max(visible_top) - visible_top;
    let end_row = range.end_row.min(visible_bottom) - visible_top;
    let start_column = if range.start_row < visible_top {
        0
    } else {
        range.start_column
    };
    let end_column = if range.end_row > visible_bottom {
        screen.cols
    } else {
        range.end_column
    };
    if start_row == end_row && start_column == end_column {
        return None;
    }

    Some(TerminalSelectionSnapshot {
        start_row,
        start_column,
        end_row,
        end_column,
    })
}

/// Extract selected text from scrollback/line text with trailing-space trim per row.
pub fn selected_text_from_lines(lines: &[&str], selection: TerminalSelectionRange) -> String {
    let mut selected = Vec::new();
    for row in selection.start_row..=selection.end_row {
        let line = lines.get(row).copied().unwrap_or_default();
        let characters: Vec<char> = line.chars().collect();
        let start = if row == selection.start_row {
            selection.start_column.min(characters.len())
        } else {
            0
        };
        let end = if row == selection.end_row {
            selection.end_column.min(characters.len())
        } else {
            characters.len()
        };
        let mut segment: String = characters[start..end].iter().collect();
        while segment.ends_with(' ') {
            segment.pop();
        }
        selected.push(segment);
    }
    selected.join("\n")
}

pub fn selected_text_from_screen(
    screen: &crate::terminal::session::TerminalScreenSnapshot,
    selection: TerminalSelectionRange,
) -> String {
    let visible_top = top_visible_buffer_line(screen);
    let visible_bottom = visible_top.saturating_add(screen.rows.saturating_sub(1));
    if selection.end_row < visible_top || selection.start_row > visible_bottom {
        return String::new();
    }

    let clipped = TerminalSelectionRange {
        start_row: selection.start_row.max(visible_top),
        start_column: if selection.start_row < visible_top {
            0
        } else {
            selection.start_column
        },
        end_row: selection.end_row.min(visible_bottom),
        end_column: if selection.end_row > visible_bottom {
            screen.cols
        } else {
            selection.end_column
        },
    };
    if clipped.start_row > clipped.end_row {
        return String::new();
    }

    let lines: Vec<String> = screen
        .lines
        .iter()
        .map(|line| line.iter().map(|cell| cell.character).collect::<String>())
        .collect();
    let mut selected = Vec::new();
    for buffer_row in clipped.start_row..=clipped.end_row {
        let viewport_row = buffer_row.saturating_sub(visible_top);
        let line = lines
            .get(viewport_row)
            .map(String::as_str)
            .unwrap_or_default();
        let characters: Vec<char> = line.chars().collect();
        let start = if buffer_row == clipped.start_row {
            clipped.start_column.min(characters.len())
        } else {
            0
        };
        let end = if buffer_row == clipped.end_row {
            clipped.end_column.min(characters.len())
        } else {
            characters.len()
        };
        let mut segment: String = characters.get(start..end).unwrap_or(&[]).iter().collect();
        while segment.ends_with(' ') {
            segment.pop();
        }
        selected.push(segment);
    }
    selected.join("\n")
}

pub fn begin_simple_selection(endpoint: TerminalSelectionEndpoint) -> TerminalSelection {
    TerminalSelection {
        anchor: endpoint,
        head: endpoint,
        moved: false,
        mode: TerminalSelectionMode::Simple,
    }
}

pub fn extend_selection_head(
    selection: &mut TerminalSelection,
    endpoint: TerminalSelectionEndpoint,
) {
    selection.head = endpoint;
    selection.moved = selection.anchor != endpoint;
    selection.mode = TerminalSelectionMode::Simple;
}

pub fn finish_simple_selection(selection: Option<TerminalSelection>) -> Option<TerminalSelection> {
    let selection = selection?;
    if !selection.moved && matches!(selection.mode, TerminalSelectionMode::Simple) {
        None
    } else {
        Some(selection)
    }
}

/// Ctrl+C copies when a committed selection exists; otherwise it remains interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalCtrlCAction {
    CopySelection,
    Interrupt,
}

pub fn terminal_ctrl_c_action(has_copyable_selection: bool) -> TerminalCtrlCAction {
    if has_copyable_selection {
        TerminalCtrlCAction::CopySelection
    } else {
        TerminalCtrlCAction::Interrupt
    }
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
        let palette = crate::terminal::view::terminal_render_palette_from_tokens(tokens);
        assert_eq!(palette.canvas, 0x010101);
        assert_eq!(palette.panel, 0x161616);
        assert_eq!(palette.panel_header, 0x020202);
        assert_eq!(palette.row, 0x030303);
        assert_eq!(palette.row_hover, 0x040404);
        assert_eq!(palette.button_hover, 0x040404);
        assert_eq!(palette.border, 0x060606);
        assert_eq!(palette.text_primary, 0x070707);
        assert_eq!(palette.text_muted, 0x080808);
        assert_eq!(palette.text_subtle, 0x080808);
        assert_eq!(palette.text_dim, 0x080808);
        assert_eq!(palette.selection_text, 0x0a0a0a);
        assert_eq!(palette.primary, 0x0b0b0b);
        assert_eq!(palette.primary_muted, 0x0c0c0c);
        assert_eq!(palette.danger, 0x0d0d0d);
        assert_eq!(palette.danger_bg, 0x0e0e0e);
        assert_eq!(palette.warning, 0x0f0f0f);
        assert_eq!(palette.success, 0x101010);
        assert_eq!(palette.terminal_bg, PREVIEW_SENTINEL.to_u32());
        assert_eq!(palette.terminal_fg, 0x121212);
        assert_eq!(palette.terminal_cursor, 0x101010);
        assert_eq!(palette.terminal_selection, 0x141414);
        assert_eq!(palette.selection_bg, 0x141414);
        // The scrollbar's colours are resolved against the TERMINAL plane, not
        // the shell, so they do not follow the sentinel surfaces this fixture
        // pins. Their identity is asserted in
        // `themed_palette_scrollbar_matches_the_shared_spec` instead.
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
        assert_eq!(palette.scrollbar_thumb_hover, theme::TEXT_PRIMARY);
        // Geometry is shared even by the legacy palette: one look.
        assert_eq!(
            palette.scrollbar_spec.idle_thumb_width,
            crate::terminal::view::terminal_scrollbar_spec().idle_thumb_width
        );
    }

    #[test]
    fn effective_style_replaces_default_foreground_with_palette_terminal_fg() {
        let palette = crate::terminal::view::terminal_render_palette_from_tokens(sentinel_tokens());
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
        let palette = crate::terminal::view::terminal_render_palette_from_tokens(sentinel_tokens());
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
        let palette = crate::terminal::view::terminal_render_palette_from_tokens(sentinel_tokens());
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

    #[test]
    fn effective_style_preserves_explicit_ansi_rgb_background_and_text_attrs() {
        let palette = crate::terminal::view::terminal_render_palette_from_tokens(sentinel_tokens());
        let cell = TerminalCellSnapshot {
            character: 'Z',
            zero_width: Vec::new(),
            foreground: 0x22c55e,
            background: 0x1e3a5f,
            bold: true,
            dim: true,
            italic: true,
            underline: true,
            undercurl: true,
            strike: true,
            hidden: false,
            has_hyperlink: false,
            default_background: false,
            default_foreground: false,
        };
        let style = effective_cell_style(&cell, false, None, palette);
        assert_eq!(style.foreground, 0x22c55e);
        assert_eq!(style.background, 0x1e3a5f);
        assert!(style.paint_background);
        assert!(style.bold);
        assert!(style.dim);
        assert!(style.italic);
        assert!(style.underline);
        assert!(style.undercurl);
        assert!(style.strike);
        assert_ne!(style.foreground, palette.terminal_fg);
        assert_ne!(style.background, palette.terminal_bg);
    }

    #[test]
    fn effective_style_block_cursor_uses_visible_palette_cursor() {
        use crate::terminal::session::TerminalCursorSnapshot;
        use alacritty_terminal::vte::ansi::CursorShape;

        let palette = crate::terminal::view::terminal_render_palette_from_tokens(sentinel_tokens());
        let cell = TerminalCellSnapshot {
            character: 'c',
            zero_width: Vec::new(),
            foreground: 0xe4e4e7,
            background: 0x09090b,
            bold: false,
            dim: true,
            italic: false,
            underline: false,
            undercurl: false,
            strike: false,
            hidden: false,
            has_hyperlink: false,
            default_background: true,
            default_foreground: true,
        };
        let cursor = TerminalCursorSnapshot {
            row: 0,
            column: 0,
            shape: CursorShape::Block,
        };
        let style = effective_cell_style(&cell, false, Some(cursor), palette);
        assert_eq!(style.background, palette.terminal_cursor);
        assert_eq!(style.foreground, palette.panel);
        assert!(style.bold);
        assert!(!style.dim);
        assert!(style.paint_background);
    }

    fn relative_luminance(color: u32) -> f32 {
        let channel = |value: u32| {
            let c = (value as f32) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let r = channel((color >> 16) & 0xff);
        let g = channel((color >> 8) & 0xff);
        let b = channel(color & 0xff);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    fn contrast_ratio(a: u32, b: u32) -> f32 {
        let (lighter, darker) = {
            let la = relative_luminance(a);
            let lb = relative_luminance(b);
            if la >= lb {
                (la, lb)
            } else {
                (lb, la)
            }
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    #[test]
    fn v041_derived_chrome_hierarchy_is_readable() {
        let legacy = TerminalRenderPalette::legacy_default();
        assert!(
            relative_luminance(legacy.terminal_bg) < relative_luminance(legacy.canvas),
            "terminal cell plane must sit darker than outer canvas"
        );
        assert!(
            relative_luminance(legacy.terminal_bg) < relative_luminance(legacy.panel_header),
            "terminal cell plane must sit darker than header chrome"
        );
        assert!(
            contrast_ratio(legacy.terminal_fg, legacy.terminal_bg) >= 7.0,
            "monospace default fg needs strong contrast on terminal bg"
        );
        assert_eq!(legacy.terminal_cursor, theme::SUCCESS_TEXT);
        assert_ne!(legacy.scrollbar_thumb, legacy.scrollbar_track);
        assert_ne!(legacy.selection_bg, legacy.terminal_bg);

        let tokens = dark(Density::Comfortable, Scale::Scale100);
        let palette = crate::terminal::view::terminal_render_palette_from_tokens(tokens);
        assert!(
            relative_luminance(palette.terminal_bg) <= relative_luminance(palette.canvas),
            "themed terminal bg must not wash above canvas"
        );
        assert!(
            contrast_ratio(palette.terminal_fg, palette.terminal_bg) >= 4.5,
            "themed terminal fg/bg must stay readable"
        );
        assert_eq!(
            palette.terminal_cursor,
            tokens.status.success.to_u32(),
            "cursor chrome follows v0.4.1 success-colored visibility via ThemeTokens"
        );
        assert_eq!(
            palette.text_dim,
            tokens.text.muted.to_u32(),
            "chrome labels use muted (readable) rather than disabled"
        );
        assert_eq!(
            palette.scrollbar_thumb,
            tokens
                .scrollbar
                .colors_on(tokens.terminal.background)
                .thumb_idle
                .to_u32()
        );
        assert_ne!(palette.scrollbar_thumb, palette.scrollbar_track);
        assert_ne!(palette.scrollbar_thumb, palette.scrollbar_thumb_hover);
        // Explicit ANSI must remain process-owned even after chrome remapping.
        let ansi = TerminalCellSnapshot {
            character: '!',
            zero_width: Vec::new(),
            foreground: 0xfacc15,
            background: 0x1d4ed8,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            undercurl: false,
            strike: false,
            hidden: false,
            has_hyperlink: false,
            default_background: false,
            default_foreground: false,
        };
        let style = effective_cell_style(&ansi, false, None, palette);
        assert_eq!(style.foreground, 0xfacc15);
        assert_eq!(style.background, 0x1d4ed8);
    }
}

#[cfg(test)]
mod cell_pitch_tests {
    use super::{
        last_measured_terminal_cell_pitch, measured_terminal_cell_advance,
        record_measured_terminal_cell_advance, terminal_cell_pitch, terminal_column_offset,
        terminal_grid_size_for_bounds, terminal_pane_from_replica, ReplicaPaneRequest,
        TerminalReplicaOverlay, FALLBACK_TERMINAL_CELL_WIDTH, TERMINAL_FONT_SIZE,
    };
    use crate::state::{SessionDimensions, SessionRuntimeState};
    use crate::terminal::session::{TerminalBackend, TerminalScreenSnapshot, TerminalSessionView};
    use std::path::PathBuf;

    /// The advance GPUI's text system reports for Cascadia Mono at 13 px.
    /// Injected, not measured, so this test does not depend on any font being
    /// installed on the machine running it.
    const MEASURED_CASCADIA_MONO_13PX: f32 = 7.6172;
    /// Consolas is the first fallback. A substitution must be followed, never
    /// rounded back to the constant.
    const MEASURED_CONSOLAS_13PX: f32 = 7.1475;

    fn replica_view(host_reported_cell_width: u16) -> TerminalSessionView {
        let mut dimensions = SessionDimensions::default();
        dimensions.cell_width = host_reported_cell_width;
        TerminalSessionView {
            runtime: SessionRuntimeState::new(
                "pitch-session",
                PathBuf::from("."),
                dimensions,
                TerminalBackend::PortablePtyFeedingAlacritty,
            ),
            screen: TerminalScreenSnapshot::default(),
        }
    }

    #[test]
    fn paint_pitch_is_the_measured_advance_not_the_fallback_constant() {
        let pitch = terminal_cell_pitch(Some(MEASURED_CASCADIA_MONO_13PX));
        assert_eq!(pitch, MEASURED_CASCADIA_MONO_13PX);
        assert_eq!(
            terminal_cell_pitch(Some(MEASURED_CONSOLAS_13PX)),
            MEASURED_CONSOLAS_13PX
        );

        // Every painted x in the grid — background quads, glyph runs and the
        // cursor — is `terminal_column_offset(column, pitch)`, so this is the
        // pitch the paint pass actually uses.
        assert_eq!(
            terminal_column_offset(80, pitch),
            MEASURED_CASCADIA_MONO_13PX * 80.0
        );

        // The defect being guarded: on the constant, column 80 was painted
        // 30.6 px right of the glyph the shaping had put there.
        let drift = terminal_column_offset(80, FALLBACK_TERMINAL_CELL_WIDTH)
            - terminal_column_offset(80, pitch);
        assert!(
            drift > 30.0,
            "the fallback constant must not be the paint pitch; drift was {drift}"
        );
    }

    #[test]
    fn the_constant_is_used_only_when_nothing_could_be_measured() {
        assert_eq!(terminal_cell_pitch(None), FALLBACK_TERMINAL_CELL_WIDTH);
        assert_eq!(terminal_cell_pitch(Some(0.0)), FALLBACK_TERMINAL_CELL_WIDTH);
        assert_eq!(
            terminal_cell_pitch(Some(-1.0)),
            FALLBACK_TERMINAL_CELL_WIDTH
        );
        assert_eq!(
            terminal_cell_pitch(Some(f32::NAN)),
            FALLBACK_TERMINAL_CELL_WIDTH
        );
        assert_eq!(
            terminal_cell_pitch(Some(f32::INFINITY)),
            FALLBACK_TERMINAL_CELL_WIDTH
        );
    }

    #[test]
    fn every_window_free_reader_takes_the_injected_advance() {
        record_measured_terminal_cell_advance(TERMINAL_FONT_SIZE, MEASURED_CASCADIA_MONO_13PX);
        assert_eq!(
            measured_terminal_cell_advance(TERMINAL_FONT_SIZE),
            Some(MEASURED_CASCADIA_MONO_13PX)
        );
        let pitch = last_measured_terminal_cell_pitch(TERMINAL_FONT_SIZE);
        assert_eq!(pitch, MEASURED_CASCADIA_MONO_13PX);

        // Column arithmetic follows the font: 800 px holds 105 columns at the
        // real advance and only 100 at the constant.
        assert_eq!(
            terminal_grid_size_for_bounds(800.0, 180.0, pitch, 18.0).0,
            105
        );
        assert_eq!(
            terminal_grid_size_for_bounds(800.0, 180.0, FALLBACK_TERMINAL_CELL_WIDTH, 18.0).0,
            100
        );

        // The replica pane model must ignore the host's rounded u16 — that
        // field is the fallback the host had no way to measure.
        let view = replica_view(99);
        let model = terminal_pane_from_replica(ReplicaPaneRequest {
            active_project: "",
            session_label: "pitch",
            replica_view: Some(&view),
            last_valid_view: None,
            overlay: TerminalReplicaOverlay::None,
            selection: None,
            search: None,
            search_highlight: None,
            scrollbar: None,
        });
        assert_eq!(model.cell_width, MEASURED_CASCADIA_MONO_13PX);
    }
}

#[cfg(test)]
mod selection_helper_tests {
    use super::{
        begin_simple_selection, extend_selection_head, finish_simple_selection,
        selected_text_from_lines, selected_text_from_screen, selection_mode_for_click,
        selection_range_from, terminal_ctrl_c_action, terminal_endpoint_for_mouse,
        terminal_grid_size_for_bounds, terminal_selection_for_click, top_visible_buffer_line,
        TerminalCellSide, TerminalCtrlCAction, TerminalGridPosition, TerminalSelectionEndpoint,
        TerminalSelectionMode, TerminalSelectionRange, TerminalTextBounds,
    };
    use crate::terminal::session::{TerminalCellSnapshot, TerminalScreenSnapshot};
    use gpui::{point, px};

    fn snapshot_cell(character: char) -> TerminalCellSnapshot {
        TerminalCellSnapshot {
            character,
            zero_width: Vec::new(),
            foreground: 0,
            background: 0,
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
        }
    }

    #[test]
    fn terminal_grid_size_uses_the_full_painted_bounds() {
        assert_eq!(
            terminal_grid_size_for_bounds(1_200.0, 756.0, 10.0, 18.0),
            (120, 42)
        );
        assert_eq!(terminal_grid_size_for_bounds(2.0, 2.0, 10.0, 18.0), (1, 1));
    }

    #[test]
    fn selected_text_from_screen_maps_absolute_buffer_rows_through_visible_top() {
        // total=100, rows=2, offset=0 → visible_top = 98 (buffer rows 98..=99).
        let line0: Vec<_> = "hello world".chars().map(snapshot_cell).collect();
        let line1: Vec<_> = "second line".chars().map(snapshot_cell).collect();
        let screen = TerminalScreenSnapshot {
            lines: vec![line0, line1],
            cols: 11,
            rows: 2,
            total_lines: 100,
            history_size: 98,
            display_offset: 0,
            ..Default::default()
        };
        assert_eq!(top_visible_buffer_line(&screen), 98);
        let range = TerminalSelectionRange {
            start_row: 98,
            start_column: 0,
            end_row: 99,
            end_column: 6,
        };
        assert_eq!(
            selected_text_from_screen(&screen, range),
            "hello world\nsecond"
        );
    }

    #[test]
    fn selected_text_from_screen_respects_scrolled_back_display_offset() {
        // total=12, rows=3, display_offset=2 → visible_top = 7.
        let lines: Vec<Vec<_>> = ["alpha   ", "bravo   ", "charlie "]
            .into_iter()
            .map(|line| line.chars().map(snapshot_cell).collect())
            .collect();
        let screen = TerminalScreenSnapshot {
            lines,
            cols: 8,
            rows: 3,
            total_lines: 12,
            history_size: 9,
            display_offset: 2,
            ..Default::default()
        };
        assert_eq!(top_visible_buffer_line(&screen), 7);
        let range = TerminalSelectionRange {
            start_row: 7,
            start_column: 0,
            end_row: 8,
            end_column: 5,
        };
        assert_eq!(selected_text_from_screen(&screen, range), "alpha\nbravo");
    }

    #[test]
    fn drag_ordered_selection_extracts_exact_multiline_trimmed_text() {
        let lines = ["alpha   ", "bravo   ", "charlie "];
        let mut selection = begin_simple_selection(TerminalSelectionEndpoint {
            position: TerminalGridPosition { row: 0, column: 0 },
            side: TerminalCellSide::Left,
        });
        extend_selection_head(
            &mut selection,
            TerminalSelectionEndpoint {
                position: TerminalGridPosition { row: 1, column: 4 },
                side: TerminalCellSide::Right,
            },
        );
        let range = selection_range_from(selection, 8).expect("moved selection");
        assert_eq!(selected_text_from_lines(&lines, range), "alpha\nbravo");

        // Reverse drag must order the same way.
        let mut reverse = begin_simple_selection(TerminalSelectionEndpoint {
            position: TerminalGridPosition { row: 1, column: 4 },
            side: TerminalCellSide::Right,
        });
        extend_selection_head(
            &mut reverse,
            TerminalSelectionEndpoint {
                position: TerminalGridPosition { row: 0, column: 0 },
                side: TerminalCellSide::Left,
            },
        );
        let reverse_range = selection_range_from(reverse, 8).expect("moved selection");
        assert_eq!(
            selected_text_from_lines(&lines, reverse_range),
            "alpha\nbravo"
        );
    }

    #[test]
    fn copy_vs_interrupt_key_precedence_follows_selection_presence() {
        assert_eq!(
            terminal_ctrl_c_action(true),
            TerminalCtrlCAction::CopySelection
        );
        assert_eq!(
            terminal_ctrl_c_action(false),
            TerminalCtrlCAction::Interrupt
        );
        assert!(
            finish_simple_selection(Some(begin_simple_selection(TerminalSelectionEndpoint {
                position: TerminalGridPosition { row: 0, column: 0 },
                side: TerminalCellSide::Left,
            })))
            .is_none()
        );
    }

    #[test]
    fn hit_testing_clamps_to_actual_grid_bounds() {
        let bounds = TerminalTextBounds {
            left: 10.0,
            top: 20.0,
            width: 40.0,
            height: 20.0,
            cell_width: 10.0,
            row_height: 10.0,
            rows: 2,
            cols: 4,
        };
        let left_half =
            terminal_endpoint_for_mouse(point(px(14.0), px(25.0)), bounds, true).unwrap();
        let right_half =
            terminal_endpoint_for_mouse(point(px(17.0), px(25.0)), bounds, true).unwrap();
        let outside =
            terminal_endpoint_for_mouse(point(px(200.0), px(200.0)), bounds, true).unwrap();
        let rejected = terminal_endpoint_for_mouse(point(px(200.0), px(200.0)), bounds, false);

        assert_eq!(left_half.position.column, 0);
        assert_eq!(left_half.side, TerminalCellSide::Left);
        assert_eq!(right_half.position.column, 0);
        assert_eq!(right_half.side, TerminalCellSide::Right);
        assert_eq!(outside.position.row, 1);
        assert_eq!(outside.position.column, 3);
        assert_eq!(outside.side, TerminalCellSide::Right);
        assert!(rejected.is_none());
    }

    #[test]
    fn rendered_terminal_grid_never_claims_pointer_events_outside_its_bounds() {
        let source = include_str!("view.rs");
        let start = source
            .find("let on_mouse_down = interaction.on_mouse_down.clone();")
            .expect("terminal grid pointer handlers");
        let body = &source[start..];
        let end = body
            .find("\n            }\n        },")
            .expect("terminal grid canvas handlers end");
        let body = &body[..end];
        assert_eq!(
            body.matches("terminal_endpoint_for_mouse(event.position, text_bounds, false)")
                .count(),
            3,
            "terminal mouse down, drag, and release must not clamp unrelated window events into the grid"
        );
        assert!(
            !body.contains("terminal_endpoint_for_mouse(event.position, text_bounds, true)"),
            "a visible terminal must not become a window-wide modal pointer surface"
        );
    }

    #[test]
    fn semantic_and_line_click_modes_select_expected_ranges() {
        assert_eq!(
            selection_mode_for_click(2),
            Some(TerminalSelectionMode::Semantic)
        );
        assert_eq!(
            selection_mode_for_click(3),
            Some(TerminalSelectionMode::Lines)
        );
        let line: Vec<TerminalCellSnapshot> = "cargo test".chars().map(snapshot_cell).collect();
        let screen = TerminalScreenSnapshot {
            lines: vec![line],
            cols: 10,
            rows: 1,
            total_lines: 1,
            ..Default::default()
        };
        let semantic = terminal_selection_for_click(
            &screen,
            TerminalGridPosition { row: 0, column: 2 },
            TerminalSelectionMode::Semantic,
        )
        .unwrap();
        let range = selection_range_from(semantic, screen.cols).unwrap();
        assert_eq!(
            range,
            TerminalSelectionRange {
                start_row: 0,
                start_column: 0,
                end_row: 0,
                end_column: 5,
            }
        );
    }

    /// The whole point of the lane: the terminal's gutter and the shared shell
    /// scrollbar must be the SAME scrollbar, not two that currently agree.
    ///
    /// They are proved equal by construction -- the terminal calls
    /// `crate::ui::scrollbar`'s geometry directly -- so this asserts the
    /// numbers that a reader would otherwise have to take on trust, and it is
    /// sabotage-checked below by moving a token and watching both sides move.
    #[test]
    fn terminal_scrollbar_geometry_equals_the_shared_spec() {
        use crate::ui::scrollbar::{thumb_geometry, track_geometry};
        let tokens = crate::ui::tokens::dark(
            crate::ui::tokens::Density::Comfortable,
            crate::ui::tokens::Scale::Scale100,
        );
        let palette = crate::terminal::view::terminal_render_palette_from_tokens(tokens);
        let spec = palette.scrollbar_spec;
        assert_eq!(
            spec, tokens.scrollbar,
            "the terminal reads the shell's spec"
        );
        assert_eq!(
            crate::terminal::view::terminal_scrollbar_gutter_width(spec),
            spec.gutter_width
        );

        for gutter_height in [120.0_f32, 480.0, 1440.0] {
            for visible in [0.02_f32, 0.25, 0.75] {
                for position in [0.0_f32, 0.5, 1.0] {
                    let idle = thumb_geometry(spec, gutter_height, visible, position, false)
                        .expect("idle thumb");
                    let hovered = thumb_geometry(spec, gutter_height, visible, position, true)
                        .expect("hover thumb");
                    assert_eq!(idle.width, 4.0);
                    assert_eq!(hovered.width, 10.0);
                    assert!(idle.height >= spec.min_thumb_length);
                    let track = track_geometry(spec, gutter_height);
                    assert!(idle.top >= track.top);
                    assert!(idle.top + idle.height <= track.top + track.height + 1e-3);
                }
            }
        }
    }

    /// Sabotage: change the token and both the shell geometry and the terminal
    /// palette have to move. If either stayed put it was reading a constant.
    #[test]
    fn moving_the_scrollbar_token_moves_the_terminal_too() {
        use crate::ui::scrollbar::thumb_geometry;
        let mut tokens = crate::ui::tokens::dark(
            crate::ui::tokens::Density::Comfortable,
            crate::ui::tokens::Scale::Scale100,
        );
        let before =
            crate::terminal::view::terminal_render_palette_from_tokens(tokens).scrollbar_spec;
        let before_thumb = thumb_geometry(before, 400.0, 0.5, 0.0, false).expect("thumb");

        tokens.scrollbar.idle_thumb_width += 7.0;
        tokens.scrollbar.gutter_width += 7.0;
        let after =
            crate::terminal::view::terminal_render_palette_from_tokens(tokens).scrollbar_spec;
        let after_thumb = thumb_geometry(after, 400.0, 0.5, 0.0, false).expect("thumb");

        assert_eq!(
            crate::terminal::view::terminal_scrollbar_gutter_width(after),
            crate::terminal::view::terminal_scrollbar_gutter_width(before) + 7.0
        );
        assert_eq!(after_thumb.width, before_thumb.width + 7.0);
    }

    /// A screen that fits paints no thumb at all -- the same predicate the
    /// shell surfaces use, so an empty log and an empty list agree.
    #[test]
    fn a_terminal_with_no_scrollback_paints_no_thumb() {
        use crate::ui::scrollbar::thumb_geometry;
        let spec = crate::terminal::view::terminal_scrollbar_spec();
        assert!(thumb_geometry(spec, 400.0, 1.0, 0.0, false).is_none());
    }

    /// The hover state is what widens the thumb, and it must reach the colour
    /// as well as the width or a 10 px bar in the idle grey reads as a bug.
    #[test]
    fn hovering_the_terminal_gutter_changes_both_width_and_colour() {
        use crate::ui::scrollbar::thumb_geometry;
        let tokens = crate::ui::tokens::dark(
            crate::ui::tokens::Density::Comfortable,
            crate::ui::tokens::Scale::Scale100,
        );
        let palette = crate::terminal::view::terminal_render_palette_from_tokens(tokens);
        let spec = palette.scrollbar_spec;
        let idle = thumb_geometry(spec, 400.0, 0.4, 0.2, false).expect("idle");
        let hovered = thumb_geometry(spec, 400.0, 0.4, 0.2, true).expect("hover");
        assert!(hovered.width > idle.width);
        assert_eq!(
            palette.scrollbar_thumb_color(false),
            palette.scrollbar_thumb
        );
        assert_eq!(
            palette.scrollbar_thumb_color(true),
            palette.scrollbar_thumb_hover
        );
        assert_ne!(
            palette.scrollbar_thumb_color(false),
            palette.scrollbar_thumb_color(true)
        );
    }

    /// The light theme's terminal is a dark island in a near-white shell, so
    /// its gutter must NOT take the shell's dark-on-light thumb.
    #[test]
    fn the_light_theme_terminal_gutter_takes_the_dark_ground_colours() {
        let tokens = crate::ui::tokens::light(
            crate::ui::tokens::Density::Comfortable,
            crate::ui::tokens::Scale::Scale100,
        );
        let palette = crate::terminal::view::terminal_render_palette_from_tokens(tokens);
        assert_eq!(
            palette.scrollbar_thumb,
            tokens.scrollbar.on_dark.thumb_idle.to_u32()
        );
        assert_ne!(
            palette.scrollbar_thumb,
            tokens.scrollbar.on_light.thumb_idle.to_u32()
        );
    }
}
