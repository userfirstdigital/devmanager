use super::*;
use crate::domain::TaskCockpitResult;
use crate::ui::components::{
    button::native_toolbar_button as button, text_field::native_text_input as input,
};
use gpui_component::Disableable;

#[derive(Default)]
pub(super) struct SshUi {
    pub(super) editor: Option<SshEditor>,
    pub(super) pending: Option<(RequestId, Instant)>,
    pub(super) active: Option<(HostTaskKey, String, ResourceId)>,
    pub(super) disconnecting: Option<(CommandId, HostTaskKey, ResourceId)>,
    opening: Option<(HostTaskKey, String)>,
    sessions: BTreeMap<(HostTaskKey, String), ResourceId>,
    pub(super) side_owner: Option<HostTaskKey>,
    error: Option<String>,
    archive: Option<String>,
    stacked: bool,
}

pub(super) struct SshEditor {
    id: Option<String>,
    label: Entity<InputState>,
    host: Entity<InputState>,
    username: Entity<InputState>,
    port: Entity<InputState>,
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
        });
        self.terminal_input_owner = None;
        self.pending_terminal_focus = false;
        self.ssh_ui.error = None;
    }

    fn dispatch_ssh_query(&mut self, task_id: TaskId, query: TaskCockpitQuery) -> bool {
        if self.ssh_ui.pending.is_some() {
            return false;
        }
        let host = self.local_host_id();
        match self.dispatch_action_recorded_for_owner(
            &host,
            ActionRequest::TaskCockpit { task_id, query },
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
        };
        self.dispatch_ssh_query(TaskId::new(), query);
    }

    pub(super) fn open_ssh_connection(&mut self, endpoint: String) {
        let Some(owner) = self
            .selected_task_key
            .clone()
            .filter(|o| o.host == self.local_host_id())
        else {
            self.ssh_ui.error =
                Some("Select a local task to open SSH beside its conversation.".into());
            return;
        };
        if self.ssh_ui.pending.is_some() {
            return;
        }
        if let Some(resource) = self
            .ssh_ui
            .sessions
            .get(&(owner.clone(), endpoint.clone()))
            .copied()
        {
            if self.terminal_strip_for(&owner).is_some_and(|s| {
                s.terminals
                    .iter()
                    .any(|t| t.resource_id == resource && t.exit.is_none())
            }) {
                self.ssh_ui.active = Some((owner.clone(), endpoint.clone(), resource));
                self.ssh_ui.side_owner = Some(owner.clone());
                self.set_pane_view(&owner, PaneView::Conversation);
                self.focus_terminal_chip(&owner, resource);
                return;
            }
        }
        let Some(revision) = self
            .local_slot()
            .client_model
            .as_ref()
            .and_then(|m| m.task(owner.task_id))
            .map(|s| s.task.revision)
        else {
            self.ssh_ui.error = Some("Wait for this task to finish connecting.".into());
            return;
        };
        if self.dispatch_ssh_query(
            owner.task_id,
            TaskCockpitQuery::OpenSshTerminal {
                endpoint_id: endpoint.clone(),
                expected_task_revision: revision,
            },
        ) {
            self.ssh_ui.active = None;
            self.ssh_ui.opening = Some((owner.clone(), endpoint));
            self.ssh_ui.side_owner = Some(owner.clone());
            self.set_pane_view(&owner, PaneView::Conversation);
        }
    }

    pub(super) fn disconnect_ssh(&mut self, owner: HostTaskKey, resource: ResourceId) {
        if self.ssh_ui.disconnecting.is_some() {
            return;
        }
        match self.dispatch_action_recorded_for_owner(
            &owner.host,
            ActionRequest::TerminalClose {
                task_id: owner.task_id,
                resource_id: resource,
            },
        ) {
            Ok(record) => {
                self.ssh_ui.disconnecting =
                    native_command_id(&record.command).map(|id| (id, owner, resource));
            }
            Err(error) => self.ssh_ui.error = Some(error.message),
        }
    }

    pub(super) fn settle_ssh_outcome(&mut self, outcome: &NativeHostActionOutcome) {
        if self
            .ssh_ui
            .disconnecting
            .as_ref()
            .is_some_and(|(id, _, _)| native_command_id(&outcome.action().command) == Some(*id))
        {
            match outcome {
                NativeHostActionOutcome::Accepted {
                    receipt: crate::domain::command::CommandReceipt::Accepted { .. },
                    ..
                } => {}
                _ => {
                    self.ssh_ui.disconnecting = None;
                    self.ssh_ui.error = Some(match outcome {
                        NativeHostActionOutcome::Failed { error, .. }
                        | NativeHostActionOutcome::Uncertain { error, .. } => {
                            format!("Disconnect did not complete: {error}")
                        }
                        NativeHostActionOutcome::Accepted { receipt, .. } => {
                            format!("Disconnect was refused: {receipt:?}")
                        }
                        _ => "Disconnect did not complete. The terminal remains open; try again."
                            .into(),
                    });
                }
            }
        }
        let Some((pending, _)) = self.ssh_ui.pending else {
            return;
        };
        if native_request_id(&outcome.action().command) != Some(pending) {
            return;
        }
        let success = match outcome {
            NativeHostActionOutcome::Queried {
                body: NativeHostQueryBody::TaskCockpit(TaskCockpitResult::Config(_)),
                ..
            } => {
                self.ssh_ui.editor = None;
                self.ssh_ui.archive = None;
                true
            }
            NativeHostActionOutcome::Queried {
                body: NativeHostQueryBody::TaskCockpit(TaskCockpitResult::TaskTerminals(strip)),
                ..
            } => {
                if self
                    .ssh_ui
                    .opening
                    .as_ref()
                    .is_some_and(|(owner, _)| owner.task_id == strip.task_id)
                    && strip.focused.is_some_and(|id| {
                        strip
                            .terminals
                            .iter()
                            .any(|t| t.resource_id == id && !t.is_provider)
                    })
                {
                    let (owner, endpoint) = self.ssh_ui.opening.take().expect("checked opening");
                    let resource = strip.focused.expect("checked focus");
                    self.ssh_ui.active = Some((owner.clone(), endpoint.clone(), resource));
                    self.ssh_ui.sessions.insert((owner, endpoint), resource);
                    true
                } else {
                    false
                }
            }
            _ => false,
        };
        if !success {
            self.ssh_ui.error = Some(match outcome {
                NativeHostActionOutcome::Failed { error, .. }
                | NativeHostActionOutcome::Uncertain { error, .. } => error.clone(),
                NativeHostActionOutcome::Queried {
                    body: NativeHostQueryBody::TaskCockpit(result),
                    ..
                } => cockpit_result_detail("ssh", result),
                _ => "SSH request did not complete. Refresh before trying again.".into(),
            });
            self.ssh_ui.opening = None;
        }
        self.ssh_ui.pending = None;
    }

    pub(super) fn ssh_terminal_is_visible(&self, owner: &HostTaskKey) -> bool {
        self.selected_task_key.as_ref() == Some(owner)
            && self.ssh_ui.side_owner.as_ref() == Some(owner)
            && self.ssh_ui.active.as_ref().is_some_and(|(o, _, id)| {
                o == owner && self.focused_terminal_target(owner) == TerminalTarget::Resource(*id)
            })
    }

    pub(super) fn tick_ssh_requests(&mut self) {
        if let Some((_, owner, resource)) = self.ssh_ui.disconnecting.clone() {
            if self
                .terminal_strip_for(&owner)
                .is_some_and(|strip| !strip.terminals.iter().any(|t| t.resource_id == resource))
            {
                self.ssh_ui.sessions.retain(|_, id| *id != resource);
                self.ssh_ui.disconnecting = None;
                if self
                    .ssh_ui
                    .active
                    .as_ref()
                    .is_some_and(|(o, _, id)| o == &owner && *id == resource)
                {
                    self.ssh_ui.active = None;
                    self.ssh_ui.side_owner = None;
                    self.terminal_input_owner = None;
                }
            }
        }
        if self
            .ssh_ui
            .pending
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(90))
        {
            self.ssh_ui.pending = None;
            self.ssh_ui.opening = None;
            self.ssh_ui.error = Some(
                "SSH request timed out. The host may have completed it; refresh before retrying."
                    .into(),
            );
        }
    }

    pub(super) fn ssh_sidebar(
        &self,
        tokens: crate::ui::tokens::ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.ssh_ui.pending.is_some();
        let mut section = div()
            .id("native-ssh-list")
            .flex()
            .flex_col()
            .flex_none()
            .max_h(px(390.0))
            .border_t_1()
            .border_color(tokens.borders.subtle.to_gpui())
            .p(px(10.0))
            .gap(px(6.0))
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child("SSH")
                    .child(button("ssh-add", "+ Add", !busy).on_click(cx.listener(
                        |s, _, w, cx| {
                            s.edit_ssh_connection(None, w, cx);
                            cx.notify();
                        },
                    ))),
            );
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
            section = section.child(
                div()
                    .id(("ssh-saved-row", index))
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        div().flex_1().min_w(px(0.0)).child(
                            button("ssh-open", "", !busy)
                                .label(row.label.clone())
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
        if self.local_slot().config_sidebar.ssh_connections.is_empty()
            && self.ssh_ui.editor.is_none()
        {
            section = section.child(
                div()
                    .text_size(px(12.0))
                    .text_color(tokens.text.muted.to_gpui())
                    .child("Save a server to open its terminal alongside your agent."),
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
                                s.dispatch_ssh_query(
                                    TaskId::new(),
                                    TaskCockpitQuery::ConfigArchiveSsh {
                                        connection_id: id.clone(),
                                    },
                                );
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
            section = section.child(div().text_size(px(11.0)).text_color(tokens.text.muted.to_gpui())
                .child("Uses OpenSSH keys/config. Password and host-key prompts appear in the terminal. Editing preserves saved credentials."))
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

    pub(super) fn ssh_workspace_size(&mut self, available: Size<Pixels>) -> Size<Pixels> {
        if self
            .ssh_ui
            .side_owner
            .as_ref()
            .is_none_or(|o| self.selected_task_key.as_ref() != Some(o))
        {
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
        let Some(owner) = self
            .ssh_ui
            .side_owner
            .as_ref()
            .filter(|o| self.selected_task_key.as_ref() == Some(*o))
        else {
            return conversation;
        };
        let owner = owner.clone();
        let close_owner = owner.clone();
        let active = self
            .ssh_ui
            .active
            .as_ref()
            .filter(|(o, _, _)| o == &owner)
            .map(|(_, _, resource)| *resource);
        let title = self
            .ssh_ui
            .active
            .as_ref()
            .filter(|(o, _, _)| o == &owner)
            .and_then(|(_, endpoint, _)| {
                self.local_slot()
                    .config_sidebar
                    .ssh_connections
                    .iter()
                    .find(|row| &row.config_id == endpoint)
            })
            .map(|row| format!("SSH · {}", row.label))
            .unwrap_or_else(|| "SSH terminal".into());
        let terminal = if active
            .is_some_and(|id| self.focused_terminal_target(&owner) == TerminalTarget::Resource(id))
        {
            self.center_provider_terminal_surface_for(&owner, tokens, cx)
        } else {
            div()
                .p(px(12.0))
                .child(if self.ssh_ui.pending.is_some() {
                    "Opening SSH…"
                } else {
                    "Select an SSH connection to show its terminal."
                })
                .into_any_element()
        };
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
            .child(
                div()
                    .id("native-ssh-terminal-panel")
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
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .p(px(8.0))
                            .child(div().flex_1().min_w(px(0.0)).overflow_hidden().child(title))
                            .child(
                                button(
                                    "ssh-disconnect",
                                    if self.ssh_ui.disconnecting.is_some() {
                                        "Disconnecting…"
                                    } else {
                                        "Disconnect"
                                    },
                                    active.is_some() && self.ssh_ui.disconnecting.is_none(),
                                )
                                .on_click(cx.listener(
                                    move |s, _, _, cx| {
                                        if let Some(resource) = active {
                                            s.disconnect_ssh(close_owner.clone(), resource);
                                        }
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                button("ssh-hide", "Hide", true)
                                    .tooltip("Hide this panel; the connection stays open")
                                    .on_click(cx.listener(|s, _, _, cx| {
                                        s.ssh_ui.side_owner = None;
                                        s.terminal_input_owner = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(terminal),
            )
            .into_any_element()
    }
}
