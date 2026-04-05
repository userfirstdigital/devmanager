use super::{GitField, GitView, GitWindow};
use crate::git::git_service::{DiffLineKind, GitFileStatus};
use crate::icons;
use crate::theme;
use gpui::{
    div, prelude::*, px, rgb, AnyElement, App, ClipboardItem, Context, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window,
};

// ── Colors (GitHub Desktop palette adapted to dark theme) ───────────────────

const GIT_GREEN: u32 = 0x2ea043;
const GIT_GREEN_BG: u32 = 0x1b2b1e;
const GIT_RED: u32 = 0xf85149;
const GIT_RED_BG: u32 = 0x2d1b1e;
const GIT_ORANGE: u32 = 0xd29922;
const GIT_BLUE: u32 = 0x388bfd;
const GIT_GREY: u32 = 0x8b949e;
const TOOLBAR_BG: u32 = 0x161b22;
const TOOLBAR_BORDER: u32 = 0x30363d;
const TAB_ACTIVE_BORDER: u32 = 0x388bfd;
const FILE_SELECTED_BG: u32 = 0x1f2937;
const COMMIT_BUTTON_BG: u32 = 0x238636;
const COMMIT_BUTTON_HOVER: u32 = 0x2ea043;
const DIFF_HEADER_BG: u32 = 0x1c2128;
const HUNK_HEADER_BG: u32 = 0x1c2d4f;
const HUNK_HEADER_TEXT: u32 = 0x79c0ff;

// ── Main window render ─────────────────────────────────────────────────────

pub fn render_git_window(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(render_toolbar(state, cx))
        // Login banner or device code prompt
        .children(render_login_bar(state, cx))
        .child(render_tab_bar(state, cx))
        .children(
            state
                .operation_result
                .as_ref()
                .map(|(success, msg)| render_operation_banner(*success, msg)),
        )
        .child(match state.active_view {
            GitView::Changes => render_changes_view(state, cx),
            GitView::History => render_history_view(state, cx),
        })
        .into_any_element()
}

fn render_login_bar(state: &GitWindow, cx: &mut Context<GitWindow>) -> Option<AnyElement> {
    // If we're in the device code flow, show the code
    if let Some(ref login) = state.login_state {
        return Some(
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .gap(px(12.0))
                .px_3()
                .py(px(8.0))
                .bg(rgb(HUNK_HEADER_BG))
                .border_b_1()
                .border_color(rgb(TOOLBAR_BORDER))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("Enter this code on GitHub:"),
                )
                .child(
                    div()
                        .px_3()
                        .py(px(4.0))
                        .rounded_md()
                        .bg(rgb(TOOLBAR_BG))
                        .border_1()
                        .border_color(rgb(GIT_BLUE))
                        .text_size(px(18.0))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(rgb(0xffffff))
                        .child(SharedString::from(login.user_code.clone())),
                )
                .child({
                    let code = login.user_code.clone();
                    div()
                        .px_2()
                        .py(px(4.0))
                        .rounded_sm()
                        .bg(rgb(TOOLBAR_BORDER))
                        .text_size(px(11.0))
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::BUTTON_HOVER_BG)))
                        .child("Copy")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
                                this.operation_result =
                                    Some((true, "Code copied to clipboard".to_string()));
                                cx.notify();
                            }),
                        )
                })
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(theme::TEXT_DIM))
                        .child("Waiting for authorization..."),
                )
                .into_any_element(),
        );
    }

    // If not logged in, show login button
    if state.github_token.is_none() {
        return Some(
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .px_3()
                .py(px(6.0))
                .bg(rgb(HUNK_HEADER_BG))
                .border_b_1()
                .border_color(rgb(TOOLBAR_BORDER))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("Sign in to GitHub for AI commit messages and push/pull"),
                )
                .child(
                    div()
                        .px_3()
                        .py(px(4.0))
                        .rounded_sm()
                        .bg(rgb(COMMIT_BUTTON_BG))
                        .text_size(px(12.0))
                        .text_color(rgb(0xffffff))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(COMMIT_BUTTON_HOVER)))
                        .child("Login with GitHub")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                                this.start_github_login(cx);
                            }),
                        ),
                )
                .into_any_element(),
        );
    }

    None
}

// ── Toolbar (3 sections) ────────────────────────────────────────────────────

fn render_toolbar(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let branch_name = state
        .status
        .as_ref()
        .and_then(|s| s.branch.as_deref())
        .unwrap_or("(no branch)");

    let is_detached = state
        .status
        .as_ref()
        .map(|s| s.is_detached)
        .unwrap_or(false);

    let has_upstream = state
        .status
        .as_ref()
        .and_then(|s| s.upstream.as_ref())
        .is_some();

    let ahead = state.status.as_ref().map(|s| s.ahead).unwrap_or(0);
    let behind = state.status.as_ref().map(|s| s.behind).unwrap_or(0);
    let _has_remote = state
        .status
        .as_ref()
        .and_then(|s| s.upstream.as_ref())
        .is_some();

    // Sync button label and state
    let (sync_label, sync_detail) = if state.is_pushing {
        ("Pushing...".to_string(), String::new())
    } else if state.is_pulling {
        ("Pulling...".to_string(), String::new())
    } else if state.is_fetching {
        ("Fetching...".to_string(), String::new())
    } else if !has_upstream {
        ("Publish branch".to_string(), String::new())
    } else if ahead > 0 && behind > 0 {
        (
            format!("Push origin"),
            format!("\u{2191}{} \u{2193}{}", ahead, behind),
        )
    } else if ahead > 0 {
        (format!("Push origin"), format!("\u{2191}{}", ahead))
    } else if behind > 0 {
        (format!("Pull origin"), format!("\u{2193}{}", behind))
    } else {
        let detail = state
            .last_fetch_at
            .map(|t| {
                let secs = t.elapsed().as_secs();
                if secs < 60 {
                    "Last fetched just now".to_string()
                } else {
                    format!("Last fetched {}m ago", secs / 60)
                }
            })
            .unwrap_or_default();
        ("Fetch origin".to_string(), detail)
    };

    div()
        .w_full()
        .flex()
        .bg(rgb(TOOLBAR_BG))
        .border_b_1()
        .border_color(rgb(TOOLBAR_BORDER))
        // Left: Repository
        .child({
            let has_multiple = state.repos.len() > 1;
            div()
                .flex_1()
                .flex()
                .flex_col()
                .px_3()
                .py(px(8.0))
                .border_r_1()
                .border_color(rgb(TOOLBAR_BORDER))
                .when(has_multiple, |d| {
                    d.cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                                this.show_repo_dropdown = !this.show_repo_dropdown;
                                if this.show_repo_dropdown {
                                    this.refresh_all_repo_statuses(cx);
                                }
                                cx.notify();
                            }),
                        )
                })
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(GIT_GREY))
                        .child("Current repository"),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .font_weight(gpui::FontWeight::BOLD)
                                .child(SharedString::from(state.repo_label().to_string())),
                        )
                        .when(has_multiple, |d| {
                            d.child(icons::app_icon(
                                icons::CHEVRON_DOWN,
                                12.0,
                                theme::TEXT_MUTED,
                            ))
                        }),
                )
        })
        // Center: Branch
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .px_3()
                .py(px(8.0))
                .border_r_1()
                .border_color(rgb(TOOLBAR_BORDER))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                        this.show_branch_dropdown = !this.show_branch_dropdown;
                        if this.show_branch_dropdown {
                            this.load_branches(cx);
                            this.active_field = Some(GitField::BranchFilter);
                            this.cursor = 0;
                        }
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(GIT_GREY))
                        .child("Current branch"),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(icons::app_icon(
                            icons::GIT_BRANCH,
                            14.0,
                            theme::TEXT_PRIMARY,
                        ))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .font_weight(gpui::FontWeight::BOLD)
                                .child(SharedString::from(if is_detached {
                                    format!("(detached @ {})", branch_name)
                                } else {
                                    branch_name.to_string()
                                })),
                        )
                        .child(icons::app_icon(
                            icons::CHEVRON_DOWN,
                            12.0,
                            theme::TEXT_MUTED,
                        )),
                ),
        )
        // Right: Sync button
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .px_3()
                .py(px(8.0))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
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
                    }),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(icons::app_icon(
                            icons::REFRESH_CW,
                            14.0,
                            theme::TEXT_PRIMARY,
                        ))
                        .child(
                            div()
                                .text_size(px(13.0))
                                .font_weight(gpui::FontWeight::BOLD)
                                .child(SharedString::from(sync_label)),
                        )
                        .children(if !sync_detail.is_empty() {
                            Some(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(rgb(GIT_BLUE))
                                    .child(SharedString::from(sync_detail.clone())),
                            )
                        } else {
                            None
                        }),
                )
                .children(if sync_detail.is_empty() {
                    None
                } else {
                    None::<gpui::Div>
                }),
        )
        // Far right: user icon
        .children(state.github_username.as_ref().map(|username| {
            div()
                .flex_none()
                .flex()
                .flex_col()
                .items_center()
                .px_3()
                .py(px(4.0))
                .border_l_1()
                .border_color(rgb(TOOLBAR_BORDER))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(
                            div()
                                .w(px(8.0))
                                .h(px(8.0))
                                .rounded_full()
                                .bg(rgb(GIT_GREEN)),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(SharedString::from(username.clone())),
                        ),
                )
                .child(
                    div()
                        .text_size(px(9.0))
                        .text_color(rgb(theme::TEXT_DIM))
                        .cursor_pointer()
                        .hover(|s| s.text_color(rgb(GIT_RED)))
                        .child("logout")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                                this.logout_github(cx);
                            }),
                        ),
                )
                .into_any_element()
        }))
        .into_any_element()
}

// ── Tab bar ─────────────────────────────────────────────────────────────────

fn render_tab_bar(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let file_count = state.status.as_ref().map(|s| s.entries.len()).unwrap_or(0);

    div()
        .w_full()
        .flex()
        .bg(rgb(TOOLBAR_BG))
        .border_b_1()
        .border_color(rgb(TOOLBAR_BORDER))
        .child(render_tab(
            &format!("Changes ({})", file_count),
            state.active_view == GitView::Changes,
            cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                this.active_view = GitView::Changes;
                cx.notify();
            }),
        ))
        .child(render_tab(
            "History",
            state.active_view == GitView::History,
            cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                this.active_view = GitView::History;
                if this.log_entries.is_empty() {
                    this.load_history(cx);
                }
                cx.notify();
            }),
        ))
        .into_any_element()
}

fn render_tab(
    label: &str,
    active: bool,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .px_3()
        .py(px(8.0))
        .text_size(px(12.0))
        .cursor_pointer()
        .text_color(if active {
            rgb(theme::TEXT_PRIMARY)
        } else {
            rgb(theme::TEXT_MUTED)
        })
        .when(active, |d| {
            d.border_b_2().border_color(rgb(TAB_ACTIVE_BORDER))
        })
        .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
        .child(SharedString::from(label.to_string()))
        .on_mouse_down(MouseButton::Left, move |ev, window, app| {
            on_click(ev, window, app)
        })
        .into_any_element()
}

// ── Operation banner ────────────────────────────────────────────────────────

fn render_operation_banner(success: bool, msg: &str) -> AnyElement {
    div()
        .w_full()
        .px_3()
        .py(px(6.0))
        .bg(rgb(if success { theme::SUCCESS_BG } else { 0x2d1b1e }))
        .text_size(px(12.0))
        .text_color(rgb(if success {
            theme::SUCCESS_TEXT
        } else {
            GIT_RED
        }))
        .child(SharedString::from(msg.to_string()))
        .into_any_element()
}

// ── Changes view ────────────────────────────────────────────────────────────

fn render_changes_view(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .min_h_0()
        // Left: file list + commit form
        .child(
            div()
                .w(px(340.0))
                .flex_none()
                .flex()
                .flex_col()
                .border_r_1()
                .border_color(rgb(TOOLBAR_BORDER))
                .child(render_file_filter(state, cx))
                .child(render_file_list_header(state, cx))
                .child(render_file_list(state, cx))
                .child(render_commit_form(state, cx)),
        )
        // Right: diff preview
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .min_w_0()
                .child(render_diff_header(state))
                .child(render_diff_panel(state)),
        )
        .into_any_element()
}

// ── File filter ─────────────────────────────────────────────────────────────

fn render_file_filter(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    div()
        .w_full()
        .px_2()
        .py(px(4.0))
        .border_b_1()
        .border_color(rgb(TOOLBAR_BORDER))
        .child(
            div()
                .w_full()
                .px_2()
                .py(px(3.0))
                .rounded_sm()
                .bg(rgb(theme::EDITOR_FIELD_BG))
                .border_1()
                .border_color(rgb(TOOLBAR_BORDER))
                .text_size(px(12.0))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                        this.active_field = Some(GitField::FileFilter);
                        this.cursor = this.file_filter.len();
                        cx.notify();
                    }),
                )
                .child(if state.file_filter.is_empty() {
                    div()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child("Filter")
                        .into_any_element()
                } else {
                    div()
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(SharedString::from(state.file_filter.clone()))
                        .into_any_element()
                }),
        )
        .into_any_element()
}

// ── File list header ────────────────────────────────────────────────────────

fn render_file_list_header(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let entries = state.filtered_entries();
    let total = entries.len();
    let all_staged = total > 0 && entries.iter().all(|e| e.staged);

    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(6.0))
        .px_2()
        .py(px(4.0))
        .border_b_1()
        .border_color(rgb(TOOLBAR_BORDER))
        .child(render_checkbox(
            all_staged,
            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                if all_staged {
                    this.unstage_all(cx);
                } else {
                    this.stage_all(cx);
                }
            }),
        ))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(format!("{} changed files", total))),
        )
        .into_any_element()
}

// ── File list ───────────────────────────────────────────────────────────────

fn render_file_list(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let entries = state.filtered_entries();
    let selected = state.selected_file.as_deref();

    div()
        .flex_1()
        .id("git-file-list")
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .children(entries.iter().map(|entry| {
            let path = entry.path.clone();
            let is_selected = selected == Some(path.as_str());
            let staged = entry.staged;

            let (status_label, status_color) = match entry.status {
                GitFileStatus::Added => ("+", GIT_GREEN),
                GitFileStatus::Modified => ("\u{25CF}", GIT_ORANGE), // ●
                GitFileStatus::Deleted => ("\u{2212}", GIT_RED),     // −
                GitFileStatus::Renamed => ("R", GIT_BLUE),
                GitFileStatus::Copied => ("C", GIT_BLUE),
                GitFileStatus::Untracked => ("?", GIT_GREY),
                GitFileStatus::Conflicted => ("!", GIT_RED),
            };

            let click_path = path.clone();
            let check_path = path.clone();

            div()
                .id(SharedString::from(format!("file-{}", &path)))
                .w_full()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px_2()
                .py(px(3.0))
                .cursor_pointer()
                .when(is_selected, |d| d.bg(rgb(FILE_SELECTED_BG)))
                .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
                .child(render_checkbox(
                    staged,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        if staged {
                            this.unstage_file(&check_path, cx);
                        } else {
                            this.stage_file(&check_path, cx);
                        }
                    }),
                ))
                .child(
                    div()
                        .flex_1()
                        .text_size(px(12.0))
                        .overflow_x_hidden()
                        .child(SharedString::from(path.clone())),
                )
                .child(
                    div()
                        .text_size(px(14.0))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(rgb(status_color))
                        .child(status_label),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        this.select_file(&click_path, cx);
                    }),
                )
                .into_any_element()
        }))
        .into_any_element()
}

// ── Checkbox ────────────────────────────────────────────────────────────────

fn render_checkbox(
    checked: bool,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .w(px(16.0))
        .h(px(16.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .border_1()
        .border_color(rgb(if checked { GIT_BLUE } else { TOOLBAR_BORDER }))
        .bg(rgb(if checked { GIT_BLUE } else { 0x00000000 }))
        .cursor_pointer()
        .child(if checked {
            div()
                .text_size(px(11.0))
                .text_color(rgb(0xffffff))
                .child("\u{2713}") // ✓
                .into_any_element()
        } else {
            div().into_any_element()
        })
        .on_mouse_down(MouseButton::Left, on_click)
        .into_any_element()
}

// ── Commit form ─────────────────────────────────────────────────────────────

fn render_commit_form(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let branch_name = state
        .status
        .as_ref()
        .and_then(|s| s.branch.as_deref())
        .unwrap_or("branch");

    let staged_count = state.staged_count();

    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .p_2()
        .border_t_1()
        .border_color(rgb(TOOLBAR_BORDER))
        // Summary field
        .child(
            div()
                .w_full()
                .px_2()
                .py(px(6.0))
                .rounded_sm()
                .bg(rgb(theme::EDITOR_FIELD_BG))
                .border_1()
                .border_color(rgb(
                    if matches!(state.active_field, Some(GitField::CommitSummary)) {
                        GIT_BLUE
                    } else {
                        TOOLBAR_BORDER
                    },
                ))
                .text_size(px(12.0))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                        this.active_field = Some(GitField::CommitSummary);
                        this.cursor = this.commit_summary.len();
                        cx.notify();
                    }),
                )
                .child(if state.commit_summary.is_empty() {
                    div()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child("Summary (required)")
                        .into_any_element()
                } else {
                    div()
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(SharedString::from(state.commit_summary.clone()))
                        .into_any_element()
                }),
        )
        // Description field
        .child(
            div()
                .w_full()
                .px_2()
                .py(px(6.0))
                .rounded_sm()
                .bg(rgb(theme::EDITOR_FIELD_BG))
                .border_1()
                .border_color(rgb(
                    if matches!(state.active_field, Some(GitField::CommitDescription)) {
                        GIT_BLUE
                    } else {
                        TOOLBAR_BORDER
                    },
                ))
                .text_size(px(12.0))
                .min_h(px(40.0))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                        this.active_field = Some(GitField::CommitDescription);
                        this.cursor = this.commit_description.len();
                        cx.notify();
                    }),
                )
                .child(if state.commit_description.is_empty() {
                    div()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child("Description")
                        .into_any_element()
                } else {
                    div()
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(SharedString::from(state.commit_description.clone()))
                        .into_any_element()
                }),
        )
        // AI generate button
        .children(state.github_token.as_ref().map(|_| {
            let is_generating = state.is_generating_message;
            div()
                .w_full()
                .px_2()
                .py(px(4.0))
                .rounded_sm()
                .bg(rgb(theme::EDITOR_CARD_BG))
                .border_1()
                .border_color(rgb(TOOLBAR_BORDER))
                .text_size(px(11.0))
                .text_color(rgb(if is_generating {
                    theme::TEXT_DIM
                } else {
                    GIT_BLUE
                }))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
                .flex()
                .items_center()
                .justify_center()
                .gap(px(4.0))
                .child(icons::app_icon(
                    icons::SPARKLES,
                    12.0,
                    if is_generating {
                        theme::TEXT_DIM
                    } else {
                        0x388bfd
                    },
                ))
                .child(if is_generating {
                    "Generating..."
                } else {
                    "Generate commit message with AI"
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                        if !this.is_generating_message {
                            this.generate_commit_message(cx);
                        }
                    }),
                )
                .into_any_element()
        }))
        // Commit button
        .child(
            div()
                .w_full()
                .px_2()
                .py(px(6.0))
                .rounded_sm()
                .bg(rgb(if staged_count > 0 {
                    COMMIT_BUTTON_BG
                } else {
                    theme::BORDER_PRIMARY
                }))
                .text_size(px(13.0))
                .text_color(rgb(if staged_count > 0 {
                    0xffffff
                } else {
                    theme::TEXT_DIM
                }))
                .cursor(if staged_count > 0 {
                    gpui::CursorStyle::PointingHand
                } else {
                    gpui::CursorStyle::default()
                })
                .when(staged_count > 0, |d| {
                    d.hover(|s| s.bg(rgb(COMMIT_BUTTON_HOVER)))
                })
                .flex()
                .justify_center()
                .font_weight(gpui::FontWeight::BOLD)
                .child(SharedString::from(if staged_count > 0 {
                    format!("Commit {} files to {}", staged_count, branch_name)
                } else {
                    format!("Commit to {}", branch_name)
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                        this.commit_action(cx);
                    }),
                ),
        )
        .into_any_element()
}

// ── Diff header ─────────────────────────────────────────────────────────────

fn render_diff_header(state: &GitWindow) -> AnyElement {
    let file_name = state.selected_file.as_deref().unwrap_or("No file selected");

    div()
        .w_full()
        .px_3()
        .py(px(6.0))
        .bg(rgb(DIFF_HEADER_BG))
        .border_b_1()
        .border_color(rgb(TOOLBAR_BORDER))
        .text_size(px(12.0))
        .text_color(rgb(theme::TEXT_MUTED))
        .child(SharedString::from(file_name.to_string()))
        .into_any_element()
}

// ── Diff panel ──────────────────────────────────────────────────────────────

fn render_diff_panel(state: &GitWindow) -> AnyElement {
    let Some(ref diff) = state.file_diff else {
        return div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(13.0))
            .text_color(rgb(theme::TEXT_DIM))
            .child(if state.is_loading {
                "Loading..."
            } else if state.selected_file.is_some() {
                "Loading diff..."
            } else {
                "Select a file to view diff"
            })
            .into_any_element();
    };

    if diff.is_binary {
        return div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(13.0))
            .text_color(rgb(theme::TEXT_DIM))
            .child("Binary file changed")
            .into_any_element();
    }

    let total_lines: usize = diff.hunks.iter().map(|h| h.lines.len()).sum();
    let truncated = diff.hunks.len() > 50 || total_lines > MAX_DIFF_LINES;
    div()
        .flex_1()
        .id("git-diff-panel")
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .children(
            diff.hunks
                .iter()
                .take(50)
                .map(|hunk| render_diff_hunk(hunk)),
        )
        .children(truncated.then(|| {
            div()
                .w_full()
                .px_2()
                .py(px(6.0))
                .text_size(px(11.0))
                .text_color(rgb(theme::TEXT_DIM))
                .child(SharedString::from(format!(
                    "Diff truncated ({} hunks, {} lines total)",
                    diff.hunks.len(),
                    total_lines
                )))
                .into_any_element()
        }))
        .into_any_element()
}

const MAX_DIFF_LINES: usize = 500;

fn render_diff_hunk(hunk: &crate::git::git_service::GitDiffHunk) -> AnyElement {
    // Batch lines into three groups by type to minimize element count.
    // Each group is rendered as a single pre-formatted text block per contiguous run.
    let mut children: Vec<AnyElement> = Vec::new();

    // Hunk header
    children.push(
        div()
            .w_full()
            .px_2()
            .py(px(2.0))
            .bg(rgb(HUNK_HEADER_BG))
            .text_size(px(11.0))
            .text_color(rgb(HUNK_HEADER_TEXT))
            .child(SharedString::from(hunk.header.clone()))
            .into_any_element(),
    );

    // Group consecutive lines of the same kind into batches
    let lines = &hunk.lines;
    let limit = lines.len().min(MAX_DIFF_LINES);
    let mut i = 0;
    while i < limit {
        let kind = &lines[i].kind;
        let batch_start = i;
        while i < limit && &lines[i].kind == kind {
            i += 1;
        }
        let batch = &lines[batch_start..i];

        let (bg, text_color) = match kind {
            DiffLineKind::Add => (GIT_GREEN_BG, GIT_GREEN),
            DiffLineKind::Delete => (GIT_RED_BG, GIT_RED),
            DiffLineKind::Context => (0x00000000, theme::TEXT_MUTED),
            DiffLineKind::HunkHeader => (HUNK_HEADER_BG, HUNK_HEADER_TEXT),
        };

        // Build a single pre-formatted string for this batch
        let mut text = String::new();
        for line in batch {
            let old_no = line
                .old_lineno
                .map(|n| format!("{:>4}", n))
                .unwrap_or_else(|| "    ".to_string());
            let new_no = line
                .new_lineno
                .map(|n| format!("{:>4}", n))
                .unwrap_or_else(|| "    ".to_string());
            let prefix = match line.kind {
                DiffLineKind::Add => "+",
                DiffLineKind::Delete => "-",
                DiffLineKind::Context => " ",
                DiffLineKind::HunkHeader => "@@",
            };
            text.push_str(&old_no);
            text.push(' ');
            text.push_str(&new_no);
            text.push(' ');
            text.push_str(prefix);
            text.push(' ');
            text.push_str(&line.content);
            text.push('\n');
        }

        children.push(
            div()
                .w_full()
                .px(px(4.0))
                .bg(rgb(bg))
                .text_size(px(12.0))
                .text_color(rgb(text_color))
                .child(SharedString::from(text))
                .into_any_element(),
        );
    }

    if lines.len() > MAX_DIFF_LINES {
        children.push(
            div()
                .w_full()
                .px_2()
                .py(px(4.0))
                .text_size(px(11.0))
                .text_color(rgb(theme::TEXT_DIM))
                .child(SharedString::from(format!(
                    "... {} more lines not shown",
                    lines.len() - MAX_DIFF_LINES
                )))
                .into_any_element(),
        );
    }

    div()
        .w_full()
        .flex()
        .flex_col()
        .children(children)
        .into_any_element()
}

// ── History view ────────────────────────────────────────────────────────────

fn render_history_view(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .min_h_0()
        // Left: commit list
        .child(
            div()
                .w(px(340.0))
                .flex_none()
                .flex()
                .flex_col()
                .border_r_1()
                .border_color(rgb(TOOLBAR_BORDER))
                .child(render_commit_list(state, cx)),
        )
        // Right: commit diff
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .min_w_0()
                .child(render_commit_diff_panel(state)),
        )
        .into_any_element()
}

fn render_commit_list(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    if state.log_entries.is_empty() {
        return div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(13.0))
            .text_color(rgb(theme::TEXT_DIM))
            .child("Loading history...")
            .into_any_element();
    }

    div()
        .flex_1()
        .id("git-commit-list")
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .children(state.log_entries.iter().map(|entry| {
            let hash = entry.hash.clone();
            let is_selected = state.selected_commit.as_deref() == Some(&hash);
            let click_hash = hash.clone();

            div()
                .id(SharedString::from(format!("commit-{}", &hash)))
                .w_full()
                .flex()
                .flex_col()
                .px_2()
                .py(px(6.0))
                .cursor_pointer()
                .when(is_selected, |d| d.bg(rgb(FILE_SELECTED_BG)))
                .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
                .border_b_1()
                .border_color(rgb(TOOLBAR_BORDER))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        this.select_commit(&click_hash, cx);
                    }),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(SharedString::from(entry.subject.clone())),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .text_size(px(11.0))
                        .text_color(rgb(theme::TEXT_DIM))
                        .child(SharedString::from(entry.hash.clone()))
                        .child(SharedString::from(entry.author_name.clone()))
                        .child(SharedString::from(format_relative_date(&entry.date))),
                )
                .into_any_element()
        }))
        // Load more button
        .child(
            div()
                .w_full()
                .px_2()
                .py(px(8.0))
                .flex()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
                .text_size(px(12.0))
                .text_color(rgb(GIT_BLUE))
                .child("Load more...")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                        this.log_page += 1;
                        this.load_history(cx);
                    }),
                ),
        )
        .into_any_element()
}

fn render_commit_diff_panel(state: &GitWindow) -> AnyElement {
    let Some(ref diff) = state.commit_diff else {
        return div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(13.0))
            .text_color(rgb(theme::TEXT_DIM))
            .child(if state.selected_commit.is_some() {
                "Loading diff..."
            } else {
                "Select a commit to view diff"
            })
            .into_any_element();
    };

    if diff.is_binary {
        return div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_color(rgb(theme::TEXT_DIM))
            .child("Binary file changed")
            .into_any_element();
    }

    let total_lines: usize = diff.hunks.iter().map(|h| h.lines.len()).sum();
    let truncated = diff.hunks.len() > 50 || total_lines > MAX_DIFF_LINES;
    div()
        .flex_1()
        .id("git-commit-diff")
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .children(
            diff.hunks
                .iter()
                .take(50)
                .map(|hunk| render_diff_hunk(hunk)),
        )
        .children(truncated.then(|| {
            div()
                .w_full()
                .px_2()
                .py(px(6.0))
                .text_size(px(11.0))
                .text_color(rgb(theme::TEXT_DIM))
                .child(SharedString::from(format!(
                    "Diff truncated ({} hunks, {} lines total)",
                    diff.hunks.len(),
                    total_lines
                )))
                .into_any_element()
        }))
        .into_any_element()
}

// ── Branch dropdown ─────────────────────────────────────────────────────────

pub fn render_branch_dropdown(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    let filter = state.branch_filter.to_lowercase();
    let filtered_branches: Vec<_> = state
        .branches
        .iter()
        .filter(|b| filter.is_empty() || b.name.to_lowercase().contains(&filter))
        .collect();

    div()
        .id("git-branch-dropdown")
        .occlude()
        .absolute()
        .top(px(70.0))
        .left(px(200.0))
        .w(px(300.0))
        .max_h(px(400.0))
        .overflow_y_scroll()
        .bg(rgb(TOOLBAR_BG))
        .border_1()
        .border_color(rgb(TOOLBAR_BORDER))
        .rounded_md()
        .shadow_lg()
        .flex()
        .flex_col()
        // Filter input
        .child(
            div()
                .px_2()
                .py(px(6.0))
                .border_b_1()
                .border_color(rgb(TOOLBAR_BORDER))
                .child(
                    div()
                        .w_full()
                        .px_2()
                        .py(px(3.0))
                        .rounded_sm()
                        .bg(rgb(theme::EDITOR_FIELD_BG))
                        .border_1()
                        .border_color(rgb(TOOLBAR_BORDER))
                        .text_size(px(12.0))
                        .child(if state.branch_filter.is_empty() {
                            div()
                                .text_color(rgb(theme::TEXT_DIM))
                                .child("Filter branches")
                                .into_any_element()
                        } else {
                            div()
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(SharedString::from(state.branch_filter.clone()))
                                .into_any_element()
                        }),
                ),
        )
        // Branch list
        .children(filtered_branches.iter().map(|branch| {
            let name = branch.name.clone();
            let is_current = branch.is_current;
            let click_name = name.clone();

            div()
                .id(SharedString::from(format!("branch-{}", &name)))
                .w_full()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px_2()
                .py(px(4.0))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
                .when(is_current, |d| d.bg(rgb(FILE_SELECTED_BG)))
                .child(
                    div()
                        .w(px(16.0))
                        .text_size(px(11.0))
                        .text_color(rgb(GIT_GREEN))
                        .child(if is_current { "\u{2713}" } else { "" }),
                )
                .child(
                    div()
                        .flex_1()
                        .text_size(px(12.0))
                        .child(SharedString::from(name.clone())),
                )
                .children(branch.upstream.as_ref().map(|u| {
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(theme::TEXT_DIM))
                        .child(SharedString::from(u.clone()))
                        .into_any_element()
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        this.switch_branch_action(&click_name, cx);
                    }),
                )
                .into_any_element()
        }))
        // New branch input
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px_2()
                .py(px(6.0))
                .border_t_1()
                .border_color(rgb(TOOLBAR_BORDER))
                .child(
                    div()
                        .flex_1()
                        .px_2()
                        .py(px(3.0))
                        .rounded_sm()
                        .bg(rgb(theme::EDITOR_FIELD_BG))
                        .border_1()
                        .border_color(rgb(TOOLBAR_BORDER))
                        .text_size(px(12.0))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                                this.active_field = Some(GitField::NewBranchName);
                                this.cursor = this.new_branch_name.len();
                                cx.notify();
                            }),
                        )
                        .child(if state.new_branch_name.is_empty() {
                            div()
                                .text_color(rgb(theme::TEXT_DIM))
                                .child("New branch name")
                                .into_any_element()
                        } else {
                            div()
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(SharedString::from(state.new_branch_name.clone()))
                                .into_any_element()
                        }),
                )
                .child(
                    div()
                        .px_2()
                        .py(px(3.0))
                        .rounded_sm()
                        .bg(rgb(COMMIT_BUTTON_BG))
                        .text_size(px(11.0))
                        .text_color(rgb(0xffffff))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(COMMIT_BUTTON_HOVER)))
                        .child("Create")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                                this.create_branch_action(cx);
                            }),
                        ),
                ),
        )
        .into_any_element()
}

// ── Repo dropdown ───────────────────────────────────────────────────────────

pub fn render_repo_dropdown(state: &GitWindow, cx: &mut Context<GitWindow>) -> AnyElement {
    div()
        .id("git-repo-dropdown")
        .occlude()
        .absolute()
        .top(px(50.0))
        .left(px(8.0))
        .w(px(320.0))
        .max_h(px(400.0))
        .overflow_y_scroll()
        .bg(rgb(TOOLBAR_BG))
        .border_1()
        .border_color(rgb(TOOLBAR_BORDER))
        .rounded_md()
        .shadow_lg()
        .flex()
        .flex_col()
        .children(state.repos.iter().enumerate().map(|(i, repo)| {
            let is_active = i == state.active_repo;
            let has_changes = repo.has_changes;
            let behind = repo.behind;
            div()
                .id(SharedString::from(format!("repo-{}", i)))
                .w_full()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px_2()
                .py(px(6.0))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
                .when(is_active, |d| d.bg(rgb(FILE_SELECTED_BG)))
                .border_b_1()
                .border_color(rgb(TOOLBAR_BORDER))
                .child(
                    div()
                        .w(px(16.0))
                        .text_size(px(11.0))
                        .text_color(rgb(GIT_GREEN))
                        .child(if is_active { "\u{2713}" } else { "" }),
                )
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(12.0))
                                .font_weight(gpui::FontWeight::BOLD)
                                .child(SharedString::from(repo.label.clone())),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(rgb(theme::TEXT_DIM))
                                .child(SharedString::from(repo.path.clone())),
                        ),
                )
                // Status indicators (right side)
                .children((behind > 0).then(|| {
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(GIT_BLUE))
                        .child(SharedString::from(format!("\u{2193}{}", behind)))
                        .into_any_element()
                }))
                .children(has_changes.then(|| {
                    div()
                        .w(px(10.0))
                        .h(px(10.0))
                        .rounded_full()
                        .bg(rgb(GIT_BLUE))
                        .into_any_element()
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        this.switch_repo(i, cx);
                    }),
                )
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
