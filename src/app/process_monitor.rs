use crate::process::registry::ManagedProcessFence;
use crate::state::{
    aggregate_memory_snapshots, AppState, ProcessResourceNode, ResourceMemoryMetric,
    ResourceMemoryTotal, ResourceMetricValueState, RuntimeState, SessionKind, SessionRuntimeState,
    SessionStatus,
};
use crate::{icons, theme};
use gpui::{
    anchored, deferred, div, px, rgb, AnyElement, App, Corner, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    Window,
};
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct ProcessMonitorState {
    pub expanded_sessions: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub enum ProcessMonitorAction {
    Close,
    ToggleSession(String),
    KillProcess {
        session_id: String,
        pid: u32,
        fence: ManagedProcessFence,
    },
    KillProcessTree {
        session_id: String,
        pid: u32,
        fence: ManagedProcessFence,
    },
    StopSession(String),
}

pub struct ProcessMonitorActions<'a> {
    pub on_action:
        &'a dyn Fn(ProcessMonitorAction) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    /// Resolves the fence published with the current exact Job-member
    /// snapshot. No fence means the row is diagnostic-only and cannot expose
    /// a close action.
    pub fence_for_session: &'a dyn Fn(&str) -> Option<ManagedProcessFence>,
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
        "{open_terminals} terminal{} · {} total",
        if open_terminals == 1 { "" } else { "s" },
        format_metric_memory(
            total_memory.bytes,
            total_memory.metric,
            total_memory.value_state,
        ),
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
    let memory_metric = entry.memory_metric;
    let process_count = entry.process_count;
    let metrics_unavailable = entry.metrics_unavailable;
    let metrics_status = entry.metrics_status;
    let cpu_value_state = entry.cpu_value_state;
    let memory_value_state = entry.memory_value_state;
    let metrics_stale = entry.metrics_stale;
    let core_equivalent_percent = entry.core_equivalent_percent;
    let unreaped = entry.unreaped;
    let logical_cpu_count = entry.logical_cpu_count;
    let processes = entry.processes;
    let process_fence = (actions.fence_for_session)(&session_id);

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
                                        )
                                        .children((metrics_unavailable || metrics_status != crate::domain::snapshot::ProcessMetricStatus::Complete).then(|| {
                                            div()
                                                .px(px(5.0))
                                                .py(px(1.0))
                                                .rounded_sm()
                                                .bg(rgb(theme::PRIMARY_MUTED))
                                                .text_xs()
                                                .text_color(rgb(theme::WARNING_TEXT))
                                                .child(SharedString::from(process_metrics_label(metrics_status)))
                                                .into_any_element()
                                        })),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(theme::TEXT_SUBTLE))
                                        .child(SharedString::from(format!(
                                            "{project_name} · {} · {process_count} proc · {} · {}{}",
                                            root_pid
                                                .map(|pid| format!("pid {pid}"))
                                                .unwrap_or_else(|| "no root pid".to_string()),
                                            format_metric_cpu_detail(
                                                cpu,
                                                core_equivalent_percent,
                                                logical_cpu_count,
                                                cpu_value_state,
                                            ),
                                            format_metric_memory(
                                                memory,
                                                memory_metric,
                                                memory_value_state,
                                            ),
                                            if metrics_stale {
                                                " · stale"
                                            } else if metrics_unavailable {
                                                " · partial"
                                            } else {
                                                ""
                                            },
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
                            render_process_row(
                                &session_id,
                                node,
                                root_pid,
                                logical_cpu_count,
                                process_fence.as_ref(),
                                actions,
                            )
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
    logical_cpu_count: u32,
    process_fence: Option<&ManagedProcessFence>,
    actions: &ProcessMonitorActions<'_>,
) -> impl IntoElement {
    let is_root = root_pid == Some(node.pid);
    let indent = if is_root { 0.0 } else { 16.0 };
    let session_id = session_id.to_string();
    let pid = node.pid;
    let resource_label = node
        .resource_kind
        .clone()
        .unwrap_or_else(|| "resource".to_string());
    let command_label = node
        .command_label
        .clone()
        .unwrap_or_else(|| node.name.clone());
    let metrics_label = process_metrics_label(node.metrics_status);
    let identity_label = node
        .creation_time_100ns
        .map(|creation| format!("pid {pid} · start {creation}"))
        .unwrap_or_else(|| format!("pid {pid} · identity pending"));
    let executable_label = node
        .executable
        .as_deref()
        .and_then(|path| Path::new(path).file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "executable unknown".to_string());

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
                        .child(SharedString::from(identity_label)),
                )
                .child(div().text_xs().text_color(rgb(theme::TEXT_SUBTLE)).child(
                    SharedString::from(format!(
                        "{} · {} · {} · {} · {} child{}{}",
                        format_metric_cpu_detail(
                            node.cpu_percent,
                            node.core_equivalent_percent,
                            logical_cpu_count,
                            node.cpu_value_state,
                        ),
                        format_metric_memory(
                            node.memory_bytes,
                            node.memory_metric,
                            node.memory_value_state,
                        ),
                        resource_label,
                        command_label,
                        executable_label,
                        node.child_count,
                        if metrics_label.is_empty() {
                            "".to_string()
                        } else {
                            format!(" · {metrics_label}")
                        }
                    )),
                )),
        )
        .child(if let Some(process_fence) = process_fence {
            let kill_action =
                build_process_monitor_kill_action(&session_id, pid, Some(process_fence), false)
                    .expect("a fence is required for a process close action");
            let kill_tree_action =
                build_process_monitor_kill_action(&session_id, pid, Some(process_fence), true)
                    .expect("a fence is required for a process close action");
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(render_text_button(
                    "Kill",
                    theme::WARNING_TEXT,
                    (actions.on_action)(kill_action),
                ))
                .child(render_text_button(
                    "Kill tree",
                    theme::DANGER_TEXT,
                    (actions.on_action)(kill_tree_action),
                ))
                .into_any_element()
        } else {
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_DIM))
                .child("Exact close unavailable")
                .into_any_element()
        })
}

fn build_process_monitor_kill_action(
    session_id: &str,
    pid: u32,
    process_fence: Option<&ManagedProcessFence>,
    kill_tree: bool,
) -> Option<ProcessMonitorAction> {
    let fence = process_fence?.clone();
    Some(if kill_tree {
        ProcessMonitorAction::KillProcessTree {
            session_id: session_id.to_string(),
            pid,
            fence,
        }
    } else {
        ProcessMonitorAction::KillProcess {
            session_id: session_id.to_string(),
            pid,
            fence,
        }
    })
}

fn process_metrics_label(status: crate::domain::snapshot::ProcessMetricStatus) -> &'static str {
    match status {
        crate::domain::snapshot::ProcessMetricStatus::Complete => "",
        crate::domain::snapshot::ProcessMetricStatus::Partial => "partial metrics",
        crate::domain::snapshot::ProcessMetricStatus::Unknown => "metrics unknown",
        crate::domain::snapshot::ProcessMetricStatus::Failed => "metrics failed",
    }
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
                core_equivalent_percent: session.resources.core_equivalent_percent,
                memory_bytes: session.resources.memory_bytes,
                memory_metric: session.resources.memory_metric,
                process_count,
                metrics_unavailable: session.resources.metrics_unavailable,
                metrics_status: session.resources.metrics_status,
                cpu_value_state: session.resources.cpu_value_state,
                memory_value_state: session.resources.memory_value_state,
                metrics_stale: session.resources.metrics_stale,
                unreaped: session.reap_incomplete,
                logical_cpu_count: session.resources.logical_cpu_count.max(1),
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
    let memory_metric = session.resources.memory_metric;
    session
        .resources
        .process_ids
        .iter()
        .map(|pid| ProcessResourceNode {
            pid: *pid,
            parent_pid: None,
            name: format!("pid-{pid} (compatibility-only observation)"),
            cpu_percent: 0.0,
            core_equivalent_percent: 0.0,
            memory_bytes: 0,
            memory_metric,
            creation_time_100ns: None,
            executable: None,
            command_label: None,
            command_arg_count: 0,
            command_arg_bytes: 0,
            resource_id: None,
            resource_kind: None,
            child_count: 0,
            lifecycle: crate::state::ProcessResourceLifecycle::Unknown,
            metrics_status: crate::domain::snapshot::ProcessMetricStatus::Unknown,
            metric_values: crate::state::ResourceMetricValueState::Unavailable,
            cpu_value_state: crate::state::ResourceMetricValueState::Unavailable,
            memory_value_state: crate::state::ResourceMetricValueState::Unavailable,
            sampling_generation: 0,
        })
        .collect()
}

fn monitor_totals(runtime: &RuntimeState) -> (usize, ResourceMemoryTotal) {
    let relevant = || {
        runtime
            .sessions
            .values()
            .filter(|session| session.status.is_live() || session.reap_incomplete)
    };
    let count = relevant().count();
    let memory = aggregate_memory_snapshots(relevant().map(|session| &session.resources));
    (count, memory)
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

fn format_cpu_detail_with_core(
    system_cpu_percent: f32,
    core_equivalent_percent: f32,
    logical_cpu_count: u32,
) -> String {
    let cores = if core_equivalent_percent.is_finite() && core_equivalent_percent > 0.0 {
        core_equivalent_percent / 100.0
    } else {
        crate::state::equivalent_cpu_cores(system_cpu_percent, logical_cpu_count)
    };
    format!("{system_cpu_percent:.1}% machine · {cores:.2} cores")
}

fn format_metric_cpu_detail(
    system_cpu_percent: f32,
    core_equivalent_percent: f32,
    logical_cpu_count: u32,
    value_state: ResourceMetricValueState,
) -> String {
    match value_state {
        ResourceMetricValueState::Observed => format_cpu_detail_with_core(
            system_cpu_percent,
            core_equivalent_percent,
            logical_cpu_count,
        ),
        ResourceMetricValueState::Partial => format!(
            "{} (partial)",
            format_cpu_detail_with_core(
                system_cpu_percent,
                core_equivalent_percent,
                logical_cpu_count,
            )
        ),
        ResourceMetricValueState::LastKnown => format!(
            "{} (last known)",
            format_cpu_detail_with_core(
                system_cpu_percent,
                core_equivalent_percent,
                logical_cpu_count,
            )
        ),
        ResourceMetricValueState::Unavailable => "CPU unavailable".to_string(),
    }
}

fn format_metric_memory(
    memory_bytes: u64,
    memory_metric: ResourceMemoryMetric,
    value_state: ResourceMetricValueState,
) -> String {
    match value_state {
        ResourceMetricValueState::Observed => {
            format!("{} {}", memory_metric.label(), format_memory(memory_bytes))
        }
        ResourceMetricValueState::Partial => format!(
            "{} {} (partial)",
            memory_metric.label(),
            format_memory(memory_bytes)
        ),
        ResourceMetricValueState::LastKnown => format!(
            "{} {} (last known)",
            memory_metric.label(),
            format_memory(memory_bytes)
        ),
        ResourceMetricValueState::Unavailable => {
            format!("{} unavailable", memory_metric.label())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_process_monitor_kill_action, format_cpu_detail_with_core, format_metric_cpu_detail,
        format_metric_memory, monitor_sessions, monitor_totals, ordered_process_nodes,
        pointer_disposition, process_monitor_entries, session_label, PointerDisposition,
        PointerTarget,
    };
    use crate::domain::id::ResourceId;
    use crate::domain::operation::ResourceFence;
    use crate::models::Project;
    use crate::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
    use crate::process::registry::ManagedProcessFence;
    use crate::state::{
        AppState, ProcessResourceNode, ResourceSnapshot, RuntimeState, SessionDimensions,
        SessionKind, SessionRuntimeState, SessionStatus,
    };
    use crate::terminal::session::TerminalBackend;
    use std::path::PathBuf;

    fn test_process_fence() -> ManagedProcessFence {
        let identity = ManagedProcessIdentity::new(
            ManagedProcessId::new(42, 7).expect("test process identity"),
            std::env::current_exe().expect("test executable"),
        )
        .expect("canonical test executable");
        ManagedProcessFence::new(
            ResourceFence::new(ResourceId::new(), 3),
            ProcessOwner::Host,
            identity,
        )
    }

    #[test]
    fn process_monitor_kill_actions_fail_closed_without_fence() {
        assert!(build_process_monitor_kill_action("session", 42, None, false).is_none());
        assert!(build_process_monitor_kill_action("session", 42, None, true).is_none());
    }

    #[test]
    fn process_monitor_kill_actions_carry_exact_fence_and_diagnostic_pid() {
        let fence = test_process_fence();
        let action = build_process_monitor_kill_action("session", 9_999, Some(&fence), false)
            .expect("fenced process action");
        match action {
            super::ProcessMonitorAction::KillProcess {
                session_id,
                pid,
                fence: action_fence,
            } => {
                assert_eq!(session_id, "session");
                assert_eq!(pid, 9_999);
                assert_eq!(action_fence, fence);
            }
            other => panic!("unexpected action: {other:?}"),
        }

        let tree_action = build_process_monitor_kill_action("session", 10_000, Some(&fence), true)
            .expect("fenced tree action");
        match tree_action {
            super::ProcessMonitorAction::KillProcessTree {
                session_id,
                pid,
                fence: action_fence,
            } => {
                assert_eq!(session_id, "session");
                assert_eq!(pid, 10_000);
                assert_eq!(action_fence, fence);
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

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
    fn monitor_total_excludes_stale_memory_and_marks_current_subset_partial() {
        let mut runtime = RuntimeState::new(false);
        for (id, bytes, state) in [
            (
                "current",
                64,
                crate::state::ResourceMetricValueState::Observed,
            ),
            (
                "stale",
                128,
                crate::state::ResourceMetricValueState::LastKnown,
            ),
        ] {
            let mut session = SessionRuntimeState::new(
                id,
                PathBuf::from("."),
                SessionDimensions::default(),
                TerminalBackend::PortablePtyFeedingAlacritty,
            );
            session.status = SessionStatus::Running;
            session.resources.memory_bytes = bytes;
            session.resources.memory_value_state = state;
            runtime.sessions.insert(id.to_string(), session);
        }

        let (count, total) = monitor_totals(&runtime);
        assert_eq!(count, 2);
        assert_eq!(total.bytes, 64);
        assert_eq!(
            total.value_state,
            crate::state::ResourceMetricValueState::Partial
        );
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
                    core_equivalent_percent: 1.0,
                    memory_bytes: 100,
                    memory_metric: crate::state::ResourceMemoryMetric::PrivateResident,
                    creation_time_100ns: None,
                    executable: None,
                    command_label: Some("node".to_string()),
                    command_arg_count: 0,
                    command_arg_bytes: 0,
                    resource_id: None,
                    resource_kind: None,
                    child_count: 0,
                    lifecycle: crate::state::ProcessResourceLifecycle::Running,
                    metrics_status: crate::domain::snapshot::ProcessMetricStatus::Complete,
                    metric_values: crate::state::ResourceMetricValueState::Observed,
                    cpu_value_state: crate::state::ResourceMetricValueState::Observed,
                    memory_value_state: crate::state::ResourceMetricValueState::Observed,
                    sampling_generation: 1,
                },
                ProcessResourceNode {
                    pid: 10,
                    parent_pid: None,
                    name: "shell".to_string(),
                    cpu_percent: 0.1,
                    core_equivalent_percent: 0.1,
                    memory_bytes: 50,
                    memory_metric: crate::state::ResourceMemoryMetric::PrivateResident,
                    creation_time_100ns: None,
                    executable: None,
                    command_label: Some("shell".to_string()),
                    command_arg_count: 0,
                    command_arg_bytes: 0,
                    resource_id: None,
                    resource_kind: None,
                    child_count: 1,
                    lifecycle: crate::state::ProcessResourceLifecycle::Running,
                    metrics_status: crate::domain::snapshot::ProcessMetricStatus::Complete,
                    metric_values: crate::state::ResourceMetricValueState::Observed,
                    cpu_value_state: crate::state::ResourceMetricValueState::Observed,
                    memory_value_state: crate::state::ResourceMetricValueState::Observed,
                    sampling_generation: 1,
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
    fn process_monitor_entries_preserve_and_mark_partial_metrics() {
        let app_state = AppState::default();
        let mut runtime = RuntimeState::new(false);
        let mut session = SessionRuntimeState::new(
            "partial",
            PathBuf::from("."),
            SessionDimensions::default(),
            TerminalBackend::PortablePtyFeedingAlacritty,
        );
        session.status = SessionStatus::Running;
        session.resources.metrics_unavailable = true;
        session.resources.io_read_bytes = Some(75);
        session.resources.io_write_bytes = Some(60);
        runtime.sessions.insert(session.session_id.clone(), session);

        let entries = process_monitor_entries(&app_state, &runtime);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].metrics_unavailable);
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

    #[test]
    fn cpu_detail_explains_machine_percent_and_equivalent_cores() {
        assert_eq!(
            format_cpu_detail_with_core(1.953_125, 0.0, 64),
            "2.0% machine · 1.25 cores"
        );
        assert_eq!(
            format_cpu_detail_with_core(0.0, 0.0, 1),
            "0.0% machine · 0.00 cores"
        );
        assert_eq!(
            format_cpu_detail_with_core(100.0, 1_600.0, 8),
            "100.0% machine · 16.00 cores"
        );
        assert_eq!(
            format_cpu_detail_with_core(100.0, 1_600.0, 8),
            "100.0% machine · 16.00 cores"
        );
    }

    #[test]
    fn unavailable_and_stale_metrics_never_render_as_idle() {
        assert!(format_metric_cpu_detail(
            6.25,
            50.0,
            8,
            crate::state::ResourceMetricValueState::Partial,
        )
        .contains("partial"));
        assert!(format_metric_memory(
            4_096,
            crate::state::ResourceMemoryMetric::PrivateCommitted,
            crate::state::ResourceMetricValueState::Partial,
        )
        .contains("partial"));
        assert_eq!(
            format_metric_cpu_detail(
                0.0,
                0.0,
                8,
                crate::state::ResourceMetricValueState::Unavailable,
            ),
            "CPU unavailable"
        );
        assert_eq!(
            format_metric_memory(
                0,
                crate::state::ResourceMemoryMetric::PrivateCommitted,
                crate::state::ResourceMetricValueState::Unavailable,
            ),
            "private committed unavailable"
        );
        assert!(format_metric_cpu_detail(
            12.5,
            100.0,
            8,
            crate::state::ResourceMetricValueState::LastKnown,
        )
        .contains("last known"));
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
    core_equivalent_percent: f32,
    memory_bytes: u64,
    memory_metric: ResourceMemoryMetric,
    process_count: u32,
    metrics_unavailable: bool,
    metrics_status: crate::domain::snapshot::ProcessMetricStatus,
    cpu_value_state: ResourceMetricValueState,
    memory_value_state: ResourceMetricValueState,
    metrics_stale: bool,
    unreaped: bool,
    logical_cpu_count: u32,
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
