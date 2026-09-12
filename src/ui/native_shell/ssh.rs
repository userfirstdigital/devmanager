use super::*;
use crate::domain::cockpit::SshSecretEdit;
use crate::domain::{TaskCockpitResult, TaskTerminalProjection};
use crate::ui::components::{
    button::native_toolbar_button as button, text_field::native_text_input as input,
};
use crate::ui::task_cockpit::dock::ContextDock;

/// Saved SSH connections open as host-owned terminals: no Task, the way
/// 0.4.1 opened them. The panel shows one terminal at a time; every other one
/// keeps running on the host until it is disconnected.
pub(super) struct SshUi {
    pub(super) editor: Option<SshEditor>,
    pub(super) pending: Option<(RequestId, Instant)>,
    error: Option<String>,
    archive: Option<String>,
    stacked: bool,
    list_scroll: gpui::ScrollHandle,
    /// Host terminals this client opened, by saved connection id.
    terminals: BTreeMap<String, ResourceId>,
    /// The saved connection an in-flight open is for.
    opening: Option<String>,
    /// The terminal the panel shows.
    active: Option<ResourceId>,
    panel_open: bool,
    projection: Option<TaskTerminalProjection>,
    /// The one screen read (poll, resize or scroll) in flight.
    screen_in_flight: Option<(RequestId, Instant)>,
    next_refresh: Option<Instant>,
    /// Last input sequence sent; the host requires `accepted + 1`.
    next_input_sequence: u64,
    last_size: Option<(u16, u16)>,
    scroll_px: f32,
    focus: gpui::FocusHandle,
    focus_requested: bool,
}

impl SshUi {
    pub(super) fn new(focus: gpui::FocusHandle) -> Self {
        Self {
            editor: None,
            pending: None,
            error: None,
            archive: None,
            stacked: false,
            list_scroll: gpui::ScrollHandle::new(),
            terminals: BTreeMap::new(),
            opening: None,
            active: None,
            panel_open: false,
            projection: None,
            screen_in_flight: None,
            next_refresh: None,
            next_input_sequence: 0,
            last_size: None,
            scroll_px: 0.0,
            focus,
            focus_requested: false,
        }
    }

    /// Whether a host reply belongs to this panel (its outcome settles here).
    pub(super) fn owns_request(&self, id: Option<RequestId>) -> bool {
        let Some(id) = id else {
            return false;
        };
        self.pending.is_some_and(|(pending, _)| pending == id)
            || self.screen_in_flight.is_some_and(|(read, _)| read == id)
    }

    pub(super) fn take_focus_request(&mut self) -> bool {
        std::mem::take(&mut self.focus_requested)
    }

    pub(super) fn focus_handle(&self) -> &gpui::FocusHandle {
        &self.focus
    }

    /// The panel is showing a live terminal, so its screen is polled.
    pub(super) fn is_polling(&self) -> bool {
        self.panel_open && self.active.is_some()
    }
}

/// Saved connections show this many rows before the list scrolls, so a long
/// server list cannot push the task board off the sidebar.
const SSH_MAX_VISIBLE_ROWS: usize = 10;
const SSH_ROW_HEIGHT: f32 = 28.0;
const SSH_ROW_GAP: f32 = 4.0;
/// How often the visible SSH screen is refreshed while nothing is typed.
const SSH_SCREEN_REFRESH: Duration = Duration::from_millis(250);
const SSH_WHEEL_LINE_HEIGHT: f32 = 18.0;

pub(super) struct SshEditor {
    id: Option<String>,
    label: Entity<InputState>,
    host: Entity<InputState>,
    username: Entity<InputState>,
    port: Entity<InputState>,
    /// Blank means "keep what is saved"; the host never sends secrets back.
    password: Entity<InputState>,
    private_key: Entity<InputState>,
    has_password: bool,
    has_private_key: bool,
    clear_password: bool,
    clear_private_key: bool,
}

/// One credential field's save: a removal, a new value, or leave it be.
fn secret_edit(clear: bool, field: &Entity<InputState>, cx: &gpui::App) -> SshSecretEdit {
    if clear {
        return SshSecretEdit::Clear;
    }
    let value = field.read(cx).value().to_string();
    if value.trim().is_empty() {
        SshSecretEdit::Keep
    } else {
        SshSecretEdit::Set(value)
    }
}

impl NativeShell {
    fn edit_ssh_connection(
        &mut self,
        id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ssh_ui.pending.is_some() {
            return;
        }
        let row = id
            .as_ref()
            .and_then(|id| {
                self.local_slot()
                    .config_sidebar
                    .ssh_connections
                    .iter()
                    .find(|row| &row.config_id == id)
            })
            .cloned();
        let has_password = row.as_ref().is_some_and(|r| r.has_password);
        let has_private_key = row.as_ref().is_some_and(|r| r.has_private_key);
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(if has_password {
                    "Saved · leave blank to keep it"
                } else {
                    "Leave blank to use keys or an agent"
                })
        });
        let private_key = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .rows(4)
                .placeholder(if has_private_key {
                    "Saved · leave blank to keep it"
                } else {
                    "Paste a private key (OpenSSH or PEM)"
                })
        });
        let mut field = |placeholder: &'static str, value: String| {
            cx.new(|cx| {
                let mut state = InputState::new(window, cx).placeholder(placeholder);
                state.set_value(value, window, cx);
                state
            })
        };
        self.ssh_ui.editor = Some(SshEditor {
            id,
            label: field(
                "Connection name",
                row.as_ref().map(|r| r.label.clone()).unwrap_or_default(),
            ),
            host: field(
                "Hostname or SSH config alias",
                row.as_ref().map(|r| r.host.clone()).unwrap_or_default(),
            ),
            username: field(
                "Username",
                row.as_ref().map(|r| r.username.clone()).unwrap_or_default(),
            ),
            port: field(
                "Port",
                row.as_ref()
                    .map(|r| r.port.to_string())
                    .unwrap_or_else(|| "22".into()),
            ),
            password,
            private_key,
            has_password,
            has_private_key,
            clear_password: false,
            clear_private_key: false,
        });
        self.terminal_input_owner = None;
        self.pending_terminal_focus = false;
        self.ssh_ui.error = None;
    }

    /// One host-global SSH request (config save/remove, open, close). SSH
    /// queries carry no Task, so the task id here is never sent.
    fn dispatch_ssh_query(&mut self, query: TaskCockpitQuery) -> bool {
        if self.ssh_ui.pending.is_some() {
            return false;
        }
        let host = self.local_host_id();
        match self.dispatch_action_recorded_for_owner(
            &host,
            ActionRequest::TaskCockpit {
                task_id: TaskId::new(),
                query,
            },
        ) {
            Ok(record) => {
                self.ssh_ui.pending =
                    native_request_id(&record.command).map(|id| (id, Instant::now()));
                self.ssh_ui.error = None;
                true
            }
            Err(error) => {
                self.ssh_ui.error = Some(error.message);
                false
            }
        }
    }

    fn save_ssh_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.ssh_ui.editor else {
            return;
        };
        let port = match editor.port.read(cx).value().trim().parse::<u16>() {
            Ok(port) if port > 0 => port,
            _ => {
                self.ssh_ui.error = Some("Enter a port between 1 and 65535.".into());
                return;
            }
        };
        if editor.label.read(cx).value().trim().is_empty()
            || editor.host.read(cx).value().trim().is_empty()
            || editor.username.read(cx).value().trim().is_empty()
        {
            self.ssh_ui.error = Some("Enter a connection name, hostname and username.".into());
            return;
        }
        let query = TaskCockpitQuery::ConfigUpsertSsh {
            connection_id: editor.id.clone(),
            label: editor.label.read(cx).value().trim().into(),
            host: editor.host.read(cx).value().trim().into(),
            username: editor.username.read(cx).value().trim().into(),
            port,
            password: secret_edit(editor.clear_password, &editor.password, cx),
            private_key: secret_edit(editor.clear_private_key, &editor.private_key, cx),
        };
        self.dispatch_ssh_query(query);
    }

    /// Open a saved connection's terminal and connect. The host reuses the
    /// live session for this connection, or reconnects one that has ended.
    pub(super) fn open_ssh_connection(&mut self, endpoint: String) {
        if let Some(resource) = self.ssh_ui.terminals.get(&endpoint).copied() {
            // Show it straight away; the open below still revalidates it.
            self.show_ssh_terminal(resource);
        }
        if self.dispatch_ssh_query(TaskCockpitQuery::HostSshOpen {
            endpoint_id: endpoint.clone(),
        }) {
            self.ssh_ui.opening = Some(endpoint);
            self.ssh_ui.panel_open = true;
        }
    }

    pub(super) fn disconnect_ssh(&mut self, resource: ResourceId) {
        self.dispatch_ssh_query(TaskCockpitQuery::HostTerminalClose {
            resource_id: resource,
        });
    }

    fn show_ssh_terminal(&mut self, resource: ResourceId) {
        if self.ssh_ui.active != Some(resource) {
            self.ssh_ui.active = Some(resource);
            self.ssh_ui.projection = None;
            self.ssh_ui.next_input_sequence = 0;
            self.ssh_ui.last_size = None;
            self.ssh_ui.screen_in_flight = None;
        }
        self.ssh_ui.panel_open = true;
        self.ssh_ui.focus_requested = true;
        self.ssh_ui.next_refresh = None;
        self.ssh_ui.error = None;
    }

    fn forget_ssh_terminal(&mut self, resource: ResourceId) {
        self.ssh_ui.terminals.retain(|_, id| *id != resource);
        if self.ssh_ui.active == Some(resource) {
            self.ssh_ui.active = None;
            self.ssh_ui.projection = None;
            self.ssh_ui.screen_in_flight = None;
            self.ssh_ui.last_size = None;
            match self.ssh_ui.terminals.values().next().copied() {
                Some(next) => self.show_ssh_terminal(next),
                None => self.ssh_ui.panel_open = false,
            }
        }
    }

    fn accept_ssh_screen(&mut self, projection: &TaskTerminalProjection) {
        if self.ssh_ui.active != Some(projection.resource_id) {
            return;
        }
        // A new session behind the same terminal restarts input sequencing.
        if self
            .ssh_ui
            .projection
            .as_ref()
            .is_some_and(|previous| previous.session_id != projection.session_id)
        {
            self.ssh_ui.next_input_sequence = 0;
        }
        self.ssh_ui.next_input_sequence = self
            .ssh_ui
            .next_input_sequence
            .max(projection.accepted_input_sequence);
        self.ssh_ui.projection = Some(projection.clone());
    }

    pub(super) fn settle_ssh_outcome(&mut self, outcome: &NativeHostActionOutcome) {
        let id = native_request_id(&outcome.action().command);
        if id.is_some() && self.ssh_ui.screen_in_flight.map(|(read, _)| read) == id {
            self.ssh_ui.screen_in_flight = None;
            match outcome {
                NativeHostActionOutcome::Queried {
                    body:
                        NativeHostQueryBody::TaskCockpit(TaskCockpitResult::HostTerminal(projection)),
                    ..
                } => self.accept_ssh_screen(projection),
                NativeHostActionOutcome::Failed { error, .. } if error.contains("NotFound") => {
                    if let Some(resource) = self.ssh_ui.active {
                        self.forget_ssh_terminal(resource);
                    }
                    self.ssh_ui.error = Some(
                        "That SSH session is no longer open. Click the connection to reconnect."
                            .into(),
                    );
                }
                _ => {}
            }
            return;
        }
        let Some((pending, _)) = self.ssh_ui.pending else {
            return;
        };
        if id != Some(pending) {
            return;
        }
        self.ssh_ui.pending = None;
        match outcome {
            NativeHostActionOutcome::Queried {
                body: NativeHostQueryBody::TaskCockpit(TaskCockpitResult::Config(_)),
                ..
            } => {
                self.ssh_ui.editor = None;
                self.ssh_ui.archive = None;
            }
            NativeHostActionOutcome::Queried {
                body: NativeHostQueryBody::TaskCockpit(TaskCockpitResult::HostTerminal(projection)),
                ..
            } => {
                if let Some(endpoint) = self.ssh_ui.opening.take() {
                    self.ssh_ui
                        .terminals
                        .retain(|_, id| *id != projection.resource_id);
                    self.ssh_ui
                        .terminals
                        .insert(endpoint, projection.resource_id);
                }
                self.show_ssh_terminal(projection.resource_id);
                self.accept_ssh_screen(projection);
            }
            NativeHostActionOutcome::Queried {
                body:
                    NativeHostQueryBody::TaskCockpit(TaskCockpitResult::HostTerminalClosed {
                        resource_id,
                    }),
                ..
            } => self.forget_ssh_terminal(*resource_id),
            other => {
                self.ssh_ui.opening = None;
                self.ssh_ui.error = Some(match other {
                    NativeHostActionOutcome::Failed { error, .. }
                    | NativeHostActionOutcome::Uncertain { error, .. } => error.clone(),
                    NativeHostActionOutcome::Queried {
                        body: NativeHostQueryBody::TaskCockpit(result),
                        ..
                    } => cockpit_result_detail("ssh", result),
                    _ => "SSH request did not complete. Try again.".into(),
                });
                if self.ssh_ui.active.is_none() {
                    self.ssh_ui.panel_open = false;
                }
            }
        }
    }

    /// Input acks for the SSH panel's terminal; `true` when handled here.
    pub(super) fn settle_ssh_input_ack(
        &mut self,
        request: &TerminalInputRequest,
        ack: &InputAck,
    ) -> bool {
        let Some(projection) = self.ssh_ui.projection.as_ref() else {
            return false;
        };
        if request.context.task_id != projection.task_id
            || request.context.resource_id != projection.resource_id
        {
            return false;
        }
        let accepted = projection.accepted_input_sequence;
        match ack {
            InputAck::Accepted { sequence } | InputAck::Duplicate { sequence } => {
                self.ssh_ui.next_input_sequence = self.ssh_ui.next_input_sequence.max(*sequence);
                // Show the echo now rather than on the next poll.
                self.ssh_ui.next_refresh = None;
                if self.ssh_ui.screen_in_flight.is_none() {
                    if let Some(resource_id) = self.ssh_ui.active {
                        self.request_ssh_screen(TaskCockpitQuery::HostTerminal { resource_id });
                    }
                }
            }
            InputAck::Rejected { reason } => {
                self.ssh_ui.next_input_sequence = accepted;
                self.ssh_ui.error = Some(format!(
                    "The SSH terminal rejected that input ({reason:?})."
                ));
            }
        }
        true
    }

    fn local_terminal_client_id(&self) -> Option<ClientId> {
        match self.local_slot().host_runtime.as_ref() {
            Some(NativeHostRuntimeAttachment::Client(runtime)) => runtime
                .current_host_admission()
                .ok()
                .map(|admission| admission.client_id),
            #[cfg(test)]
            Some(NativeHostRuntimeAttachment::Injected(_)) => Some(ClientId::new()),
            _ => None,
        }
    }

    /// Send bytes to the visible SSH terminal.
    pub(super) fn send_ssh_input(&mut self, bytes: Vec<u8>) -> bool {
        let Some(projection) = self.ssh_ui.projection.clone() else {
            return false;
        };
        let Some(client_id) = self.local_terminal_client_id() else {
            self.ssh_ui.error = Some("The host connection is not ready for input.".into());
            return false;
        };
        let Some(sequence) = self
            .ssh_ui
            .next_input_sequence
            .max(projection.accepted_input_sequence)
            .checked_add(1)
        else {
            return false;
        };
        let request = match terminal_input_request(client_id, &projection, sequence, bytes) {
            Ok(request) => request,
            Err(message) => {
                self.ssh_ui.error = Some(message);
                return false;
            }
        };
        let host = self.local_host_id();
        let Some(mut record) = self
            .local_slot_mut()
            .interaction
            .host_terminal_input(request)
        else {
            return false;
        };
        match self.enqueue_host_action_for_owner(&host, &mut record) {
            NativeHostActionResult::Queued => {
                self.ssh_ui.next_input_sequence = sequence;
                true
            }
            _ => {
                self.ssh_ui.error = Some("The host did not take that input. Try again.".into());
                false
            }
        }
    }

    fn request_ssh_screen(&mut self, query: TaskCockpitQuery) {
        let host = self.local_host_id();
        if let Ok(record) = self.dispatch_action_recorded_for_owner_inner(
            &host,
            ActionRequest::TaskCockpit {
                task_id: TaskId::new(),
                query,
            },
            true,
            NativeActionPurpose::Ordinary,
        ) {
            self.ssh_ui.screen_in_flight =
                native_request_id(&record.command).map(|id| (id, Instant::now()));
        }
    }

    /// Refresh the visible SSH screen on the controller cadence.
    pub(super) fn poll_ssh_terminal(&mut self, now: Instant) {
        if !self.ssh_ui.is_polling() {
            return;
        }
        let Some(resource_id) = self.ssh_ui.active else {
            return;
        };
        if self
            .ssh_ui
            .screen_in_flight
            .is_some_and(|(_, at)| at.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        if self
            .ssh_ui
            .next_refresh
            .is_some_and(|deadline| deadline > now)
        {
            return;
        }
        self.ssh_ui.next_refresh = Some(now + SSH_SCREEN_REFRESH);
        self.request_ssh_screen(TaskCockpitQuery::HostTerminal { resource_id });
    }

    fn resize_ssh_terminal(&mut self, cols: u16, rows: u16) -> bool {
        let Some(resource_id) = self.ssh_ui.active else {
            return false;
        };
        if cols == 0 || rows == 0 || self.ssh_ui.last_size == Some((cols, rows)) {
            return false;
        }
        self.ssh_ui.last_size = Some((cols, rows));
        self.request_ssh_screen(TaskCockpitQuery::HostTerminalResize {
            resource_id,
            cols,
            rows,
        });
        true
    }

    fn scroll_ssh_terminal(&mut self, event: &ScrollWheelEvent) -> bool {
        let Some(resource_id) = self.ssh_ui.active else {
            return false;
        };
        let delta: f32 = event.delta.pixel_delta(px(SSH_WHEEL_LINE_HEIGHT)).y.into();
        let lines = terminal_scroll_lines_from_pixels(
            &mut self.ssh_ui.scroll_px,
            delta,
            SSH_WHEEL_LINE_HEIGHT,
        );
        if lines == 0 {
            return false;
        }
        self.request_ssh_screen(TaskCockpitQuery::HostTerminalScroll {
            resource_id,
            delta_lines: lines.clamp(-256, 256),
        });
        true
    }

    fn handle_ssh_terminal_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(projection) = self.ssh_ui.projection.as_ref() else {
            return false;
        };
        let command = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        let text = if command && event.keystroke.key.eq_ignore_ascii_case("v") {
            cx.read_from_clipboard().and_then(|item| item.text())
        } else {
            let app_cursor = ContextDock::terminal_pane_model_for_projection(projection)
                .session
                .is_some_and(|session| session.screen.mode.app_cursor);
            native_terminal_key_text(&event.keystroke, app_cursor)
        };
        match text {
            Some(text) if !text.is_empty() => self.send_ssh_input(text.into_bytes()),
            _ => false,
        }
    }

    pub(super) fn tick_ssh_requests(&mut self) {
        if self
            .ssh_ui
            .pending
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(90))
        {
            self.ssh_ui.pending = None;
            self.ssh_ui.opening = None;
            self.ssh_ui.error =
                Some("SSH request timed out. The host may have completed it; try again.".into());
        }
    }

    #[cfg(test)]
    pub(super) fn ssh_active_terminal(&self) -> Option<ResourceId> {
        self.ssh_ui.active
    }

    pub(super) fn ssh_sidebar(
        &self,
        tokens: crate::ui::tokens::ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.ssh_ui.pending.is_some();
        let count = self.local_slot().config_sidebar.ssh_connections.len();
        // An open editor or confirmation is never hidden behind a collapsed
        // header: the user just asked for it.
        let collapsed = self.layout.ssh_collapsed
            && self.ssh_ui.editor.is_none()
            && self.ssh_ui.archive.is_none();
        let mut section = div()
            .id("native-ssh-list")
            .flex()
            .flex_col()
            .flex_none()
            .max_h(px(560.0))
            .border_t_1()
            .border_color(tokens.borders.subtle.to_gpui())
            .p(px(10.0))
            .gap(px(6.0))
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .id("ssh-collapse-toggle")
                            .flex()
                            .flex_1()
                            .min_w(px(0.0))
                            .items_center()
                            .gap(px(6.0))
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|s, _: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    s.layout.ssh_collapsed = !s.layout.ssh_collapsed;
                                    s.mark_layout_dirty();
                                    cx.notify();
                                }),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(tokens.text.muted.to_gpui())
                                    .child(if collapsed { "\u{25b8}" } else { "\u{25be}" }),
                            )
                            .child("SSH")
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(tokens.text.muted.to_gpui())
                                    .child(count.to_string()),
                            ),
                    )
                    .child(button("ssh-add", "+ Add", !busy).on_click(cx.listener(
                        |s, _, w, cx| {
                            if s.layout.ssh_collapsed {
                                s.layout.ssh_collapsed = false;
                                s.mark_layout_dirty();
                            }
                            s.edit_ssh_connection(None, w, cx);
                            cx.notify();
                        },
                    ))),
            );
        if collapsed {
            if let Some(error) = &self.ssh_ui.error {
                section = section.child(div().text_size(px(12.0)).child(error.clone()));
            }
            return section.into_any_element();
        }
        let visible_rows = count.min(SSH_MAX_VISIBLE_ROWS);
        let mut rows = div()
            .id("native-ssh-rows")
            .relative()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(SSH_ROW_GAP))
            .max_h(px(visible_rows as f32 * SSH_ROW_HEIGHT
                + visible_rows.saturating_sub(1) as f32 * SSH_ROW_GAP))
            .overflow_y_scroll()
            .track_scroll(&self.ssh_ui.list_scroll);
        if count > SSH_MAX_VISIBLE_ROWS {
            rows = rows.child(crate::ui::scrollbar::AppScrollbar::vertical(
                "native-ssh-rows-scrollbar",
                &self.ssh_ui.list_scroll,
                tokens.scrollbar,
                tokens.surfaces.canvas,
            ));
        }
        for (index, row) in self
            .local_slot()
            .config_sidebar
            .ssh_connections
            .iter()
            .enumerate()
        {
            let endpoint = row.config_id.clone();
            let edit_id = endpoint.clone();
            let remove_id = endpoint.clone();
            let live = self.ssh_ui.terminals.contains_key(&endpoint);
            rows = rows.child(
                div()
                    .id(("ssh-saved-row", index))
                    .flex()
                    .flex_none()
                    .h(px(SSH_ROW_HEIGHT))
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        div().flex_1().min_w(px(0.0)).child(
                            button("ssh-open", "", !busy)
                                .label(if live {
                                    format!("\u{25cf} {}", row.label)
                                } else {
                                    row.label.clone()
                                })
                                .tooltip(format!("{}@{}:{}", row.username, row.host, row.port))
                                .on_click(cx.listener(move |s, _, _, cx| {
                                    s.open_ssh_connection(endpoint.clone());
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(button("ssh-edit", "Edit", !busy).on_click(cx.listener(
                        move |s, _, w, cx| {
                            s.edit_ssh_connection(Some(edit_id.clone()), w, cx);
                            cx.notify();
                        },
                    )))
                    .child(
                        button("ssh-remove", "×", !busy)
                            .tooltip("Remove saved connection")
                            .on_click(cx.listener(move |s, _, _, cx| {
                                s.ssh_ui.archive = Some(remove_id.clone());
                                cx.notify();
                            })),
                    ),
            );
        }
        if count > 0 {
            section = section.child(rows);
        }
        if self.local_slot().config_sidebar.ssh_connections.is_empty()
            && self.ssh_ui.editor.is_none()
        {
            section = section.child(
                div()
                    .text_size(px(12.0))
                    .text_color(tokens.text.muted.to_gpui())
                    .child("Save a server to open its terminal with one click."),
            );
        }
        if let Some(id) = &self.ssh_ui.archive {
            let id = id.clone();
            section = section.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child("Remove this saved connection? Open terminals stay running.")
                    .child(
                        button("ssh-confirm-remove", "Remove", !busy).on_click(cx.listener(
                            move |s, _, _, cx| {
                                s.dispatch_ssh_query(TaskCockpitQuery::ConfigArchiveSsh {
                                    connection_id: id.clone(),
                                });
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        button("ssh-cancel-remove", "Cancel", !busy).on_click(cx.listener(
                            |s, _, _, cx| {
                                s.ssh_ui.archive = None;
                                cx.notify();
                            },
                        )),
                    ),
            );
        }
        if let Some(editor) = &self.ssh_ui.editor {
            for (label, field) in [
                ("Name", &editor.label),
                ("Host", &editor.host),
                ("Username", &editor.username),
                ("Port", &editor.port),
            ] {
                section = section
                    .child(div().text_size(px(12.0)).child(label))
                    .child(input(field).disabled(busy));
            }
            for (label, field, saved, clearing, id) in [
                (
                    "Password",
                    &editor.password,
                    editor.has_password,
                    editor.clear_password,
                    "ssh-clear-password",
                ),
                (
                    "Private key",
                    &editor.private_key,
                    editor.has_private_key,
                    editor.clear_private_key,
                    "ssh-clear-private-key",
                ),
            ] {
                let field_input = input(field).disabled(busy || clearing);
                let field_input = if id == "ssh-clear-private-key" {
                    // The section is a height-capped column, which shrinks
                    // any child without a fixed minimum -- a multi-line
                    // input has none, so it collapsed to one line. A pasted
                    // key needs its room kept.
                    div()
                        .flex_none()
                        .h(px(96.0))
                        .child(field_input.h(px(96.0)))
                        .into_any_element()
                } else {
                    field_input.into_any_element()
                };
                section = section
                    .child(div().text_size(px(12.0)).child(label))
                    .child(field_input);
                if saved {
                    let is_password = id == "ssh-clear-password";
                    section = section.child(
                        button(
                            id,
                            if clearing {
                                "Keep the saved one"
                            } else if is_password {
                                "Remove saved password"
                            } else {
                                "Remove saved key"
                            },
                            !busy,
                        )
                        .on_click(cx.listener(move |s, _, _, cx| {
                            if let Some(editor) = s.ssh_ui.editor.as_mut() {
                                if is_password {
                                    editor.clear_password = !editor.clear_password;
                                } else {
                                    editor.clear_private_key = !editor.clear_private_key;
                                }
                            }
                            cx.notify();
                        })),
                    );
                }
            }
            section = section.child(div().text_size(px(11.0)).text_color(tokens.text.muted.to_gpui())
                .child("A saved key is tried first; a saved password is typed in when the server asks for one. Host-key and other prompts appear in the terminal. Passwords and keys are encrypted with the system keyring, never written to config.json."))
                .child(div().flex().gap(px(8.0))
                    .child(button("ssh-save", "Save connection", !busy).on_click(cx.listener(|s, _, _, cx| {s.save_ssh_editor(cx); cx.notify();})))
                    .child(button("ssh-cancel-edit", "Cancel", !busy).on_click(cx.listener(|s, _, _, cx| {s.ssh_ui.editor = None; cx.notify();}))));
        }
        if busy {
            section = section.child("Working…");
        }
        if let Some(error) = &self.ssh_ui.error {
            section = section.child(div().text_size(px(12.0)).child(error.clone()));
        }
        section.into_any_element()
    }

    /// The conversation's share of the workspace while the SSH panel is open.
    /// With no task selected the panel takes the whole workspace.
    pub(super) fn ssh_workspace_size(&mut self, available: Size<Pixels>) -> Size<Pixels> {
        if !self.ssh_ui.panel_open || self.selected_task_key.is_none() {
            return available;
        }
        self.ssh_ui.stacked = f32::from(available.width) < 900.0;
        if self.ssh_ui.stacked {
            size(
                available.width,
                px(((f32::from(available.height) - 8.0) / 2.0).max(1.0)),
            )
        } else {
            size(
                px(((f32::from(available.width) - 8.0) / 2.0).max(1.0)),
                available.height,
            )
        }
    }

    pub(super) fn with_ssh_panel(
        &self,
        conversation: AnyElement,
        tokens: crate::ui::tokens::ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.ssh_ui.panel_open {
            return conversation;
        }
        let panel = self.ssh_terminal_panel(tokens, cx);
        if self.selected_task_key.is_none() {
            return div()
                .flex()
                .flex_1()
                .min_w(px(0.0))
                .min_h(px(0.0))
                .child(panel)
                .into_any_element();
        }
        div()
            .flex()
            .when(self.ssh_ui.stacked, |d| d.flex_col())
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .child(conversation),
            )
            .child(panel)
            .into_any_element()
    }

    fn ssh_terminal_label(&self, resource: ResourceId) -> String {
        self.ssh_ui
            .terminals
            .iter()
            .find(|(_, id)| **id == resource)
            .and_then(|(endpoint, _)| {
                self.local_slot()
                    .config_sidebar
                    .ssh_connections
                    .iter()
                    .find(|row| &row.config_id == endpoint)
            })
            .map(|row| row.label.clone())
            .unwrap_or_else(|| "SSH".into())
    }

    fn ssh_terminal_panel(
        &self,
        tokens: crate::ui::tokens::ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.ssh_ui.active;
        let title = match (active, &self.ssh_ui.opening) {
            (Some(resource), _) => format!("SSH · {}", self.ssh_terminal_label(resource)),
            (None, Some(_)) => "SSH · connecting…".into(),
            (None, None) => "SSH".into(),
        };
        let header = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .p(px(8.0))
            .child(div().flex_1().min_w(px(0.0)).overflow_hidden().child(title))
            .child(
                button(
                    "ssh-disconnect",
                    "Disconnect",
                    active.is_some() && self.ssh_ui.pending.is_none(),
                )
                .on_click(cx.listener(move |s, _, _, cx| {
                    if let Some(resource) = active {
                        s.disconnect_ssh(resource);
                    }
                    cx.notify();
                })),
            )
            .child(
                button("ssh-hide", "Hide", true)
                    .tooltip("Hide this panel; the connection stays open")
                    .on_click(cx.listener(|s, _, _, cx| {
                        s.ssh_ui.panel_open = false;
                        cx.notify();
                    })),
            );
        let tabs = (self.ssh_ui.terminals.len() > 1).then(|| {
            let mut row = div()
                .flex()
                .flex_wrap()
                .gap(px(4.0))
                .px(px(8.0))
                .pb(px(6.0));
            for (index, resource) in self.ssh_ui.terminals.values().copied().enumerate() {
                row = row.child(
                    div()
                        .id(("ssh-tab", index))
                        .px(px(8.0))
                        .py(px(2.0))
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .text_size(px(12.0))
                        .when(Some(resource) == active, |tab| {
                            tab.bg(tokens.surfaces.hover.to_gpui())
                        })
                        .child(self.ssh_terminal_label(resource))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |s, _: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                s.show_ssh_terminal(resource);
                                cx.notify();
                            }),
                        ),
                );
            }
            row
        });
        let body = match self.ssh_ui.projection.as_ref() {
            Some(projection) => {
                let pane = ContextDock::terminal_pane_model_for_projection(projection);
                let layout_shell = cx.weak_entity();
                let focus_shell = cx.weak_entity();
                let interaction = crate::terminal::view::TerminalGridInteraction {
                    on_layout: Arc::new(
                        move |(cols, rows): (u16, u16),
                              _window: &mut Window,
                              app: &mut gpui::App| {
                            let _ = layout_shell.update(app, |shell, cx| {
                                if shell.resize_ssh_terminal(cols, rows) {
                                    cx.notify();
                                }
                            });
                        },
                    ),
                    on_paint: Arc::new(
                        |_bounds: Bounds<Pixels>, _window: &mut Window, _app: &mut gpui::App| {},
                    ),
                    on_mouse_down: Arc::new(
                        move |_event: &MouseDownEvent,
                              _pointer: crate::terminal::view::TerminalGridPointerEvent,
                              window: &mut Window,
                              app: &mut gpui::App| {
                            let _ = focus_shell
                                .update(app, |shell, _| window.focus(&shell.ssh_ui.focus));
                        },
                    ),
                    on_mouse_move: Arc::new(
                        |_event: &gpui::MouseMoveEvent,
                         _pointer: crate::terminal::view::TerminalGridPointerEvent,
                         _window: &mut Window,
                         _app: &mut gpui::App| {},
                    ),
                    on_mouse_up: Arc::new(
                        |_event: &gpui::MouseUpEvent,
                         _pointer: crate::terminal::view::TerminalGridPointerEvent,
                         _window: &mut Window,
                         _app: &mut gpui::App| {},
                    ),
                };
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .bg(tokens.terminal.background.to_gpui())
                    .child(
                        crate::terminal::view::render_terminal_surface_with_tokens_and_grid(
                            &pane,
                            None,
                            Some(interaction),
                            tokens,
                        ),
                    )
                    .into_any_element()
            }
            None => div()
                .p(px(12.0))
                .text_color(tokens.text.muted.to_gpui())
                .child(if self.ssh_ui.pending.is_some() {
                    "Connecting…".to_string()
                } else {
                    self.ssh_ui
                        .error
                        .clone()
                        .unwrap_or_else(|| "Select an SSH connection to open its terminal.".into())
                })
                .into_any_element(),
        };
        div()
            .id("native-ssh-terminal-panel")
            .track_focus(&self.ssh_ui.focus)
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .bg(tokens.surfaces.overlay.to_gpui())
            .border_1()
            .border_color(tokens.borders.subtle.to_gpui())
            .rounded(px(8.0))
            .overflow_hidden()
            .on_key_down(cx.listener(|s, event: &KeyDownEvent, window, cx| {
                if s.handle_ssh_terminal_key(event, cx) {
                    window.prevent_default();
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .on_scroll_wheel(cx.listener(|s, event: &ScrollWheelEvent, _, cx| {
                if s.scroll_ssh_terminal(event) {
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|s, _: &MouseDownEvent, window, _| {
                    window.focus(&s.ssh_ui.focus);
                }),
            )
            .child(header)
            .children(tabs)
            .child(body)
            .into_any_element()
    }
}
