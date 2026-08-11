//! Native-next GPUI boundary for the task header and one global top bar.
//!
//! The renderer consumes one atomic host snapshot and a bounded attachment
//! channel. The canonical native shell owns the sole `HostClient` and its
//! background pump; this module never opens another connection or runtime. It
//! never reads NativeShell, session persistence, provider runtime state, or a
//! legacy quota renderer.

use std::num::NonZeroU64;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};

use gpui::{
    div, uniform_list, App, Context, InteractiveElement, IntoElement, KeyBinding, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Window,
};
use gpui_component::button::Button;
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::tooltip::Tooltip;

use crate::client::ClientModel;
use crate::ui::components::AccessibleRole;
use crate::ui::shell::Shell;

use super::header::TitleLayout;
use super::header::TopBarUnavailable;
use super::{
    ActionTarget, AgentProjection, HeaderField, HeaderLayout, OverflowControl,
    PrimaryAgentProjection, ProjectedAction, QuotaProjection, TaskHeaderModel, TopBarModel,
    TopBarProjectionController, TopBarProjectionError, TopBarProjectionInput, TopBarStatusLink,
    WorkspaceProjection, NARROW_HEADER_WIDTH_PX,
};
use crate::ui::tokens::Scale;

gpui::actions!(native_next_task_cockpit, [OpenTaskDetailsAction]);

/// Maximum number of command/event messages drained by one UI turn.
pub const NATIVE_NEXT_HOST_CHANNEL_CAPACITY: usize = 32;
pub const NATIVE_NEXT_HOST_EVENT_DRAIN_LIMIT: usize = 32;
pub const NATIVE_NEXT_ACTION_QUEUE_CAPACITY: usize = 32;
pub const NATIVE_NEXT_ACTION_DRAIN_LIMIT: usize = 32;
/// Semantic accessibility snapshots stay bounded while the concrete GPUI
/// uniform list virtualizes the complete specialist collection.
const NATIVE_NEXT_SPECIALIST_SEMANTIC_WINDOW: usize = 32;

/// One atomic snapshot issued by the host/client subscription boundary.
///
/// `sequence` and `header_revision` are intentionally non-zero and private;
/// all model, shell epochs, and top-bar observations are validated together.
#[derive(Clone, PartialEq)]
pub struct HostSnapshot {
    sequence: NonZeroU64,
    header_revision: NonZeroU64,
    model: ClientModel,
    shell: Shell,
    top_bar: TopBarProjectionInput,
}

impl std::fmt::Debug for HostSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostSnapshot")
            .field("sequence", &self.sequence)
            .field("header_revision", &self.header_revision)
            .field("model_sequence", &self.model.last_applied_sequence())
            .field("shell", &self.shell)
            .field("top_bar", &self.top_bar)
            .finish()
    }
}

impl std::fmt::Display for HostSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("host-snapshot[redacted]")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum HostSnapshotError {
    ZeroSequence,
    ZeroHeaderRevision,
    ClientEpochMismatch { shell: u64, model: u64 },
    TopBar(TopBarProjectionError),
    NoAttachment,
    EpochRegression { dimension: &'static str },
    TopBarGenerationRegression,
    TopBarTimestampRegression,
    SequenceRegression,
    HeaderRevisionRegression,
}

impl std::fmt::Display for HostSnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroSequence => formatter.write_str("host snapshot sequence must be non-zero"),
            Self::ZeroHeaderRevision => {
                formatter.write_str("host snapshot header revision must be non-zero")
            }
            Self::ClientEpochMismatch { shell, model } => write!(
                formatter,
                "host snapshot client epoch mismatch (shell {shell}, model {model})"
            ),
            Self::TopBar(error) => write!(formatter, "host snapshot top bar: {error}"),
            Self::NoAttachment => formatter.write_str("native-next host attachment is unavailable"),
            Self::EpochRegression { dimension } => {
                write!(formatter, "host snapshot {dimension} epoch regressed")
            }
            Self::TopBarGenerationRegression => {
                formatter.write_str("host snapshot top-bar generation regressed")
            }
            Self::TopBarTimestampRegression => {
                formatter.write_str("host snapshot top-bar timestamp regressed")
            }
            Self::SequenceRegression => formatter.write_str("host snapshot sequence regressed"),
            Self::HeaderRevisionRegression => {
                formatter.write_str("host snapshot header revision regressed")
            }
        }
    }
}

impl std::error::Error for HostSnapshotError {}

impl From<TopBarProjectionError> for HostSnapshotError {
    fn from(error: TopBarProjectionError) -> Self {
        Self::TopBar(error)
    }
}

impl HostSnapshot {
    /// Validate and capture one host-issued model/shell/top-bar transaction.
    pub fn try_from_host(
        sequence: u64,
        header_revision: u64,
        model: ClientModel,
        shell: Shell,
        top_bar: TopBarProjectionInput,
    ) -> Result<Self, HostSnapshotError> {
        let sequence = NonZeroU64::new(sequence).ok_or(HostSnapshotError::ZeroSequence)?;
        let header_revision =
            NonZeroU64::new(header_revision).ok_or(HostSnapshotError::ZeroHeaderRevision)?;
        top_bar.preflight()?;
        if shell.client_epoch() != model.last_applied_sequence() {
            return Err(HostSnapshotError::ClientEpochMismatch {
                shell: shell.client_epoch(),
                model: model.last_applied_sequence(),
            });
        }
        Ok(Self {
            sequence,
            header_revision,
            model,
            shell,
            top_bar,
        })
    }

    pub fn sequence(&self) -> u64 {
        self.sequence.get()
    }

    pub fn header_revision(&self) -> u64 {
        self.header_revision.get()
    }

    pub fn model(&self) -> &ClientModel {
        &self.model
    }

    pub fn shell(&self) -> &Shell {
        &self.shell
    }

    pub fn top_bar(&self) -> &TopBarProjectionInput {
        &self.top_bar
    }
}

/// Typed command sent from the GPUI/UI thread to the canonical shell's host
/// adapter.
#[derive(Clone, Debug, PartialEq)]
pub enum NativeNextHostCommand {
    Dispatch(ProjectedAction),
}

/// Typed receipt/state update sent by the canonical shell's host adapter.
#[derive(Clone, Debug, PartialEq)]
pub enum NativeNextHostEvent {
    Accepted {
        action_id: &'static str,
    },
    Rejected {
        action_id: &'static str,
        reason: NativeNextUnavailable,
    },
    Snapshot(HostSnapshot),
    Unavailable {
        reason: NativeNextUnavailable,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeNextUnavailable {
    NoAttachment,
    Backpressure,
    Disconnected,
    StaleSnapshot,
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeNextDispatchStatus {
    Queued,
    Rejected,
    Backpressured,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeNextHostState {
    Attached,
    Unavailable,
    Backpressured,
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeNextHostReceipt {
    pub action_id: &'static str,
    pub accepted: bool,
}

/// UI-side bounded channel handle. `try_send` is nonblocking and never queues
/// locally when the canonical shell's host adapter is saturated.
pub struct NativeNextHostClient {
    command_tx: SyncSender<NativeNextHostCommand>,
    event_rx: Receiver<NativeNextHostEvent>,
}

/// Background-side endpoint used by the canonical shell's host adapter and by
/// deterministic fake-host tests. It has no callback or unbounded queue.
pub struct NativeNextHostWorker {
    command_rx: Receiver<NativeNextHostCommand>,
    event_tx: SyncSender<NativeNextHostEvent>,
}

pub fn native_next_host_channel(capacity: usize) -> (NativeNextHostClient, NativeNextHostWorker) {
    let capacity = capacity
        .clamp(1, NATIVE_NEXT_HOST_CHANNEL_CAPACITY)
        .min(NATIVE_NEXT_HOST_CHANNEL_CAPACITY);
    let (command_tx, command_rx) = mpsc::sync_channel(capacity);
    let (event_tx, event_rx) = mpsc::sync_channel(capacity);
    (
        NativeNextHostClient {
            command_tx,
            event_rx,
        },
        NativeNextHostWorker {
            command_rx,
            event_tx,
        },
    )
}

impl NativeNextHostWorker {
    pub fn try_recv(&self) -> Result<NativeNextHostCommand, TryRecvError> {
        self.command_rx.try_recv()
    }

    pub fn send_event(&self, event: NativeNextHostEvent) -> Result<(), NativeNextUnavailable> {
        self.event_tx.try_send(event).map_err(|error| match error {
            TrySendError::Full(_) => NativeNextUnavailable::Backpressure,
            TrySendError::Disconnected(_) => NativeNextUnavailable::Disconnected,
        })
    }
}

impl NativeNextHostClient {
    fn try_dispatch(&self, action: ProjectedAction) -> NativeNextDispatchStatus {
        match self
            .command_tx
            .try_send(NativeNextHostCommand::Dispatch(action))
        {
            Ok(()) => NativeNextDispatchStatus::Queued,
            Err(TrySendError::Full(_)) => NativeNextDispatchStatus::Backpressured,
            Err(TrySendError::Disconnected(_)) => NativeNextDispatchStatus::Unavailable,
        }
    }

    fn drain_events(&mut self) -> (Vec<NativeNextHostEvent>, bool) {
        let mut events = Vec::with_capacity(NATIVE_NEXT_HOST_EVENT_DRAIN_LIMIT);
        let mut disconnected = false;
        while events.len() < NATIVE_NEXT_HOST_EVENT_DRAIN_LIMIT {
            match self.event_rx.try_recv() {
                Ok(event) => {
                    // Intermediate host snapshots are superseded by the
                    // newest contiguous snapshot. Receipts remain ordered,
                    // while a busy host cannot force the UI to replay stale
                    // projections one by one.
                    if matches!(&event, NativeNextHostEvent::Snapshot(_))
                        && matches!(events.last(), Some(NativeNextHostEvent::Snapshot(_)))
                    {
                        if let Some(last) = events.last_mut() {
                            *last = event;
                        }
                    } else {
                        events.push(event);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (events, disconnected)
    }
}

/// Register the GPUI action and keyboard shortcut owned by the native-next
/// task cockpit.
pub fn bind_native_next_actions(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("ctrl-m", OpenTaskDetailsAction, None)]);
}

/// The only immutable projection consumed by the native-next renderer.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeNextTaskCockpitProjection {
    pub header: Option<TaskHeaderModel>,
    pub top_bar: TopBarModel,
}

impl NativeNextTaskCockpitProjection {
    pub fn new(header: Option<TaskHeaderModel>, top_bar: TopBarModel) -> Self {
        Self { header, top_bar }
    }

    pub fn from_client_model(model: &ClientModel, shell: &Shell, top_bar: TopBarModel) -> Self {
        Self::new(shell.task_header(model), top_bar)
    }

    pub fn from_snapshot(snapshot: &HostSnapshot) -> Self {
        let top_bar = TopBarProjectionController::new(snapshot.top_bar().clone())
            .map(|controller| controller.model())
            .unwrap_or_else(|_| TopBarModel::unavailable());
        Self::from_client_model(snapshot.model(), snapshot.shell(), top_bar)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeNextHeaderMenuItem {
    pub field: HeaderField,
    pub label: String,
    pub description: String,
    pub tooltip: String,
    pub role: AccessibleRole,
    pub focusable: bool,
    pub action: ProjectedAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeNextHeaderMenu {
    pub label: String,
    pub description: String,
    pub tooltip: String,
    pub accessible_description: String,
    pub role: AccessibleRole,
    pub focusable: bool,
    pub items: Vec<NativeNextHeaderMenuItem>,
}

/// One render snapshot. `header_layout` is the renderer's source of truth for
/// width-dependent fields; `overflow_menu` is present only when fields are
/// actually hidden behind the accessible menu trigger.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeNextTaskCockpitSurface {
    pub header: Option<TaskHeaderModel>,
    pub header_layout: Option<HeaderLayout>,
    pub overflow_control: Option<OverflowControl>,
    pub overflow_menu: Option<NativeNextHeaderMenu>,
    pub top_bar: TopBarModel,
}

/// Semantic metadata attached to one actual renderer element. GPUI 0.2 has no
/// ARIA API, so the renderer carries the role/description next to the concrete
/// element ID, label, tooltip, and tab-stop state used by the platform tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeNextRenderNode {
    pub id: String,
    pub label: String,
    pub role: AccessibleRole,
    pub focusable: bool,
    pub accessible_description: String,
    pub tooltip: String,
    pub keyboard_shortcut: String,
    pub virtualized: bool,
    pub children: Vec<NativeNextRenderNode>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeNextRenderTree {
    pub top_bar: NativeNextRenderNode,
    pub header: Option<NativeNextRenderNode>,
    pub overflow_menu: Option<NativeNextRenderNode>,
    pub title_lines: Vec<String>,
}

/// Width/DPI facts used by the semantic top-bar layout and by the GPUI
/// surface. Width is physical pixels; `logical_width_px` is the bounded
/// width after Windows scaling has been applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeNextTopBarLayout {
    pub width_px: u16,
    pub logical_width_px: u16,
    pub scale: Scale,
    pub rows: u16,
    pub item_count: usize,
    pub visible_item_count: usize,
    pub hidden_item_count: usize,
    pub virtualized_quota_count: usize,
}

impl NativeNextTopBarLayout {
    fn for_model(top_bar: &TopBarModel, width_px: u16, scale: Scale) -> Self {
        let logical_width_px = ((u32::from(width_px) * 100) / u32::from(scale.percent()))
            .clamp(1, u32::from(u16::MAX)) as u16;
        let item_count = top_bar_item_count(top_bar);
        let min_item_width = 96u32;
        let row_capacity = (u32::from(logical_width_px) / min_item_width).max(1) as usize;
        // Flex wrapping keeps every rendered control reachable. Only quota
        // observations omitted by the bounded projection are hidden behind
        // the explicit quota overflow control.
        let visible_item_count = item_count;
        let hidden_item_count = top_bar.quota_hidden_count;
        let rows = if item_count == 0 {
            1
        } else {
            ((item_count + row_capacity - 1) / row_capacity).min(u16::MAX as usize) as u16
        };
        Self {
            width_px,
            logical_width_px,
            scale,
            rows,
            item_count,
            visible_item_count,
            hidden_item_count,
            virtualized_quota_count: top_bar.quotas.len(),
        }
    }
}

/// Immutable result produced by the bounded background/tick adapter. GPUI
/// consumes a clone of this projection; it never owns a receiver or drains a
/// host channel during paint or input dispatch.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeNextHostTick {
    pub projection: NativeNextTaskCockpitProjection,
    pub state: NativeNextHostState,
    pub last_receipt: Option<NativeNextHostReceipt>,
    pub actions_drained: usize,
    pub events_drained: usize,
}

/// Bounded host-attachment state consumed by the renderer.
pub struct NativeNextHostAttachment {
    snapshot: HostSnapshot,
    top_bar: TopBarProjectionController,
    host_client: NativeNextHostClient,
    state: NativeNextHostState,
    last_receipt: Option<NativeNextHostReceipt>,
}

impl NativeNextHostAttachment {
    pub fn new(snapshot: HostSnapshot, host_client: NativeNextHostClient) -> Self {
        let top_bar = TopBarProjectionController::new(snapshot.top_bar().clone())
            .expect("validated host snapshot must contain preflighted top bar");
        Self {
            snapshot,
            top_bar,
            host_client,
            state: NativeNextHostState::Attached,
            last_receipt: None,
        }
    }

    pub fn projection(&self) -> NativeNextTaskCockpitProjection {
        let top_bar = match self.state {
            NativeNextHostState::Unavailable => TopBarModel::unavailable(),
            NativeNextHostState::Attached
            | NativeNextHostState::Backpressured
            | NativeNextHostState::Rejected => self.top_bar.model(),
        };
        NativeNextTaskCockpitProjection::from_client_model(
            self.snapshot.model(),
            self.snapshot.shell(),
            top_bar,
        )
    }

    pub fn snapshot(&self) -> &HostSnapshot {
        &self.snapshot
    }

    pub fn state(&self) -> NativeNextHostState {
        self.state
    }

    pub fn last_receipt(&self) -> Option<&NativeNextHostReceipt> {
        self.last_receipt.as_ref()
    }

    pub fn apply_snapshot(&mut self, snapshot: HostSnapshot) -> Result<bool, HostSnapshotError> {
        if snapshot.sequence() <= self.snapshot.sequence() {
            return Err(HostSnapshotError::SequenceRegression);
        }
        if snapshot.header_revision() < self.snapshot.header_revision() {
            return Err(HostSnapshotError::HeaderRevisionRegression);
        }
        if snapshot.top_bar().generation < self.snapshot.top_bar().generation {
            return Err(HostSnapshotError::TopBarGenerationRegression);
        }
        if snapshot.top_bar().generation == self.snapshot.top_bar().generation
            && snapshot.top_bar().now_ms < self.snapshot.top_bar().now_ms
        {
            return Err(HostSnapshotError::TopBarTimestampRegression);
        }
        let current_shell = self.snapshot.shell();
        let incoming_shell = snapshot.shell();
        for (dimension, current, incoming) in [
            (
                "resource",
                current_shell.resource_generation(),
                incoming_shell.resource_generation(),
            ),
            (
                "connection",
                current_shell.connection_epoch(),
                incoming_shell.connection_epoch(),
            ),
            (
                "focus",
                current_shell.focus_epoch(),
                incoming_shell.focus_epoch(),
            ),
            (
                "client",
                current_shell.client_epoch(),
                incoming_shell.client_epoch(),
            ),
            (
                "navigation",
                current_shell.navigation_epoch(),
                incoming_shell.navigation_epoch(),
            ),
        ] {
            if incoming < current {
                return Err(HostSnapshotError::EpochRegression { dimension });
            }
        }
        // The snapshot is a whole client projection, not only the currently
        // selected row. Compare every task that exists in both generations so
        // a later selection cannot revive an older task/action fence.
        for (task_id, current_task) in self.snapshot.model().tasks() {
            let Some(incoming_task) = snapshot.model().task(*task_id) else {
                continue;
            };
            if incoming_task.task.revision < current_task.task.revision {
                return Err(HostSnapshotError::EpochRegression { dimension: "task" });
            }
            if incoming_task.task.action_epoch < current_task.task.action_epoch {
                return Err(HostSnapshotError::EpochRegression {
                    dimension: "action",
                });
            }
        }
        let mut next_top_bar = self.top_bar.clone();
        let top_bar_changed = next_top_bar
            .apply(snapshot.top_bar().clone())
            .map_err(HostSnapshotError::TopBar)?;
        let model_changed =
            snapshot.model() != self.snapshot.model() || snapshot.shell() != self.snapshot.shell();
        self.snapshot = snapshot;
        self.top_bar = next_top_bar;
        self.state = NativeNextHostState::Attached;
        Ok(top_bar_changed || model_changed)
    }

    fn dispatch_projected_action(&mut self, action: &ProjectedAction) -> NativeNextDispatchStatus {
        if self.state == NativeNextHostState::Unavailable {
            return NativeNextDispatchStatus::Unavailable;
        }
        let valid = match action.target() {
            ActionTarget::Task(_) | ActionTarget::Agent(_) => self
                .snapshot
                .shell()
                .dispatch_task_action(self.snapshot.model(), action),
            ActionTarget::Host { .. }
            | ActionTarget::Connect { .. }
            | ActionTarget::Update { .. }
            | ActionTarget::QuotaSummary { .. }
            | ActionTarget::Quota { .. } => self.top_bar.model().accepts_action(action),
        };
        if !valid {
            self.state = NativeNextHostState::Rejected;
            return NativeNextDispatchStatus::Rejected;
        }
        let status = self.host_client.try_dispatch(action.clone());
        self.state = match status {
            NativeNextDispatchStatus::Queued => NativeNextHostState::Attached,
            NativeNextDispatchStatus::Rejected => NativeNextHostState::Rejected,
            NativeNextDispatchStatus::Backpressured => NativeNextHostState::Backpressured,
            NativeNextDispatchStatus::Unavailable => NativeNextHostState::Unavailable,
        };
        status
    }

    fn drain_host_events(&mut self) -> usize {
        let mut count = 0;
        let (events, disconnected) = self.host_client.drain_events();
        for event in events {
            count += 1;
            match event {
                NativeNextHostEvent::Accepted { action_id } => {
                    self.last_receipt = Some(NativeNextHostReceipt {
                        action_id,
                        accepted: true,
                    });
                    self.state = NativeNextHostState::Attached;
                }
                NativeNextHostEvent::Rejected { action_id, .. } => {
                    self.last_receipt = Some(NativeNextHostReceipt {
                        action_id,
                        accepted: false,
                    });
                    self.state = NativeNextHostState::Rejected;
                }
                NativeNextHostEvent::Snapshot(snapshot) => {
                    if self.apply_snapshot(snapshot).is_err() {
                        self.state = NativeNextHostState::Rejected;
                    }
                }
                NativeNextHostEvent::Unavailable { .. } => {
                    self.state = NativeNextHostState::Unavailable;
                }
            }
        }
        if disconnected {
            self.state = NativeNextHostState::Unavailable;
        }
        count
    }
}

/// Bounded background/tick adapter for the host attachment.
///
/// The adapter is the only owner that drains host events or dispatches a
/// projected action to the canonical shell. A GPUI click/keyboard callback
/// only places an action in `action_rx`'s bounded queue; the caller invokes
/// [`Self::tick`] from a background/tick boundary and then publishes the
/// returned immutable snapshot to the view.
pub struct NativeNextHostTickAdapter {
    attachment: NativeNextHostAttachment,
    action_rx: Receiver<ProjectedAction>,
}

impl NativeNextHostTickAdapter {
    fn new(attachment: NativeNextHostAttachment, action_rx: Receiver<ProjectedAction>) -> Self {
        Self {
            attachment,
            action_rx,
        }
    }

    fn initial_tick(&self) -> NativeNextHostTick {
        NativeNextHostTick {
            projection: self.attachment.projection(),
            state: self.attachment.state(),
            last_receipt: self.attachment.last_receipt().cloned(),
            actions_drained: 0,
            events_drained: 0,
        }
    }

    /// Drain at most the bounded action/event budgets and return one
    /// immutable projection. No caller may observe a partially applied
    /// snapshot because `NativeNextHostAttachment` is updated before this
    /// value is published.
    pub fn tick(&mut self) -> NativeNextHostTick {
        // Apply the newest host snapshot before consuming queued input so an
        // action is always fenced against the latest immutable projection.
        let events_drained = self.attachment.drain_host_events();
        let mut actions_drained = 0;
        while actions_drained < NATIVE_NEXT_ACTION_DRAIN_LIMIT {
            match self.action_rx.try_recv() {
                Ok(action) => {
                    actions_drained += 1;
                    self.attachment.dispatch_projected_action(&action);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        NativeNextHostTick {
            projection: self.attachment.projection(),
            state: self.attachment.state(),
            last_receipt: self.attachment.last_receipt().cloned(),
            actions_drained,
            events_drained,
        }
    }

    fn apply_snapshot(
        &mut self,
        snapshot: HostSnapshot,
    ) -> Result<NativeNextHostTick, HostSnapshotError> {
        self.attachment.apply_snapshot(snapshot)?;
        Ok(self.initial_tick())
    }

    fn reject(&mut self) {
        self.attachment.state = NativeNextHostState::Rejected;
    }
}

/// GPUI renderer/controller for the native-next task cockpit.
pub struct NativeNextTaskCockpit {
    projection: NativeNextTaskCockpitProjection,
    action_tx: Option<SyncSender<ProjectedAction>>,
    host_tick: Option<NativeNextHostTickAdapter>,
    host_state: NativeNextHostState,
    last_receipt: Option<NativeNextHostReceipt>,
}

impl NativeNextTaskCockpit {
    pub fn new(projection: NativeNextTaskCockpitProjection) -> Self {
        Self {
            projection,
            action_tx: None,
            host_tick: None,
            host_state: NativeNextHostState::Unavailable,
            last_receipt: None,
        }
    }

    pub fn from_host_snapshot(snapshot: HostSnapshot, host_client: NativeNextHostClient) -> Self {
        let attachment = NativeNextHostAttachment::new(snapshot, host_client);
        let projection = attachment.projection();
        let (action_tx, action_rx) = mpsc::sync_channel(NATIVE_NEXT_ACTION_QUEUE_CAPACITY);
        Self {
            projection,
            action_tx: Some(action_tx),
            host_tick: Some(NativeNextHostTickAdapter::new(attachment, action_rx)),
            host_state: NativeNextHostState::Attached,
            last_receipt: None,
        }
    }

    pub fn from_attachment(attachment: NativeNextHostAttachment) -> Self {
        let projection = attachment.projection();
        let (action_tx, action_rx) = mpsc::sync_channel(NATIVE_NEXT_ACTION_QUEUE_CAPACITY);
        Self {
            projection,
            action_tx: Some(action_tx),
            host_tick: Some(NativeNextHostTickAdapter::new(attachment, action_rx)),
            host_state: NativeNextHostState::Attached,
            last_receipt: None,
        }
    }

    pub fn unavailable() -> Self {
        Self::new(NativeNextTaskCockpitProjection::new(
            None,
            TopBarModel::unavailable(),
        ))
    }

    pub fn projection(&self) -> &NativeNextTaskCockpitProjection {
        &self.projection
    }

    pub fn host_state(&self) -> NativeNextHostState {
        self.host_state
    }

    pub fn last_receipt(&self) -> Option<&NativeNextHostReceipt> {
        self.last_receipt.as_ref()
    }

    /// Run one bounded background/tick adapter turn and publish its immutable
    /// projection. This method is intentionally separate from `element()`;
    /// callers must invoke it from the shell's subscription/tick boundary.
    pub fn tick(&mut self) -> NativeNextHostTick {
        let Some(adapter) = self.host_tick.as_mut() else {
            return NativeNextHostTick {
                projection: self.projection.clone(),
                state: self.host_state,
                last_receipt: self.last_receipt.clone(),
                actions_drained: 0,
                events_drained: 0,
            };
        };
        let tick = adapter.tick();
        self.apply_tick(&tick);
        tick
    }

    fn apply_tick(&mut self, tick: &NativeNextHostTick) {
        self.projection = tick.projection.clone();
        self.host_state = tick.state;
        self.last_receipt = tick.last_receipt.clone();
    }

    pub fn apply_host_snapshot(
        &mut self,
        snapshot: HostSnapshot,
    ) -> Result<bool, HostSnapshotError> {
        let Some(adapter) = self.host_tick.as_mut() else {
            return Err(HostSnapshotError::NoAttachment);
        };
        let previous = self.projection.clone();
        let tick = match adapter.apply_snapshot(snapshot) {
            Ok(tick) => tick,
            Err(error) => {
                adapter.reject();
                self.host_state = NativeNextHostState::Rejected;
                return Err(error);
            }
        };
        self.apply_tick(&tick);
        Ok(previous != self.projection)
    }

    /// Queue an action for the background adapter. This operation is bounded,
    /// nonblocking, and does not dispatch to the host from a GPUI input
    /// callback.
    pub fn queue_action(&self, action: &ProjectedAction) -> NativeNextDispatchStatus {
        if self.host_state == NativeNextHostState::Unavailable {
            return NativeNextDispatchStatus::Unavailable;
        }
        let Some(action_tx) = self.action_tx.as_ref() else {
            return NativeNextDispatchStatus::Unavailable;
        };
        match action_tx.try_send(action.clone()) {
            Ok(()) => NativeNextDispatchStatus::Queued,
            Err(TrySendError::Full(_)) => NativeNextDispatchStatus::Backpressured,
            Err(TrySendError::Disconnected(_)) => NativeNextDispatchStatus::Unavailable,
        }
    }

    pub fn activate_open_task_details(&mut self) -> bool {
        let Some(header) = self.projection.header.as_ref() else {
            return false;
        };
        let action = header
            .responsive_layout(NARROW_HEADER_WIDTH_PX.saturating_sub(1))
            .overflow_control
            .map(|control| control.action)
            .unwrap_or_else(|| header.status.action.clone());
        self.queue_action(&action) == NativeNextDispatchStatus::Queued
    }

    pub fn top_bar_layout(&self, width_px: u16, scale: Scale) -> NativeNextTopBarLayout {
        NativeNextTopBarLayout::for_model(&self.projection.top_bar, width_px, scale)
    }

    pub fn render_surface(&self, width_px: u16) -> NativeNextTaskCockpitSurface {
        self.render_surface_with_scale(width_px, Scale::Scale100)
    }

    pub fn render_surface_with_scale(
        &self,
        width_px: u16,
        scale: Scale,
    ) -> NativeNextTaskCockpitSurface {
        let logical_width_px = self.top_bar_layout(width_px, scale).logical_width_px;
        let (header_layout, overflow_control, overflow_menu) = self
            .projection
            .header
            .as_ref()
            .map(|header| {
                let layout = header.responsive_layout(logical_width_px);
                let overflow_control = layout.overflow_control.clone();
                let overflow_menu = build_header_menu(header, &layout);
                (Some(layout), overflow_control, overflow_menu)
            })
            .unwrap_or((None, None, None));
        NativeNextTaskCockpitSurface {
            header: self.projection.header.clone(),
            header_layout,
            overflow_control,
            overflow_menu,
            top_bar: self.projection.top_bar.clone(),
        }
    }

    /// Build the semantic tree consumed by `element()`. Tests inspect this
    /// tree to assert the real renderer's children, roles, labels, and bounds.
    pub fn render_tree(&self, width_px: u16) -> NativeNextRenderTree {
        self.render_tree_with_scale(width_px, Scale::Scale100)
    }

    pub fn render_tree_with_scale(&self, width_px: u16, scale: Scale) -> NativeNextRenderTree {
        let layout = self.top_bar_layout(width_px, scale);
        let logical_width_px = layout.logical_width_px;
        let surface = self.render_surface_with_scale(width_px, scale);
        let mut top_bar = build_top_bar_tree(&surface.top_bar);
        top_bar.label = format!(
            "Host and resource status ({} rows, {} of {} visible)",
            layout.rows, layout.visible_item_count, layout.item_count
        );
        if self.host_state() != NativeNextHostState::Attached {
            let (label, description) = host_state_copy(self.host_state());
            top_bar.children.push(semantic_node(
                "native-next-host-state",
                label,
                AccessibleRole::Status,
                false,
                description,
                description,
                Vec::new(),
            ));
            top_bar.accessible_description =
                format!("{}. {}", top_bar.accessible_description, description);
        }
        let (header, overflow_menu, title_lines) = match (&surface.header, &surface.header_layout) {
            (Some(header), Some(layout)) => {
                let title_lines = title_lines(&layout.title);
                let header_node = build_header_tree(header, layout, logical_width_px);
                let overflow = surface.overflow_menu.as_ref().map(|menu| {
                    let mut node = semantic_node(
                        "native-next-task-overflow",
                        menu.label.clone(),
                        menu.role,
                        menu.focusable,
                        menu.accessible_description.clone(),
                        menu.tooltip.clone(),
                        menu.items
                            .iter()
                            .map(|item| {
                                semantic_node(
                                    format!("native-next-task-overflow-{:?}", item.field),
                                    item.label.clone(),
                                    item.role,
                                    item.focusable,
                                    item.description.clone(),
                                    item.tooltip.clone(),
                                    Vec::new(),
                                )
                            })
                            .collect(),
                    );
                    node.keyboard_shortcut = "Ctrl+M".to_string();
                    node
                });
                (Some(header_node), overflow, title_lines)
            }
            _ => (
                Some(semantic_node(
                    "native-next-task-header-unavailable",
                    "Task header unavailable",
                    AccessibleRole::Status,
                    false,
                    "Task details are unavailable until the host supplies an attached snapshot.",
                    "Task header unavailable.",
                    Vec::new(),
                )),
                None,
                Vec::new(),
            ),
        };
        NativeNextRenderTree {
            top_bar,
            header,
            overflow_menu,
            title_lines,
        }
    }

    fn render_projected_button(
        cx: &mut Context<Self>,
        id: impl Into<String>,
        label: impl Into<SharedString>,
        tooltip: impl Into<SharedString>,
        action: ProjectedAction,
    ) -> gpui::AnyElement {
        let id = id.into();
        let handler = cx.listener(move |this: &mut Self, _: &gpui::ClickEvent, _window, cx| {
            if this.queue_action(&action) == NativeNextDispatchStatus::Queued {
                cx.notify();
            }
        });
        Button::new(SharedString::from(id))
            .label(label)
            .tooltip(tooltip)
            .tab_stop(true)
            .on_click(handler)
            .into_any_element()
    }

    fn render_status_link(
        cx: &mut Context<Self>,
        prefix: &str,
        link: &TopBarStatusLink,
    ) -> gpui::AnyElement {
        Self::render_projected_button(
            cx,
            format!("native-next-top-bar-{prefix}"),
            link.label.clone(),
            link.tooltip.clone(),
            link.action.clone(),
        )
    }

    fn render_quota(
        cx: &mut Context<Self>,
        index: usize,
        quota: &QuotaProjection,
    ) -> gpui::AnyElement {
        Self::render_projected_button(
            cx,
            format!("native-next-top-bar-quota-{index}"),
            format!("{}: {}", quota.provider, quota.detail),
            quota.tooltip.clone(),
            quota.action.clone(),
        )
    }

    fn render_agent(
        cx: &mut Context<Self>,
        index: usize,
        agent: &AgentProjection,
    ) -> gpui::AnyElement {
        Self::render_projected_button(
            cx,
            format!("native-next-task-agent-{index}"),
            agent.label.clone(),
            agent.tooltip.clone(),
            agent.action.clone(),
        )
    }

    fn render_header_field(
        cx: &mut Context<Self>,
        header: &TaskHeaderModel,
        layout: &HeaderLayout,
        field: HeaderField,
        width_px: u16,
    ) -> gpui::AnyElement {
        match field {
            HeaderField::Title => render_title_element(&layout.title),
            HeaderField::Project => semantic_text_element(
                "native-next-task-project",
                format!("Project: {}", header.project.label),
                "Project identity for this task.",
                false,
            ),
            HeaderField::Workspace => semantic_text_element(
                "native-next-task-workspace",
                format!("Workspace: {}", workspace_label(&header.workspace)),
                "Workspace and branch for this task.",
                false,
            ),
            HeaderField::Primary => match &header.primary {
                PrimaryAgentProjection::Present(agent) => Self::render_agent(cx, 0, agent),
                PrimaryAgentProjection::Unavailable {
                    label, description, ..
                } => semantic_text_element(
                    "native-next-task-primary-unavailable",
                    label,
                    description,
                    false,
                ),
            },
            HeaderField::Specialists => render_specialists_element(cx, header, width_px),
            HeaderField::TurnStatus => Self::render_projected_button(
                cx,
                "native-next-task-status",
                header.status.label.clone(),
                header.status.tooltip.clone(),
                header.status.action.clone(),
            ),
        }
    }

    fn render_overflow_menu(
        cx: &mut Context<Self>,
        menu: &NativeNextHeaderMenu,
    ) -> gpui::AnyElement {
        let menu_items = menu.items.clone();
        let entity = cx.entity();
        Button::new("native-next-task-overflow")
            .label(menu.label.clone())
            .tooltip(menu.tooltip.clone())
            .tab_stop(menu.focusable)
            .dropdown_menu(move |popup, _, _| {
                menu_items.iter().cloned().fold(popup, |popup, item| {
                    let action = item.action.clone();
                    let entity = entity.clone();
                    let label = format!("{} — {}", item.label, item.description);
                    popup.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            if this.queue_action(&action) == NativeNextDispatchStatus::Queued {
                                cx.notify();
                            }
                        });
                    }))
                })
            })
            .into_any_element()
    }

    fn element(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width_px = window_width_px(window);
        let scale = scale_from_factor(window.scale_factor());
        let logical_width_px = self.top_bar_layout(width_px, scale).logical_width_px;
        let surface = self.render_surface_with_scale(width_px, scale);
        let tree = self.render_tree_with_scale(width_px, scale);

        let top_bar_description: SharedString = tree.top_bar.accessible_description.clone().into();
        let mut top_bar = div()
            .id(SharedString::from(tree.top_bar.id.clone()))
            .tooltip(move |window, cx| Tooltip::new(top_bar_description.clone()).build(window, cx))
            .flex()
            .flex_wrap()
            .items_start()
            .gap_2();
        if let Some(host) = &surface.top_bar.host {
            top_bar = top_bar.child(Self::render_status_link(cx, "host", host));
        }
        if let Some(connect) = &surface.top_bar.connect {
            top_bar = top_bar.child(Self::render_status_link(cx, "connect", connect));
        }
        if let Some(update) = &surface.top_bar.update {
            top_bar = top_bar.child(Self::render_status_link(cx, "update", update));
        }
        if let Some(resources) = &surface.top_bar.resources {
            if let Some(cpu) = &resources.cpu {
                top_bar = top_bar.child(semantic_text_element(
                    "native-next-top-bar-cpu",
                    tree.top_bar
                        .children
                        .iter()
                        .find(|node| node.id == "native-next-top-bar-cpu")
                        .map_or_else(
                            || format!("CPU {:.1}%", cpu.whole_machine_percent),
                            |node| node.label.clone(),
                        ),
                    tree.top_bar
                        .children
                        .iter()
                        .find(|node| node.id == "native-next-top-bar-cpu")
                        .map_or_else(
                            || "Whole-machine CPU utilization.".to_string(),
                            |node| node.accessible_description.clone(),
                        ),
                    false,
                ));
            }
            if let Some(memory) = resources.memory_bytes {
                top_bar = top_bar.child(semantic_text_element(
                    "native-next-top-bar-memory",
                    tree.top_bar
                        .children
                        .iter()
                        .find(|node| node.id == "native-next-top-bar-memory")
                        .map_or_else(
                            || format!("Memory {}", format_bytes(memory)),
                            |node| node.label.clone(),
                        ),
                    tree.top_bar
                        .children
                        .iter()
                        .find(|node| node.id == "native-next-top-bar-memory")
                        .map_or_else(
                            || "Host memory usage.".to_string(),
                            |node| node.accessible_description.clone(),
                        ),
                    false,
                ));
            }
        }
        for (index, quota) in surface.top_bar.quotas.iter().enumerate() {
            top_bar = top_bar.child(Self::render_quota(cx, index, quota));
        }
        if let Some(action) = &surface.top_bar.quota_overflow_action {
            top_bar = top_bar.child(Self::render_projected_button(
                cx,
                "native-next-top-bar-quota-overflow",
                format!("More quotas ({})", surface.top_bar.quota_hidden_count),
                "Open remaining quota details.",
                action.clone(),
            ));
        }
        if !surface.top_bar.unavailable.is_empty()
            || (surface.top_bar.host.is_none()
                && surface.top_bar.connect.is_none()
                && surface.top_bar.update.is_none()
                && surface.top_bar.resources.is_none()
                && surface.top_bar.quotas.is_empty())
        {
            top_bar = top_bar.child(semantic_text_element(
                "native-next-top-bar-unavailable",
                tree.top_bar
                    .children
                    .iter()
                    .find(|node| node.id == "native-next-top-bar-unavailable")
                    .map_or_else(
                        || unavailable_label(&surface.top_bar.unavailable),
                        |node| node.label.clone(),
                    ),
                tree.top_bar.accessible_description.clone(),
                false,
            ));
        }
        if self.host_state() != NativeNextHostState::Attached {
            let (label, description) = host_state_copy(self.host_state());
            top_bar = top_bar.child(semantic_text_element(
                "native-next-host-state",
                label,
                description,
                false,
            ));
        }

        let header_description: SharedString = tree
            .header
            .as_ref()
            .map_or_else(
                || "Task header unavailable.".to_string(),
                |node| node.accessible_description.clone(),
            )
            .into();
        let mut header_element = div()
            .id(SharedString::from(tree.header.as_ref().map_or_else(
                || "native-next-task-header".to_string(),
                |node| node.id.clone(),
            )))
            .tooltip(move |window, cx| Tooltip::new(header_description.clone()).build(window, cx))
            .flex()
            .flex_wrap()
            .items_start()
            .gap_2();
        if let (Some(header), Some(layout)) = (&surface.header, &surface.header_layout) {
            for field in &layout.inline {
                header_element = header_element.child(Self::render_header_field(
                    cx,
                    header,
                    layout,
                    *field,
                    logical_width_px,
                ));
            }
            if let Some(menu) = &surface.overflow_menu {
                header_element = header_element.child(Self::render_overflow_menu(cx, menu));
            }
        } else {
            header_element = header_element.child(semantic_text_element(
                "native-next-task-header-unavailable",
                "Task header unavailable",
                "Task details are unavailable until the host supplies an attached snapshot.",
                false,
            ));
        }

        div()
            .id("native-next-task-cockpit")
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .on_action::<OpenTaskDetailsAction>(cx.listener(|this, _, _, cx| {
                if this.activate_open_task_details() {
                    cx.notify();
                }
            }))
            .child(top_bar)
            .child(header_element)
    }
}

impl Render for NativeNextTaskCockpit {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.element(window, cx)
    }
}

fn semantic_node(
    id: impl Into<String>,
    label: impl Into<String>,
    role: AccessibleRole,
    focusable: bool,
    accessible_description: impl Into<String>,
    tooltip: impl Into<String>,
    children: Vec<NativeNextRenderNode>,
) -> NativeNextRenderNode {
    NativeNextRenderNode {
        id: id.into(),
        label: label.into(),
        role,
        focusable,
        accessible_description: accessible_description.into(),
        tooltip: tooltip.into(),
        keyboard_shortcut: if focusable {
            "Enter/Space".to_string()
        } else {
            String::new()
        },
        virtualized: false,
        children,
    }
}

fn top_bar_item_count(top_bar: &TopBarModel) -> usize {
    usize::from(top_bar.host.is_some())
        + usize::from(top_bar.connect.is_some())
        + usize::from(top_bar.update.is_some())
        + top_bar.resources.as_ref().map_or(0, |resources| {
            usize::from(resources.cpu.is_some()) + usize::from(resources.memory_bytes.is_some())
        })
        + top_bar.quotas.len()
        + usize::from(top_bar.quota_overflow_action.is_some())
        + usize::from(!top_bar.unavailable.is_empty())
}

fn build_top_bar_tree(top_bar: &TopBarModel) -> NativeNextRenderNode {
    let mut children = Vec::new();
    if let Some(link) = &top_bar.host {
        children.push(semantic_node(
            "native-next-top-bar-host",
            link.label.clone(),
            link.role,
            link.focusable,
            link.description.clone(),
            link.tooltip.clone(),
            Vec::new(),
        ));
    }
    if let Some(link) = &top_bar.connect {
        children.push(semantic_node(
            "native-next-top-bar-connect",
            link.label.clone(),
            link.role,
            link.focusable,
            link.description.clone(),
            link.tooltip.clone(),
            Vec::new(),
        ));
    }
    if let Some(link) = &top_bar.update {
        children.push(semantic_node(
            "native-next-top-bar-update",
            link.label.clone(),
            link.role,
            link.focusable,
            link.description.clone(),
            link.tooltip.clone(),
            Vec::new(),
        ));
    }
    if let Some(resources) = &top_bar.resources {
        if let Some(cpu) = &resources.cpu {
            children.push(semantic_node(
                "native-next-top-bar-cpu",
                format!("CPU {:.1}%", cpu.whole_machine_percent),
                AccessibleRole::Status,
                false,
                "Whole-machine CPU utilization.",
                "Whole-machine CPU utilization.",
                Vec::new(),
            ));
        }
        if let Some(memory) = resources.memory_bytes {
            children.push(semantic_node(
                "native-next-top-bar-memory",
                format!("Memory {}", format_bytes(memory)),
                AccessibleRole::Status,
                false,
                "Host memory usage.",
                "Host memory usage.",
                Vec::new(),
            ));
        }
    }
    for (index, quota) in top_bar.quotas.iter().enumerate() {
        children.push(semantic_node(
            format!("native-next-top-bar-quota-{index}"),
            format!("{}: {}", quota.provider, quota.detail),
            quota.role,
            quota.focusable,
            quota.tooltip.clone(),
            quota.tooltip.clone(),
            Vec::new(),
        ));
    }
    if top_bar.quota_overflow_action.is_some() {
        children.push(semantic_node(
            "native-next-top-bar-quota-overflow",
            format!("More quotas ({})", top_bar.quota_hidden_count),
            AccessibleRole::Button,
            true,
            "Open remaining quota details.",
            "Open remaining quota details.",
            Vec::new(),
        ));
    }
    for unavailable in &top_bar.unavailable {
        children.push(semantic_node(
            format!("native-next-top-bar-unavailable-{:?}", unavailable),
            unavailable_label(std::slice::from_ref(unavailable)),
            AccessibleRole::Status,
            false,
            top_bar.accessible_description.clone(),
            top_bar.accessible_description.clone(),
            Vec::new(),
        ));
    }
    if children.is_empty() {
        children.push(semantic_node(
            "native-next-top-bar-unavailable",
            "Host status unavailable",
            AccessibleRole::Status,
            false,
            top_bar.accessible_description.clone(),
            top_bar.accessible_description.clone(),
            Vec::new(),
        ));
    }
    semantic_node(
        "native-next-top-bar",
        "Host and resource status",
        AccessibleRole::Region,
        false,
        top_bar.accessible_description.clone(),
        top_bar.accessible_description.clone(),
        children,
    )
}

fn build_header_tree(
    header: &TaskHeaderModel,
    layout: &HeaderLayout,
    width_px: u16,
) -> NativeNextRenderNode {
    let mut children = Vec::new();
    for field in &layout.inline {
        match field {
            HeaderField::Title => children.push(semantic_node(
                "native-next-task-title",
                header.title.clone(),
                AccessibleRole::Region,
                false,
                "Task title.",
                "Task title.",
                title_lines(&layout.title)
                    .into_iter()
                    .enumerate()
                    .map(|(index, line)| {
                        semantic_node(
                            format!("native-next-task-title-line-{index}"),
                            line,
                            AccessibleRole::Region,
                            false,
                            "Task title line.",
                            "Task title line.",
                            Vec::new(),
                        )
                    })
                    .collect(),
            )),
            HeaderField::Project => children.push(semantic_node(
                "native-next-task-project",
                format!("Project: {}", header.project.label),
                AccessibleRole::Region,
                false,
                "Project identity for this task.",
                "Project identity for this task.",
                Vec::new(),
            )),
            HeaderField::Workspace => children.push(semantic_node(
                "native-next-task-workspace",
                format!("Workspace: {}", workspace_label(&header.workspace)),
                AccessibleRole::Region,
                false,
                "Workspace and branch for this task.",
                "Workspace and branch for this task.",
                Vec::new(),
            )),
            HeaderField::Primary => {
                let (label, description, role, focusable) = match &header.primary {
                    PrimaryAgentProjection::Present(agent) => (
                        agent.label.clone(),
                        agent.accessible_description.clone(),
                        agent.accessibility_role,
                        agent.focusable,
                    ),
                    PrimaryAgentProjection::Unavailable {
                        label, description, ..
                    } => (
                        label.clone(),
                        description.clone(),
                        AccessibleRole::Status,
                        false,
                    ),
                };
                children.push(semantic_node(
                    "native-next-task-primary",
                    label,
                    role,
                    focusable,
                    description.clone(),
                    description,
                    Vec::new(),
                ));
            }
            HeaderField::Specialists => {
                let visible = if width_px < NARROW_HEADER_WIDTH_PX {
                    0
                } else {
                    header
                        .specialists
                        .len()
                        .min(NATIVE_NEXT_SPECIALIST_SEMANTIC_WINDOW)
                };
                let specialist_children: Vec<NativeNextRenderNode> = header
                    .specialists
                    .iter()
                    .take(visible)
                    .enumerate()
                    .map(|(index, agent)| {
                        semantic_node(
                            format!("native-next-task-agent-{}", index + 1),
                            agent.label.clone(),
                            agent.accessibility_role,
                            agent.focusable,
                            agent.accessible_description.clone(),
                            agent.tooltip.clone(),
                            Vec::new(),
                        )
                    })
                    .collect();
                children.push(NativeNextRenderNode {
                    id: "native-next-task-specialists".to_string(),
                    label: format!(
                        "Specialists: {} shown, {} hidden",
                        visible,
                        header.specialist_total.saturating_sub(visible)
                    ),
                    role: AccessibleRole::Region,
                    focusable: false,
                    accessible_description: format!(
                        "Specialist agents: {} shown, {} hidden.",
                        visible,
                        header.specialist_total.saturating_sub(visible)
                    ),
                    tooltip: "Specialist agents attached to this task.".to_string(),
                    keyboard_shortcut: String::new(),
                    virtualized: true,
                    children: specialist_children,
                });
            }
            HeaderField::TurnStatus => children.push(semantic_node(
                "native-next-task-status",
                header.status.label.clone(),
                header.status.role,
                header.status.focusable,
                header.status.description.clone(),
                header.status.tooltip.clone(),
                Vec::new(),
            )),
        }
    }
    if !layout.inline.contains(&HeaderField::Specialists) {
        children.push(NativeNextRenderNode {
            id: "native-next-task-specialists".to_string(),
            label: format!("Specialists: 0 shown, {} hidden", header.specialist_total),
            role: AccessibleRole::Region,
            focusable: false,
            accessible_description: format!(
                "Specialist agents are in More task details. {} hidden.",
                header.specialist_total
            ),
            tooltip: "Specialist agents attached to this task.".to_string(),
            keyboard_shortcut: String::new(),
            virtualized: true,
            children: Vec::new(),
        });
    }
    semantic_node(
        "native-next-task-header",
        "Task header",
        AccessibleRole::Region,
        false,
        layout.accessible_description.clone(),
        layout.accessible_description.clone(),
        children,
    )
}

fn build_header_menu(
    header: &TaskHeaderModel,
    layout: &HeaderLayout,
) -> Option<NativeNextHeaderMenu> {
    if layout.overflow.is_empty() {
        return None;
    }
    let items = layout
        .overflow
        .iter()
        .map(|field| {
            let label = header_field_label(header, *field);
            let description = header_field_description(*field);
            NativeNextHeaderMenuItem {
                field: *field,
                label,
                description: description.clone(),
                tooltip: description,
                role: AccessibleRole::Button,
                focusable: true,
                action: header_field_action(header, *field),
            }
        })
        .collect();
    Some(NativeNextHeaderMenu {
        label: layout
            .overflow_control
            .as_ref()
            .map(|control| control.label.clone())
            .unwrap_or_else(|| "More task details".to_string()),
        description: "Open additional task header details.".to_string(),
        tooltip: layout
            .overflow_control
            .as_ref()
            .map(|control| control.tooltip.clone())
            .unwrap_or_else(|| "Open More task details".to_string()),
        accessible_description: layout.accessible_description.clone(),
        role: AccessibleRole::Menu,
        focusable: true,
        items,
    })
}

fn header_field_action(header: &TaskHeaderModel, field: HeaderField) -> ProjectedAction {
    match field {
        HeaderField::Primary => match &header.primary {
            PrimaryAgentProjection::Present(agent) => agent.action.clone(),
            PrimaryAgentProjection::Unavailable { .. } => header.status.action.clone(),
        },
        _ => header.status.action.clone(),
    }
}

fn header_field_label(header: &TaskHeaderModel, field: HeaderField) -> String {
    match field {
        HeaderField::Title => format!("Title: {}", header.title),
        HeaderField::Project => format!("Project: {}", header.project.label),
        HeaderField::Workspace => format!("Workspace: {}", workspace_label(&header.workspace)),
        HeaderField::Primary => match &header.primary {
            PrimaryAgentProjection::Present(agent) => format!("Primary agent: {}", agent.label),
            PrimaryAgentProjection::Unavailable { label, .. } => format!("Primary agent: {label}"),
        },
        HeaderField::Specialists => format!(
            "Specialists: {} shown, {} hidden",
            header.specialists.len(),
            header.specialist_hidden_count
        ),
        HeaderField::TurnStatus => format!("Status: {}", header.status.label),
    }
}

fn header_field_description(field: HeaderField) -> String {
    match field {
        HeaderField::Title => "Task title.".to_string(),
        HeaderField::Project => "Project identity for this task.".to_string(),
        HeaderField::Workspace => "Workspace and branch for this task.".to_string(),
        HeaderField::Primary => "Primary agent and provider for this task.".to_string(),
        HeaderField::Specialists => "Specialist agents attached to this task.".to_string(),
        HeaderField::TurnStatus => "Current task turn status.".to_string(),
    }
}

fn workspace_label(workspace: &WorkspaceProjection) -> String {
    match workspace {
        WorkspaceProjection::Main => "main workspace".to_string(),
        WorkspaceProjection::Worktree { branch, .. } => format!("worktree {branch}"),
        WorkspaceProjection::External { .. } => "external workspace".to_string(),
    }
}

fn title_lines(layout: &TitleLayout) -> Vec<String> {
    match layout {
        TitleLayout::SingleLine(value) | TitleLayout::Truncated(value) => vec![value.clone()],
        TitleLayout::Wrapped(lines) => lines.clone(),
    }
}

fn render_title_element(layout: &TitleLayout) -> gpui::AnyElement {
    let lines = title_lines(layout);
    div()
        .id("native-next-task-title")
        .flex()
        .flex_col()
        .children(lines.into_iter().enumerate().map(|(index, line)| {
            div()
                .id(SharedString::from(format!(
                    "native-next-task-title-line-{index}"
                )))
                .child(line)
        }))
        .into_any_element()
}

fn render_specialists_element(
    cx: &mut Context<NativeNextTaskCockpit>,
    header: &TaskHeaderModel,
    width_px: u16,
) -> gpui::AnyElement {
    if width_px < NARROW_HEADER_WIDTH_PX {
        return semantic_text_element(
            "native-next-task-specialists-overflow",
            format!(
                "{} specialists in More task details",
                header.specialist_total
            ),
            "Specialist agents are available in More task details.",
            false,
        );
    }
    let agents = header.specialists.iter().cloned().collect::<Vec<_>>();
    let entity = cx.entity();
    let list = uniform_list(
        "native-next-task-specialists-list",
        agents.len(),
        move |range, _window, _app| {
            range
                .filter_map(|index| agents.get(index).cloned())
                .enumerate()
                .map(|(index, agent)| {
                    let action = agent.action.clone();
                    let entity = entity.clone();
                    Button::new(SharedString::from(format!(
                        "native-next-task-agent-{}",
                        index + 1
                    )))
                    .label(agent.label.clone())
                    .tooltip(agent.tooltip.clone())
                    .tab_stop(true)
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| {
                            if this.queue_action(&action) == NativeNextDispatchStatus::Queued {
                                cx.notify();
                            }
                        });
                    })
                })
                .collect::<Vec<_>>()
        },
    );
    div()
        .id("native-next-task-specialists")
        .overflow_y_scroll()
        .child(list)
        .into_any_element()
}

fn semantic_text_element(
    id: impl Into<String>,
    label: impl Into<String>,
    tooltip: impl Into<SharedString>,
    focusable: bool,
) -> gpui::AnyElement {
    let id: SharedString = id.into().into();
    let tooltip = tooltip.into();
    div()
        .id(id)
        .tab_stop(focusable)
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .child(label.into())
        .into_any_element()
}

fn unavailable_label(unavailable: &[TopBarUnavailable]) -> String {
    if unavailable.is_empty() {
        return "Host status unavailable".to_string();
    }
    unavailable
        .iter()
        .map(|value| match value {
            TopBarUnavailable::HostStatus => "Host status",
            TopBarUnavailable::ConnectionStatus => "Connection status",
            TopBarUnavailable::UpdateStatus => "Update status",
            TopBarUnavailable::Cpu => "CPU",
            TopBarUnavailable::Memory => "Memory",
            TopBarUnavailable::Quota => "Quota",
        })
        .collect::<Vec<_>>()
        .join(", ")
        + " unavailable"
}

fn host_state_copy(state: NativeNextHostState) -> (&'static str, &'static str) {
    match state {
        NativeNextHostState::Attached => ("Host attached", "Host client is attached."),
        NativeNextHostState::Unavailable => (
            "Host client unavailable",
            "Host client is unavailable. Reconnect to restore live status.",
        ),
        NativeNextHostState::Backpressured => (
            "Host client busy",
            "Host client is busy. Try the action again when the queue drains.",
        ),
        NativeNextHostState::Rejected => (
            "Host action rejected",
            "The host rejected the action or snapshot. Refresh the task details.",
        ),
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn window_width_px(window: &Window) -> u16 {
    window
        .bounds()
        .size
        .width
        .to_f64()
        .round()
        .clamp(0.0, f64::from(u16::MAX)) as u16
}

fn scale_from_factor(factor: f32) -> Scale {
    match factor {
        value if value >= 1.75 => Scale::Scale200,
        value if value >= 1.375 => Scale::Scale150,
        value if value >= 1.125 => Scale::Scale125,
        _ => Scale::Scale100,
    }
}

pub fn is_task_details_action(action: &ProjectedAction) -> bool {
    action.id() == crate::client::action::ACTION_TASK_SHOW
        && matches!(action.target(), ActionTarget::Task(_))
}
