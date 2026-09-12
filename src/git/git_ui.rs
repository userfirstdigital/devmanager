//! The Git window, painted with the same vocabulary as the task panels:
//! `task_cockpit::panel` rows, captions and buttons, the panel frame's tab
//! row, `overlay_chrome` menus and inputs, and the task panels' diff painter.
//!
//! Columns are sized in pixels from the window's width rather than by
//! percentages, because GPUI only paints a real ellipsis into a definite
//! width (see `overlay_chrome::ellipsised`).

use super::{GitField, GitView, GitWindow};
use crate::git::git_service::{GitDiffResult, GitFileStatus, GitLogEntry};
use crate::icons;
use crate::ui::overlay_chrome::{
    self, approx_text_width, ellipsised, OverlayRowState, INPUT_PADDING_X, INPUT_PADDING_Y,
    INPUT_RADIUS, OVERLAY_ANCHOR_DROP, OVERLAY_MAX_HEIGHT, TITLE_FONT_SIZE,
};
use crate::ui::task_cockpit::panel::{
    panel_button_shell, panel_empty_state, panel_row_shell, META_FONT_SIZE, REGION_PADDING,
    ROW_FONT_SIZE, ROW_GAP, ROW_PADDING_X, ROW_PADDING_Y,
};
use crate::ui::tokens::ThemeTokens;
use gpui::{
    div, prelude::*, px, AnyElement, App, ClickEvent, ClipboardItem, Context, Div, ElementId,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, SharedString,
    Stateful, StatefulInteractiveElement, Styled, Window,
};

/// The rail header and the repository toolbar share a height so their bottom
/// rules meet across the window.
const HEADER_HEIGHT: f32 = 36.0;
/// The filter row and the diff header share a height for the same reason.
const SUBHEADER_HEIGHT: f32 = 36.0;
/// The panel frame's tab row (`ui::panel::render`): 28 px, 11.5 px tabs with
/// 3/5 padding and a radius-6 pill on the selected tab.
const TAB_ROW_HEIGHT: f32 = 28.0;
const TAB_GAP: f32 = 2.0;
const TAB_PADDING_X: f32 = 9.0;
const TAB_PADDING_TOP: f32 = 3.0;
const TAB_PADDING_BOTTOM: f32 = 5.0;
const TAB_RADIUS: f32 = 6.0;
const TABS_PADDING_TOP: f32 = 6.0;
/// Button labels (rule 4) and the row-title/meta line heights overlay rows use.
const BUTTON_FONT_SIZE: f32 = 11.0;
const TITLE_LINE_HEIGHT: f32 = 16.0;
const META_LINE_HEIGHT: f32 = 15.0;
const ICON_BUTTON_SIZE: f32 = 22.0;
const ICON_SIZE: f32 = 14.0;
const CHECKBOX_SIZE: f32 = 16.0;
const STATUS_MARK_WIDTH: f32 = 12.0;
/// The task panels' diff gutter (`task_cockpit::changes_panel`): two 44 px
/// line-number columns and a 20 px +/- mark.
const DIFF_GUTTER_WIDTH: f32 = 2.0 * 44.0 + 20.0;
/// A patch past these bounds is cut, with a sentence saying how much is shown.
const MAX_DIFF_HUNKS: usize = 100;
const MAX_DIFF_LINES: usize = 2_000;
/// Below this window width the repository rail folds into a menu on the
/// toolbar, so the file list and the diff keep a usable width.
const RAIL_MIN_WINDOW_WIDTH: f32 = 760.0;
/// The uncommitted-changes light and the slot every repository row keeps
/// for it before the name.
const REPO_DOT_SIZE: f32 = 7.0;
const REPO_DOT_SLOT: f32 = 8.0;
const REPO_DOT_GAP: f32 = 6.0;
/// A commit's hover card: wide enough for a description paragraph to wrap
/// into readable lines, and bounded so a long one stays a card.
const COMMIT_TOOLTIP_WIDTH: f32 = 420.0;
const COMMIT_TOOLTIP_MAX_CHARS: usize = 1200;
const COMMIT_TOOLTIP_MAX_LINES: usize = 20;

/// Pixel widths of the window's columns.
#[derive(Clone, Copy)]
struct Columns {
    rail: f32,
    detail: f32,
    list: f32,
    diff: f32,
}

fn rail_visible(state: &GitWindow) -> bool {
    state.is_native() && state.viewport_width >= RAIL_MIN_WINDOW_WIDTH
}

fn columns(state: &GitWindow) -> Columns {
    let width = state.viewport_width.max(320.0);
    let rail = if rail_visible(state) {
        (width * 0.25).clamp(200.0, 240.0)
    } else {
        0.0
    };
    let detail = (width - rail).max(0.0);
    let list = (detail * 0.42).clamp(220.0, 360.0).min(detail);
    Columns {
        rail,
        detail,
        list,
        diff: (detail - list).max(0.0),
    }
}

/// The width a one-line label needs, capped at what the slot can give it, so
/// a short label does not reserve its whole slot and a long one ellipsises.
fn fitted_width(text: &str, font_size: f32, available: f32) -> f32 {
    (approx_text_width(text, font_size) + 2.0).min(available.max(0.0))
}

/// `ellipsised` for a label that sits in a flex ROW.
///
/// In a row, taffy measures a flex item at max-content to find its automatic
/// minimum width, GPUI caches that full-length measure for the frame, and the
/// label hard-clips mid-glyph instead of ellipsising. Inside a column of its
/// own the text is only ever measured at its definite width -- the same shape
/// an overlay row's title has, which is why those ellipsise.
fn label(width: f32, text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .w(px(width.max(0.0)))
        .flex()
        .flex_col()
        .child(ellipsised(width, text))
}

// ── Shared pieces ──────────────────────────────────────────────────────────

/// A bare 14 px glyph in a 22 px box: the panel chrome's quiet affordance.
fn icon_button(
    id: &'static str,
    glyph: &'static str,
    tooltip: &'static str,
    enabled: bool,
    tokens: ThemeTokens,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .size(px(ICON_BUTTON_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(TAB_RADIUS))
        .child(icons::app_icon(
            glyph,
            ICON_SIZE,
            if enabled {
                tokens.text.secondary.to_u32()
            } else {
                tokens.text.disabled.to_u32()
            },
        ))
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .hover(|style| style.bg(tokens.surfaces.hover.to_gpui()))
        })
        .tooltip(move |window, app| {
            gpui_component::tooltip::Tooltip::new(tooltip).build(window, app)
        })
}

/// Rule 4's default button with a label.
fn text_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
    tokens: ThemeTokens,
) -> Stateful<Div> {
    panel_button_shell(tokens, enabled)
        .id(id)
        .whitespace_nowrap()
        .child(label.into())
}

/// The shared input look (`overlay_chrome`): sunken, one hairline, radius 4.
fn input_shell(active: bool, tokens: ThemeTokens) -> Div {
    div()
        .w_full()
        .px(px(INPUT_PADDING_X))
        .py(px(INPUT_PADDING_Y))
        .rounded(px(INPUT_RADIUS))
        .bg(tokens.surfaces.sunken.to_gpui())
        .border_1()
        .border_color(if active {
            tokens.borders.focus.to_gpui()
        } else {
            tokens.borders.default.to_gpui()
        })
        .text_size(px(ROW_FONT_SIZE))
        .cursor(gpui::CursorStyle::IBeam)
}

fn input_value(value: &str, placeholder: &'static str, tokens: ThemeTokens) -> AnyElement {
    let line = div().whitespace_nowrap().overflow_hidden();
    if value.is_empty() {
        line.text_color(tokens.text.muted.to_gpui())
            .child(placeholder)
            .into_any_element()
    } else {
        line.text_color(tokens.text.primary.to_gpui())
            .child(value.to_string())
            .into_any_element()
    }
}

fn caption_text(text: impl Into<SharedString>, tokens: ThemeTokens) -> Div {
    div()
        .text_size(px(META_FONT_SIZE))
        .text_color(tokens.text.muted.to_gpui())
        .child(text.into())
}

/// A history row's two lines. The slot before the subject carries an ↑ when
/// the commit is on no remote yet, in the same place the repository list puts
/// its uncommitted light, so both lists read the same way.
fn commit_row_lines(
    entry: &GitLogEntry,
    width: f32,
    state: OverlayRowState,
    tokens: ThemeTokens,
) -> [AnyElement; 2] {
    let indent = REPO_DOT_SLOT + REPO_DOT_GAP;
    let text_width = (width - indent).max(0.0);
    let short_hash: String = entry.hash.chars().take(7).collect();
    let meta = format!(
        "{}{short_hash} · {} · {}",
        if entry.unpushed { "Not pushed · " } else { "" },
        entry.author_name,
        format_relative_date(&entry.date)
    );
    [
        div()
            .flex()
            .items_center()
            .gap(px(REPO_DOT_GAP))
            .child(
                div()
                    .w(px(REPO_DOT_SLOT))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .text_size(px(META_FONT_SIZE))
                    .line_height(px(TITLE_LINE_HEIGHT))
                    .text_color(tokens.text.secondary.to_gpui())
                    .when(entry.unpushed, |slot| slot.child("\u{2191}")),
            )
            .child(
                label(text_width, entry.subject.clone())
                    .text_size(px(ROW_FONT_SIZE))
                    .line_height(px(TITLE_LINE_HEIGHT))
                    .text_color(overlay_chrome::row_title_colour(state, tokens).to_gpui()),
            )
            .into_any_element(),
        ellipsised(text_width, meta)
            .ml(px(indent))
            .text_size(px(META_FONT_SIZE))
            .line_height(px(META_LINE_HEIGHT))
            .text_color(overlay_chrome::row_meta_colour(state, tokens).to_gpui())
            .into_any_element(),
    ]
}

/// The hover card for a commit: the full subject, the description, and who
/// and when -- everything the one-line row has to cut.
fn commit_tooltip(entry: &GitLogEntry) -> SharedString {
    let mut text = entry.subject.clone();
    if let Some(body) = entry
        .body
        .as_deref()
        .map(str::trim)
        .filter(|body| !body.is_empty())
    {
        text.push_str("\n\n");
        text.push_str(&bounded_text(
            body,
            COMMIT_TOOLTIP_MAX_CHARS,
            COMMIT_TOOLTIP_MAX_LINES,
        ));
    }
    text.push_str(&format!(
        "\n\n{} · {} · {}",
        entry.hash,
        entry.author_name,
        format_relative_date(&entry.date)
    ));
    if entry.unpushed {
        text.push_str("\nNot pushed yet");
    }
    text.into()
}

fn bounded_text(text: &str, max_chars: usize, max_lines: usize) -> String {
    let mut lines = text.lines();
    let mut out = lines
        .by_ref()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n");
    let mut cut = lines.next().is_some();
    if out.chars().count() > max_chars {
        out = out.chars().take(max_chars).collect();
        cut = true;
    }
    if cut {
        out.push('…');
    }
    out
}

/// A repository's status line, in the destructive colour when it could not be
/// read so a failure is not mistaken for "no changes".
fn repository_meta_colour(
    repo: &super::RepoEntry,
    state: OverlayRowState,
    tokens: ThemeTokens,
) -> crate::ui::tokens::Color {
    if repo.status_error.is_some() {
        tokens.status.destructive
    } else {
        overlay_chrome::row_meta_colour(state, tokens)
    }
}

fn repository_tooltip(repo: &super::RepoEntry) -> SharedString {
    SharedString::from(match &repo.status_error {
        Some(error) => format!("{}\n{error}", repo.label),
        None if repo.has_uncommitted() => format!(
            "{}\n{} {}",
            repo.label,
            repo.uncommitted_label(),
            if repo.change_count == 1 && !repo.changes_truncated {
                "file"
            } else {
                "files"
            }
        ),
        None => repo.label.clone(),
    })
}

/// The uncommitted-changes light: a small `status.attention` dot, the colour
/// the board gives work that is waiting for you. The row's status line always
/// says the count as well, so the colour never carries the meaning alone.
fn uncommitted_dot(tokens: ThemeTokens) -> Div {
    div()
        .flex_none()
        .size(px(REPO_DOT_SIZE))
        .rounded_full()
        .bg(tokens.status.attention.to_gpui())
}

/// A repository row's two lines, with the light's slot before the name. The
/// slot is reserved on every row, lit or not, so the names stay aligned.
fn repository_row_lines(
    repo: &super::RepoEntry,
    meta: String,
    width: f32,
    state: OverlayRowState,
    tokens: ThemeTokens,
) -> [AnyElement; 2] {
    let indent = REPO_DOT_SLOT + REPO_DOT_GAP;
    let text_width = (width - indent).max(0.0);
    [
        div()
            .flex()
            .items_center()
            .gap(px(REPO_DOT_GAP))
            .child(
                div()
                    .w(px(REPO_DOT_SLOT))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(repo.has_uncommitted(), |slot| {
                        slot.child(uncommitted_dot(tokens))
                    }),
            )
            .child(
                label(text_width, repo.label.clone())
                    .text_size(px(ROW_FONT_SIZE))
                    .line_height(px(TITLE_LINE_HEIGHT))
                    .text_color(overlay_chrome::row_title_colour(state, tokens).to_gpui()),
            )
            .into_any_element(),
        ellipsised(text_width, meta)
            .ml(px(indent))
            .text_size(px(META_FONT_SIZE))
            .line_height(px(META_LINE_HEIGHT))
            .text_color(repository_meta_colour(repo, state, tokens).to_gpui())
            .into_any_element(),
    ]
}

// ── Main window render ─────────────────────────────────────────────────────

pub fn render_git_window(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    div()
        .size_full()
        .flex()
        .when(rail_visible(state), |view| {
            view.child(render_repository_rail(state, cx))
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(render_repository_detail(state, cx)),
        )
        .into_any_element()
}

// ── Repository rail ────────────────────────────────────────────────────────

fn render_repository_rail(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let tokens = state.tokens;
    let cols = columns(state);
    let idle = !state.is_mutating && !state.is_loading;
    let text_width = (cols.rail - 2.0 * ROW_PADDING_X - 1.0).max(0.0);
    div()
        .id("git-repositories")
        .w(px(cols.rail))
        .flex_none()
        .h_full()
        .flex()
        .flex_col()
        .bg(tokens.surfaces.canvas.to_gpui())
        .border_r_1()
        .border_color(tokens.borders.subtle.to_gpui())
        .child(
            div()
                .flex_none()
                .h(px(HEADER_HEIGHT))
                .flex()
                .items_center()
                .gap(px(2.0))
                .pl(px(ROW_PADDING_X))
                .pr(px(6.0))
                .border_b_1()
                .border_color(tokens.borders.subtle.to_gpui())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(TITLE_FONT_SIZE))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child("Repositories"),
                        )
                        .child(caption_text(state.repos.len().to_string(), tokens).flex_none()),
                )
                .child(
                    icon_button(
                        "git-refresh-repositories",
                        icons::REFRESH_CW,
                        "Refresh local changes and last fetched sync status",
                        idle,
                        tokens,
                    )
                    .when(idle, |button| {
                        button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.reload_repositories(cx);
                        }))
                    }),
                )
                // The window may have no system title bar (client-side
                // decorations on Wayland), so it always carries its own way out.
                .child(
                    icon_button(
                        "git-close-window",
                        icons::X,
                        "Close the Git window (Esc)",
                        true,
                        tokens,
                    )
                    .on_click(cx.listener(|_, _: &ClickEvent, window, _| {
                        window.remove_window();
                    })),
                ),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(ROW_GAP))
                .px(px(ROW_PADDING_X))
                .pt(px(8.0))
                .child({
                    let enabled = idle && !state.repos.is_empty();
                    text_button("git-fetch-all", "Fetch all", enabled, tokens)
                        .tooltip(|window, app| {
                            gpui_component::tooltip::Tooltip::new(
                                "Fetch every repository to update commits to push or pull",
                            )
                            .build(window, app)
                        })
                        .when(enabled, |button| {
                            button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.fetch_all_repositories(cx)
                            }))
                        })
                }),
        )
        .child(overlay_chrome::section_label("Project folders", tokens))
        .child(
            div()
                .id("git-repository-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .children(state.repos.iter().enumerate().map(|(index, repo)| {
                    let row_state = OverlayRowState::selected_when(index == state.active_repo);
                    let tooltip = repository_tooltip(repo);
                    overlay_chrome::overlay_row(("git-repository", index), row_state, tokens)
                        .children(repository_row_lines(
                            repo,
                            repo.status_label(),
                            text_width,
                            row_state,
                            tokens,
                        ))
                        .tooltip(move |window, app| {
                            gpui_component::tooltip::Tooltip::new(tooltip.clone())
                                .build(window, app)
                        })
                        // `switch_repo` decides whether a switch can happen now.
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.switch_repo(index, cx);
                        }))
                        .into_any_element()
                })),
        )
        .child(
            // The dot stays beside its words: the caption shrinks and wraps
            // its own text in a narrow rail rather than dropping below it.
            div()
                .flex_none()
                .flex()
                .items_start()
                .gap(px(REPO_DOT_GAP))
                .px(px(ROW_PADDING_X))
                .py(px(8.0))
                .border_t_1()
                .border_color(tokens.borders.subtle.to_gpui())
                .child(
                    div()
                        .flex_none()
                        .h(px(META_LINE_HEIGHT))
                        .flex()
                        .items_center()
                        .child(uncommitted_dot(tokens)),
                )
                .child(
                    caption_text("uncommitted · ↑ to push · ↓ to pull", tokens)
                        .flex_1()
                        .min_w_0()
                        .line_height(px(META_LINE_HEIGHT)),
                ),
        )
        .into_any_element()
}

// ── Repository detail ──────────────────────────────────────────────────────

fn render_repository_detail(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let tokens = state.tokens;
    div()
        .size_full()
        .flex()
        .flex_col()
        .bg(tokens.surfaces.raised.to_gpui())
        .child(render_toolbar(state, cx))
        .children(render_login_bar(state, cx))
        .child(render_tab_bar(state, cx))
        .children(
            state
                .operation_result
                .as_ref()
                .map(|(success, msg)| render_operation_banner(tokens, *success, msg)),
        )
        .child(match state.active_view {
            GitView::Changes => render_changes_view(state, cx),
            GitView::History => render_history_view(state, cx),
        })
        .into_any_element()
}

/// The device-code prompt while a GitHub sign-in is waiting for the user.
fn render_login_bar(state: &GitWindow, cx: &mut Context<GitWindow>) -> Option<AnyElement> {
    let tokens = state.tokens;
    let login = state.login_state.as_ref()?;
    let code = login.user_code.clone();
    Some(
        div()
            .w_full()
            .flex_none()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(ROW_GAP))
            .px(px(ROW_PADDING_X))
            .py(px(ROW_PADDING_Y))
            .border_b_1()
            .border_color(tokens.borders.subtle.to_gpui())
            .child(caption_text("Enter this code on GitHub", tokens))
            .child(
                div()
                    .flex_none()
                    .px(px(8.0))
                    .py(px(1.0))
                    .rounded(px(INPUT_RADIUS))
                    .border_1()
                    .border_color(tokens.borders.strong.to_gpui())
                    .font_family("Consolas")
                    .text_size(px(TITLE_FONT_SIZE))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(login.user_code.clone()),
            )
            .child(
                text_button("git-copy-code", "Copy", true, tokens).on_click(cx.listener(
                    move |this, _: &ClickEvent, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
                        this.operation_result =
                            Some((true, "Code copied to clipboard".to_string()));
                        cx.notify();
                    },
                )),
            )
            .child(caption_text("Waiting for authorization…", tokens))
            .into_any_element(),
    )
}

// ── Toolbar ─────────────────────────────────────────────────────────────────

/// The toolbar's branch label: the branch, or where HEAD is detached.
fn toolbar_branch_text(state: &GitWindow) -> String {
    let status = state.status.as_ref();
    let branch_name = status
        .and_then(|s| s.branch.as_deref())
        .unwrap_or("(no branch)");
    if status.map(|s| s.is_detached).unwrap_or(false) {
        format!("detached @ {branch_name}")
    } else {
        branch_name.to_string()
    }
}

/// The one sync action the toolbar offers, from the branch's upstream state.
struct SyncAction {
    label: &'static str,
    count: String,
    tooltip: String,
}

/// The toolbar's widths, paid for control by control so the repository name
/// ellipsises into what is left, and so the branch menu can hang under the
/// branch control instead of at a guessed offset.
struct ToolbarLayout {
    title_width: f32,
    branch_label_width: f32,
    branch_left: f32,
    has_multiple: bool,
    show_close: bool,
}

fn toolbar_layout(state: &GitWindow) -> ToolbarLayout {
    let cols = columns(state);
    let rail_shown = rail_visible(state);
    // Without the rail the repository name is the way to every repository.
    let has_multiple = state.repos.len() > 1 && !rail_shown;
    // And the rail's close button moves here.
    let show_close = state.is_native() && !rail_shown;
    let sync = sync_action(state);
    let sync_width = approx_text_width(sync.label, BUTTON_FONT_SIZE)
        + if sync.count.is_empty() {
            0.0
        } else {
            4.0 + approx_text_width(&sync.count, META_FONT_SIZE)
        }
        + 12.0
        + 4.0
        + 18.0;
    let branch_label_width = fitted_width(
        &toolbar_branch_text(state),
        ROW_FONT_SIZE,
        (cols.detail * 0.35).max(80.0) - 44.0,
    );
    let branch_width = branch_label_width + ICON_SIZE + 12.0 + 8.0 + 12.0;
    let close_width = if show_close {
        ICON_BUTTON_SIZE + ROW_GAP
    } else {
        0.0
    };
    // The picker's gap, chevron and padding, less the margin that keeps the
    // name on the row's left edge.
    let picker_width = if has_multiple {
        4.0 + 12.0 + 2.0 * 6.0 - 6.0
    } else {
        0.0
    };
    let title_width = fitted_width(
        state.repo_label(),
        TITLE_FONT_SIZE,
        (cols.detail
            - 2.0 * ROW_PADDING_X
            - branch_width
            - sync_width
            - close_width
            - 3.0 * ROW_GAP
            - picker_width)
            .max(24.0),
    );
    ToolbarLayout {
        title_width,
        branch_label_width,
        branch_left: cols.rail + ROW_PADDING_X + title_width + picker_width + ROW_GAP,
        has_multiple,
        show_close,
    }
}

fn sync_action(state: &GitWindow) -> SyncAction {
    let status = state.status.as_ref();
    let has_upstream = status.and_then(|s| s.upstream.as_ref()).is_some();
    let ahead = status.map(|s| s.ahead).unwrap_or(0);
    let behind = status.map(|s| s.behind).unwrap_or(0);

    let last_fetched = state
        .last_fetch_at
        .map(|t| {
            let secs = t.elapsed().as_secs();
            if secs < 60 {
                "Last fetched just now".to_string()
            } else {
                format!("Last fetched {}m ago", secs / 60)
            }
        })
        .unwrap_or_else(|| "Not fetched in this session".to_string());
    let (label, count, tooltip) = if state.is_pushing {
        ("Pushing…", String::new(), String::new())
    } else if state.is_pulling {
        ("Pulling…", String::new(), String::new())
    } else if state.is_fetching {
        ("Fetching…", String::new(), String::new())
    } else if !has_upstream {
        (
            "Publish branch",
            String::new(),
            "Push this branch to origin and track it".to_string(),
        )
    } else if behind > 0 {
        // Behind comes first even when also ahead: the remote refuses a push
        // until its new commits are pulled in (GitHub Desktop does the same).
        (
            "Pull origin",
            if ahead > 0 {
                format!("\u{2193}{behind} \u{2191}{ahead}")
            } else {
                format!("\u{2193}{behind}")
            },
            if ahead > 0 {
                format!("Pull the remote's {behind} new commit(s) first, then push your {ahead}")
            } else {
                format!("{behind} commit(s) to pull")
            },
        )
    } else if ahead > 0 {
        (
            "Push origin",
            format!("\u{2191}{ahead}"),
            format!("{ahead} commit(s) to push"),
        )
    } else {
        ("Fetch origin", String::new(), last_fetched)
    };
    SyncAction {
        label,
        count,
        tooltip,
    }
}

fn render_toolbar(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let tokens = state.tokens;
    let status = state.status.as_ref();
    let has_upstream = status.and_then(|s| s.upstream.as_ref()).is_some();
    let ahead = status.map(|s| s.ahead).unwrap_or(0);
    let behind = status.map(|s| s.behind).unwrap_or(0);
    let busy = state.is_pushing || state.is_pulling || state.is_fetching;
    let SyncAction {
        label: sync_label,
        count: sync_count,
        tooltip: sync_tooltip,
    } = sync_action(state);
    let branch_text = toolbar_branch_text(state);
    let ToolbarLayout {
        title_width,
        branch_label_width,
        has_multiple,
        show_close,
        ..
    } = toolbar_layout(state);

    let title = div()
        .id("git-repository-title")
        .flex_none()
        .flex()
        .items_center()
        .gap(px(4.0))
        .child(
            label(title_width, state.repo_label().to_string())
                .text_size(px(TITLE_FONT_SIZE))
                .font_weight(gpui::FontWeight::SEMIBOLD),
        )
        .when(has_multiple, |title| {
            title
                .ml(px(-6.0))
                .px(px(6.0))
                .py(px(2.0))
                .rounded(px(TAB_RADIUS))
                .cursor_pointer()
                .when(state.show_repo_dropdown, |title| {
                    title.bg(tokens.surfaces.selection.to_gpui())
                })
                .hover(|style| style.bg(tokens.surfaces.hover.to_gpui()))
                .child(icons::app_icon(
                    icons::CHEVRON_DOWN,
                    12.0,
                    tokens.text.muted.to_u32(),
                ))
                .tooltip(|window, app| {
                    gpui_component::tooltip::Tooltip::new("Switch repository").build(window, app)
                })
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.show_repo_dropdown = !this.show_repo_dropdown;
                    if this.show_repo_dropdown {
                        this.refresh_all_repo_statuses(cx);
                    }
                    cx.notify();
                }))
        });

    let branch = div()
        .id("git-branch-selector")
        .flex_none()
        .flex()
        .items_center()
        .gap(px(4.0))
        .px(px(6.0))
        .py(px(2.0))
        .rounded(px(TAB_RADIUS))
        .cursor_pointer()
        .when(state.show_branch_dropdown, |chip| {
            chip.bg(tokens.surfaces.selection.to_gpui())
        })
        .hover(|style| style.bg(tokens.surfaces.hover.to_gpui()))
        .child(icons::app_icon(
            icons::GIT_BRANCH,
            ICON_SIZE,
            tokens.text.muted.to_u32(),
        ))
        .child(label(branch_label_width, branch_text).text_size(px(ROW_FONT_SIZE)))
        .child(icons::app_icon(
            icons::CHEVRON_DOWN,
            12.0,
            tokens.text.muted.to_u32(),
        ))
        .tooltip(|window, app| {
            gpui_component::tooltip::Tooltip::new("Switch or create a branch").build(window, app)
        })
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.show_branch_dropdown = !this.show_branch_dropdown;
            if this.show_branch_dropdown {
                this.load_branches(cx);
                this.focus(window);
                this.active_field = Some(GitField::BranchFilter);
                this.cursor = 0;
            }
            cx.notify();
        }));

    let sync = panel_button_shell(tokens, !busy)
        .id("git-sync")
        .flex()
        .items_center()
        .gap(px(4.0))
        .whitespace_nowrap()
        .child(icons::app_icon(
            icons::REFRESH_CW,
            12.0,
            if busy {
                tokens.text.disabled.to_u32()
            } else {
                tokens.text.secondary.to_u32()
            },
        ))
        .child(sync_label)
        .when(!sync_count.is_empty(), |button| {
            button.child(caption_text(sync_count.clone(), tokens))
        })
        .when(!sync_tooltip.is_empty(), |button| {
            let tooltip = SharedString::from(sync_tooltip.clone());
            button.tooltip(move |window, app| {
                gpui_component::tooltip::Tooltip::new(tooltip.clone()).build(window, app)
            })
        })
        .when(!busy, |button| {
            button.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                if this.is_pushing || this.is_pulling || this.is_fetching {
                    return;
                }
                if !has_upstream {
                    this.push_action(cx);
                } else if behind > 0 {
                    this.pull_action(cx);
                } else if ahead > 0 {
                    this.push_action(cx);
                } else {
                    this.fetch_action(cx);
                }
            }))
        });

    div()
        .w_full()
        .flex_none()
        .h(px(HEADER_HEIGHT))
        .flex()
        .items_center()
        .gap(px(ROW_GAP))
        .px(px(ROW_PADDING_X))
        .overflow_hidden()
        .border_b_1()
        .border_color(tokens.borders.subtle.to_gpui())
        .child(title)
        .child(branch)
        .child(div().flex_1())
        .child(sync)
        .when(show_close, |row| {
            row.child(
                icon_button(
                    "git-close-window",
                    icons::X,
                    "Close the Git window (Esc)",
                    true,
                    tokens,
                )
                .on_click(cx.listener(|_, _: &ClickEvent, window, _| {
                    window.remove_window();
                })),
            )
        })
        .into_any_element()
}

// ── Tab bar ─────────────────────────────────────────────────────────────────

fn render_tab_bar(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let tokens = state.tokens;
    let file_count = state
        .status
        .as_ref()
        .map(|s| {
            s.entries
                .iter()
                .map(|entry| &entry.path)
                .collect::<std::collections::HashSet<_>>()
                .len()
        })
        .unwrap_or(0);
    let count = match state.status.as_ref().map_or(0, |s| s.omitted_entries) {
        0 => file_count.to_string(),
        _ => format!("{file_count}+"),
    };
    let idle = !state.is_loading && !state.is_mutating;

    div()
        .w_full()
        .flex_none()
        .h(px(TAB_ROW_HEIGHT))
        .flex()
        .items_end()
        .gap(px(TAB_GAP))
        .pt(px(TABS_PADDING_TOP))
        .px(px(ROW_PADDING_X))
        .border_b_1()
        .border_color(tokens.borders.subtle.to_gpui())
        .text_size(px(ROW_FONT_SIZE))
        .child(render_tab(
            "git-tab-changes",
            "Changes",
            Some(count),
            state.active_view == GitView::Changes,
            tokens,
            cx.listener(|this, _: &ClickEvent, _, cx| {
                this.active_view = GitView::Changes;
                cx.notify();
            }),
        ))
        .child(render_tab(
            "git-tab-history",
            "History",
            None,
            state.active_view == GitView::History,
            tokens,
            cx.listener(|this, _: &ClickEvent, _, cx| {
                this.active_view = GitView::History;
                if this.log_entries.is_empty() {
                    this.load_history(cx);
                }
                cx.notify();
            }),
        ))
        .child(div().flex_1())
        .child(
            icon_button(
                "git-refresh",
                icons::REFRESH_CW,
                "Refresh this repository",
                idle,
                tokens,
            )
            .size(px(20.0))
            .mb(px(3.0))
            .when(idle, |button| {
                button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.operation_result = None;
                    this.refresh_status(cx);
                    if this.active_view == GitView::History {
                        this.log_page = 0;
                        this.log_entries.clear();
                        this.load_history(cx);
                    }
                    cx.notify();
                }))
            }),
        )
        .into_any_element()
}

/// One tab, painted like the panel frame's: a radius-6 pill when selected,
/// `text.secondary` with a hover fill otherwise.
fn render_tab(
    id: &'static str,
    label: &'static str,
    count: Option<String>,
    active: bool,
    tokens: ThemeTokens,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.0))
        .px(px(TAB_PADDING_X))
        .pt(px(TAB_PADDING_TOP))
        .pb(px(TAB_PADDING_BOTTOM))
        .rounded_tl(px(TAB_RADIUS))
        .rounded_tr(px(TAB_RADIUS))
        .cursor_pointer()
        .when(active, |tab| {
            tab.bg(tokens.surfaces.selection.to_gpui())
                .text_color(tokens.text.primary.to_gpui())
        })
        .when(!active, |tab| {
            tab.text_color(tokens.text.secondary.to_gpui())
                .hover(|style| style.bg(tokens.surfaces.hover.to_gpui()))
        })
        .child(label)
        .children(count.map(|count| caption_text(count, tokens)))
        .on_click(on_click)
        .into_any_element()
}

// ── Operation banner ────────────────────────────────────────────────────────

fn render_operation_banner(tokens: ThemeTokens, success: bool, msg: &str) -> AnyElement {
    div()
        .w_full()
        .flex_none()
        .px(px(ROW_PADDING_X))
        .py(px(ROW_PADDING_Y))
        .border_b_1()
        .border_color(tokens.borders.subtle.to_gpui())
        .text_size(px(BUTTON_FONT_SIZE))
        .text_color(if success {
            tokens.status.success.to_gpui()
        } else {
            tokens.status.destructive.to_gpui()
        })
        .child(msg.to_string())
        .into_any_element()
}

// ── Changes view ────────────────────────────────────────────────────────────

fn render_changes_view(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let tokens = state.tokens;
    let cols = columns(state);
    div()
        .flex_1()
        .flex()
        .min_h_0()
        .child(
            div()
                .w(px(cols.list))
                .flex_none()
                .flex()
                .flex_col()
                .border_r_1()
                .border_color(tokens.borders.subtle.to_gpui())
                .child(render_file_filter(state, cx))
                .child(render_file_list_header(state, cx))
                .child(render_file_list(state, cx))
                .child(render_commit_form(state, cx)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .flex()
                .flex_col()
                .bg(tokens.surfaces.sunken.to_gpui())
                .child(render_diff_header(state))
                .child(render_diff_panel(state)),
        )
        .into_any_element()
}

fn render_file_filter(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let tokens = state.tokens;
    div()
        .w_full()
        .flex_none()
        .h(px(SUBHEADER_HEIGHT))
        .flex()
        .items_center()
        .px(px(ROW_PADDING_X))
        .border_b_1()
        .border_color(tokens.borders.subtle.to_gpui())
        .child(
            input_shell(
                matches!(state.active_field, Some(GitField::FileFilter)),
                tokens,
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.focus(window);
                    this.active_field = Some(GitField::FileFilter);
                    this.cursor = this.file_filter.len();
                    cx.notify();
                }),
            )
            .child(input_value(
                &state.file_filter,
                "Filter changed files",
                tokens,
            )),
        )
        .into_any_element()
}

fn render_file_list_header(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let tokens = state.tokens;
    let entries = state.filtered_entries();
    let total = entries
        .iter()
        .map(|entry| &entry.path)
        .collect::<std::collections::HashSet<_>>()
        .len();
    let all_included = total > 0 && entries.iter().all(|e| state.is_included(&e.path));
    let listed_paths: Vec<String> = entries.iter().map(|entry| entry.path.clone()).collect();
    let noun = if total == 1 { "file" } else { "files" };

    div()
        .w_full()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(ROW_GAP))
        .px(px(ROW_PADDING_X))
        .py(px(ROW_PADDING_Y))
        .border_b_1()
        .border_color(tokens.borders.subtle.to_gpui())
        .child(render_checkbox(
            tokens,
            all_included,
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                this.set_included_for(listed_paths.clone(), !all_included, cx);
            }),
        ))
        .child(
            caption_text(
                match state.status.as_ref().map_or(0, |s| s.omitted_entries) {
                    0 => format!("{total} changed {noun}"),
                    omitted => format!("{total} changed {noun} · {omitted} more not shown"),
                },
                tokens,
            )
            .flex_1()
            .min_w_0(),
        )
        .into_any_element()
}

fn render_file_list(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let tokens = state.tokens;
    let cols = columns(state);
    let entries = state.filtered_entries();
    let selected = state.selected_file.as_deref();
    let path_width =
        (cols.list - 2.0 * ROW_PADDING_X - CHECKBOX_SIZE - STATUS_MARK_WIDTH - 2.0 * ROW_GAP - 1.0)
            .max(0.0);

    let empty = entries.is_empty().then(|| {
        panel_empty_state(
            if state.is_loading {
                "Loading changes…"
            } else if !state.file_filter.is_empty() {
                "No changed files match the filter."
            } else {
                "No changes in this repository."
            },
            tokens,
        )
    });

    div()
        .flex_1()
        .min_h_0()
        .id("git-file-list")
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .children(empty)
        .children(entries.iter().map(|entry| {
            let path = entry.path.clone();
            let is_selected =
                selected == Some(path.as_str()) && state.selected_file_staged == entry.staged;
            let staged = entry.staged;
            // Ticked by default: the tick is the window's own selection of
            // what to commit, not the index (see `GitWindow::excluded_paths`).
            let included = state.is_included(&path);
            // The same one-letter marks as the task panels' Git view, in
            // `text.muted`; only a conflict spends colour, because it is a
            // problem rather than a kind of change.
            let (mark, mark_colour) = match entry.status {
                GitFileStatus::Added => ("A", tokens.text.muted),
                GitFileStatus::Modified => ("M", tokens.text.muted),
                GitFileStatus::Deleted => ("D", tokens.text.muted),
                GitFileStatus::Renamed => ("R", tokens.text.muted),
                GitFileStatus::Copied => ("C", tokens.text.muted),
                GitFileStatus::Untracked => ("?", tokens.text.muted),
                GitFileStatus::Conflicted => ("!", tokens.status.destructive),
            };
            let click_path = path.clone();
            let check_path = path.clone();
            let tooltip = SharedString::from(path.clone());

            panel_row_shell(tokens, is_selected)
                .id(SharedString::from(format!("file-{}-{}", &path, staged)))
                .cursor_pointer()
                .child(render_checkbox(
                    tokens,
                    included,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        this.toggle_included(&check_path, cx);
                    }),
                ))
                .child(
                    div()
                        .w(px(STATUS_MARK_WIDTH))
                        .flex_none()
                        .text_size(px(META_FONT_SIZE))
                        .text_color(mark_colour.to_gpui())
                        .child(mark),
                )
                .child(label(path_width, path))
                .tooltip(move |window, app| {
                    gpui_component::tooltip::Tooltip::new(tooltip.clone()).build(window, app)
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        this.select_file_side(&click_path, staged, cx);
                    }),
                )
                .into_any_element()
        }))
        .into_any_element()
}

/// The stage checkbox, as the task panels' Git view paints it.
fn render_checkbox(
    tokens: ThemeTokens,
    checked: bool,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .size(px(CHECKBOX_SIZE))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(INPUT_RADIUS))
        .border_1()
        .cursor_pointer()
        .when(checked, |box_| {
            box_.border_color(tokens.actions.primary.default.background.to_gpui())
                .bg(tokens.actions.primary.default.background.to_gpui())
                .child(icons::app_icon(
                    icons::CHECK,
                    11.0,
                    tokens.actions.primary.default.foreground.to_u32(),
                ))
        })
        .when(!checked, |box_| {
            box_.border_color(tokens.borders.strong.to_gpui())
        })
        .on_mouse_down(MouseButton::Left, on_click)
        .into_any_element()
}

// ── Commit form ─────────────────────────────────────────────────────────────

fn render_commit_form(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    use crate::ui::components::text_field::native_text_input;
    let tokens = state.tokens;
    let branch_name = state
        .status
        .as_ref()
        .and_then(|s| s.branch.as_deref())
        .unwrap_or("branch");
    // Ticked files, which is what the commit will contain.
    let staged_count = state.included_count();
    let busy = state.is_mutating || state.is_generating_message || state.is_loading;
    let native = state.commit_inputs.is_some();
    let can_commit = if native {
        !busy && staged_count > 0 && !state.commit_summary.trim().is_empty()
    } else {
        staged_count > 0 && !state.is_committing
    };
    let commit_label = if state.is_committing {
        "Committing…".to_string()
    } else if staged_count > 0 {
        format!(
            "Commit {staged_count} {} to {branch_name}",
            if staged_count == 1 { "file" } else { "files" }
        )
    } else {
        format!("Commit to {branch_name}")
    };
    let signed_in = state.github_token.is_some();
    // Signed out, the AI button is the way in: it starts the GitHub sign-in.
    let ai_enabled = if signed_in {
        !busy && staged_count > 0
    } else {
        state.login_state.is_none()
    };
    let ai_label = if state.is_generating_message {
        "Writing…"
    } else if signed_in {
        "Write with AI"
    } else {
        "Sign in for AI"
    };

    let form = div()
        .w_full()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(ROW_GAP))
        .p(px(REGION_PADDING))
        .border_t_1()
        .border_color(tokens.borders.subtle.to_gpui())
        .child(overlay_chrome::field_label("Commit", tokens));

    let form = if let Some((summary, description)) = &state.commit_inputs {
        // At the body size, like every other line in the window; the
        // component's own small size is a point larger.
        form.child(native_text_input(summary).text_size(px(ROW_FONT_SIZE)))
            .child(
                native_text_input(description)
                    .text_size(px(ROW_FONT_SIZE))
                    .h(px(64.0)),
            )
    } else {
        form.child(
            input_shell(
                matches!(state.active_field, Some(GitField::CommitSummary)),
                tokens,
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.focus(window);
                    this.active_field = Some(GitField::CommitSummary);
                    this.cursor = this.commit_summary.len();
                    cx.notify();
                }),
            )
            .child(input_value(&state.commit_summary, "Summary", tokens)),
        )
        .child(
            input_shell(
                matches!(state.active_field, Some(GitField::CommitDescription)),
                tokens,
            )
            .min_h(px(48.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.focus(window);
                    this.active_field = Some(GitField::CommitDescription);
                    this.cursor = this.commit_description.len();
                    cx.notify();
                }),
            )
            .child(if state.commit_description.is_empty() {
                div()
                    .text_color(tokens.text.muted.to_gpui())
                    .child("Description (optional)")
            } else {
                div()
                    .text_color(tokens.text.primary.to_gpui())
                    .child(state.commit_description.clone())
            }),
        )
    };

    form.child(
        panel_button_shell(tokens, can_commit)
            .id("git-create-commit")
            .w_full()
            .flex()
            .justify_center()
            .py(px(4.0))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(ROW_FONT_SIZE))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .when(can_commit, |button| {
                button.border_color(tokens.borders.strong.to_gpui())
            })
            .child(commit_label)
            .when(can_commit, |button| {
                button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.commit_action(cx)))
            }),
    )
    .child(
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(ROW_GAP))
            .child(
                panel_button_shell(tokens, ai_enabled)
                    .id("git-ai-commit-message")
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .whitespace_nowrap()
                    .child(icons::app_icon(
                        icons::SPARKLES,
                        12.0,
                        if ai_enabled {
                            tokens.text.secondary.to_u32()
                        } else {
                            tokens.text.disabled.to_u32()
                        },
                    ))
                    .child(ai_label)
                    .tooltip(move |window, app| {
                        gpui_component::tooltip::Tooltip::new(if signed_in {
                            "Summarize your staged changes with GitHub AI"
                        } else {
                            "Sign in to GitHub to write commit messages with AI"
                        })
                        .build(window, app)
                    })
                    .when(ai_enabled, |button| {
                        button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            if this.github_token.is_none() {
                                this.start_github_login(cx);
                            } else if !this.is_generating_message {
                                this.generate_commit_message(cx);
                            }
                        }))
                    }),
            )
            .child(
                caption_text(
                    if staged_count == 0 {
                        "Tick the files to include.".to_string()
                    } else {
                        format!("{staged_count} selected")
                    },
                    tokens,
                )
                .flex_1()
                .min_w(px(60.0)),
            ),
    )
    .children(state.github_username.as_ref().map(|name| {
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.0))
            .child(caption_text(format!("GitHub: {name}"), tokens))
            .child(
                div()
                    .id("git-sign-out")
                    .text_size(px(META_FONT_SIZE))
                    .text_color(tokens.text.secondary.to_gpui())
                    .cursor_pointer()
                    .hover(|style| style.text_color(tokens.text.primary.to_gpui()))
                    .child("Sign out")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.logout_github(cx);
                    })),
            )
    }))
    .into_any_element()
}

// ── Diff ────────────────────────────────────────────────────────────────────

fn render_diff_header(state: &GitWindow) -> AnyElement {
    let tokens = state.tokens;
    let cols = columns(state);
    let row = div()
        .w_full()
        .flex_none()
        .h(px(SUBHEADER_HEIGHT))
        .flex()
        .items_center()
        .gap(px(ROW_GAP))
        .px(px(ROW_PADDING_X))
        .bg(tokens.surfaces.raised.to_gpui())
        .border_b_1()
        .border_color(tokens.borders.subtle.to_gpui());
    match state.selected_file.as_deref() {
        None => row
            .child(caption_text("No file selected", tokens))
            .into_any_element(),
        Some(path) => {
            let side = if state.selected_file_staged {
                "Staged"
            } else {
                "Unstaged"
            };
            let width = fitted_width(
                path,
                ROW_FONT_SIZE,
                cols.diff
                    - 2.0 * ROW_PADDING_X
                    - ROW_GAP
                    - approx_text_width(side, META_FONT_SIZE)
                    - 1.0,
            );
            row.child(label(width, path.to_string()))
                .child(caption_text(side, tokens).flex_none())
                .into_any_element()
        }
    }
}

fn render_diff_panel(state: &GitWindow) -> AnyElement {
    let tokens = state.tokens;
    let Some(diff) = state.file_diff.as_ref() else {
        return div()
            .flex_1()
            .child(panel_empty_state(
                if state.is_loading {
                    "Loading…"
                } else if state.selected_file.is_some() {
                    "Loading the diff…"
                } else {
                    "Select a file to see its changes."
                },
                tokens,
            ))
            .into_any_element();
    };
    render_diff_rows("git-diff-panel", diff, tokens)
}

/// A patch through the task panels' diff painter, bounded so a huge file
/// cannot stall the window.
///
/// The painter keeps each line on one row; here the rows are laid out at the
/// width of the longest line and the region scrolls both ways, so a long line
/// in a narrow window can be scrolled to rather than being cut off.
fn render_diff_rows(id: &'static str, diff: &GitDiffResult, tokens: ThemeTokens) -> AnyElement {
    let (bounded, note) = bounded_diff(diff);
    let widest = bounded
        .hunks
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .map(|line| approx_text_width(&line.content, ROW_FONT_SIZE))
        .fold(0.0_f32, f32::max);
    let content_width = widest + DIFF_GUTTER_WIDTH + 2.0 * ROW_PADDING_X;
    div()
        .id(id)
        .flex_1()
        .min_h_0()
        .overflow_scroll()
        .bg(tokens.surfaces.sunken.to_gpui())
        .child(
            div()
                .w_full()
                .min_w(px(content_width))
                .flex()
                .flex_col()
                .children(crate::ui::task_cockpit::changes_panel::diff_rows(
                    &bounded, &tokens,
                ))
                .children(note.map(|note| panel_empty_state(note, tokens))),
        )
        .into_any_element()
}

fn bounded_diff(diff: &GitDiffResult) -> (GitDiffResult, Option<String>) {
    let total: usize = diff.hunks.iter().map(|hunk| hunk.lines.len()).sum();
    if diff.hunks.len() <= MAX_DIFF_HUNKS && total <= MAX_DIFF_LINES {
        return (diff.clone(), None);
    }
    let mut remaining = MAX_DIFF_LINES;
    let mut hunks = Vec::new();
    for hunk in diff.hunks.iter().take(MAX_DIFF_HUNKS) {
        if remaining == 0 {
            break;
        }
        let mut hunk = hunk.clone();
        hunk.lines.truncate(remaining);
        remaining -= hunk.lines.len();
        hunks.push(hunk);
    }
    let shown: usize = hunks.iter().map(|hunk| hunk.lines.len()).sum();
    (
        GitDiffResult {
            hunks,
            is_binary: diff.is_binary,
        },
        Some(format!(
            "Showing the first {shown} of {total} lines in this patch."
        )),
    )
}

// ── History view ────────────────────────────────────────────────────────────

fn render_history_view(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let tokens = state.tokens;
    let cols = columns(state);
    div()
        .flex_1()
        .flex()
        .min_h_0()
        .child(
            div()
                .w(px(cols.list))
                .flex_none()
                .flex()
                .flex_col()
                .border_r_1()
                .border_color(tokens.borders.subtle.to_gpui())
                .child(render_commit_list(state, cx)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .flex()
                .flex_col()
                .bg(tokens.surfaces.sunken.to_gpui())
                .child(render_commit_diff_panel(state)),
        )
        .into_any_element()
}

fn render_commit_list(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let tokens = state.tokens;
    if state.log_entries.is_empty() {
        return div()
            .flex_1()
            .child(panel_empty_state("Loading history…", tokens))
            .into_any_element();
    }
    let width = (columns(state).list - 2.0 * ROW_PADDING_X - 1.0).max(0.0);
    let unpushed = state
        .log_entries
        .iter()
        .filter(|entry| entry.unpushed)
        .count();

    div()
        .flex_1()
        .min_h_0()
        .id("git-commit-list")
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .children((unpushed > 0).then(|| {
            caption_text(
                format!(
                    "\u{2191} {unpushed} {} waiting to push",
                    if unpushed == 1 { "commit" } else { "commits" }
                ),
                tokens,
            )
            .px(px(ROW_PADDING_X))
            .py(px(ROW_PADDING_Y))
        }))
        .children(state.log_entries.iter().map(|entry| {
            let hash = entry.hash.clone();
            let row_state =
                OverlayRowState::selected_when(state.selected_commit.as_deref() == Some(&hash));
            let tooltip = commit_tooltip(entry);
            overlay_chrome::overlay_row(
                SharedString::from(format!("commit-{hash}")),
                row_state,
                tokens,
            )
            .children(commit_row_lines(entry, width, row_state, tokens))
            .tooltip(move |window, app| {
                gpui_component::tooltip::Tooltip::new(tooltip.clone())
                    .max_w(px(COMMIT_TOOLTIP_WIDTH))
                    .build(window, app)
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.select_commit(&hash, cx);
            }))
            .into_any_element()
        }))
        .child(div().flex().justify_center().p(px(REGION_PADDING)).child(
            text_button("git-load-more", "Load more", true, tokens).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.log_page += 1;
                    this.load_history(cx);
                },
            )),
        ))
        .into_any_element()
}

fn render_commit_diff_panel(state: &GitWindow) -> AnyElement {
    let tokens = state.tokens;
    let Some(diff) = state.commit_diff.as_ref() else {
        return div()
            .flex_1()
            .child(panel_empty_state(
                if state.selected_commit.is_some() {
                    "Loading the diff…"
                } else {
                    "Select a commit to see its changes."
                },
                tokens,
            ))
            .into_any_element();
    };
    render_diff_rows("git-commit-diff", diff, tokens)
}

// ── Branch menu ─────────────────────────────────────────────────────────────

pub fn render_branch_dropdown(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let tokens = state.tokens;
    let width = (columns(state).detail - 2.0 * ROW_PADDING_X).clamp(200.0, 320.0);
    let text_width = width - 2.0 * ROW_PADDING_X - ICON_SIZE - ROW_GAP - 2.0;
    // Rule 7: the menu hangs under the control that opened it, pulled left
    // only as far as it must to stay inside the window.
    let left = toolbar_layout(state)
        .branch_left
        .min(state.viewport_width - width - ROW_PADDING_X)
        .max(ROW_PADDING_X);
    let filter = state.branch_filter.to_lowercase();
    let mut filtered_branches: Vec<_> = state
        .branches
        .iter()
        .filter(|b| filter.is_empty() || b.name.to_lowercase().contains(&filter))
        .collect();
    // The current branch first, so the menu opens on where you are.
    filtered_branches.sort_by_key(|branch| !branch.is_current);
    let can_create = !state.new_branch_name.trim().is_empty();

    overlay_chrome::overlay_surface("git-branch-dropdown", tokens)
        .occlude()
        .absolute()
        .top(px(HEADER_HEIGHT + OVERLAY_ANCHOR_DROP))
        .left(px(left))
        .w(px(width))
        .max_h(px(OVERLAY_MAX_HEIGHT + 80.0))
        .overflow_y_scroll()
        .child(
            div().px(px(ROW_PADDING_X)).py(px(4.0)).child(
                input_shell(
                    matches!(state.active_field, Some(GitField::BranchFilter)),
                    tokens,
                )
                .child(input_value(
                    &state.branch_filter,
                    "Filter branches",
                    tokens,
                )),
            ),
        )
        .child(overlay_chrome::section_label("Branches", tokens))
        .children(
            filtered_branches
                .is_empty()
                .then(|| panel_empty_state("No branches match.", tokens)),
        )
        .children(filtered_branches.iter().enumerate().map(|(index, branch)| {
            let row_state = OverlayRowState::selected_when(branch.is_current);
            let click_name = branch.name.clone();
            overlay_chrome::overlay_row(("git-branch", index), row_state, tokens)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(ROW_GAP))
                        .child(
                            div()
                                .w(px(ICON_SIZE))
                                .flex_none()
                                .when(branch.is_current, |slot| {
                                    slot.child(icons::app_icon(
                                        icons::CHECK,
                                        12.0,
                                        tokens.text.secondary.to_u32(),
                                    ))
                                }),
                        )
                        .child(
                            label(text_width, branch.name.clone())
                                .text_size(px(ROW_FONT_SIZE))
                                .line_height(px(TITLE_LINE_HEIGHT))
                                .text_color(
                                    overlay_chrome::row_title_colour(row_state, tokens).to_gpui(),
                                ),
                        ),
                )
                .children(branch.upstream.as_ref().map(|upstream| {
                    ellipsised(text_width, upstream.clone())
                        .ml(px(ICON_SIZE + ROW_GAP))
                        .text_size(px(META_FONT_SIZE))
                        .line_height(px(META_LINE_HEIGHT))
                        .text_color(overlay_chrome::row_meta_colour(row_state, tokens).to_gpui())
                }))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.switch_branch_action(&click_name, cx);
                }))
                .into_any_element()
        }))
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(ROW_GAP))
                .mt(px(4.0))
                .px(px(ROW_PADDING_X))
                .pt(px(6.0))
                .pb(px(2.0))
                .border_t_1()
                .border_color(tokens.borders.subtle.to_gpui())
                .child(
                    input_shell(
                        matches!(state.active_field, Some(GitField::NewBranchName)),
                        tokens,
                    )
                    .flex_1()
                    .min_w_0()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            this.focus(window);
                            this.active_field = Some(GitField::NewBranchName);
                            this.cursor = this.new_branch_name.len();
                            cx.notify();
                        }),
                    )
                    .child(input_value(
                        &state.new_branch_name,
                        "New branch name",
                        tokens,
                    )),
                )
                .child(
                    text_button("git-create-branch", "Create", can_create, tokens).when(
                        can_create,
                        |button| {
                            button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.create_branch_action(cx);
                            }))
                        },
                    ),
                ),
        )
        .into_any_element()
}

// ── Repository menu (when the rail is folded or absent) ────────────────────

pub fn render_repo_dropdown(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let tokens = state.tokens;
    let width = (columns(state).detail - 2.0 * ROW_PADDING_X).clamp(200.0, 360.0);
    let text_width = width - 2.0 * ROW_PADDING_X;
    let idle = !state.is_mutating && !state.is_loading;
    overlay_chrome::overlay_surface("git-repo-dropdown", tokens)
        .occlude()
        .absolute()
        .top(px(HEADER_HEIGHT + OVERLAY_ANCHOR_DROP))
        .left(px(ROW_PADDING_X))
        .w(px(width))
        .max_h(px(OVERLAY_MAX_HEIGHT + 160.0))
        .overflow_y_scroll()
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(ROW_GAP))
                .px(px(ROW_PADDING_X))
                .pt(px(4.0))
                .pb(px(4.0))
                .child(
                    caption_text(
                        format!("{} repositories in your project folders", state.repos.len()),
                        tokens,
                    )
                    .flex_1()
                    .min_w_0(),
                )
                .when(state.is_native(), |row| {
                    let enabled = idle && !state.repos.is_empty();
                    row.child(
                        text_button("git-menu-fetch-all", "Fetch all", enabled, tokens).when(
                            enabled,
                            |button| {
                                button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.fetch_all_repositories(cx)
                                }))
                            },
                        ),
                    )
                    .child(
                        icon_button(
                            "git-menu-refresh",
                            icons::REFRESH_CW,
                            "Refresh local changes and last fetched sync status",
                            idle,
                            tokens,
                        )
                        .when(idle, |button| {
                            button.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.reload_repositories(cx);
                            }))
                        }),
                    )
                }),
        )
        .children(state.repos.iter().enumerate().map(|(index, repo)| {
            let row_state = OverlayRowState::selected_when(index == state.active_repo);
            let mut meta = repo.status_label();
            if !state.is_native() {
                meta = format!("{meta} · {}", repo.path);
            }
            let tooltip = repository_tooltip(repo);
            overlay_chrome::overlay_row(("git-repo", index), row_state, tokens)
                .children(repository_row_lines(
                    repo, meta, text_width, row_state, tokens,
                ))
                .tooltip(move |window, app| {
                    gpui_component::tooltip::Tooltip::new(tooltip.clone()).build(window, app)
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.show_repo_dropdown = false;
                    this.switch_repo(index, cx);
                }))
                .into_any_element()
        }))
        .into_any_element()
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn format_relative_date(iso_date: &str) -> String {
    // Simple relative date from ISO 8601
    // For now just show the date portion
    if let Some(date) = iso_date.split('T').next() {
        date.to_string()
    } else {
        iso_date.to_string()
    }
}
