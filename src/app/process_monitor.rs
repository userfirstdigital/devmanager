use crate::state::{
    AppState, ProcessResourceNode, RuntimeState, SessionKind, SessionRuntimeState, SessionStatus,
};
use crate::{icons, theme};
use gpui::{
    anchored, deferred, div, px, rgb, AnyElement, App, Corner, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    Window,
};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Default)]
pub struct ProcessMonitorState {
    pub expanded_sessions: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub enum ProcessMonitorAction {
    Close,
    ToggleSession(String),
    KillProcess { session_id: String, pid: u32 },
    KillProcessTree { session_id: String, pid: u32 },
    StopSession(String),
}

pub struct ProcessMonitorActions<'a> {
    pub on_action:
        &'a dyn Fn(ProcessMonitorAction) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
}

pub fn render_process_monitor(
    state: &ProcessMonitorState,
    app_state: &AppState,
    runtime: &RuntimeState,
    actions: ProcessMonitorActions<'_>,
) -> AnyElement {
    let (open_terminals, total_memory) = monitor_totals(runtime);
    let sessions = process_monitor_entries(app_state, runtime);
    let description = format!(
        "{open_terminals} terminal{} · {} total memory",
        if open_terminals == 1 { "" } else { "s" },
        format_memory(total_memory)
    );

    let body = if sessions.is_empty() {
        div()
            .text_sm()
            .text_color(rgb(theme::TEXT_SUBTLE))
            .child("No managed terminals or tracked subprocesses right now.")
            .into_any_element()
    } else {
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .children(
                sessions
                    .into_iter()
                    .map(|entry| render_session_card(state, entry, &actions).into_any_element()),
            )
            .into_any_element()
    };

    deferred(
        anchored().snap_to_window().anchor(Corner::TopLeft).child(
            div()
                .id("process-monitor-backdrop")
                .occlude()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(
                    MouseButton::Left,
                    wrap_pointer_action(
                        PointerTarget::Backdrop,
                        (actions.on_action)(ProcessMonitorAction::Close),
                    ),
                )
                .child(
                    div()
                        .id("process-monitor-frame")
                        .w(px(820.0))
                        .max_h(px(680.0))
                        .rounded_md()
                        .bg(rgb(theme::EDITOR_CARD_BG))
                        .border_1()
                        .border_color(rgb(theme::BORDER_PRIMARY))
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                            if pointer_disposition(PointerTarget::Panel)
                                == PointerDisposition::Consume
                            {
                                cx.stop_propagation();
                            }
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(16.0))
                                .px(px(18.0))
                                .py(px(10.0))
                                .bg(rgb(theme::TOPBAR_BG))
                                .border_b_1()
                                .border_color(rgb(theme::BORDER_PRIMARY))
                                .child(
                                    div()
                                        .flex_1()
                                        .flex()
                                        .items_center()
                                        .gap(px(12.0))
                                        .child(
                                            div()
                                                .size(px(10.0))
                                                .rounded_full()
                                                .bg(rgb(theme::PRIMARY)),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .flex_col()
                                                .gap(px(2.0))
                                                .child(
                                                    div()
                                                        .text_sm()
                                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                                        .text_color(rgb(theme::TEXT_PRIMARY))
                                                        .child("Process Monitor"),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(rgb(theme::TEXT_SUBTLE))
                                                        .child(SharedString::from(description)),
                                                ),
                                        ),
                                )
                                .child(render_text_button(
                                    "Close",
                                    theme::TEXT_MUTED,
                                    (actions.on_action)(ProcessMonitorAction::Close),
                                )),
                        )
                        .child(
                            div()
                                .flex_1()
                                .id("process-monitor-scroll")
                                .overflow_y_scroll()
                                .scrollbar_width(px(6.0))
                                .child(
                                    div()
                                        .px(px(20.0))
                                        .py(px(10.0))
                                        .flex()
                                        .flex_col()
                                        .gap(px(12.0))
                                        .child(body),
                                ),
                        ),
                ),
        ),
    )
    .with_priority(2)
    .into_any_element()
}

fn render_session_card(
    state: &ProcessMonitorState,
    entry: ProcessMonitorEntry,
    actions: &ProcessMonitorActions<'_>,
) -> impl IntoElement {
    let session_id = entry.session_id.clone();
    let expanded = state.expanded_sessions.contains(&session_id);
    let label = entry.label.clone();
    let kind_label = entry.kind_label;
    let status_label = entry.status_label;
    let project_name = entry.project_name.clone();
    let root_pid = entry.pid;
    let cpu = entry.cpu_percent;
    let memory = entry.memory_bytes;
    let process_count = entry.process_count;
    let unreaped = entry.unreaped;
    let processes = entry.processes;

    div()
        .rounded_sm()
        .border_1()
        .border_color(rgb(if unreaped {
            theme::DANGER_TEXT
        } else {
            theme::BORDER_PRIMARY
        }))
        .bg(rgb(theme::EDITOR_FIELD_BG))
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(10.0))
                .px(px(10.0))
                .py(px(7.0))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            wrap_pointer_action(
                                PointerTarget::Control,
                                (actions.on_action)(ProcessMonitorAction::ToggleSession(
                                    session_id.clone(),
                                )),
                            ),
                        )
                        .child(icons::app_icon(
                            if expanded {
                                icons::CHEVRON_DOWN
                            } else {
                                icons::CHEVRON_RIGHT
                            },
                            12.0,
                            theme::TEXT_SUBTLE,
                        ))
                        .child(icons::app_icon(
                            session_kind_icon(entry.kind),
                            12.0,
                            session_kind_color(entry.kind),
                        ))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(1.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.0))
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(rgb(theme::TEXT_PRIMARY))
                                                .child(SharedString::from(label)),
                                        )
                                        .child(
                                            div()
                                                .px(px(5.0))
                                                .py(px(1.0))
                                                .rounded_sm()
                                                .bg(rgb(theme::PRIMARY_MUTED))
                                                .text_xs()
                                                .text_color(rgb(session_kind_color(entry.kind)))
                                                .child(kind_label),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(if unreaped {
                                                    theme::DANGER_TEXT
                                                } else {
                                                    theme::TEXT_MUTED
                                                }))
                                                .child(status_label),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(theme::TEXT_SUBTLE))
                                        .child(SharedString::from(format!(
                                            "{project_name} · {} · {process_count} proc · {:.1}% CPU · {}",
                                            root_pid
                                                .map(|pid| format!("pid {pid}"))
                                                .unwrap_or_else(|| "no root pid".to_string()),
                                            cpu,
                                            format_memory(memory),
                                        ))),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .children(unreaped.then(|| {
                            div()
                                .text_xs()
                                .text_color(rgb(theme::DANGER_TEXT))
                                .child("unreaped")
                                .into_any_element()
                        }))
                        .child(render_text_button(
                            "Stop",
                            theme::DANGER_TEXT,
                            (actions.on_action)(ProcessMonitorAction::StopSession(
                                session_id.clone(),
                            )),
                        )),
                ),
        )
        .children(expanded.then(|| {
            div()
                .border_t_1()
                .border_color(rgb(theme::BORDER_SECONDARY))
                .px(px(12.0))
                .py(px(8.0))
                .flex()
                .flex_col()
                .gap(px(4.0))
                .children(if processes.is_empty() {
                    vec![div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child("No subprocess details yet.")
                        .into_any_element()]
                } else {
                    processes
                        .into_iter()
                        .map(|node| {
                            render_process_row(&session_id, node, root_pid, actions)
                                .into_any_element()
                        })
                        .collect()
                })
        }))
}

fn render_process_row(
    session_id: &str,
    node: ProcessResourceNode,
    root_pid: Option<u32>,
    actions: &ProcessMonitorActions<'_>,
) -> impl IntoElement {
    let is_root = root_pid == Some(node.pid);
    let indent = if is_root { 0.0 } else { 16.0 };
    let session_id = session_id.to_string();
    let pid = node.pid;

    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .pl(px(indent))
        .py(px(4.0))
        .child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .gap(px(8.0))
                .min_w_0()
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(SharedString::from(node.name)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_DIM))
                        .child(SharedString::from(format!("pid {}", node.pid))),
                )
                .child(div().text_xs().text_color(rgb(theme::TEXT_SUBTLE)).child(
                    SharedString::from(format!(
                        "{:.1}% · {}",
                        node.cpu_percent,
                        format_memory(node.memory_bytes)
                    )),
                )),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(render_text_button(
                    "Kill",
                    theme::WARNING_TEXT,
                    (actions.on_action)(ProcessMonitorAction::KillProcess {
                        session_id: session_id.clone(),
                        pid,
                    }),
                ))
                .child(render_text_button(
                    "Kill tree",
                    theme::DANGER_TEXT,
                    (actions.on_action)(ProcessMonitorAction::KillProcessTree { session_id, pid }),
                )),
        )
}

fn render_text_button(
    label: &str,
    color: u32,
    handler: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .px(px(8.0))
        .py(px(3.0))
        .rounded_sm()
        .border_1()
        .border_color(rgb(theme::BORDER_PRIMARY))
        .text_xs()
        .text_color(rgb(color))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::BUTTON_HOVER_BG)))
        .child(SharedString::from(label.to_string()))
        .on_mouse_down(
            MouseButton::Left,
            wrap_pointer_action(PointerTarget::Control, handler),
        )
}

fn monitor_sessions(runtime: &RuntimeState) -> Vec<SessionRuntimeState> {
    let mut sessions: Vec<_> = runtime
        .sessions
        .values()
        .filter(|session| {
            session.status.is_live()
                || session.reap_incomplete
                || !session.resources.process_ids.is_empty()
        })
        .cloned()
        .collect();
    sessions.sort_by(|left, right| {
        session_label(left)
            .cmp(&session_label(right))
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    sessions
}

fn process_monitor_entries(
    app_state: &AppState,
    runtime: &RuntimeState,
) -> Vec<ProcessMonitorEntry> {
    let mut entries = monitor_sessions(runtime)
        .into_iter()
        .map(|session| {
            let process_count = session.resources.process_count.max(
                session
                    .resources
                    .processes
                    .len()
                    .max(session.resources.process_ids.len()) as u32,
            );
            ProcessMonitorEntry {
                session_id: session.session_id.clone(),
                label: session_label(&session),
                project_name: session_project_name(app_state, &session),
                kind: session.session_kind,
                kind_label: session_kind_label(session.session_kind),
                status_label: session_status_label(&session),
                pid: session.pid,
                cpu_percent: session.resources.cpu_percent,
                memory_bytes: session.resources.memory_bytes,
                process_count,
                unreaped: session.reap_incomplete,
                processes: ordered_process_nodes(&session),
            }
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        process_monitor_priority(left)
            .cmp(&process_monitor_priority(right))
            .then_with(|| left.project_name.cmp(&right.project_name))
            .then_with(|| left.kind_label.cmp(right.kind_label))
            .then_with(|| left.label.cmp(&right.label))
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    entries
}

fn process_monitor_priority(entry: &ProcessMonitorEntry) -> u8 {
    if entry.unreaped {
        0
    } else {
        1
    }
}

fn session_project_name(app_state: &AppState, session: &SessionRuntimeState) -> String {
    let project_id = session.project_id.as_deref().or_else(|| {
        session
            .tab_id
            .as_deref()
            .and_then(|tab_id| app_state.find_tab(tab_id))
            .map(|tab| tab.project_id.as_str())
    });
    if let Some(project) = project_id.and_then(|id| app_state.find_project(id)) {
        return project.name.clone();
    }
    if let Some(command) = session
        .command_id
        .as_deref()
        .and_then(|command_id| app_state.find_command(command_id))
    {
        return command.project.name.clone();
    }
    "Unknown project".to_string()
}

fn session_status_label(session: &SessionRuntimeState) -> &'static str {
    if session.reap_incomplete {
        return "Unreaped";
    }
    match session.status {
        SessionStatus::Stopped => "Stopped",
        SessionStatus::Starting => "Starting",
        SessionStatus::Running => "Running",
        SessionStatus::Stopping => "Stopping",
        SessionStatus::Crashed => "Crashed",
        SessionStatus::Exited => "Exited",
        SessionStatus::Failed => "Failed",
    }
}

fn ordered_process_nodes(session: &SessionRuntimeState) -> Vec<ProcessResourceNode> {
    if !session.resources.processes.is_empty() {
        let mut nodes = session.resources.processes.clone();
        if let Some(root_pid) = session.pid {
            nodes.sort_by_key(|node| (node.pid != root_pid, node.pid));
        } else {
            nodes.sort_by_key(|node| node.pid);
        }
        return nodes;
    }
    session
        .resources
        .process_ids
        .iter()
        .map(|pid| ProcessResourceNode {
            pid: *pid,
            parent_pid: None,
            name: format!("pid-{pid}"),
            cpu_percent: 0.0,
            memory_bytes: 0,
        })
        .collect()
}

fn monitor_totals(runtime: &RuntimeState) -> (usize, u64) {
    runtime
        .sessions
        .values()
        .filter(|session| session.status.is_live() || session.reap_incomplete)
        .fold((0, 0), |(count, memory), session| {
            (
                count + 1,
                memory.saturating_add(session.resources.memory_bytes),
            )
        })
}

fn session_label(session: &SessionRuntimeState) -> String {
    if let Some(title) = session.title.as_deref().filter(|value| !value.is_empty()) {
        return title.to_string();
    }
    if let Some(command_id) = session.command_id.as_deref() {
        return command_id.to_string();
    }
    if let Some(tab_id) = session.tab_id.as_deref() {
        return tab_id.to_string();
    }
    session.session_id.clone()
}

fn session_kind_label(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Shell => "Shell",
        SessionKind::Server => "Server",
        SessionKind::Claude => "Claude",
        SessionKind::Codex => "Codex",
        SessionKind::Ssh => "SSH",
    }
}

fn session_kind_icon(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Shell => icons::TERMINAL,
        SessionKind::Server => icons::SERVER,
        SessionKind::Claude | SessionKind::Codex => icons::BOT,
        SessionKind::Ssh => icons::GLOBE,
    }
}

fn session_kind_color(kind: SessionKind) -> u32 {
    match kind {
        SessionKind::Shell => theme::TEXT_MUTED,
        SessionKind::Server => theme::SUCCESS_TEXT,
        SessionKind::Claude | SessionKind::Codex => theme::AI_DOT,
        SessionKind::Ssh => theme::SSH_DOT,
    }
}

fn format_memory(bytes: u64) -> String {
    let mb = bytes as f64 / 1024.0 / 1024.0;
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{:.0} MB", mb)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        monitor_sessions, ordered_process_nodes, pointer_disposition, process_monitor_entries,
        session_label, PointerDisposition, PointerTarget,
    };
    use crate::models::Project;
    use crate::state::{
        AppState, ProcessResourceNode, ResourceSnapshot, RuntimeState, SessionDimensions,
        SessionKind, SessionRuntimeState, SessionStatus,
    };
    use crate::terminal::session::TerminalBackend;
    use std::path::PathBuf;

    #[test]
    fn monitor_sessions_includes_live_and_unreaped() {
        let mut runtime = RuntimeState::new(false);

        let mut live = SessionRuntimeState::new(
            "live-1",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        live.status = SessionStatus::Running;
        live.resources.memory_bytes = 10;

        let mut unreaped = SessionRuntimeState::new(
            "dead-1",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        unreaped.status = SessionStatus::Failed;
        unreaped.reap_incomplete = true;
        unreaped.resources.process_ids = vec![99];

        let mut ignored = SessionRuntimeState::new(
            "idle-1",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        ignored.status = SessionStatus::Stopped;

        runtime.sessions.insert(live.session_id.clone(), live);
        runtime
            .sessions
            .insert(unreaped.session_id.clone(), unreaped);
        runtime.sessions.insert(ignored.session_id.clone(), ignored);

        let sessions = monitor_sessions(&runtime);
        assert_eq!(sessions.len(), 2);
        assert!(sessions
            .iter()
            .any(|session| session.session_id == "live-1"));
        assert!(sessions
            .iter()
            .any(|session| session.session_id == "dead-1"));
    }

    #[test]
    fn ordered_process_nodes_prefers_root_first() {
        let mut session = SessionRuntimeState::new(
            "session-1",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.pid = Some(10);
        session.resources = ResourceSnapshot {
            processes: vec![
                ProcessResourceNode {
                    pid: 20,
                    parent_pid: Some(10),
                    name: "node".to_string(),
                    cpu_percent: 1.0,
                    memory_bytes: 100,
                },
                ProcessResourceNode {
                    pid: 10,
                    parent_pid: None,
                    name: "shell".to_string(),
                    cpu_percent: 0.1,
                    memory_bytes: 50,
                },
            ],
            ..Default::default()
        };

        let nodes = ordered_process_nodes(&session);
        assert_eq!(nodes[0].pid, 10);
        assert_eq!(nodes[1].pid, 20);
        assert_eq!(session_label(&session), "session-1");
    }

    #[test]
    fn process_monitor_entries_identify_project_kind_and_status_at_compact_density() {
        let mut app_state = AppState::default();
        app_state.config.projects = vec![
            Project {
                id: "portal".to_string(),
                name: "360 Portal".to_string(),
                ..Project::default()
            },
            Project {
                id: "devmanager".to_string(),
                name: "DevManager".to_string(),
                ..Project::default()
            },
        ];
        let mut runtime = RuntimeState::new(false);

        for (id, project_id, kind) in [
            ("codex", "portal", SessionKind::Codex),
            ("server", "devmanager", SessionKind::Server),
            ("ssh", "missing", SessionKind::Ssh),
            ("claude", "portal", SessionKind::Claude),
            ("shell", "devmanager", SessionKind::Shell),
        ] {
            let mut session = SessionRuntimeState::new(
                id,
                PathBuf::from("."),
                SessionDimensions::default(),
                TerminalBackend::PortablePtyFeedingAlacritty,
            );
            session.status = SessionStatus::Running;
            session.session_kind = kind;
            session.project_id = Some(project_id.to_string());
            session.title = Some("Work".to_string());
            session.pid = Some(100);
            session.resources.process_count = 2;
            session.resources.memory_bytes = 20 * 1024 * 1024;
            runtime.sessions.insert(id.to_string(), session);
        }

        let entries = process_monitor_entries(&app_state, &runtime);
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].project_name, "360 Portal");
        assert_eq!(entries[0].kind_label, "Claude");
        assert_eq!(entries[1].project_name, "360 Portal");
        assert_eq!(entries[1].kind_label, "Codex");
        assert_eq!(entries[4].project_name, "Unknown project");
        assert_eq!(entries[4].kind_label, "SSH");
        assert!(entries.iter().all(|entry| entry.status_label == "Running"));
        assert!(entries.iter().all(|entry| entry.process_count == 2));
    }

    #[test]
    fn process_monitor_entries_sort_problem_sessions_before_live_sessions() {
        let app_state = AppState::default();
        let mut runtime = RuntimeState::new(false);
        let mut running = SessionRuntimeState::new(
            "running",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        running.status = SessionStatus::Running;
        let mut unreaped = SessionRuntimeState::new(
            "unreaped",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        unreaped.status = SessionStatus::Failed;
        unreaped.reap_incomplete = true;
        unreaped.resources.process_ids = vec![99];
        runtime.sessions.insert(running.session_id.clone(), running);
        runtime
            .sessions
            .insert(unreaped.session_id.clone(), unreaped);

        let entries = process_monitor_entries(&app_state, &runtime);
        assert_eq!(entries[0].session_id, "unreaped");
        assert_eq!(entries[0].status_label, "Unreaped");
        assert_eq!(entries[1].session_id, "running");
    }

    #[test]
    fn modal_interactions_consume_internal_pointer_events() {
        assert_eq!(
            pointer_disposition(PointerTarget::Backdrop),
            PointerDisposition::Close
        );
        assert_eq!(
            pointer_disposition(PointerTarget::Panel),
            PointerDisposition::Consume
        );
        assert_eq!(
            pointer_disposition(PointerTarget::Control),
            PointerDisposition::Consume
        );
    }
}

#[derive(Debug, Clone)]
struct ProcessMonitorEntry {
    session_id: String,
    label: String,
    project_name: String,
    kind: SessionKind,
    kind_label: &'static str,
    status_label: &'static str,
    pid: Option<u32>,
    cpu_percent: f32,
    memory_bytes: u64,
    process_count: u32,
    unreaped: bool,
    processes: Vec<ProcessResourceNode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerTarget {
    Backdrop,
    Panel,
    Control,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerDisposition {
    Close,
    Consume,
}

fn pointer_disposition(target: PointerTarget) -> PointerDisposition {
    match target {
        PointerTarget::Backdrop => PointerDisposition::Close,
        PointerTarget::Panel | PointerTarget::Control => PointerDisposition::Consume,
    }
}

fn wrap_pointer_action(
    target: PointerTarget,
    handler: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
    Box::new(move |event, window, cx| {
        if pointer_disposition(target) == PointerDisposition::Consume {
            cx.stop_propagation();
        }
        handler(event, window, cx);
    })
}
