use crate::models::SessionTab;
use crate::state::{AppState, RuntimeState, SessionRuntimeState, SessionStatus};
use crate::{icons, theme};
use gpui::{
    div, px, rgb, AnyElement, App, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, SharedString, Styled, Window,
};

const SIDEBAR_WIDTH_PX: f32 = 220.0;
const SIDEBAR_COLLAPSED_WIDTH_PX: f32 = 40.0;

pub struct SidebarActions<'a> {
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
    actions: SidebarActions<'_>,
) -> impl IntoElement {
    if state.sidebar_collapsed {
        render_collapsed_sidebar(actions).into_any_element()
    } else {
        render_expanded_sidebar(state, runtime, actions).into_any_element()
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
        .child(icon_button("▸", (actions.on_toggle_sidebar)()))
        .child(icon_button("+", (actions.on_add_project)()))
        .child(div().flex_1())
        .child(icon_button("■", (actions.on_stop_all_servers)()))
        .child(icon_button("⚙", (actions.on_open_settings)()))
        .into_any_element()
}

fn render_expanded_sidebar(
    state: &AppState,
    runtime: &RuntimeState,
    actions: SidebarActions<'_>,
) -> AnyElement {
    let project_rows = state
        .projects()
        .iter()
        .map(|project| render_project_group(state, runtime, project, &actions));
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
                .child(icon_button("◂", (actions.on_toggle_sidebar)())),
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
                        .gap(px(1.0))
                        .child(section_label("PROJECTS"))
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
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(section_label("SSH"))
                                .child(accent_text_action(
                                    "+ Add SSH",
                                    theme::SSH_DOT,
                                    (actions.on_add_ssh)(),
                                )),
                        )
                        .children(
                            state
                                .ssh_connections()
                                .is_empty()
                                .then(|| empty_state("No saved SSH connections.")),
                        )
                        .children(ssh_rows),
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
                        .child(primary_button("+ Add Project", (actions.on_add_project)()))
                        .child(icon_button("■", (actions.on_stop_all_servers)()))
                        .child(icon_button("⚙", (actions.on_open_settings)())),
                ),
        )
        .into_any_element()
}

fn render_project_group(
    state: &AppState,
    runtime: &RuntimeState,
    project: &crate::models::Project,
    actions: &SidebarActions<'_>,
) -> impl IntoElement {
    let project_accent = theme::parse_hex_color(project.color.as_deref(), theme::PROJECT_DOT);
    let is_active_project = state
        .active_project()
        .map(|active| active.id == project.id)
        .unwrap_or(false);
    let project_id = project.id.clone();
    let ai_rows = state
        .ai_tabs()
        .filter(move |tab| tab.project_id == project_id)
        .map(|tab| render_ai_row(state, runtime, tab, actions));
    let folder_rows = project
        .folders
        .iter()
        .map(|folder| render_folder_group(state, runtime, project, folder, actions));

    div()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .pb(px(4.0))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(4.0))
                .px_2()
                .py(px(4.0))
                .bg(rgb(if is_active_project {
                    theme::AGENT_ROW_BG
                } else {
                    theme::SIDEBAR_BG
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(5.0))
                        .child(div().text_xs().text_color(rgb(theme::TEXT_DIM)).child("▾"))
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
                        .child(row_icon_action(
                            "+",
                            (actions.on_add_folder)(project.id.clone()),
                        ))
                        .child(row_icon_action(
                            "⋯",
                            (actions.on_edit_project)(project.id.clone()),
                        )),
                ),
        )
        .child(
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
                    theme::AI_DOT,
                    (actions.on_launch_claude)(project.id.clone()),
                ))
                .child(icon_text_action(
                    icons::BOT,
                    10.0,
                    "+ Codex",
                    theme::SUCCESS_TEXT,
                    (actions.on_launch_codex)(project.id.clone()),
                ))
                .children(project.notes.as_ref().map(|_| {
                    text_action("notes", (actions.on_open_project_notes)(project.id.clone()))
                })),
        )
        .children(ai_rows)
        .children(
            project
                .folders
                .is_empty()
                .then(|| empty_state_with_indent("No folders configured.", 14.0)),
        )
        .children(folder_rows)
}

fn render_ai_row(
    state: &AppState,
    runtime: &RuntimeState,
    tab: &SessionTab,
    actions: &SidebarActions<'_>,
) -> impl IntoElement {
    let session = tab
        .pty_session_id
        .as_deref()
        .and_then(|session_id| runtime.sessions.get(session_id));
    let is_active = state.active_tab_id.as_deref() == Some(tab.id.as_str());
    let label = session
        .and_then(|session| session.title.clone())
        .unwrap_or_else(|| state.tab_label(tab));
    let status_label = ai_status_label(session);
    let status_color = ai_status_color(session, tab);

    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(4.0))
        .pl_4()
        .pr_2()
        .py(px(2.0))
        .bg(rgb(if is_active {
            theme::PROJECT_ROW_BG
        } else {
            theme::SIDEBAR_BG
        }))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(5.0))
                .on_mouse_down(
                    MouseButton::Left,
                    (actions.on_select_ai_tab)(tab.id.clone()),
                )
                .child(match tab.tab_type {
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
                })
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(SharedString::from(label)),
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
                        .text_color(rgb(status_color))
                        .child(status_label),
                )
                .children(
                    session
                        .is_some()
                        .then(|| row_icon_action("×", (actions.on_close_ai_tab)(tab.id.clone()))),
                )
                .children(
                    session
                        .is_none()
                        .then(|| row_icon_action("▶", (actions.on_restart_ai_tab)(tab.id.clone()))),
                ),
        )
}

fn render_folder_group(
    state: &AppState,
    runtime: &RuntimeState,
    project: &crate::models::Project,
    folder: &crate::models::ProjectFolder,
    actions: &SidebarActions<'_>,
) -> AnyElement {
    if folder.commands.len() == 1 {
        return render_single_command_folder_row(
            state,
            runtime,
            project,
            folder,
            &folder.commands[0],
            actions,
        )
        .into_any_element();
    }

    let command_rows = folder
        .commands
        .iter()
        .map(|command| render_command_row(state, runtime, project, folder, command, actions));

    div()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .pl_4()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(4.0))
                .px_2()
                .py(px(3.0))
                .child(
                    div()
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
                        .child(row_icon_action(
                            "+",
                            (actions.on_add_command)(project.id.clone(), folder.id.clone()),
                        ))
                        .child(row_icon_action(
                            "⋯",
                            (actions.on_edit_folder)(project.id.clone(), folder.id.clone()),
                        )),
                ),
        )
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
    project: &crate::models::Project,
    folder: &crate::models::ProjectFolder,
    command: &crate::models::RunCommand,
    actions: &SidebarActions<'_>,
) -> impl IntoElement {
    let session = runtime.sessions.get(&command.id);
    let status = session
        .map(|session| session.status)
        .unwrap_or(SessionStatus::Stopped);
    let is_active = state.active_tab_id.as_deref() == Some(command.id.as_str());

    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(4.0))
        .pl_4()
        .pr_2()
        .py(px(2.0))
        .bg(rgb(if is_active {
            theme::PROJECT_ROW_BG
        } else {
            theme::SIDEBAR_BG
        }))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(5.0))
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
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(status_color(status)))
                        .child(status_label(status)),
                )
                .children(
                    (!status.is_live()).then(|| {
                        row_icon_action("▶", (actions.on_start_server)(command.id.clone()))
                    }),
                )
                .children(
                    status.is_live().then(|| {
                        row_icon_action("■", (actions.on_stop_server)(command.id.clone()))
                    }),
                )
                .child(row_icon_action(
                    "⋯",
                    (actions.on_edit_folder)(project.id.clone(), folder.id.clone()),
                )),
        )
}

fn render_command_row(
    state: &AppState,
    runtime: &RuntimeState,
    _project: &crate::models::Project,
    _folder: &crate::models::ProjectFolder,
    command: &crate::models::RunCommand,
    actions: &SidebarActions<'_>,
) -> impl IntoElement {
    let session = runtime.sessions.get(&command.id);
    let status = session
        .map(|session| session.status)
        .unwrap_or(SessionStatus::Stopped);
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

    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(4.0))
        .pl_5()
        .pr_2()
        .py(px(2.0))
        .bg(rgb(if is_active {
            theme::PROJECT_ROW_BG
        } else if status.is_live() {
            theme::SIDEBAR_BG
        } else {
            theme::SIDEBAR_BG
        }))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(5.0))
                .on_mouse_down(
                    MouseButton::Left,
                    (actions.on_select_server_tab)(command.id.clone()),
                )
                .child(
                    div()
                        .size(px(6.0))
                        .rounded_full()
                        .bg(rgb(status_color(status))),
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
                        .text_color(rgb(status_color(status)))
                        .child(status_label(status)),
                )
                .children(
                    (!status.is_live()).then(|| {
                        row_icon_action("▶", (actions.on_start_server)(command.id.clone()))
                    }),
                )
                .children(
                    status.is_live().then(|| {
                        row_icon_action("■", (actions.on_stop_server)(command.id.clone()))
                    }),
                )
                .child(row_icon_action(
                    "⋯",
                    (actions.on_edit_command)(command.id.clone()),
                )),
        )
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

    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(4.0))
        .px_2()
        .py(px(2.0))
        .bg(rgb(if is_active {
            theme::PROJECT_ROW_BG
        } else if session.is_some() {
            theme::SIDEBAR_BG
        } else {
            theme::SIDEBAR_BG
        }))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(5.0))
                .on_mouse_down(
                    MouseButton::Left,
                    (actions.on_open_ssh_tab)(connection.id.clone()),
                )
                .child(icons::app_icon(icons::TERMINAL, 10.0, color))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(SharedString::from(connection.label.clone())),
                        )
                        .child(div().text_xs().text_color(rgb(theme::TEXT_DIM)).child(
                            SharedString::from(format!(
                                "{}@{}",
                                connection.username, connection.host
                            )),
                        )),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(div().text_xs().text_color(rgb(color)).child(label))
                .child(row_icon_action(
                    if session.is_some() { "■" } else { "▶" },
                    if session.is_some() {
                        (actions.on_disconnect_ssh)(connection.id.clone())
                    } else {
                        (actions.on_connect_ssh)(connection.id.clone())
                    },
                ))
                .child(row_icon_action(
                    "⋯",
                    (actions.on_edit_ssh)(connection.id.clone()),
                )),
        )
}

fn icon_button(
    label: &str,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .w(px(18.0))
        .h(px(18.0))
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .text_color(rgb(theme::TEXT_MUTED))
        .child(SharedString::from(label.to_string()))
        .on_mouse_down(MouseButton::Left, on_click)
}

fn row_icon_action(
    label: &str,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .min_w(px(10.0))
        .text_xs()
        .text_color(rgb(theme::TEXT_MUTED))
        .child(SharedString::from(label.to_string()))
        .on_mouse_down(MouseButton::Left, on_click)
}

fn text_action(
    label: &str,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .text_xs()
        .text_color(rgb(theme::TEXT_MUTED))
        .child(SharedString::from(label.to_string()))
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
        .child(icons::app_icon(icon_path, icon_size_px, color))
        .child(
            div()
                .text_xs()
                .text_color(rgb(color))
                .child(SharedString::from(label.to_string())),
        )
        .on_mouse_down(MouseButton::Left, on_click)
}

fn primary_button(
    label: &str,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .flex_1()
        .px_2()
        .py(px(5.0))
        .rounded_sm()
        .bg(rgb(theme::PROJECT_DOT))
        .text_xs()
        .text_color(rgb(theme::SELECTION_TEXT))
        .child(SharedString::from(label.to_string()))
        .on_mouse_down(MouseButton::Left, on_click)
}

fn section_label(label: &str) -> impl IntoElement {
    div()
        .px_1()
        .text_xs()
        .text_color(rgb(theme::TEXT_DIM))
        .child(SharedString::from(label.to_string()))
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
        SessionStatus::Stopped => "stopped",
        SessionStatus::Starting => "starting",
        SessionStatus::Running => "running",
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
        "live"
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
