use crate::models::SessionTab;
use crate::state::{AppState, RuntimeState, SessionRuntimeState, SessionStatus};
use crate::{icons, theme};
use gpui::{
    anchored, deferred, div, px, rgb, AnyElement, App, Corner, Div, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, SharedString, Styled, Window,
};
use std::collections::HashMap;

const SIDEBAR_WIDTH_PX: f32 = 220.0;
const SIDEBAR_COLLAPSED_WIDTH_PX: f32 = 40.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarContextMenu {
    Project {
        project_id: String,
    },
    Folder {
        project_id: String,
        folder_id: String,
    },
    SingleCommandFolder {
        project_id: String,
        folder_id: String,
        command_id: String,
    },
    Command {
        command_id: String,
    },
    Ssh {
        connection_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerIndicatorState {
    Stopped,
    Unready,
    Ready,
    Stopping,
    Crashed,
    Exited,
    Failed,
}

pub struct SidebarActions<'a> {
    pub mutations_allowed: bool,
    pub on_open_settings: &'a dyn Fn() -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_toggle_sidebar: &'a dyn Fn() -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_stop_all_servers: &'a dyn Fn() -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_add_project: &'a dyn Fn() -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_edit_project: &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_open_project_notes:
        &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_delete_project:
        &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_add_folder: &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_edit_folder:
        &'a dyn Fn(String, String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_delete_folder:
        &'a dyn Fn(String, String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_add_command:
        &'a dyn Fn(String, String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_edit_command: &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_delete_command:
        &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_add_ssh: &'a dyn Fn() -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_edit_ssh: &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_delete_ssh: &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_open_ssh_tab: &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_connect_ssh: &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_disconnect_ssh:
        &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_restart_ssh: &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_start_server: &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_stop_server: &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_restart_server:
        &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_select_server_tab:
        &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_launch_claude: &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_launch_codex: &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_select_ai_tab: &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_restart_ai_tab:
        &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_close_ai_tab: &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_toggle_project_collapse:
        &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_move_project_up:
        &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_move_project_down:
        &'a dyn Fn(String) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_toggle_context_menu:
        &'a dyn Fn(SidebarContextMenu) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub on_dismiss_context_menu:
        &'a dyn Fn() -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    pub open_context_menu: &'a Option<SidebarContextMenu>,
    pub on_open_git: &'a dyn Fn() -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
}

pub fn sidebar_width_px(collapsed: bool) -> f32 {
    if collapsed {
        SIDEBAR_COLLAPSED_WIDTH_PX
    } else {
        SIDEBAR_WIDTH_PX
    }
}

pub fn render_sidebar(
    state: &AppState,
    runtime: &RuntimeState,
    server_indicators: &HashMap<String, ServerIndicatorState>,
    actions: SidebarActions<'_>,
) -> impl IntoElement {
    if state.sidebar_collapsed {
        render_collapsed_sidebar(actions).into_any_element()
    } else {
        render_expanded_sidebar(state, runtime, server_indicators, actions).into_any_element()
    }
}

fn render_collapsed_sidebar(actions: SidebarActions<'_>) -> AnyElement {
    div()
        .w(px(SIDEBAR_COLLAPSED_WIDTH_PX))
        .h_full()
        .flex_none()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(4.0))
        .py(px(6.0))
        .bg(rgb(theme::SIDEBAR_BG))
        .border_r_1()
        .border_color(rgb(theme::BORDER_PRIMARY))
        .child(icon_button(
            icons::CHEVRON_RIGHT,
            (actions.on_toggle_sidebar)(),
        ))
        .children(
            actions
                .mutations_allowed
                .then(|| icon_button(icons::PLUS, (actions.on_add_project)()).into_any_element()),
        )
        .child(div().flex_1())
        .children(actions.mutations_allowed.then(|| {
            icon_button(icons::SQUARE, (actions.on_stop_all_servers)()).into_any_element()
        }))
        .child(icon_button(icons::SETTINGS, (actions.on_open_settings)()))
        .into_any_element()
}

fn render_expanded_sidebar(
    state: &AppState,
    runtime: &RuntimeState,
    server_indicators: &HashMap<String, ServerIndicatorState>,
    actions: SidebarActions<'_>,
) -> AnyElement {
    let project_count = state.projects().len();
    let project_rows = state.projects().iter().enumerate().map(|(index, project)| {
        render_project_group(
            state,
            runtime,
            server_indicators,
            project,
            index,
            project_count,
            &actions,
        )
    });
    let ssh_rows = state
        .ssh_connections()
        .iter()
        .map(|connection| render_ssh_row(state, runtime, connection, &actions));

    div()
        .w(px(SIDEBAR_WIDTH_PX))
        .h_full()
        .flex_none()
        .flex()
        .flex_col()
        .bg(rgb(theme::SIDEBAR_BG))
        .border_r_1()
        .border_color(rgb(theme::BORDER_PRIMARY))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_2()
                .py(px(6.0))
                .border_b_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap(px(4.0))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui::FontWeight::BOLD)
                                .child("DEVMANAGER"),
                        )
                        .child(div().text_xs().text_color(rgb(theme::TEXT_DIM)).child(
                            SharedString::from(format!("v{}", env!("CARGO_PKG_VERSION"))),
                        )),
                )
                .child(icon_button(
                    icons::CHEVRON_LEFT,
                    (actions.on_toggle_sidebar)(),
                )),
        )
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .px_1()
                .py_1()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .children(
                            state
                                .projects()
                                .is_empty()
                                .then(|| empty_state("Add a project to get started.")),
                        )
                        .children(project_rows),
                )
                .child(
                    div()
                        .pt(px(6.0))
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .child(section_label("SSH"))
                        .children(ssh_rows)
                        .children(actions.mutations_allowed.then(|| {
                            accent_text_action("+ Add SSH", theme::SSH_DOT, (actions.on_add_ssh)())
                                .into_any_element()
                        })),
                ),
        )
        .child(
            div()
                .px_2()
                .py(px(6.0))
                .border_t_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .children(actions.mutations_allowed.then(|| {
                            icon_button(icons::PLUS, (actions.on_add_project)()).into_any_element()
                        }))
                        .child(icon_button(icons::GIT_BRANCH, (actions.on_open_git)()))
                        .child(div().flex_1())
                        .children(actions.mutations_allowed.then(|| {
                            icon_button(icons::SQUARE, (actions.on_stop_all_servers)())
                                .into_any_element()
                        }))
                        .child(icon_button(icons::SETTINGS, (actions.on_open_settings)())),
                ),
        )
        .into_any_element()
}

fn render_project_group(
    state: &AppState,
    runtime: &RuntimeState,
    server_indicators: &HashMap<String, ServerIndicatorState>,
    project: &crate::models::Project,
    index: usize,
    project_count: usize,
    actions: &SidebarActions<'_>,
) -> impl IntoElement {
    let can_move_up = index > 0;
    let can_move_down = index + 1 < project_count;
    let project_accent = theme::parse_hex_color(project.color.as_deref(), theme::PROJECT_DOT);
    let is_active_project = state
        .active_project()
        .map(|active| active.id == project.id)
        .unwrap_or(false);
    let collapsed = state.is_project_collapsed(&project.id);
    let claude_rows = state
        .ai_tabs_for_project(&project.id, crate::models::TabType::Claude)
        .map(|tab| render_ai_row(state, runtime, tab, project_accent, actions));
    let codex_rows = state
        .ai_tabs_for_project(&project.id, crate::models::TabType::Codex)
        .map(|tab| render_ai_row(state, runtime, tab, project_accent, actions));
    let has_visible_folders = project
        .folders
        .iter()
        .any(|folder| !folder.hidden.unwrap_or(false));
    let folder_rows = project
        .folders
        .iter()
        .filter(|folder| !folder.hidden.unwrap_or(false))
        .map(|folder| {
            render_folder_group(
                state,
                runtime,
                server_indicators,
                project,
                folder,
                project_accent,
                actions,
            )
        });
    let ai_launch_row = actions.mutations_allowed.then(|| {
        div()
            .flex()
            .items_center()
            .gap(px(5.0))
            .pl_4()
            .text_xs()
            .child(icon_text_action(
                icons::SPARKLES,
                10.0,
                "+ Claude",
                0xb07d3a, // muted amber, subtle
                (actions.on_launch_claude)(project.id.clone()),
            ))
            .child(icon_text_action(
                icons::BOT,
                10.0,
                "+ Codex",
                0x6a9c89, // muted teal, subtle
                (actions.on_launch_codex)(project.id.clone()),
            ))
    });

    let menu_open = matches!(
        actions.open_context_menu,
        Some(SidebarContextMenu::Project { ref project_id }) if *project_id == project.id
    );

    let chevron_icon = if collapsed {
        icons::CHEVRON_RIGHT
    } else {
        icons::CHEVRON_DOWN
    };

    let mut group = div()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .child(
            div()
                .group("project-row")
                .flex()
                .items_center()
                .justify_between()
                .gap(px(4.0))
                .px_2()
                .py(px(5.0))
                .rounded_sm()
                .cursor_pointer()
                .bg(rgb(if is_active_project {
                    theme::AGENT_ROW_BG
                } else {
                    theme::SIDEBAR_BG
                }))
                .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .on_mouse_down(
                            MouseButton::Left,
                            (actions.on_toggle_project_collapse)(project.id.clone()),
                        )
                        .child(icons::app_icon(chevron_icon, 10.0, theme::TEXT_DIM))
                        .child(div().size(px(6.0)).rounded_full().bg(rgb(project_accent)))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(SharedString::from(project.name.clone())),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .opacity(if menu_open { 1.0 } else { 0.0 })
                        .group_hover("project-row", |s| s.opacity(1.0))
                        .children(project.notes.as_ref().map(|_| {
                            row_icon_action(
                                icons::FILE_TEXT,
                                (actions.on_open_project_notes)(project.id.clone()),
                            )
                        }))
                        .children((actions.mutations_allowed && can_move_up).then(|| {
                            row_icon_action(
                                icons::CHEVRON_UP,
                                (actions.on_move_project_up)(project.id.clone()),
                            )
                        }))
                        .children((actions.mutations_allowed && can_move_down).then(|| {
                            row_icon_action(
                                icons::CHEVRON_DOWN,
                                (actions.on_move_project_down)(project.id.clone()),
                            )
                        }))
                        .children(actions.mutations_allowed.then(|| {
                            row_icon_action(
                                icons::PLUS,
                                (actions.on_add_folder)(project.id.clone()),
                            )
                            .into_any_element()
                        }))
                        .child(row_icon_action(
                            icons::MORE_HORIZONTAL,
                            (actions.on_toggle_context_menu)(SidebarContextMenu::Project {
                                project_id: project.id.clone(),
                            }),
                        )),
                ),
        )
        .children(menu_open.then(|| {
            let mut items: Vec<AnyElement> = vec![context_menu_item(
                "Edit Project",
                (actions.on_edit_project)(project.id.clone()),
            )
            .into_any_element()];
            if actions.mutations_allowed {
                items.push(
                    context_menu_item("Add Folder", (actions.on_add_folder)(project.id.clone()))
                        .into_any_element(),
                );
            }
            if project.notes.is_some() {
                items.push(
                    context_menu_item(
                        "View Notes",
                        (actions.on_open_project_notes)(project.id.clone()),
                    )
                    .into_any_element(),
                );
            }
            if actions.mutations_allowed {
                items.push(
                    context_menu_danger_item(
                        "Delete Project",
                        (actions.on_delete_project)(project.id.clone()),
                    )
                    .into_any_element(),
                );
            }
            context_menu_panel(items, (actions.on_dismiss_context_menu)()).into_any_element()
        }));

    if !collapsed {
        group = group
            .children(
                (!has_visible_folders)
                    .then(|| empty_state_with_indent("No folders configured.", 14.0)),
            )
            .children(folder_rows)
            .children(claude_rows)
            .children(codex_rows)
            .children(ai_launch_row);
    }

    group
}

fn render_ai_row(
    state: &AppState,
    runtime: &RuntimeState,
    tab: &SessionTab,
    project_accent: u32,
    actions: &SidebarActions<'_>,
) -> impl IntoElement {
    let session = tab
        .pty_session_id
        .as_deref()
        .and_then(|session_id| runtime.sessions.get(session_id));
    let is_active = state.active_tab_id.as_deref() == Some(tab.id.as_str());
    let label = session
        .and_then(|s| s.title.clone())
        .filter(|t| is_meaningful_title(t))
        .map(|t| truncate_label(&t, 20))
        .unwrap_or_else(|| state.tab_label(tab));
    let status_label = ai_status_label(session);
    let status_color = ai_status_color(session, tab);
    let show_ready_light = ai_ready_light_visible(session);
    let is_thinking =
        session.is_some_and(|s| matches!(s.ai_activity, Some(crate::state::AiActivity::Thinking)));
    let icon_opacity = if is_thinking {
        let elapsed_ms = session
            .and_then(|s| s.thinking_since)
            .map(|since| since.elapsed().as_millis() as f32)
            .unwrap_or(0.0);
        0.35 + 0.65 * (elapsed_ms / 800.0 * std::f32::consts::PI).sin().abs()
    } else {
        1.0
    };

    let mut row = div()
        .group("ai-row")
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(4.0))
        .pl_4()
        .pr_2()
        .py(px(2.0))
        .rounded_sm()
        .cursor_pointer()
        .bg(rgb(if is_active {
            theme::PROJECT_ROW_BG
        } else {
            theme::SIDEBAR_BG
        }))
        .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)));

    if is_active {
        row = row.border_l_2().border_color(rgb(project_accent));
    }

    row.child(
        div()
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .items_center()
            .gap(px(5.0))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                (actions.on_select_ai_tab)(tab.id.clone()),
            )
            .child(
                div().opacity(icon_opacity).child(match tab.tab_type {
                    crate::models::TabType::Claude => {
                        icons::app_icon(icons::SPARKLES, 10.0, status_color).into_any_element()
                    }
                    crate::models::TabType::Codex => {
                        icons::app_icon(icons::BOT, 10.0, status_color).into_any_element()
                    }
                    _ => div()
                        .size(px(6.0))
                        .rounded_full()
                        .bg(rgb(status_color))
                        .into_any_element(),
                }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_PRIMARY))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(SharedString::from(label)),
            ),
    )
    .child(
        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(4.0))
            .children(show_ready_light.then(|| {
                div()
                    .size(px(6.0))
                    .rounded_full()
                    .bg(rgb(theme::SUCCESS_TEXT))
                    .into_any_element()
            }))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(status_color))
                    .child(status_label),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .opacity(0.0)
                    .group_hover("ai-row", |s| s.opacity(1.0))
                    .children((actions.mutations_allowed && session.is_some()).then(|| {
                        row_icon_action(icons::X, (actions.on_close_ai_tab)(tab.id.clone()))
                    }))
                    .children((actions.mutations_allowed && session.is_none()).then(|| {
                        row_icon_action(icons::PLAY, (actions.on_restart_ai_tab)(tab.id.clone()))
                    })),
            ),
    )
}

fn render_folder_group(
    state: &AppState,
    runtime: &RuntimeState,
    server_indicators: &HashMap<String, ServerIndicatorState>,
    project: &crate::models::Project,
    folder: &crate::models::ProjectFolder,
    project_accent: u32,
    actions: &SidebarActions<'_>,
) -> AnyElement {
    if folder.commands.len() == 1 {
        return render_single_command_folder_row(
            state,
            runtime,
            server_indicators,
            project,
            folder,
            &folder.commands[0],
            project_accent,
            actions,
        )
        .into_any_element();
    }

    let command_rows = folder.commands.iter().map(|command| {
        render_command_row(
            state,
            runtime,
            server_indicators,
            project,
            folder,
            command,
            project_accent,
            actions,
        )
    });

    let menu_open = matches!(
        actions.open_context_menu,
        Some(SidebarContextMenu::Folder { ref project_id, ref folder_id })
            if *project_id == project.id && *folder_id == folder.id
    );

    div()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .pl_4()
        .child(
            div()
                .group("folder-group-row")
                .flex()
                .items_center()
                .justify_between()
                .gap(px(4.0))
                .px_2()
                .py(px(3.0))
                .rounded_sm()
                .cursor_pointer()
                .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
                .child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(icons::app_icon(icons::FOLDER, 10.0, theme::TEXT_SUBTLE))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(SharedString::from(folder.name.clone())),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .opacity(if menu_open { 1.0 } else { 0.0 })
                        .group_hover("folder-group-row", |s| s.opacity(1.0))
                        .children(actions.mutations_allowed.then(|| {
                            row_icon_action(
                                icons::PLUS,
                                (actions.on_add_command)(project.id.clone(), folder.id.clone()),
                            )
                            .into_any_element()
                        }))
                        .child(row_icon_action(
                            icons::MORE_HORIZONTAL,
                            (actions.on_toggle_context_menu)(SidebarContextMenu::Folder {
                                project_id: project.id.clone(),
                                folder_id: folder.id.clone(),
                            }),
                        )),
                ),
        )
        .children(menu_open.then(|| {
            let mut items: Vec<AnyElement> = vec![context_menu_item(
                "Edit Folder",
                (actions.on_edit_folder)(project.id.clone(), folder.id.clone()),
            )
            .into_any_element()];
            if actions.mutations_allowed {
                items.push(
                    context_menu_item(
                        "Add Command",
                        (actions.on_add_command)(project.id.clone(), folder.id.clone()),
                    )
                    .into_any_element(),
                );
                items.push(
                    context_menu_danger_item(
                        "Remove Folder",
                        (actions.on_delete_folder)(project.id.clone(), folder.id.clone()),
                    )
                    .into_any_element(),
                );
            }
            context_menu_panel(items, (actions.on_dismiss_context_menu)()).into_any_element()
        }))
        .children(
            folder
                .commands
                .is_empty()
                .then(|| empty_state_with_indent("No commands configured.", 12.0)),
        )
        .children(command_rows)
        .into_any_element()
}

fn render_single_command_folder_row(
    state: &AppState,
    runtime: &RuntimeState,
    server_indicators: &HashMap<String, ServerIndicatorState>,
    project: &crate::models::Project,
    folder: &crate::models::ProjectFolder,
    command: &crate::models::RunCommand,
    project_accent: u32,
    actions: &SidebarActions<'_>,
) -> impl IntoElement {
    let session = runtime.sessions.get(&command.id);
    let status = session
        .map(|session| session.status)
        .unwrap_or(SessionStatus::Stopped);
    let indicator = server_indicators
        .get(&command.id)
        .copied()
        .unwrap_or(ServerIndicatorState::Stopped);
    let is_active = state.active_tab_id.as_deref() == Some(command.id.as_str());
    let menu_open = matches!(
        actions.open_context_menu,
        Some(SidebarContextMenu::SingleCommandFolder { ref project_id, ref folder_id, ref command_id })
            if *project_id == project.id && *folder_id == folder.id && *command_id == command.id
    );

    let mut inner_row = div()
        .group("folder-row")
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(4.0))
        .pr_2()
        .py(px(2.0))
        .rounded_sm()
        .cursor_pointer()
        .bg(rgb(if is_active {
            theme::PROJECT_ROW_BG
        } else {
            theme::SIDEBAR_BG
        }))
        .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)));

    if is_active {
        inner_row = inner_row.border_l_2().border_color(rgb(project_accent));
    }

    div()
        .flex()
        .flex_col()
        .pl_4()
        .child(
            inner_row
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            (actions.on_select_server_tab)(command.id.clone()),
                        )
                        .child(icons::app_icon(icons::FOLDER, 10.0, theme::TEXT_SUBTLE))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(SharedString::from(folder.name.clone())),
                        )
                        .children(command.port.map(|port| {
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_DIM))
                                .child(SharedString::from(format!(":{port}")))
                        })),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(server_status_indicator(indicator))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .opacity(if menu_open { 1.0 } else { 0.0 })
                                .group_hover("folder-row", |s| s.opacity(1.0))
                                .children((actions.mutations_allowed && !status.is_live()).then(
                                    || {
                                        row_icon_action(
                                            icons::PLAY,
                                            (actions.on_start_server)(command.id.clone()),
                                        )
                                    },
                                ))
                                .children((actions.mutations_allowed && status.is_live()).then(
                                    || {
                                        row_icon_action(
                                            icons::SQUARE,
                                            (actions.on_stop_server)(command.id.clone()),
                                        )
                                    },
                                ))
                                .child(row_icon_action(
                                    icons::MORE_HORIZONTAL,
                                    (actions.on_toggle_context_menu)(
                                        SidebarContextMenu::SingleCommandFolder {
                                            project_id: project.id.clone(),
                                            folder_id: folder.id.clone(),
                                            command_id: command.id.clone(),
                                        },
                                    ),
                                )),
                        ),
                ),
        )
        .children(menu_open.then(|| {
            let mut items: Vec<AnyElement> = vec![
                context_menu_item(
                    "Edit Folder",
                    (actions.on_edit_folder)(project.id.clone(), folder.id.clone()),
                )
                .into_any_element(),
                context_menu_item(
                    "Edit Command",
                    (actions.on_edit_command)(command.id.clone()),
                )
                .into_any_element(),
            ];
            if actions.mutations_allowed && !status.is_live() {
                items.push(
                    context_menu_item("Start", (actions.on_start_server)(command.id.clone()))
                        .into_any_element(),
                );
            }
            if actions.mutations_allowed && status.is_live() {
                items.push(
                    context_menu_item("Restart", (actions.on_restart_server)(command.id.clone()))
                        .into_any_element(),
                );
                items.push(
                    context_menu_item("Stop", (actions.on_stop_server)(command.id.clone()))
                        .into_any_element(),
                );
            }
            if actions.mutations_allowed {
                items.push(
                    context_menu_danger_item(
                        "Remove Folder",
                        (actions.on_delete_folder)(project.id.clone(), folder.id.clone()),
                    )
                    .into_any_element(),
                );
            }
            context_menu_panel(items, (actions.on_dismiss_context_menu)()).into_any_element()
        }))
}

fn render_command_row(
    state: &AppState,
    runtime: &RuntimeState,
    server_indicators: &HashMap<String, ServerIndicatorState>,
    _project: &crate::models::Project,
    _folder: &crate::models::ProjectFolder,
    command: &crate::models::RunCommand,
    project_accent: u32,
    actions: &SidebarActions<'_>,
) -> impl IntoElement {
    let session = runtime.sessions.get(&command.id);
    let status = session
        .map(|session| session.status)
        .unwrap_or(SessionStatus::Stopped);
    let indicator = server_indicators
        .get(&command.id)
        .copied()
        .unwrap_or(ServerIndicatorState::Stopped);
    let is_active = state.active_tab_id.as_deref() == Some(command.id.as_str());
    let resource_line = session.and_then(|session| {
        session.resources.last_sample_at.map(|_| {
            format!(
                "{:.0}% • {} MB",
                session.resources.cpu_percent,
                session.resources.memory_bytes / 1024 / 1024
            )
        })
    });
    let menu_open = matches!(
        actions.open_context_menu,
        Some(SidebarContextMenu::Command { ref command_id }) if *command_id == command.id
    );

    let mut cmd_row = div()
        .group("command-row")
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(4.0))
        .pl_5()
        .pr_2()
        .py(px(2.0))
        .rounded_sm()
        .cursor_pointer()
        .bg(rgb(if is_active {
            theme::PROJECT_ROW_BG
        } else {
            theme::SIDEBAR_BG
        }))
        .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)));

    if is_active {
        cmd_row = cmd_row.border_l_2().border_color(rgb(project_accent));
    }

    div()
        .flex()
        .flex_col()
        .child(
            cmd_row
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            (actions.on_select_server_tab)(command.id.clone()),
                        )
                        .child(
                            div()
                                .size(px(6.0))
                                .rounded_full()
                                .bg(rgb(server_status_color(indicator))),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(theme::TEXT_PRIMARY))
                                        .child(SharedString::from(command.label.clone())),
                                )
                                .children(command.port.map(|port| {
                                    div()
                                        .text_xs()
                                        .text_color(rgb(theme::TEXT_DIM))
                                        .child(SharedString::from(format!(":{port}")))
                                }))
                                .children(resource_line.map(|line| {
                                    div()
                                        .text_xs()
                                        .text_color(rgb(theme::TEXT_DIM))
                                        .child(SharedString::from(line))
                                })),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(server_status_color(indicator)))
                                .child(server_status_label(indicator)),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .opacity(if menu_open { 1.0 } else { 0.0 })
                                .group_hover("command-row", |s| s.opacity(1.0))
                                .children((actions.mutations_allowed && !status.is_live()).then(
                                    || {
                                        row_icon_action(
                                            icons::PLAY,
                                            (actions.on_start_server)(command.id.clone()),
                                        )
                                    },
                                ))
                                .children((actions.mutations_allowed && status.is_live()).then(
                                    || {
                                        row_icon_action(
                                            icons::SQUARE,
                                            (actions.on_stop_server)(command.id.clone()),
                                        )
                                    },
                                ))
                                .child(row_icon_action(
                                    icons::MORE_HORIZONTAL,
                                    (actions.on_toggle_context_menu)(SidebarContextMenu::Command {
                                        command_id: command.id.clone(),
                                    }),
                                )),
                        ),
                ),
        )
        .children(menu_open.then(|| {
            let mut items: Vec<AnyElement> = vec![context_menu_item(
                "Edit Command",
                (actions.on_edit_command)(command.id.clone()),
            )
            .into_any_element()];
            if actions.mutations_allowed && !status.is_live() {
                items.push(
                    context_menu_item("Start", (actions.on_start_server)(command.id.clone()))
                        .into_any_element(),
                );
            }
            if actions.mutations_allowed && status.is_live() {
                items.push(
                    context_menu_item("Restart", (actions.on_restart_server)(command.id.clone()))
                        .into_any_element(),
                );
                items.push(
                    context_menu_item("Stop", (actions.on_stop_server)(command.id.clone()))
                        .into_any_element(),
                );
            }
            if actions.mutations_allowed {
                items.push(
                    context_menu_danger_item(
                        "Delete Command",
                        (actions.on_delete_command)(command.id.clone()),
                    )
                    .into_any_element(),
                );
            }
            context_menu_panel(items, (actions.on_dismiss_context_menu)()).into_any_element()
        }))
}

fn render_ssh_row(
    state: &AppState,
    runtime: &RuntimeState,
    connection: &crate::models::SSHConnection,
    actions: &SidebarActions<'_>,
) -> impl IntoElement {
    let tab = state.find_ssh_tab_by_connection(&connection.id);
    let session = tab
        .and_then(|tab| tab.pty_session_id.as_deref())
        .and_then(|session_id| runtime.sessions.get(session_id));
    let label = ssh_status_label(tab, session);
    let color = ssh_status_color(tab, session);
    let is_active = tab
        .map(|tab| state.active_tab_id.as_deref() == Some(tab.id.as_str()))
        .unwrap_or(false);
    let is_connected = session.is_some();
    let connection_target = ssh_connection_target(connection);
    let menu_open = matches!(
        actions.open_context_menu,
        Some(SidebarContextMenu::Ssh { ref connection_id }) if *connection_id == connection.id
    );

    div()
        .flex()
        .flex_col()
        .child(
            div()
                .group("ssh-row")
                .w_full()
                .flex()
                .items_start()
                .justify_between()
                .gap(px(8.0))
                .px_2()
                .py(px(6.0))
                .rounded_sm()
                .cursor_pointer()
                .bg(rgb(if is_active {
                    theme::PROJECT_ROW_BG
                } else {
                    theme::SIDEBAR_BG
                }))
                .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .items_start()
                        .gap(px(6.0))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            (actions.on_open_ssh_tab)(connection.id.clone()),
                        )
                        .child(icons::app_icon(icons::TERMINAL, 10.0, color))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.0))
                                        .min_w(px(0.0))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w(px(0.0))
                                                .text_xs()
                                                .text_color(rgb(theme::TEXT_PRIMARY))
                                                .overflow_hidden()
                                                .whitespace_nowrap()
                                                .child(SharedString::from(
                                                    connection.label.clone(),
                                                )),
                                        )
                                        .child(
                                            div()
                                                .flex_shrink_0()
                                                .px(px(6.0))
                                                .py(px(1.0))
                                                .rounded_full()
                                                .bg(rgb(theme::BUTTON_HOVER_BG))
                                                .text_size(px(9.0))
                                                .text_color(rgb(color))
                                                .child(label),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(rgb(theme::TEXT_DIM))
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .child(SharedString::from(connection_target)),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .opacity(if menu_open || is_active { 1.0 } else { 0.0 })
                        .group_hover("ssh-row", |s| s.opacity(1.0))
                        .children(actions.mutations_allowed.then(|| {
                            row_icon_action(
                                if is_connected {
                                    icons::SQUARE
                                } else {
                                    icons::PLAY
                                },
                                if is_connected {
                                    (actions.on_disconnect_ssh)(connection.id.clone())
                                } else {
                                    (actions.on_connect_ssh)(connection.id.clone())
                                },
                            )
                            .into_any_element()
                        }))
                        .child(row_icon_action(
                            icons::SETTINGS,
                            (actions.on_edit_ssh)(connection.id.clone()),
                        ))
                        .child(row_icon_action(
                            icons::MORE_HORIZONTAL,
                            (actions.on_toggle_context_menu)(SidebarContextMenu::Ssh {
                                connection_id: connection.id.clone(),
                            }),
                        )),
                ),
        )
        .children(menu_open.then(|| {
            let mut items: Vec<AnyElement> =
                vec![
                    context_menu_item("Edit SSH", (actions.on_edit_ssh)(connection.id.clone()))
                        .into_any_element(),
                ];
            if actions.mutations_allowed && is_connected {
                items.push(
                    context_menu_item("Restart", (actions.on_restart_ssh)(connection.id.clone()))
                        .into_any_element(),
                );
                items.push(
                    context_menu_item(
                        "Disconnect",
                        (actions.on_disconnect_ssh)(connection.id.clone()),
                    )
                    .into_any_element(),
                );
            } else if actions.mutations_allowed {
                items.push(
                    context_menu_item("Connect", (actions.on_connect_ssh)(connection.id.clone()))
                        .into_any_element(),
                );
            }
            if actions.mutations_allowed {
                items.push(
                    context_menu_danger_item(
                        "Delete SSH",
                        (actions.on_delete_ssh)(connection.id.clone()),
                    )
                    .into_any_element(),
                );
            }
            context_menu_panel(items, (actions.on_dismiss_context_menu)()).into_any_element()
        }))
}

fn icon_button(
    icon_path: &'static str,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .w(px(20.0))
        .h(px(20.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .hover(|s| {
            s.bg(rgb(theme::BUTTON_HOVER_BG))
                .text_color(rgb(theme::TEXT_PRIMARY))
        })
        .child(icons::app_icon(icon_path, 14.0, theme::TEXT_MUTED))
        .on_mouse_down(MouseButton::Left, on_click)
}

fn row_icon_action(
    icon_path: &'static str,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .w(px(16.0))
        .h(px(16.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .px(px(2.0))
        .hover(|s| {
            s.bg(rgb(theme::BUTTON_HOVER_BG))
                .text_color(rgb(theme::TEXT_PRIMARY))
        })
        .child(icons::app_icon(icon_path, 10.0, theme::TEXT_MUTED))
        .on_mouse_down(MouseButton::Left, on_click)
}

fn accent_text_action(
    label: &str,
    color: u32,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .text_xs()
        .text_color(rgb(color))
        .cursor_pointer()
        .child(SharedString::from(label.to_string()))
        .on_mouse_down(MouseButton::Left, on_click)
}

fn icon_text_action(
    icon_path: &'static str,
    icon_size_px: f32,
    label: &str,
    color: u32,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(3.0))
        .cursor_pointer()
        .child(icons::app_icon(icon_path, icon_size_px, color))
        .child(
            div()
                .text_xs()
                .text_color(rgb(color))
                .child(SharedString::from(label.to_string())),
        )
        .on_mouse_down(MouseButton::Left, on_click)
}

fn section_label(label: &str) -> impl IntoElement {
    div()
        .px_1()
        .text_size(px(10.0))
        .text_color(rgb(theme::TEXT_DIM))
        .child(SharedString::from(label.to_uppercase()))
}

fn empty_state(message: &str) -> impl IntoElement {
    div()
        .px_2()
        .py(px(2.0))
        .text_xs()
        .text_color(rgb(theme::TEXT_SUBTLE))
        .child(SharedString::from(message.to_string()))
}

fn empty_state_with_indent(message: &str, indent_px: f32) -> impl IntoElement {
    div()
        .pl(px(indent_px))
        .py(px(2.0))
        .text_xs()
        .text_color(rgb(theme::TEXT_SUBTLE))
        .child(SharedString::from(message.to_string()))
}

fn status_label(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Stopped => "",
        SessionStatus::Starting => "starting",
        SessionStatus::Running => "",
        SessionStatus::Stopping => "stopping",
        SessionStatus::Crashed => "crashed",
        SessionStatus::Exited => "exited",
        SessionStatus::Failed => "failed",
    }
}

fn status_color(status: SessionStatus) -> u32 {
    match status {
        SessionStatus::Running => theme::SUCCESS_TEXT,
        SessionStatus::Starting | SessionStatus::Stopping => theme::WARNING_TEXT,
        SessionStatus::Crashed | SessionStatus::Failed => theme::DANGER_TEXT,
        _ => theme::TEXT_SUBTLE,
    }
}

fn server_status_label(state: ServerIndicatorState) -> &'static str {
    match state {
        ServerIndicatorState::Stopped
        | ServerIndicatorState::Unready
        | ServerIndicatorState::Ready => "",
        ServerIndicatorState::Stopping => "stopping",
        ServerIndicatorState::Crashed => "crashed",
        ServerIndicatorState::Exited => "exited",
        ServerIndicatorState::Failed => "failed",
    }
}

fn server_status_indicator(state: ServerIndicatorState) -> Div {
    if matches!(
        state,
        ServerIndicatorState::Stopped | ServerIndicatorState::Unready | ServerIndicatorState::Ready
    ) {
        div()
            .size(px(6.0))
            .rounded_full()
            .bg(rgb(server_status_color(state)))
    } else {
        div()
            .text_xs()
            .text_color(rgb(server_status_color(state)))
            .child(server_status_label(state))
    }
}

fn server_status_color(state: ServerIndicatorState) -> u32 {
    match state {
        ServerIndicatorState::Ready => theme::SUCCESS_TEXT,
        ServerIndicatorState::Unready | ServerIndicatorState::Stopping => theme::WARNING_TEXT,
        ServerIndicatorState::Crashed | ServerIndicatorState::Failed => theme::DANGER_TEXT,
        ServerIndicatorState::Stopped | ServerIndicatorState::Exited => theme::TEXT_SUBTLE,
    }
}

fn ai_status_label(session: Option<&SessionRuntimeState>) -> &'static str {
    let Some(session) = session else {
        return "saved";
    };

    if session.unseen_ready {
        "ready"
    } else if matches!(
        session.ai_activity,
        Some(crate::state::AiActivity::Thinking)
    ) {
        "thinking"
    } else if session.status == SessionStatus::Running {
        "idle"
    } else {
        status_label(session.status)
    }
}

fn ai_status_color(session: Option<&SessionRuntimeState>, tab: &SessionTab) -> u32 {
    let Some(session) = session else {
        return match tab.tab_type {
            crate::models::TabType::Claude => theme::AI_DOT,
            crate::models::TabType::Codex => theme::SUCCESS_TEXT,
            _ => theme::TEXT_SUBTLE,
        };
    };

    if session.unseen_ready {
        theme::SUCCESS_TEXT
    } else if matches!(
        session.ai_activity,
        Some(crate::state::AiActivity::Thinking)
    ) {
        theme::WARNING_TEXT
    } else if session.status == SessionStatus::Running {
        match tab.tab_type {
            crate::models::TabType::Claude => theme::AI_DOT,
            crate::models::TabType::Codex => theme::SUCCESS_TEXT,
            _ => theme::TEXT_MUTED,
        }
    } else {
        status_color(session.status)
    }
}

fn ai_ready_light_visible(session: Option<&SessionRuntimeState>) -> bool {
    session.is_some_and(|session| session.unseen_ready)
}

fn ssh_status_label(
    tab: Option<&SessionTab>,
    session: Option<&SessionRuntimeState>,
) -> &'static str {
    let Some(session) = session else {
        return if tab.is_some() { "saved" } else { "new" };
    };

    match session.status {
        SessionStatus::Running => "connected",
        SessionStatus::Stopped | SessionStatus::Exited => "disconnected",
        status => status_label(status),
    }
}

fn truncate_label(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        text.to_string()
    } else {
        format!("{}\u{2026}", chars[..max].iter().collect::<String>())
    }
}

fn is_meaningful_title(title: &str) -> bool {
    let t = title.trim();
    if t.is_empty() {
        return false;
    }
    // Filter out raw shell paths like "C:\WINDOWS\system32\cmd.exe" or "/bin/bash"
    if t.contains("\\system32\\") || t.contains("/bin/") || t.contains("/usr/") {
        return false;
    }
    if t.ends_with(".exe") && (t.contains('\\') || t.contains('/')) {
        return false;
    }
    true
}

fn ssh_status_color(tab: Option<&SessionTab>, session: Option<&SessionRuntimeState>) -> u32 {
    let Some(session) = session else {
        return if tab.is_some() {
            theme::TEXT_SUBTLE
        } else {
            theme::TEXT_MUTED
        };
    };

    match session.status {
        SessionStatus::Running => theme::SSH_DOT,
        SessionStatus::Stopped | SessionStatus::Exited => theme::TEXT_SUBTLE,
        status => status_color(status),
    }
}

fn ssh_connection_target(connection: &crate::models::SSHConnection) -> String {
    let host = format!("{}@{}", connection.username.trim(), connection.host.trim());
    if connection.port == 22 {
        host
    } else {
        format!("{host}:{}", connection.port)
    }
}

fn context_menu_item(
    label: &str,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .px(px(10.0))
        .py(px(5.0))
        .text_xs()
        .text_color(rgb(theme::TEXT_PRIMARY))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
        .child(SharedString::from(label.to_string()))
        .on_mouse_down(MouseButton::Left, on_click)
}

fn context_menu_danger_item(
    label: &str,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .px(px(10.0))
        .py(px(5.0))
        .text_xs()
        .text_color(rgb(theme::DANGER_TEXT))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
        .child(SharedString::from(label.to_string()))
        .on_mouse_down(MouseButton::Left, on_click)
}

fn context_menu_panel(
    items: Vec<AnyElement>,
    on_dismiss: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    deferred(
        anchored()
            .anchor(Corner::TopLeft)
            .snap_to_window()
            .child(
                // Full-screen invisible backdrop to catch clicks outside the menu
                div()
                    .id("context-menu-backdrop")
                    .occlude()
                    .size_full()
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .on_mouse_down(MouseButton::Left, on_dismiss),
            )
            .child(
                div()
                    .occlude()
                    .w(px(160.0))
                    .py(px(4.0))
                    .rounded_sm()
                    .bg(rgb(theme::PANEL_HEADER_BG))
                    .border_1()
                    .border_color(rgb(theme::BORDER_PRIMARY))
                    .flex()
                    .flex_col()
                    .children(items),
            ),
    )
    .with_priority(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::TabType;
    use crate::state::{AiActivity, SessionKind};
    use crate::terminal::session::TerminalBackend;
    use std::path::PathBuf;

    fn ai_tab() -> SessionTab {
        SessionTab {
            id: "tab-1".to_string(),
            tab_type: TabType::Claude,
            project_id: "project-1".to_string(),
            command_id: None,
            pty_session_id: Some("session-1".to_string()),
            label: Some("Claude".to_string()),
            ssh_connection_id: None,
        }
    }

    fn ai_session() -> SessionRuntimeState {
        let mut session = SessionRuntimeState::new(
            "session-1",
            PathBuf::from("."),
            crate::state::SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.session_kind = SessionKind::Claude;
        session.status = SessionStatus::Running;
        session.ai_activity = Some(AiActivity::Idle);
        session
    }

    #[test]
    fn ready_ai_rows_show_ready_dot_and_success_label() {
        let mut session = ai_session();
        let tab = ai_tab();
        session.unseen_ready = true;

        assert_eq!(ai_status_label(Some(&session)), "ready");
        assert_eq!(ai_status_color(Some(&session), &tab), theme::SUCCESS_TEXT);
        assert!(ai_ready_light_visible(Some(&session)));
    }

    #[test]
    fn thinking_ai_rows_keep_warning_color_without_ready_dot() {
        let mut session = ai_session();
        let tab = ai_tab();
        session.ai_activity = Some(AiActivity::Thinking);

        assert_eq!(ai_status_label(Some(&session)), "thinking");
        assert_eq!(ai_status_color(Some(&session), &tab), theme::WARNING_TEXT);
        assert!(!ai_ready_light_visible(Some(&session)));
    }

    #[test]
    fn server_indicator_uses_warning_for_unready_and_success_for_ready() {
        assert_eq!(server_status_label(ServerIndicatorState::Unready), "");
        assert_eq!(
            server_status_color(ServerIndicatorState::Unready),
            theme::WARNING_TEXT
        );
        assert_eq!(
            server_status_color(ServerIndicatorState::Ready),
            theme::SUCCESS_TEXT
        );
        assert_eq!(server_status_label(ServerIndicatorState::Failed), "failed");
    }

    #[test]
    fn ssh_connection_target_hides_default_port_and_keeps_custom_port() {
        let default_port = crate::models::SSHConnection {
            id: "ssh-1".to_string(),
            label: "Prod".to_string(),
            host: "example.com".to_string(),
            username: "deploy".to_string(),
            port: 22,
            password: None,
        };
        let custom_port = crate::models::SSHConnection {
            port: 2222,
            ..default_port.clone()
        };

        assert_eq!(ssh_connection_target(&default_port), "deploy@example.com");
        assert_eq!(
            ssh_connection_target(&custom_port),
            "deploy@example.com:2222"
        );
    }
}
