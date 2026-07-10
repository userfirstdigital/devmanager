use crate::state::{ProcessResourceNode, RuntimeState, SessionKind, SessionRuntimeState};
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
    runtime: &RuntimeState,
    actions: ProcessMonitorActions<'_>,
) -> AnyElement {
    let (open_terminals, total_memory) = monitor_totals(runtime);
    let sessions = monitor_sessions(runtime);
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
            .gap(px(10.0))
            .children(sessions.into_iter().map(|session| {
                render_session_card(state, session, &actions).into_any_element()
            }))
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
                .on_mouse_down(MouseButton::Left, (actions.on_action)(ProcessMonitorAction::Close))
                .child(
                    div()
                        .id("process-monitor-frame")
                        .w(px(860.0))
                        .max_h(px(720.0))
                        .rounded_md()
                        .bg(rgb(theme::EDITOR_CARD_BG))
                        .border_1()
                        .border_color(rgb(theme::BORDER_PRIMARY))
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        .on_mouse_down(MouseButton::Left, |_, _, _| {})
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(16.0))
                                .px(px(18.0))
                                .py(px(14.0))
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
                                        .py(px(16.0))
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
    session: SessionRuntimeState,
    actions: &ProcessMonitorActions<'_>,
) -> impl IntoElement {
    let session_id = session.session_id.clone();
    let expanded = state.expanded_sessions.contains(&session_id);
    let label = session_label(&session);
    let kind_label = session_kind_label(session.session_kind);
    let root_pid = session.pid;
    let cpu = session.resources.cpu_percent;
    let memory = session.resources.memory_bytes;
    let process_count = session.resources.process_count.max(
        session
            .resources
            .processes
            .len()
            .max(session.resources.process_ids.len()) as u32,
    );
    let unreaped = session.reap_incomplete;
    let processes = ordered_process_nodes(&session);

    div()
        .rounded_md()
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
                .gap(px(12.0))
                .px(px(12.0))
                .py(px(10.0))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            (actions.on_action)(ProcessMonitorAction::ToggleSession(
                                session_id.clone(),
                            )),
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
                            session_kind_icon(session.session_kind),
                            12.0,
                            session_kind_color(session.session_kind),
                        ))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(rgb(theme::TEXT_PRIMARY))
                                        .child(SharedString::from(label)),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(theme::TEXT_SUBTLE))
                                        .child(SharedString::from(format!(
                                            "{kind_label} · {} · {process_count} proc · {:.1}% CPU · {}",
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
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_SUBTLE))
                        .child(SharedString::from(format!(
                            "{:.1}% · {}",
                            node.cpu_percent,
                            format_memory(node.memory_bytes)
                        ))),
                ),
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
        .on_mouse_down(MouseButton::Left, handler)
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
    use super::{monitor_sessions, ordered_process_nodes, session_label};
    use crate::state::{
        ProcessResourceNode, ResourceSnapshot, RuntimeState, SessionDimensions,
        SessionRuntimeState, SessionStatus,
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
        runtime
            .sessions
            .insert(ignored.session_id.clone(), ignored);

        let sessions = monitor_sessions(&runtime);
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|session| session.session_id == "live-1"));
        assert!(sessions.iter().any(|session| session.session_id == "dead-1"));
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
}
