use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::domain::cockpit::TaskTerminalsProjection;
use crate::domain::id::ResourceId;
use crate::domain::{
    CommandId, EventId, PrivacyClass, SemanticJournalFact, SemanticJournalPage,
    SemanticJournalPayload, TaskId, TaskTerminalProjection,
};

#[cfg(test)]
use super::TaskWorkspace;
use super::{Axis, PanePresentation, Workspace, WorkspaceError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationQueryPriority {
    Interactive,
    Background,
}

/// Slow recovery heartbeat when a ConversationDirty push may have been missed.
/// Primary conversation refresh is push-driven; this is not an idle poll cadence.
pub const CONVERSATION_RECOVERY_HEARTBEAT: Duration = Duration::from_secs(30);
/// A task already projected as working gets one bounded completion check each
/// second while it remains open. Idle tasks remain push-driven and do not poll.
pub const WORKING_CONVERSATION_POLL_INTERVAL: Duration = Duration::from_secs(1);

pub fn working_conversation_poll_due(
    projected_working: bool,
    elapsed_since_poll: Duration,
) -> bool {
    projected_working && elapsed_since_poll >= WORKING_CONVERSATION_POLL_INTERVAL
}

/// Which conversation priorities are due for slow recovery polling.
///
/// Ordinary interactive refresh is push-driven via `ConversationDirty`. This
/// returns priorities only after the recovery heartbeat has elapsed.
pub fn conversation_poll_priorities_due(
    elapsed_since_recovery: Duration,
) -> Vec<ConversationQueryPriority> {
    if elapsed_since_recovery < CONVERSATION_RECOVERY_HEARTBEAT {
        return Vec::new();
    }
    vec![
        ConversationQueryPriority::Interactive,
        ConversationQueryPriority::Background,
    ]
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationQueryPlan<K = TaskId> {
    pub task_id: K,
    pub priority: ConversationQueryPriority,
}

impl Copy for ConversationQueryPlan<TaskId> {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceSelectionGesture {
    Plain,
    Toggle,
}

/// Apply the task-list gesture without coupling the recursive model to GPUI.
/// Plain selection focuses an open task or replaces the focused slot. Shift-
/// selection adds or removes panes.
pub fn apply_workspace_selection<K: Clone + Ord + Eq>(
    workspace: &mut Option<Workspace<K>>,
    task_id: K,
    gesture: WorkspaceSelectionGesture,
) -> Result<(), WorkspaceError> {
    let Some(current) = workspace.as_mut() else {
        *workspace = Some(Workspace::single(task_id));
        return Ok(());
    };

    match gesture {
        WorkspaceSelectionGesture::Plain => {
            if current.contains_task(task_id.clone()) {
                current.focus_task(task_id)
            } else if current.pane_count() <= 1 {
                *workspace = Some(Workspace::single(task_id));
                Ok(())
            } else {
                current.replace_focused_task(task_id)
            }
        }
        WorkspaceSelectionGesture::Toggle if current.contains_task(task_id.clone()) => {
            let pane_id = current
                .pane_for_task(task_id)
                .map(|pane| pane.id)
                .ok_or(WorkspaceError::MissingPane)?;
            current.remove_pane(pane_id)?;
            if current.pane_count() == 0 {
                *workspace = None;
            }
            Ok(())
        }
        WorkspaceSelectionGesture::Toggle => {
            current.insert_after_focused(task_id, Axis::Horizontal)?;
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskConversationCache {
    facts: Vec<SemanticJournalFact>,
    high_water: u64,
    through_sequence: u64,
    next_sequence: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationAdmission {
    /// Present only when durable conversation facts changed. Cursor-only polls
    /// leave this `None` so callers do not clone or reproject the page.
    pub page: Option<SemanticJournalPage>,
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingUserMessage {
    command_id: CommandId,
    event_id: EventId,
    text: String,
}

impl TaskConversationCache {
    pub fn as_page(&self) -> SemanticJournalPage {
        SemanticJournalPage {
            oldest_sequence: 0,
            cursor_rolled_over: false,
            after_sequence: 0,
            through_sequence: self.through_sequence,
            high_water: self.high_water,
            encoded_bytes: 0,
            next_sequence: self.next_sequence,
            facts: self.facts.clone(),
        }
    }

    pub fn request_after_sequence(&self) -> u64 {
        self.next_sequence
            .unwrap_or_else(|| self.high_water.max(self.through_sequence))
    }

    pub fn high_water(&self) -> u64 {
        self.high_water
    }

    pub fn fact_count(&self) -> usize {
        self.facts.len()
    }

    /// The durable facts, borrowed. [`Self::as_page`] deep-clones the whole
    /// vector, which is the right shape for a caller that owns a page and the
    /// wrong one for a caller that only reads it -- the board recomputes its
    /// row facts on every paint, and a clone per task per frame is pure waste.
    pub fn facts(&self) -> &[SemanticJournalFact] {
        &self.facts
    }

    pub fn through_sequence(&self) -> u64 {
        self.through_sequence
    }

    fn latest_message_is_assistant(&self) -> bool {
        self.facts
            .iter()
            .rev()
            .find_map(|fact| match &fact.payload {
                SemanticJournalPayload::UserMessage { .. } => Some(false),
                SemanticJournalPayload::AssistantText { .. } => Some(true),
                _ => None,
            })
            == Some(true)
    }

    fn latest_message_is_user(&self) -> bool {
        self.facts
            .iter()
            .rev()
            .find_map(|fact| match &fact.payload {
                SemanticJournalPayload::UserMessage { .. } => Some(true),
                SemanticJournalPayload::AssistantText { .. } => Some(false),
                _ => None,
            })
            == Some(true)
    }

    /// Admit a monotonic page. Updates durable cursors always. Reports
    /// `facts_changed` only when the retained fact list mutates so empty or
    /// cursor-only polls do not force a visual reprojection.
    pub fn merge_page(&mut self, page: &SemanticJournalPage) -> bool {
        let prior_len = self.facts.len();
        let reset_changed = page.cursor_rolled_over && prior_len > 0;
        if page.cursor_rolled_over {
            self.facts.clear();
            self.high_water = 0;
            self.through_sequence = 0;
            self.next_sequence = None;
        }

        if page.facts.is_empty() {
            self.high_water = self.high_water.max(page.high_water);
            if page.next_sequence.is_some() {
                self.next_sequence = page.next_sequence;
            }
            if self.next_sequence.is_none() {
                self.through_sequence = self.through_sequence.max(page.through_sequence);
            }
            return reset_changed;
        }

        let mut changed = reset_changed;
        let mut positions = self
            .facts
            .iter()
            .enumerate()
            .map(|(index, fact)| (fact.id, index))
            .collect::<std::collections::HashMap<_, _>>();
        let mut sequences = self
            .facts
            .iter()
            .map(|fact| fact.sequence)
            .collect::<std::collections::HashSet<_>>();
        for fact in &page.facts {
            if let Some(index) = positions.get(&fact.id).copied() {
                if fact.sequence > self.facts[index].sequence {
                    sequences.remove(&self.facts[index].sequence);
                    sequences.insert(fact.sequence);
                    self.facts[index] = fact.clone();
                    changed = true;
                }
            } else if sequences.insert(fact.sequence) {
                positions.insert(fact.id, self.facts.len());
                self.facts.push(fact.clone());
                changed = true;
            }
        }
        if changed {
            self.facts.sort_by_key(|fact| fact.sequence);
        }
        self.high_water = self.high_water.max(page.high_water);
        self.through_sequence = self.through_sequence.max(page.through_sequence);
        self.next_sequence = page.next_sequence;
        if self.next_sequence.is_none() {
            self.through_sequence = self
                .facts
                .last()
                .map(|fact| fact.sequence)
                .unwrap_or(self.through_sequence)
                .max(page.through_sequence);
        }
        changed
    }

    fn latest_snippet(&self) -> Option<&str> {
        self.facts.iter().rev().find_map(fact_snippet)
    }

    fn tail_snippets(&self, max: usize) -> Vec<String> {
        let mut snippets: Vec<_> = self
            .facts
            .iter()
            .rev()
            .filter_map(fact_snippet)
            .take(max)
            .map(str::to_string)
            .collect();
        snippets.reverse();
        snippets
    }
}

fn fact_snippet(fact: &SemanticJournalFact) -> Option<&str> {
    match &fact.payload {
        SemanticJournalPayload::UserMessage { text }
        | SemanticJournalPayload::AssistantText { text }
        | SemanticJournalPayload::ReasoningSummary { text } => Some(text.as_str()),
        SemanticJournalPayload::ApprovalRequest { summary, .. } => Some(summary.as_str()),
        SemanticJournalPayload::Question { prompt, .. } => Some(prompt.as_str()),
        SemanticJournalPayload::PlanStep { title, .. } => Some(title.as_str()),
        SemanticJournalPayload::Error { message, .. } => Some(message.as_str()),
        SemanticJournalPayload::ArtifactReference { label } => Some(label.as_str()),
        _ => None,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskSurfaceState {
    pub conversation: TaskConversationCache,
    pub conversation_generation: u64,
    pub conversation_in_flight: bool,
    /// Fair-scheduling watermark; lower values are admitted first on the next
    /// bounded background wave.
    pub last_conversation_scheduled_at: u64,
    pub latest_snippet: Option<String>,
    /// One retained screen per terminal resource on the Task. The provider slot
    /// and every plain shell live here side by side; nothing in this map is
    /// "the" terminal on its own -- `focused_resource` decides that.
    pub terminals: BTreeMap<ResourceId, TaskTerminalProjection>,
    /// The Task's terminal strip as the host last answered it, when one has
    /// been queried. `None` means the strip has never been admitted, which is
    /// not the same as an empty strip.
    pub strip: Option<TaskTerminalsProjection>,
    /// Attachment and query-lease state PER TERMINAL, keyed the way the shell
    /// keys everything else terminal-shaped. Screens are per resource, so
    /// these have to be too: one record per Task meant a bounded retry for an
    /// unfocused provider relabelled the focused shell "reconnecting" and
    /// dropped its interactivity, with nothing on screen explaining why.
    attachments: BTreeMap<TerminalSurfaceTarget, TerminalAttachment>,
    pending_user_messages: Vec<PendingUserMessage>,
}

/// Which of a Task's terminals one attachment record belongs to.
///
/// `None` is the provider slot -- the terminal the legacy provider-only
/// cockpit queries address, which carries no resource id of its own until its
/// first projection lands. This mirrors the shell's `TerminalTarget` exactly,
/// and `terminal_surface_target` is the one derivation between them.
pub type TerminalSurfaceTarget = Option<ResourceId>;

/// The attachment slot one projection belongs to.
///
/// Shell recognition is the shared rule on the projection itself, so a
/// projection from a host that predates plain shells lands in the provider
/// slot -- the only terminal such a host has.
pub fn terminal_surface_target(projection: &TaskTerminalProjection) -> TerminalSurfaceTarget {
    (projection.is_plain_shell()).then_some(projection.resource_id)
}

/// One terminal's client-side attachment: what the UI should say about it, and
/// whether a screen query for it is outstanding.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TerminalAttachment {
    state: TerminalAttachmentState,
    query_in_flight: bool,
    /// The host's own sentence behind the refusal that put this terminal in
    /// `Unavailable`. The closed reason enum cannot tell "no shell is
    /// installed" from "the recipe was rejected", and the operator reading the
    /// client has no access to the host's stderr.
    detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalAttachmentState {
    #[default]
    Unavailable,
    Starting,
    Live,
    StaleReconnecting,
    Exited,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CenterSurfaceLoadingState {
    ConversationInitial,
    TerminalInitial,
}

impl TaskSurfaceState {
    /// The one terminal resource this surface is currently showing.
    ///
    /// The host-owned strip focus wins whenever the strip has been admitted
    /// and names a resource -- deliberately even before that terminal's first
    /// screen has arrived, so a newly focused chip reads as "starting" rather
    /// than silently falling back to the previous terminal. Without a strip
    /// the provider slot is the default, and a Task that has only shells falls
    /// back to the first by resource id so a screen is never dropped.
    ///
    /// The provider is recognised by the shared projection rule, not by
    /// `is_provider` alone, so a projection decoded from a host that predates
    /// plain shells is still found here.
    pub fn focused_resource(&self) -> Option<ResourceId> {
        self.strip
            .as_ref()
            .and_then(|strip| strip.focused)
            .or_else(|| {
                self.terminals
                    .values()
                    .find(|terminal| !terminal.is_plain_shell())
                    .map(|terminal| terminal.resource_id)
            })
            .or_else(|| self.terminals.keys().next().copied())
    }

    /// The focused terminal: its durable resource, and whether that resource is
    /// the Task's provider slot rather than a plain shell.
    ///
    /// The host-owned strip is the authority whenever one has been admitted and
    /// names a focused chip, because it is the only source that can answer
    /// while that terminal's first screen is still in flight -- which is
    /// exactly the window in which a resize would otherwise be aimed at the
    /// wrong PTY. Without a strip (or with a strip that focuses nothing) the
    /// retained projection answers, and a Task with neither has only the
    /// provider slot to address.
    pub fn focused_terminal(&self) -> Option<(ResourceId, bool)> {
        if let Some(strip) = self.strip.as_ref() {
            if let Some(resource_id) = strip.focused {
                let is_provider = strip
                    .terminals
                    .iter()
                    .any(|chip| chip.resource_id == resource_id && chip.is_provider);
                return Some((resource_id, is_provider));
            }
        }
        self.latest_terminal()
            .map(|terminal| (terminal.resource_id, !terminal.is_plain_shell()))
    }

    /// The focused terminal's retained projection, if one has been admitted.
    pub fn latest_terminal(&self) -> Option<&TaskTerminalProjection> {
        self.focused_resource()
            .and_then(|resource_id| self.terminals.get(&resource_id))
    }

    /// The attachment slot the VISIBLE terminal uses.
    ///
    /// `None` is the provider slot: the legacy provider-only queries address
    /// it and it has no resource id of its own until a projection lands, which
    /// is the same split the shell spells as `TerminalTarget::Provider`.
    pub fn focused_surface_target(&self) -> TerminalSurfaceTarget {
        match self.focused_terminal() {
            Some((resource_id, false)) => Some(resource_id),
            _ => None,
        }
    }

    /// The retained screen for one exact terminal, provider slot included.
    fn screen_for_target(&self, target: TerminalSurfaceTarget) -> Option<&TaskTerminalProjection> {
        match target {
            Some(resource_id) => self.terminals.get(&resource_id),
            None => self
                .terminals
                .values()
                .find(|terminal| !terminal.is_plain_shell()),
        }
    }

    fn attachment(&self, target: TerminalSurfaceTarget) -> TerminalAttachment {
        self.attachments.get(&target).cloned().unwrap_or_default()
    }

    /// Attachment state of one exact terminal.
    pub fn terminal_attachment_for(
        &self,
        target: TerminalSurfaceTarget,
    ) -> TerminalAttachmentState {
        self.attachment(target).state
    }

    /// Attachment state of the visible terminal.
    pub fn terminal_attachment(&self) -> TerminalAttachmentState {
        self.terminal_attachment_for(self.focused_surface_target())
    }

    /// Seed one slot directly. Test-only: production state moves through the
    /// `note_*` transitions so the query lease and the label cannot diverge.
    #[cfg(test)]
    pub(crate) fn set_terminal_attachment_for_test(
        &mut self,
        target: TerminalSurfaceTarget,
        state: TerminalAttachmentState,
    ) {
        self.attachments.entry(target).or_default().state = state;
    }

    pub fn center_loading_state(
        &self,
        showing_terminal: bool,
    ) -> Option<CenterSurfaceLoadingState> {
        if showing_terminal {
            let target = self.focused_surface_target();
            let attachment = self.attachment(target);
            ((attachment.query_in_flight || attachment.state == TerminalAttachmentState::Starting)
                && self.screen_for_target(target).is_none())
            .then_some(CenterSurfaceLoadingState::TerminalInitial)
        } else {
            (self.conversation_in_flight && !self.conversation_has_content())
                .then_some(CenterSurfaceLoadingState::ConversationInitial)
        }
    }

    pub fn note_terminal_query_started_for(&mut self, target: TerminalSurfaceTarget) {
        let has_screen = self.screen_for_target(target).is_some();
        let attachment = self.attachments.entry(target).or_default();
        attachment.query_in_flight = true;
        // A new query makes the previous refusal history: a stale sentence read
        // as the reason for the CURRENT state would be worse than none.
        attachment.detail = None;
        if !has_screen {
            attachment.state = TerminalAttachmentState::Starting;
        }
    }

    /// Keep the host's sentence for the refusal that is about to settle this
    /// terminal's query, so the label can name the cause instead of repeating
    /// the same four words for every distinct failure.
    pub fn note_terminal_refusal_for(
        &mut self,
        target: TerminalSurfaceTarget,
        detail: Option<String>,
    ) {
        self.attachments.entry(target).or_default().detail = detail;
    }

    pub fn note_terminal_query_started(&mut self) {
        self.note_terminal_query_started_for(self.focused_surface_target());
    }

    pub fn note_terminal_reconnecting_for(&mut self, target: TerminalSurfaceTarget) {
        let has_screen = self.screen_for_target(target).is_some();
        let attachment = self.attachments.entry(target).or_default();
        attachment.query_in_flight = false;
        attachment.state = if has_screen {
            TerminalAttachmentState::StaleReconnecting
        } else {
            // Starting is an in-flight promise, not a generic retry label.
            // Once the exact query settles without a projection, stop the
            // spinner and surface a retryable unavailable state. A later
            // query can enter Starting again without losing cached output.
            TerminalAttachmentState::Unavailable
        };
    }

    pub fn note_terminal_reconnecting(&mut self) {
        self.note_terminal_reconnecting_for(self.focused_surface_target());
    }

    /// A provider-owned restore is still in progress after this exact query
    /// settled. Release the query lease so a bounded retry can be admitted,
    /// but keep the visible startup promise instead of flashing an incorrect
    /// unavailable state between polls.
    pub fn note_terminal_start_pending_for(&mut self, target: TerminalSurfaceTarget) {
        let has_screen = self.screen_for_target(target).is_some();
        let attachment = self.attachments.entry(target).or_default();
        attachment.query_in_flight = false;
        attachment.state = if has_screen {
            TerminalAttachmentState::StaleReconnecting
        } else {
            TerminalAttachmentState::Starting
        };
    }

    pub fn note_terminal_start_pending(&mut self) {
        self.note_terminal_start_pending_for(self.focused_surface_target());
    }

    pub fn note_terminal_unavailable(&mut self) {
        let target = self.focused_surface_target();
        let attachment = self.attachments.entry(target).or_default();
        attachment.query_in_flight = false;
        attachment.state = TerminalAttachmentState::Unavailable;
    }

    pub fn note_terminal_exited(&mut self) {
        let target = self.focused_surface_target();
        let attachment = self.attachments.entry(target).or_default();
        attachment.query_in_flight = false;
        attachment.state = TerminalAttachmentState::Exited;
    }

    /// Whether a screen query is outstanding for one exact terminal.
    pub fn terminal_query_in_flight_for(&self, target: TerminalSurfaceTarget) -> bool {
        self.attachment(target).query_in_flight
    }

    pub fn terminal_query_in_flight(&self) -> bool {
        self.terminal_query_in_flight_for(self.focused_surface_target())
    }

    pub fn conversation_has_content(&self) -> bool {
        self.conversation.fact_count() > 0 || !self.pending_user_messages.is_empty()
    }

    pub fn terminal_is_interactive(&self) -> bool {
        self.terminal_attachment() == TerminalAttachmentState::Live
            && self.latest_terminal().is_some()
    }

    pub fn terminal_label(&self) -> String {
        let attachment = self.attachment(self.focused_surface_target());
        let label = match attachment.state {
            TerminalAttachmentState::Live => "Terminal is live",
            TerminalAttachmentState::StaleReconnecting => "Reconnecting — last terminal screen",
            TerminalAttachmentState::Starting => "Terminal starting",
            TerminalAttachmentState::Unavailable => "Terminal unavailable",
            TerminalAttachmentState::Exited => "Terminal exited",
        };
        // Only the refused states earn the sentence: a live terminal's label is
        // not the place for the reason the previous attempt failed.
        match attachment.detail.as_deref() {
            Some(detail)
                if !detail.trim().is_empty()
                    && matches!(
                        attachment.state,
                        TerminalAttachmentState::Unavailable | TerminalAttachmentState::Exited
                    ) =>
            {
                format!("{label}: {detail}")
            }
            _ => label.to_string(),
        }
    }

    pub fn terminal_empty_message(&self) -> &'static str {
        match self.terminal_attachment() {
            TerminalAttachmentState::Live => "Terminal is live; waiting for output.",
            TerminalAttachmentState::StaleReconnecting => "Reconnecting — last terminal screen",
            TerminalAttachmentState::Starting => "Terminal starting…",
            TerminalAttachmentState::Unavailable => "Terminal unavailable",
            TerminalAttachmentState::Exited => "Terminal exited",
        }
    }

    pub fn terminal_tail(&self, max: usize) -> Vec<String> {
        let Some(terminal) = self.latest_terminal() else {
            return Vec::new();
        };
        if terminal.screen.lines.is_empty() {
            return terminal_tail_from_indexed_cells(&terminal.screen, max);
        }
        let start = terminal.screen.lines.len().saturating_sub(max);
        terminal.screen.lines[start..]
            .iter()
            .map(|line| {
                line.iter()
                    .filter(|cell| !cell.hidden)
                    .map(|cell| cell.character)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn presentation_page(&self) -> SemanticJournalPage {
        let mut page = self.conversation.as_page();
        let mut next_sequence = page.high_water.max(page.through_sequence).saturating_add(1);
        for pending in &self.pending_user_messages {
            page.facts.push(SemanticJournalFact {
                id: pending.event_id,
                sequence: next_sequence,
                occurred_at_ms: None,
                provider: "local".into(),
                schema_version: 1,
                kind: "user_message".into(),
                visibility: "conversation".into(),
                privacy_class: PrivacyClass::LocalOnly,
                redacted: false,
                payload: SemanticJournalPayload::UserMessage {
                    text: pending.text.clone(),
                },
            });
            next_sequence = next_sequence.saturating_add(1);
        }
        page
    }

    fn presentation_latest_snippet(&self) -> Option<&str> {
        self.pending_user_messages
            .last()
            .map(|pending| pending.text.as_str())
            .or_else(|| self.conversation.latest_snippet())
    }

    fn reconcile_pending_user_messages(&mut self, page: &SemanticJournalPage) {
        for fact in &page.facts {
            let SemanticJournalPayload::UserMessage { text } = &fact.payload else {
                continue;
            };
            if let Some(index) = self
                .pending_user_messages
                .iter()
                .position(|pending| pending.text == *text)
            {
                self.pending_user_messages.remove(index);
                continue;
            }

            // Claude can coalesce multiple user steers submitted during one
            // running turn into a single canonical hook fact separated by a
            // carriage return. Retire the exact ordered optimistic prefix as
            // one unit; otherwise the UI presents the canonical combined turn
            // plus every constituent optimistic bubble again.
            let mut remainder = text.as_str();
            let mut matched = 0usize;
            for pending in &self.pending_user_messages {
                let Some(next) = remainder.strip_prefix(&pending.text) else {
                    break;
                };
                matched += 1;
                remainder = next.trim_start_matches(['\r', '\n']);
                if remainder.is_empty() {
                    break;
                }
            }
            if matched > 1 && remainder.is_empty() {
                self.pending_user_messages.drain(..matched);
            }
        }
    }
}

fn terminal_tail_from_indexed_cells(
    screen: &crate::terminal::session::TerminalScreenSnapshot,
    max: usize,
) -> Vec<String> {
    let start = screen.rows.saturating_sub(max);
    let row_count = screen.rows.saturating_sub(start);
    let mut cells = vec![vec![None; screen.cols]; row_count];
    for indexed in &screen.cells {
        if indexed.row >= start && indexed.row < screen.rows && indexed.column < screen.cols {
            cells[indexed.row - start][indexed.column] = Some(&indexed.cell);
        }
    }
    cells
        .into_iter()
        .map(|row| {
            let mut line = String::with_capacity(screen.cols);
            for cell in row {
                match cell {
                    Some(cell) if !cell.hidden => {
                        line.push(cell.character);
                        line.extend(cell.zero_width.iter().copied());
                    }
                    _ => line.push(' '),
                }
            }
            line.trim_end().to_string()
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceAdmissionError {
    MissingSurface,
    StaleGeneration,
    WrongTask,
}

/// Exposes the canonical domain [`TaskId`] inside a surface-registry owner key.
///
/// Registry identity remains the full key `K`. Terminal admission compares only
/// this raw task against [`TaskTerminalProjection::task_id`]. Future
/// host-qualified keys (e.g. fleet `HostTaskKey`) implement this trait — they
/// must not implement `PartialEq<TaskId>`, which would collapse host scope.
pub trait SurfaceTaskKey {
    fn domain_task_id(&self) -> TaskId;
}

impl SurfaceTaskKey for TaskId {
    fn domain_task_id(&self) -> TaskId {
        *self
    }
}

impl SurfaceTaskKey for crate::client::HostTaskKey {
    fn domain_task_id(&self) -> TaskId {
        self.task_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSurfaceRegistry<K = TaskId> {
    surfaces: BTreeMap<K, TaskSurfaceState>,
    /// Monotonic cursor used to fair-rotate background conversation queries.
    background_schedule_epoch: u64,
}

impl<K> Default for TaskSurfaceRegistry<K> {
    fn default() -> Self {
        Self {
            surfaces: BTreeMap::new(),
            background_schedule_epoch: 0,
        }
    }
}

impl<K: Clone + Ord + Eq> TaskSurfaceRegistry<K> {
    pub fn ensure_task(&mut self, task_id: K) -> &mut TaskSurfaceState {
        self.surfaces.entry(task_id).or_default()
    }

    pub fn state(&self, task_id: K) -> Option<&TaskSurfaceState> {
        self.surfaces.get(&task_id)
    }

    pub fn retain_tasks(&mut self, task_ids: &[K]) {
        let valid: BTreeSet<_> = task_ids.iter().cloned().collect();
        self.surfaces.retain(|task_id, _| valid.contains(task_id));
    }

    pub fn remove_task(&mut self, task_id: K) {
        self.surfaces.remove(&task_id);
    }

    pub fn begin_conversation(&mut self, task_id: K, generation: u64) {
        let state = self.ensure_task(task_id);
        state.conversation_generation = generation;
        state.conversation_in_flight = true;
    }

    pub fn cancel_conversation(&mut self, task_id: K, generation: u64) {
        if let Some(state) = self.surfaces.get_mut(&task_id) {
            if state.conversation_generation == generation {
                state.conversation_in_flight = false;
            }
        }
    }

    pub fn conversation_in_flight(&self, task_id: K) -> bool {
        self.state(task_id)
            .is_some_and(|state| state.conversation_in_flight)
    }

    pub fn conversation_after_sequence(&self, task_id: K) -> u64 {
        self.state(task_id)
            .map(|state| state.conversation.request_after_sequence())
            .unwrap_or(0)
    }

    pub fn conversation_page(&self, task_id: K) -> Option<SemanticJournalPage> {
        self.state(task_id).map(|state| state.presentation_page())
    }

    /// The durable conversation facts, borrowed, with the marker a caller
    /// memoizes on: `(through_sequence, high_water, len)`. Three parts because
    /// no one of them is sufficient -- a cursor rollover can replace history
    /// without advancing `through_sequence`, and a page can advance the
    /// sequences without changing the retained length.
    ///
    /// Deliberately excludes the optimistic pending user messages
    /// [`TaskSurfaceState::presentation_page`] appends: they are all
    /// `UserMessage` payloads, which carry no plan step and no tool call, so a
    /// reader of plan progress or doing-now sees exactly the same answer
    /// without paying for the clone that materialises them.
    pub fn conversation_facts(
        &self,
        task_id: K,
    ) -> Option<(&[SemanticJournalFact], (u64, u64, usize))> {
        self.state(task_id).map(|state| {
            let facts = state.conversation.facts();
            (
                facts,
                (
                    state.conversation.through_sequence(),
                    state.conversation.high_water(),
                    facts.len(),
                ),
            )
        })
    }

    pub fn admit_conversation(
        &mut self,
        task_id: K,
        generation: u64,
        page: &SemanticJournalPage,
    ) -> Result<ConversationAdmission, SurfaceAdmissionError> {
        let state = self
            .surfaces
            .get_mut(&task_id)
            .ok_or(SurfaceAdmissionError::MissingSurface)?;
        if state.conversation_generation != generation || !state.conversation_in_flight {
            return Err(SurfaceAdmissionError::StaleGeneration);
        }
        let pending_before = state.pending_user_messages.len();
        let facts_changed = state.conversation.merge_page(page);
        state.reconcile_pending_user_messages(page);
        let pending_changed = state.pending_user_messages.len() != pending_before;
        let changed = facts_changed || pending_changed;
        if changed {
            state.latest_snippet = state.presentation_latest_snippet().map(ToOwned::to_owned);
        }
        state.conversation_in_flight = false;
        Ok(ConversationAdmission {
            page: changed.then(|| state.presentation_page()),
            changed,
        })
    }

    /// Locally admit a pending user message keyed by the real command id.
    /// Pending rows never advance durable high-water or request cursors.
    pub fn admit_pending_user_message(
        &mut self,
        task_id: K,
        text: &str,
        command_id: CommandId,
    ) -> ConversationAdmission {
        let state = self.ensure_task(task_id);
        if let Some(existing) = state
            .pending_user_messages
            .iter_mut()
            .find(|pending| pending.command_id == command_id)
        {
            existing.text = text.to_string();
        } else {
            state.pending_user_messages.push(PendingUserMessage {
                command_id,
                event_id: EventId::new(),
                text: text.to_string(),
            });
        }
        state.latest_snippet = state.presentation_latest_snippet().map(ToOwned::to_owned);
        ConversationAdmission {
            page: Some(state.presentation_page()),
            changed: true,
        }
    }

    pub fn reject_pending_user_message(
        &mut self,
        task_id: K,
        command_id: CommandId,
    ) -> ConversationAdmission {
        let Some(state) = self.surfaces.get_mut(&task_id) else {
            return ConversationAdmission {
                page: None,
                changed: false,
            };
        };
        let before = state.pending_user_messages.len();
        state
            .pending_user_messages
            .retain(|pending| pending.command_id != command_id);
        if state.pending_user_messages.len() != before {
            state.latest_snippet = state.presentation_latest_snippet().map(ToOwned::to_owned);
            ConversationAdmission {
                page: Some(state.presentation_page()),
                changed: true,
            }
        } else {
            ConversationAdmission {
                page: None,
                changed: false,
            }
        }
    }

    pub fn displayed_user_message_count(&self, task_id: K) -> usize {
        self.state(task_id)
            .map(|state| {
                state
                    .presentation_page()
                    .facts
                    .iter()
                    .filter(|fact| {
                        matches!(fact.payload, SemanticJournalPayload::UserMessage { .. })
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    /// Whether the retained conversation proves that the latest submitted
    /// user turn has received an assistant response. This supplements stock
    /// providers whose current CLI advertises conversation hooks but does not
    /// reliably publish a terminal `Stop` event. Pending optimistic user rows
    /// keep the task working until the corresponding assistant fact arrives.
    pub fn conversation_turn_completed(&self, task_id: K) -> bool {
        self.state(task_id).is_some_and(|state| {
            state.pending_user_messages.is_empty()
                && state.conversation.latest_message_is_assistant()
        })
    }

    /// Whether the visible conversation proves that a user turn is still in
    /// flight. This is an ephemeral UI fact: it must not be persisted into the
    /// durable task activity projection.
    pub fn conversation_turn_pending(&self, task_id: K) -> bool {
        self.state(task_id).is_some_and(|state| {
            !state.pending_user_messages.is_empty() || state.conversation.latest_message_is_user()
        })
    }

    pub fn admit_terminal(
        &mut self,
        task_id: K,
        projection: &TaskTerminalProjection,
    ) -> Result<(), SurfaceAdmissionError>
    where
        K: SurfaceTaskKey,
    {
        if task_id.domain_task_id() != projection.task_id {
            return Err(SurfaceAdmissionError::WrongTask);
        }
        let state = self.ensure_task(task_id);
        state
            .terminals
            .insert(projection.resource_id, projection.clone());
        // The screen that arrived is THIS terminal's, so only its slot goes
        // live. Marking the whole Task live would tell the UI a shell is
        // attached because the provider answered.
        let target = terminal_surface_target(projection);
        let attachment = state.attachments.entry(target).or_default();
        attachment.state = TerminalAttachmentState::Live;
        attachment.query_in_flight = false;
        Ok(())
    }

    /// Admit the Task's terminal strip.
    ///
    /// The strip is the host's authority on which terminals exist, so a
    /// retained screen for a resource the strip no longer lists is dropped
    /// here rather than left to age out. This never admits screen bytes: chips
    /// carry no output, and the focused terminal's screen still arrives on the
    /// ordinary terminal query.
    pub fn admit_terminals(
        &mut self,
        task_id: K,
        projection: &TaskTerminalsProjection,
    ) -> Result<(), SurfaceAdmissionError>
    where
        K: SurfaceTaskKey,
    {
        if task_id.domain_task_id() != projection.task_id {
            return Err(SurfaceAdmissionError::WrongTask);
        }
        let state = self.ensure_task(task_id);
        state.terminals.retain(|resource_id, _| {
            projection
                .terminals
                .iter()
                .any(|chip| chip.resource_id == *resource_id)
        });
        state.strip = Some(projection.clone());
        Ok(())
    }

    /// Point the ADMITTED strip at the chip the user just picked.
    ///
    /// This is an optimistic edit of the one strip copy, not a second copy:
    /// the host owns the durable focus and the next `TaskTerminals` answer
    /// overwrites this. It exists so the very next resize, scroll or screen
    /// query addresses the chip the user clicked instead of the previous one
    /// for the length of a round trip. A focus the strip does not list is
    /// refused rather than invented, and `None` is a legal value (it is what
    /// the host records when no plain shell is focused).
    pub fn note_focused_terminal(&mut self, task_id: K, focused: Option<ResourceId>) -> bool
    where
        K: SurfaceTaskKey,
    {
        let Some(state) = self.surfaces.get_mut(&task_id) else {
            return false;
        };
        let Some(strip) = state.strip.as_mut() else {
            return false;
        };
        if let Some(focused) = focused {
            if !strip
                .terminals
                .iter()
                .any(|chip| chip.resource_id == focused)
            {
                return false;
            }
        }
        strip.focused = focused;
        true
    }

    pub fn note_terminal_reconnecting(&mut self, task_id: K) {
        self.ensure_task(task_id).note_terminal_reconnecting();
    }

    /// Mark ONE terminal reconnecting. The per-Task form addresses whichever
    /// terminal is visible; this one is for the paths that already know which
    /// query settled, so a bounded retry for an unfocused terminal cannot
    /// relabel the one on screen.
    pub fn note_terminal_reconnecting_for(&mut self, task_id: K, target: TerminalSurfaceTarget) {
        self.ensure_task(task_id)
            .note_terminal_reconnecting_for(target);
    }

    /// See [`TaskSurfaceState::note_terminal_refusal_for`].
    pub fn note_terminal_refusal_for(
        &mut self,
        task_id: K,
        target: TerminalSurfaceTarget,
        detail: Option<String>,
    ) {
        self.ensure_task(task_id)
            .note_terminal_refusal_for(target, detail);
    }

    pub fn note_terminal_query_started(&mut self, task_id: K) {
        self.ensure_task(task_id).note_terminal_query_started();
    }

    /// Take the query lease for ONE terminal.
    pub fn note_terminal_query_started_for(&mut self, task_id: K, target: TerminalSurfaceTarget) {
        self.ensure_task(task_id)
            .note_terminal_query_started_for(target);
    }

    /// Release ONE terminal's lease while keeping its startup promise.
    pub fn note_terminal_start_pending_for(&mut self, task_id: K, target: TerminalSurfaceTarget) {
        self.ensure_task(task_id)
            .note_terminal_start_pending_for(target);
    }

    /// Whether a screen query is outstanding for ONE terminal.
    pub fn terminal_query_in_flight_for(&self, task_id: K, target: TerminalSurfaceTarget) -> bool {
        self.state(task_id)
            .is_some_and(|state| state.terminal_query_in_flight_for(target))
    }

    pub fn terminal_is_interactive(&self, task_id: K) -> bool {
        self.state(task_id)
            .is_some_and(TaskSurfaceState::terminal_is_interactive)
    }

    pub fn terminal_label(&self, task_id: K) -> String {
        self.state(task_id)
            .map(TaskSurfaceState::terminal_label)
            .unwrap_or_else(|| "Terminal unavailable".to_string())
    }

    pub fn terminal_empty_message(&self, task_id: K) -> &'static str {
        self.state(task_id)
            .map(TaskSurfaceState::terminal_empty_message)
            .unwrap_or("Terminal unavailable")
    }

    pub fn latest_snippet(&self, task_id: K) -> Option<&str> {
        self.state(task_id)
            .and_then(|state| state.latest_snippet.as_deref())
    }

    pub fn conversation_tail(&self, task_id: K, max: usize) -> Vec<String> {
        self.state(task_id)
            .map(|state| state.conversation.tail_snippets(max))
            .unwrap_or_default()
    }

    pub fn terminal_tail(&self, task_id: K, max: usize) -> Vec<String> {
        self.state(task_id)
            .map(|state| state.terminal_tail(max))
            .unwrap_or_default()
    }

    pub fn conversation_query_schedule(
        &mut self,
        workspace: &Workspace<K>,
        max_background: usize,
    ) -> Vec<ConversationQueryPlan<K>> {
        let focused = workspace.focused_task();
        let mut schedule = Vec::with_capacity(max_background.saturating_add(1));
        if let Some(task_id) = focused.clone() {
            if workspace.presentation(task_id.clone()) == Some(PanePresentation::Full)
                && !self.conversation_in_flight(task_id.clone())
            {
                schedule.push(ConversationQueryPlan {
                    task_id,
                    priority: ConversationQueryPriority::Interactive,
                });
            }
        }

        if max_background == 0 {
            return schedule;
        }

        // Background cadence covers every open pane that is not already on the
        // interactive Full path — including a focused Compact summary.
        let mut background: Vec<_> = workspace
            .task_ids()
            .into_iter()
            .filter(|task_id| {
                if self.conversation_in_flight(task_id.clone()) {
                    return false;
                }
                match (
                    focused.as_ref() == Some(task_id),
                    workspace.presentation(task_id.clone()),
                ) {
                    (true, Some(PanePresentation::Full)) => false,
                    (_, Some(_)) => true,
                    (_, None) => false,
                }
            })
            .filter_map(|task_id| {
                let pane = workspace.pane_for_task(task_id.clone())?;
                let last_scheduled = self
                    .state(task_id.clone())
                    .map(|state| state.last_conversation_scheduled_at)
                    .unwrap_or(0);
                Some((last_scheduled, pane.last_focused_at, task_id))
            })
            .collect();
        background.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
        let selected: Vec<_> = background
            .into_iter()
            .take(max_background)
            .map(|(_, _, task_id)| task_id)
            .collect();
        if selected.is_empty() {
            return schedule;
        }
        self.background_schedule_epoch = self.background_schedule_epoch.saturating_add(1);
        let epoch = self.background_schedule_epoch;
        for task_id in selected {
            self.ensure_task(task_id.clone())
                .last_conversation_scheduled_at = epoch;
            schedule.push(ConversationQueryPlan {
                task_id,
                priority: ConversationQueryPriority::Background,
            });
        }
        schedule
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::domain::{
        CommandId, EventId, PrivacyClass, SemanticJournalFact, SemanticJournalPage,
        SemanticJournalPayload, TaskId,
    };

    use super::*;

    fn page(sequence: u64, text: &str) -> SemanticJournalPage {
        SemanticJournalPage {
            oldest_sequence: 0,
            cursor_rolled_over: false,
            after_sequence: sequence.saturating_sub(1),
            through_sequence: sequence,
            high_water: sequence,
            encoded_bytes: 1,
            next_sequence: None,
            facts: vec![SemanticJournalFact {
                id: EventId::new(),
                sequence,
                occurred_at_ms: None,
                provider: "test".into(),
                schema_version: 1,
                kind: "assistant_text".into(),
                visibility: "conversation".into(),
                privacy_class: PrivacyClass::LocalOnly,
                redacted: false,
                payload: SemanticJournalPayload::AssistantText { text: text.into() },
            }],
        }
    }

    fn empty_page_after(sequence: u64) -> SemanticJournalPage {
        SemanticJournalPage {
            oldest_sequence: 0,
            cursor_rolled_over: false,
            after_sequence: sequence,
            through_sequence: sequence,
            high_water: sequence,
            encoded_bytes: 0,
            next_sequence: None,
            facts: Vec::new(),
        }
    }

    #[test]
    fn semantic_upsert_replaces_cached_partial_text_by_message_identity() {
        let mut cache = TaskConversationCache::default();
        let first = page(1, "Hel");
        assert!(cache.merge_page(&first));
        let mut updated = page(4, "Hello world");
        updated.facts[0].id = first.facts[0].id;
        assert!(
            cache.merge_page(&updated),
            "same-length replacement must repaint"
        );
        assert_eq!(cache.fact_count(), 1);
        assert_eq!(cache.high_water(), 4);
        assert_eq!(cache.as_page().facts[0], updated.facts[0]);
        assert!(
            !cache.merge_page(&first),
            "stale partial cannot replace final text"
        );
        assert_eq!(cache.as_page().facts[0], updated.facts[0]);
    }

    #[test]
    fn an_unavailable_terminal_names_the_host_sentence_when_one_arrived() {
        // Without a detail the label must read exactly as it always has: a
        // trailing colon with nothing after it is worse than the bare reason.
        let mut bare = TaskSurfaceState::default();
        bare.note_terminal_query_started();
        bare.note_terminal_reconnecting();
        assert_eq!(bare.terminal_label(), "Terminal unavailable");

        let mut named = TaskSurfaceState::default();
        named.note_terminal_query_started();
        named.note_terminal_refusal_for(
            named.focused_surface_target(),
            Some("TerminalUnavailable: Claude Code was updated".to_string()),
        );
        named.note_terminal_reconnecting();
        assert_eq!(
            named.terminal_label(),
            "Terminal unavailable: TerminalUnavailable: Claude Code was updated"
        );

        // A new query makes the previous refusal history: a stale sentence read
        // as the reason for the CURRENT state is a lie the label cannot tell.
        named.note_terminal_query_started();
        named.note_terminal_reconnecting();
        assert_eq!(named.terminal_label(), "Terminal unavailable");
    }

    #[test]
    fn restore_loss_keeps_last_terminal_screen_but_disables_input() {
        let mut surface = surface_with_terminal_lines(&["build started", "compiling"]);
        surface.note_terminal_reconnecting();

        assert_eq!(
            surface.terminal_tail(8),
            vec!["build started".to_string(), "compiling".to_string()]
        );
        assert!(!surface.terminal_is_interactive());
        assert_eq!(
            surface.terminal_label(),
            "Reconnecting — last terminal screen"
        );
    }

    #[test]
    fn first_terminal_query_projects_starting_without_discarding_a_live_screen() {
        let mut empty = TaskSurfaceState::default();
        empty.note_terminal_query_started();
        assert_eq!(
            empty.terminal_attachment(),
            TerminalAttachmentState::Starting
        );
        assert!(
            empty.terminal_query_in_flight(),
            "the empty terminal surface must expose its active load"
        );

        let mut live = surface_with_terminal_lines(&["ready"]);
        live.note_terminal_query_started();
        assert_eq!(live.terminal_attachment(), TerminalAttachmentState::Live);
        assert_eq!(live.terminal_tail(1), vec!["ready".to_string()]);
        assert!(
            live.terminal_query_in_flight(),
            "cached terminal content must stay visible while its refresh is active"
        );
        live.note_terminal_reconnecting();
        assert!(
            !live.terminal_query_in_flight(),
            "a failed refresh must stop the loading animation"
        );

        empty.note_terminal_reconnecting();
        assert_eq!(
            empty.terminal_attachment(),
            TerminalAttachmentState::Unavailable,
            "a failed first load must stop presenting an unbounded startup state; a later retry may re-enter Starting"
        );
        assert_eq!(
            empty.center_loading_state(true),
            None,
            "a failed first load must clear its loading animation"
        );
    }

    #[test]
    fn provider_start_pending_keeps_an_honest_loading_state_between_retries() {
        let mut empty = TaskSurfaceState::default();
        empty.note_terminal_query_started();
        empty.note_terminal_start_pending();

        assert_eq!(
            empty.terminal_attachment(),
            TerminalAttachmentState::Starting
        );
        assert!(
            !empty.terminal_query_in_flight(),
            "the settled query must release its in-flight lease before a retry"
        );
        assert_eq!(
            empty.center_loading_state(true),
            Some(CenterSurfaceLoadingState::TerminalInitial),
            "provider restoration is still real work and must not flash Terminal unavailable"
        );
    }

    #[test]
    fn conversation_loading_only_covers_the_uncached_first_load() {
        let task = TaskId::new();
        let mut registry = TaskSurfaceRegistry::default();
        registry.ensure_task(task);

        registry.begin_conversation(task, 1);
        let first = registry.state(task).expect("first-load surface");
        assert!(first.conversation_in_flight);
        assert_eq!(
            first.center_loading_state(false),
            Some(CenterSurfaceLoadingState::ConversationInitial)
        );
        assert!(
            !first.conversation_has_content(),
            "a first request must render a loading state instead of pretending the chat is empty"
        );

        registry
            .admit_conversation(task, 1, &page(1, "cached reply"))
            .expect("admit cached conversation");
        registry.begin_conversation(task, 2);
        let syncing = registry.state(task).expect("syncing surface");
        assert!(syncing.conversation_in_flight);
        assert_eq!(
            syncing.center_loading_state(false),
            None,
            "a background refresh must stay visually silent while cached conversation content is usable"
        );
        assert!(
            syncing.conversation_has_content(),
            "a refresh must preserve cached content without flashing a transient syncing indicator"
        );

        let mut terminal = TaskSurfaceState::default();
        terminal.note_terminal_query_started();
        assert_eq!(
            terminal.center_loading_state(true),
            Some(CenterSurfaceLoadingState::TerminalInitial)
        );
        terminal.terminals = surface_with_terminal_lines(&["cached"]).terminals;
        assert_eq!(
            terminal.center_loading_state(true),
            None,
            "a live terminal refresh must preserve cached output without flashing a syncing badge"
        );
    }

    #[test]
    fn compact_wire_terminal_reconstructs_text_tail_from_indexed_cells() {
        let mut surface = surface_with_terminal_lines(&["first", "second"]);
        let focused = surface.focused_resource().expect("terminal");
        surface
            .terminals
            .get_mut(&focused)
            .expect("terminal")
            .screen
            .lines
            .clear();

        assert_eq!(
            surface.terminal_tail(2),
            vec!["first".to_string(), "second".to_string()]
        );
    }

    /// One terminal projection for `task_id`, keyed by `resource_id`.
    ///
    /// `is_provider` decides which slot the projection stands for: the task's
    /// provider terminal carries a real agent session, a plain shell carries
    /// the documented `AgentSessionId::nil()` / zero-generation sentinels the
    /// host sends for a shell.
    fn terminal_projection_fixture(
        task_id: TaskId,
        resource_id: crate::domain::id::ResourceId,
        is_provider: bool,
    ) -> TaskTerminalProjection {
        use crate::domain::id::{AgentSessionId, TerminalId};
        use crate::terminal::protocol::TerminalSessionId;
        use crate::terminal::session::TerminalScreenSnapshot;

        TaskTerminalProjection {
            accepts_input_without_conversation_id: false,
            task_id,
            terminal_id: TerminalId::new(),
            session_id: TerminalSessionId::new(),
            agent_session_id: if is_provider {
                AgentSessionId::new()
            } else {
                AgentSessionId::nil()
            },
            resource_id,
            runtime_generation: if is_provider { 1 } else { 0 },
            resource_generation: 1,
            action_epoch: if is_provider { 1 } else { 0 },
            focus_epoch: crate::terminal::protocol::FocusEpoch::initial(),
            accepted_input_sequence: 0,
            sequence: 1,
            title: None,
            text_lines: Vec::new(),
            screen: TerminalScreenSnapshot::default(),
            is_provider,
            runtime_state: crate::domain::cockpit::TerminalRuntimeStateWire::Running,
        }
    }

    fn terminal_chip_fixture(
        projection: &TaskTerminalProjection,
    ) -> crate::domain::cockpit::TaskTerminalChip {
        crate::domain::cockpit::TaskTerminalChip {
            resource_id: projection.resource_id,
            is_provider: projection.is_provider,
            title: None,
            label: if projection.is_provider {
                "terminal".into()
            } else {
                "pwsh".into()
            },
            runtime_state: projection.runtime_state.clone(),
            live_cwd: None,
            exit: None,
            created_at_ms: 0,
            last_activity_at_ms: 0,
        }
    }

    /// Screens are per resource, so attachment has to be too. A bounded retry
    /// for the provider -- which the client keeps issuing whether or not the
    /// provider is on screen -- must not relabel the focused shell
    /// "Reconnecting" and drop `terminal_is_interactive`, which is what stops
    /// the user typing into a terminal that is working perfectly.
    #[test]
    fn a_provider_retry_leaves_the_focused_shells_attachment_alone() {
        let mut registry = TaskSurfaceRegistry::<TaskId>::default();
        let task_id = TaskId::new();
        let provider =
            terminal_projection_fixture(task_id, crate::domain::id::ResourceId::new(), true);
        let shell =
            terminal_projection_fixture(task_id, crate::domain::id::ResourceId::new(), false);
        let strip = TaskTerminalsProjection {
            task_id,
            terminals: vec![
                terminal_chip_fixture(&provider),
                terminal_chip_fixture(&shell),
            ],
            order: vec![shell.resource_id],
            focused: Some(shell.resource_id),
        };
        registry.admit_terminals(task_id, &strip).unwrap();
        registry.admit_terminal(task_id, &provider).unwrap();
        registry.admit_terminal(task_id, &shell).unwrap();
        assert!(registry.terminal_is_interactive(task_id));
        assert_eq!(registry.terminal_label(task_id), "Terminal is live");

        // The provider's own retry settles without a projection.
        registry.note_terminal_reconnecting_for(task_id, None);
        assert!(
            registry.terminal_is_interactive(task_id),
            "the focused shell is still attached; the provider's retry is not about it"
        );
        assert_eq!(registry.terminal_label(task_id), "Terminal is live");
        assert_eq!(
            registry
                .state(task_id)
                .unwrap()
                .terminal_attachment_for(None),
            TerminalAttachmentState::StaleReconnecting,
            "the provider slot alone records the retry"
        );

        // The shell's own retry does reach it.
        registry.note_terminal_reconnecting_for(task_id, Some(shell.resource_id));
        assert_eq!(
            registry.terminal_label(task_id),
            "Reconnecting — last terminal screen"
        );
        assert!(!registry.terminal_is_interactive(task_id));
    }

    /// Admitting one terminal's screen marks THAT terminal live. Marking the
    /// Task live would tell the UI a shell is attached because the provider
    /// answered, which is the same conflation one step earlier.
    #[test]
    fn admitting_a_provider_screen_does_not_mark_the_focused_shell_live() {
        let mut registry = TaskSurfaceRegistry::<TaskId>::default();
        let task_id = TaskId::new();
        let provider =
            terminal_projection_fixture(task_id, crate::domain::id::ResourceId::new(), true);
        let shell =
            terminal_projection_fixture(task_id, crate::domain::id::ResourceId::new(), false);
        let strip = TaskTerminalsProjection {
            task_id,
            terminals: vec![
                terminal_chip_fixture(&provider),
                terminal_chip_fixture(&shell),
            ],
            order: vec![shell.resource_id],
            focused: Some(shell.resource_id),
        };
        registry.admit_terminals(task_id, &strip).unwrap();
        registry.admit_terminal(task_id, &provider).unwrap();
        let state = registry.state(task_id).unwrap();
        assert_eq!(
            state.terminal_attachment_for(None),
            TerminalAttachmentState::Live
        );
        assert_eq!(
            state.terminal_attachment_for(Some(shell.resource_id)),
            TerminalAttachmentState::Unavailable,
            "the shell has answered nothing yet, so it is not attached"
        );
        assert!(
            !registry.terminal_is_interactive(task_id),
            "the focused shell must not read as attached because the provider answered"
        );
    }

    #[test]
    fn registry_holds_one_projection_per_terminal_resource() {
        let mut registry = TaskSurfaceRegistry::<TaskId>::default();
        let task_id = TaskId::new();
        let provider =
            terminal_projection_fixture(task_id, crate::domain::id::ResourceId::new(), true);
        let shell =
            terminal_projection_fixture(task_id, crate::domain::id::ResourceId::new(), false);
        registry.admit_terminal(task_id, &provider).unwrap();
        registry.admit_terminal(task_id, &shell).unwrap();
        let state = registry.state(task_id).unwrap();
        assert_eq!(state.terminals.len(), 2);
        assert!(state.terminals.contains_key(&provider.resource_id));
        assert!(state.terminals.contains_key(&shell.resource_id));
        // With no strip admitted the provider is the default focus, whatever
        // order the two resource ids happen to sort in.
        assert_eq!(state.focused_resource(), Some(provider.resource_id));
        assert_eq!(
            state.latest_terminal().map(|terminal| terminal.resource_id),
            Some(provider.resource_id)
        );
    }

    #[test]
    fn admitted_strip_focus_selects_the_visible_terminal_and_retires_dropped_resources() {
        let mut registry = TaskSurfaceRegistry::<TaskId>::default();
        let task_id = TaskId::new();
        let provider =
            terminal_projection_fixture(task_id, crate::domain::id::ResourceId::new(), true);
        let shell =
            terminal_projection_fixture(task_id, crate::domain::id::ResourceId::new(), false);
        let gone =
            terminal_projection_fixture(task_id, crate::domain::id::ResourceId::new(), false);
        registry.admit_terminal(task_id, &provider).unwrap();
        registry.admit_terminal(task_id, &shell).unwrap();
        registry.admit_terminal(task_id, &gone).unwrap();

        let strip = TaskTerminalsProjection {
            task_id,
            terminals: vec![
                terminal_chip_fixture(&provider),
                terminal_chip_fixture(&shell),
            ],
            order: vec![shell.resource_id],
            focused: Some(shell.resource_id),
        };
        registry.admit_terminals(task_id, &strip).unwrap();

        let state = registry.state(task_id).unwrap();
        assert_eq!(
            state.terminals.len(),
            2,
            "a resource the strip no longer lists must not keep a cached screen"
        );
        assert!(!state.terminals.contains_key(&gone.resource_id));
        assert_eq!(state.focused_resource(), Some(shell.resource_id));
        assert_eq!(
            state.latest_terminal().map(|terminal| terminal.resource_id),
            Some(shell.resource_id)
        );
        assert_eq!(
            state.strip.as_ref().map(|strip| strip.task_id),
            Some(task_id)
        );
    }

    #[test]
    fn focus_on_a_terminal_with_no_screen_yet_shows_nothing_rather_than_the_previous_one() {
        let mut registry = TaskSurfaceRegistry::<TaskId>::default();
        let task_id = TaskId::new();
        let provider =
            terminal_projection_fixture(task_id, crate::domain::id::ResourceId::new(), true);
        let shell =
            terminal_projection_fixture(task_id, crate::domain::id::ResourceId::new(), false);
        registry.admit_terminal(task_id, &provider).unwrap();

        let strip = TaskTerminalsProjection {
            task_id,
            terminals: vec![
                terminal_chip_fixture(&provider),
                terminal_chip_fixture(&shell),
            ],
            order: vec![shell.resource_id],
            focused: Some(shell.resource_id),
        };
        registry.admit_terminals(task_id, &strip).unwrap();

        let state = registry.state(task_id).unwrap();
        assert_eq!(state.focused_resource(), Some(shell.resource_id));
        assert!(
            state.latest_terminal().is_none(),
            "the newly focused terminal has no screen yet; the provider's must not stand in for it"
        );
    }

    #[test]
    fn strip_admission_rejects_a_foreign_task() {
        let mut registry = TaskSurfaceRegistry::<TaskId>::default();
        let task_id = TaskId::new();
        let other = TaskId::new();
        let strip = TaskTerminalsProjection {
            task_id: other,
            terminals: Vec::new(),
            order: Vec::new(),
            focused: None,
        };
        assert_eq!(
            registry.admit_terminals(task_id, &strip),
            Err(SurfaceAdmissionError::WrongTask)
        );
        assert!(registry.state(task_id).is_none());
    }

    fn surface_with_terminal_lines(lines: &[&str]) -> TaskSurfaceState {
        use crate::domain::id::{AgentSessionId, ResourceId, TerminalId};
        use crate::domain::TaskTerminalProjection;
        use crate::terminal::protocol::TerminalSessionId;
        use crate::terminal::session::{
            TerminalCellSnapshot, TerminalIndexedCellSnapshot, TerminalScreenSnapshot,
        };

        let task_id = TaskId::new();
        let screen_lines: Vec<Vec<TerminalCellSnapshot>> = lines
            .iter()
            .map(|line| {
                line.chars()
                    .map(|character| TerminalCellSnapshot {
                        character,
                        zero_width: Vec::new(),
                        foreground: 0,
                        background: 0,
                        bold: false,
                        dim: false,
                        italic: false,
                        underline: false,
                        undercurl: false,
                        strike: false,
                        hidden: false,
                        has_hyperlink: false,
                        default_background: true,
                        default_foreground: true,
                    })
                    .collect()
            })
            .collect();
        let cols = screen_lines
            .iter()
            .map(|line| line.len())
            .max()
            .unwrap_or(0);
        let indexed_cells = screen_lines
            .iter()
            .enumerate()
            .flat_map(|(row, cells)| {
                cells
                    .iter()
                    .cloned()
                    .enumerate()
                    .map(move |(column, cell)| TerminalIndexedCellSnapshot { row, column, cell })
            })
            .collect();
        let mut surface = TaskSurfaceState::default();
        let projection = TaskTerminalProjection {
            accepts_input_without_conversation_id: false,
            task_id,
            terminal_id: TerminalId::new(),
            session_id: TerminalSessionId::new(),
            agent_session_id: AgentSessionId::new(),
            resource_id: ResourceId::new(),
            runtime_generation: 1,
            resource_generation: 1,
            action_epoch: 1,
            focus_epoch: crate::terminal::protocol::FocusEpoch::initial(),
            accepted_input_sequence: 0,
            sequence: 1,
            title: None,
            text_lines: Vec::new(),
            screen: TerminalScreenSnapshot {
                cells: indexed_cells,
                lines: screen_lines,
                cols,
                rows: lines.len(),
                ..Default::default()
            },
            is_provider: true,
            runtime_state: crate::domain::cockpit::TerminalRuntimeStateWire::Running,
        };
        let target = terminal_surface_target(&projection);
        surface.terminals.insert(projection.resource_id, projection);
        surface.set_terminal_attachment_for_test(target, TerminalAttachmentState::Live);
        surface
    }

    #[test]
    fn unchanged_conversation_page_reports_no_change_and_keeps_the_same_high_water() {
        let task = TaskId::new();
        let mut registry = TaskSurfaceRegistry::default();
        registry.ensure_task(task);
        registry.begin_conversation(task, 1);
        let first = registry
            .admit_conversation(task, 1, &page(1, "hello"))
            .unwrap();
        registry.begin_conversation(task, 2);
        let repeat = registry
            .admit_conversation(task, 2, &empty_page_after(1))
            .unwrap();

        assert!(first.changed);
        assert!(first.page.is_some());
        assert!(!repeat.changed);
        assert!(repeat.page.is_none());
        assert_eq!(registry.conversation_after_sequence(task), 1);
        assert_eq!(
            registry
                .state(task)
                .map(|state| state.conversation.high_water()),
            Some(1)
        );
    }

    #[test]
    fn rolled_over_history_replaces_cached_window_and_resets_cursor() {
        let mut cache = TaskConversationCache::default();
        cache.merge_page(&page(80, "old generation"));
        let mut reset = page(1, "current generation");
        reset.oldest_sequence = 1;
        reset.cursor_rolled_over = true;
        assert!(cache.merge_page(&reset));
        assert_eq!(cache.fact_count(), 1);
        assert_eq!(cache.high_water(), 1);
        assert_eq!(cache.request_after_sequence(), 1);
        assert!(matches!(&cache.as_page().facts[0].payload,
            SemanticJournalPayload::AssistantText { text } if text == "current generation"));

        let mut empty = empty_page_after(0);
        empty.cursor_rolled_over = true;
        assert!(cache.merge_page(&empty));
        assert_eq!(cache.fact_count(), 0);
        assert_eq!(cache.request_after_sequence(), 0);
        assert!(!cache.merge_page(&empty));
    }

    #[test]
    fn cursor_only_high_water_advance_does_not_return_a_visual_page() {
        let mut cache = TaskConversationCache::default();
        assert!(cache.merge_page(&page(1, "hello")));
        let prior_facts = cache.fact_count();
        assert!(!cache.merge_page(&SemanticJournalPage {
            oldest_sequence: 0,
            cursor_rolled_over: false,
            after_sequence: 1,
            through_sequence: 1,
            high_water: 4,
            encoded_bytes: 0,
            next_sequence: None,
            facts: Vec::new(),
        }));
        assert_eq!(cache.fact_count(), prior_facts);
        assert_eq!(cache.high_water(), 4);
        assert_eq!(cache.request_after_sequence(), 4);
    }

    #[test]
    fn pending_user_message_does_not_advance_durable_query_cursor() {
        let task = TaskId::new();
        let command_id = CommandId::new();
        let mut registry = TaskSurfaceRegistry::default();
        registry.ensure_task(task);
        registry.begin_conversation(task, 1);
        registry
            .admit_conversation(task, 1, &page(1, "prior"))
            .unwrap();

        let pending = registry.admit_pending_user_message(task, "hello there", command_id);
        assert!(pending.changed);
        assert_eq!(registry.conversation_after_sequence(task), 1);
        assert_eq!(
            registry
                .state(task)
                .map(|state| state.conversation.high_water()),
            Some(1)
        );
        assert_eq!(registry.displayed_user_message_count(task), 1);
        assert!(
            registry
                .conversation_page(task)
                .is_some_and(|page| page.facts.iter().any(|fact| {
                    matches!(
                        &fact.payload,
                        SemanticJournalPayload::UserMessage { text } if text == "hello there"
                    )
                })),
            "pending overlay must present the local user row"
        );

        registry.begin_conversation(task, 2);
        let durable = user_page(2, "hello there");
        let admitted = registry.admit_conversation(task, 2, &durable).unwrap();
        assert!(admitted.changed);
        assert_eq!(registry.displayed_user_message_count(task), 1);
        assert_eq!(registry.conversation_after_sequence(task), 2);
    }

    #[test]
    fn coalesced_provider_user_fact_retires_each_ordered_optimistic_message() {
        let task = TaskId::new();
        let mut registry = TaskSurfaceRegistry::default();
        registry.admit_pending_user_message(task, "first steer", CommandId::new());
        registry.admit_pending_user_message(task, "second steer", CommandId::new());
        assert_eq!(registry.displayed_user_message_count(task), 2);

        registry.begin_conversation(task, 1);
        registry
            .admit_conversation(task, 1, &user_page(1, "first steer\rsecond steer"))
            .expect("admit coalesced durable user message");

        assert_eq!(
            registry.displayed_user_message_count(task),
            1,
            "the provider's canonical coalesced fact must replace both optimistic rows"
        );
    }

    #[test]
    fn pending_user_message_keeps_stable_presentation_identity() {
        let task = TaskId::new();
        let mut registry = TaskSurfaceRegistry::default();
        registry.admit_pending_user_message(task, "stable row", CommandId::new());

        let first = registry
            .conversation_page(task)
            .and_then(|page| page.facts.last().map(|fact| fact.id))
            .expect("first pending row");
        let second = registry
            .conversation_page(task)
            .and_then(|page| page.facts.last().map(|fact| fact.id))
            .expect("second pending row");

        assert_eq!(
            first, second,
            "reprojection must not replace the pending row"
        );
    }

    #[test]
    fn assistant_fact_completes_the_latest_conversation_turn() {
        let task = TaskId::new();
        let mut registry = TaskSurfaceRegistry::default();
        registry.admit_pending_user_message(task, "hello", CommandId::new());
        assert!(registry.conversation_turn_pending(task));
        assert!(!registry.conversation_turn_completed(task));

        registry.begin_conversation(task, 1);
        registry
            .admit_conversation(task, 1, &user_page(1, "hello"))
            .expect("admit durable user message");
        assert!(registry.conversation_turn_pending(task));
        assert!(!registry.conversation_turn_completed(task));

        registry.begin_conversation(task, 2);
        registry
            .admit_conversation(task, 2, &page(2, "done"))
            .expect("admit assistant response");
        assert!(!registry.conversation_turn_pending(task));
        assert!(registry.conversation_turn_completed(task));
    }

    fn user_page(sequence: u64, text: &str) -> SemanticJournalPage {
        SemanticJournalPage {
            oldest_sequence: 0,
            cursor_rolled_over: false,
            after_sequence: sequence.saturating_sub(1),
            through_sequence: sequence,
            high_water: sequence,
            encoded_bytes: 1,
            next_sequence: None,
            facts: vec![SemanticJournalFact {
                id: EventId::new(),
                sequence,
                occurred_at_ms: None,
                provider: "test".into(),
                schema_version: 1,
                kind: "user_message".into(),
                visibility: "conversation".into(),
                privacy_class: PrivacyClass::LocalOnly,
                redacted: false,
                payload: SemanticJournalPayload::UserMessage { text: text.into() },
            }],
        }
    }

    #[test]
    fn conversation_refresh_is_push_primary_with_slow_recovery_heartbeat_only() {
        // Sub-second elapsed must not schedule queries — ConversationDirty owns
        // ordinary refresh. The old 8/30 × 16ms cadences (~128ms / ~480ms) are
        // exactly the idle CPU regression this rejects.
        assert!(
            conversation_poll_priorities_due(Duration::from_millis(128)).is_empty(),
            "must not poll conversations on the old ~128ms interactive cadence"
        );
        assert!(
            conversation_poll_priorities_due(Duration::from_millis(480)).is_empty(),
            "must not poll conversations on the old ~480ms background cadence"
        );
        assert!(
            conversation_poll_priorities_due(Duration::from_millis(29_999)).is_empty(),
            "recovery heartbeat must stay empty before the full interval"
        );
        assert_eq!(
            conversation_poll_priorities_due(CONVERSATION_RECOVERY_HEARTBEAT),
            vec![
                ConversationQueryPriority::Interactive,
                ConversationQueryPriority::Background
            ]
        );
        assert!(CONVERSATION_RECOVERY_HEARTBEAT >= Duration::from_secs(30));
    }

    #[test]
    fn working_conversations_use_a_bounded_one_second_completion_poll() {
        assert!(!working_conversation_poll_due(
            true,
            Duration::from_millis(999)
        ));
        assert!(working_conversation_poll_due(true, Duration::from_secs(1)));
        assert!(!working_conversation_poll_due(
            false,
            Duration::from_secs(30)
        ));
    }

    #[test]
    fn shift_click_toggles_membership_while_plain_click_focuses_an_open_task() {
        let first = TaskId::new();
        let second = TaskId::new();
        let third = TaskId::new();
        let mut workspace = None;

        apply_workspace_selection(&mut workspace, first, WorkspaceSelectionGesture::Plain)
            .expect("select first");
        apply_workspace_selection(&mut workspace, second, WorkspaceSelectionGesture::Toggle)
            .expect("add second");
        apply_workspace_selection(&mut workspace, third, WorkspaceSelectionGesture::Toggle)
            .expect("add third");
        apply_workspace_selection(&mut workspace, first, WorkspaceSelectionGesture::Plain)
            .expect("focus first");

        let workspace = workspace.expect("workspace");
        assert_eq!(workspace.pane_count(), 3);
        assert_eq!(workspace.focused_task(), Some(first));
    }

    #[test]
    fn plain_click_outside_workspace_opens_in_focused_slot() {
        let first = TaskId::new();
        let second = TaskId::new();
        let next = TaskId::new();
        let mut workspace = Some(TaskWorkspace::single(first));
        apply_workspace_selection(&mut workspace, second, WorkspaceSelectionGesture::Toggle)
            .unwrap();
        let slot = workspace.as_ref().unwrap().focused_pane_id();
        apply_workspace_selection(&mut workspace, next, WorkspaceSelectionGesture::Plain).unwrap();
        let workspace = workspace.unwrap();
        assert_eq!(workspace.pane_count(), 2);
        assert_eq!(workspace.focused_pane_id(), slot);
        assert_eq!(workspace.focused_task(), Some(next));
        assert!(workspace.contains_task(first));
        assert!(!workspace.contains_task(second));
    }

    #[test]
    fn plain_click_replaces_focused_slot_while_preserving_compact_geometry() {
        let first = TaskId::new();
        let second = TaskId::new();
        let third = TaskId::new();
        let mut workspace = Some(TaskWorkspace::single(first));
        apply_workspace_selection(&mut workspace, second, WorkspaceSelectionGesture::Toggle)
            .expect("open second");
        let focused = workspace.as_ref().unwrap().focused_pane_id().unwrap();
        workspace
            .as_mut()
            .unwrap()
            .set_manual_compact(second, true)
            .unwrap();
        let split_id = match workspace.as_ref().unwrap().root().unwrap() {
            crate::ui::task_workspace::WorkspaceNode::Split { id, .. } => *id,
            _ => panic!("expected split after toggle"),
        };
        workspace
            .as_mut()
            .unwrap()
            .resize_split_child(split_id, 0, 420.0)
            .unwrap();
        workspace
            .as_mut()
            .unwrap()
            .pin_task_axis_size(second, 260.0)
            .unwrap();

        apply_workspace_selection(&mut workspace, third, WorkspaceSelectionGesture::Plain)
            .expect("replace focused with third");
        let workspace = workspace.expect("workspace");
        assert_eq!(workspace.pane_count(), 2);
        assert_eq!(workspace.focused_pane_id(), Some(focused));
        assert_eq!(workspace.focused_task(), Some(third));
        assert!(workspace.contains_task(first));
        assert!(!workspace.contains_task(second));
        assert_eq!(
            workspace.presentation(third),
            Some(PanePresentation::CompactManual)
        );
        assert_eq!(
            workspace.split_child_allocation(split_id, 1),
            Some(crate::ui::task_workspace::Allocation::Pinned { logical_px: 260.0 })
        );
    }

    #[test]
    fn late_conversation_result_is_admitted_only_to_its_exact_task_surface() {
        let first = TaskId::new();
        let second = TaskId::new();
        let mut registry = TaskSurfaceRegistry::default();
        registry.begin_conversation(first, 7);
        registry.begin_conversation(second, 9);

        registry
            .admit_conversation(first, 7, &page(1, "first"))
            .expect("admit first");
        assert_eq!(registry.latest_snippet(first), Some("first"));
        assert_eq!(registry.latest_snippet(second), None);
        assert_eq!(
            registry.admit_conversation(first, 6, &page(2, "stale")),
            Err(SurfaceAdmissionError::StaleGeneration)
        );
    }

    #[test]
    fn workspace_query_scheduler_prioritizes_focused_full_and_includes_compact() {
        let first = TaskId::new();
        let second = TaskId::new();
        let third = TaskId::new();
        let fourth = TaskId::new();
        let mut workspace = TaskWorkspace::single(first);
        for task_id in [second, third, fourth] {
            workspace
                .insert_after_focused(task_id, Axis::Horizontal)
                .unwrap();
        }
        workspace.focus_task(second).unwrap();
        workspace.set_manual_compact(third, true).unwrap();
        let mut registry = TaskSurfaceRegistry::default();
        registry.begin_conversation(fourth, 4);

        let schedule = registry.conversation_query_schedule(&workspace, 2);

        assert_eq!(
            schedule.first(),
            Some(&ConversationQueryPlan {
                task_id: second,
                priority: ConversationQueryPriority::Interactive,
            })
        );
        assert!(schedule.iter().any(|plan| plan.task_id == first));
        assert!(
            schedule.iter().any(|plan| plan.task_id == third),
            "compact panes still receive bounded summary refresh"
        );
        assert!(schedule.iter().all(|plan| plan.task_id != fourth));
    }

    #[test]
    fn workspace_query_scheduler_fairly_rotates_all_open_background_panes() {
        let focused = TaskId::new();
        let older = TaskId::new();
        let mid = TaskId::new();
        let newer = TaskId::new();
        let compact = TaskId::new();
        let mut workspace = TaskWorkspace::single(older);
        for task_id in [mid, newer, compact, focused] {
            workspace
                .insert_after_focused(task_id, Axis::Horizontal)
                .unwrap();
        }
        workspace.focus_task(focused).unwrap();
        workspace.set_manual_compact(compact, true).unwrap();
        let mut registry = TaskSurfaceRegistry::default();

        let first_wave = registry.conversation_query_schedule(&workspace, 2);
        let background: Vec<_> = first_wave
            .iter()
            .filter(|plan| plan.priority == ConversationQueryPriority::Background)
            .map(|plan| plan.task_id)
            .collect();
        assert_eq!(background.len(), 2);
        assert!(!background.contains(&focused));

        let second_wave = registry.conversation_query_schedule(&workspace, 2);
        let second_background: Vec<_> = second_wave
            .iter()
            .filter(|plan| plan.priority == ConversationQueryPriority::Background)
            .map(|plan| plan.task_id)
            .collect();
        assert_eq!(second_background.len(), 2);

        let seen: BTreeSet<_> = background.into_iter().chain(second_background).collect();
        assert!(
            seen.contains(&older)
                && seen.contains(&mid)
                && seen.contains(&newer)
                && seen.contains(&compact),
            "two bounded waves must cover every open background pane including compact: {seen:?}"
        );
    }

    #[test]
    fn interactive_only_schedule_does_not_advance_background_watermarks() {
        let focused = TaskId::new();
        let background = TaskId::new();
        let mut workspace = TaskWorkspace::single(background);
        workspace
            .insert_after_focused(focused, Axis::Horizontal)
            .unwrap();
        workspace.focus_task(focused).unwrap();
        let mut registry = TaskSurfaceRegistry::default();

        let before = registry
            .state(background)
            .map(|state| state.last_conversation_scheduled_at)
            .unwrap_or(0);
        let schedule = registry.conversation_query_schedule(&workspace, 0);
        assert!(schedule.iter().all(|plan| {
            plan.priority == ConversationQueryPriority::Interactive && plan.task_id == focused
        }));
        assert_eq!(
            registry
                .state(background)
                .map(|state| state.last_conversation_scheduled_at)
                .unwrap_or(0),
            before,
            "interactive-only pump must not rotate background watermarks"
        );
    }

    #[test]
    fn focused_compact_pane_is_scheduled_on_background_cadence() {
        let task = TaskId::new();
        let mut workspace = TaskWorkspace::single(task);
        workspace.set_manual_compact(task, true).unwrap();
        let mut registry = TaskSurfaceRegistry::default();

        let interactive_only = registry.conversation_query_schedule(&workspace, 0);
        assert!(
            interactive_only.is_empty(),
            "focused compact is not interactive Full"
        );

        let with_background = registry.conversation_query_schedule(&workspace, 2);
        assert_eq!(
            with_background,
            vec![ConversationQueryPlan {
                task_id: task,
                priority: ConversationQueryPriority::Background,
            }]
        );
    }

    #[test]
    fn pump_cadence_with_five_tasks_covers_all_background_without_interactive_starvation() {
        let focused = TaskId::new();
        let mut others = Vec::new();
        let mut workspace = TaskWorkspace::single(focused);
        for _ in 0..5 {
            let task = TaskId::new();
            workspace
                .insert_after_focused(task, Axis::Horizontal)
                .unwrap();
            others.push(task);
        }
        workspace.focus_task(focused).unwrap();
        workspace.set_manual_compact(others[0], true).unwrap();
        let mut registry = TaskSurfaceRegistry::default();

        let mut seen = BTreeSet::new();
        // Recovery heartbeats (not high-frequency ticks) must still rotate
        // background panes, including compact summaries.
        for _ in 0..8 {
            let due = conversation_poll_priorities_due(CONVERSATION_RECOVERY_HEARTBEAT);
            assert!(
                due.contains(&ConversationQueryPriority::Background),
                "recovery heartbeat must admit background conversation refresh"
            );
            let schedule = registry.conversation_query_schedule(&workspace, 2);
            for plan in schedule {
                if !due.contains(&plan.priority) {
                    continue;
                }
                if plan.priority == ConversationQueryPriority::Background {
                    seen.insert(plan.task_id);
                } else {
                    assert_eq!(plan.task_id, focused);
                }
            }
        }
        for task in &others {
            assert!(
                seen.contains(task),
                "recovery heartbeat scheduling must reach every non-interactive pane including compact"
            );
        }
        assert!(
            conversation_poll_priorities_due(Duration::from_millis(128)).is_empty(),
            "background coverage must not rely on high-frequency 8/30 tick polling"
        );
    }

    /// Test-only host-scoped owner key. Carries a canonical [`TaskId`] without
    /// implementing `PartialEq<TaskId>` (host identity must stay distinct).
    #[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
    struct TestHostKey {
        host: String,
        task: TaskId,
    }

    impl SurfaceTaskKey for TestHostKey {
        fn domain_task_id(&self) -> TaskId {
            self.task
        }
    }

    fn host_key(host: &str, task: TaskId) -> TestHostKey {
        TestHostKey {
            host: host.to_string(),
            task,
        }
    }

    #[test]
    fn production_fleet_keys_keep_conversation_surfaces_distinct() {
        use crate::client::{HostId, HostTaskKey};
        let task = TaskId::new();
        let local = HostTaskKey::new(HostId::LocalProfile("dev".into()), task);
        let remote = HostTaskKey::new(HostId::Remote([1; 16]), task);
        assert_eq!(local.domain_task_id(), remote.domain_task_id());
        let mut registry = TaskSurfaceRegistry::<HostTaskKey>::default();
        registry.begin_conversation(local.clone(), 1);
        registry.begin_conversation(remote.clone(), 1);
        registry
            .admit_conversation(local.clone(), 1, &page(3, "local"))
            .unwrap();
        registry
            .admit_conversation(remote.clone(), 1, &page(5, "remote"))
            .unwrap();
        assert_eq!(registry.latest_snippet(local), Some("local"));
        assert_eq!(registry.latest_snippet(remote), Some("remote"));
    }

    #[test]
    fn host_qualified_surface_caches_stay_isolated_for_shared_raw_task_id() {
        let shared = TaskId::new();
        let local = host_key("local", shared);
        let remote = host_key("remote", shared);
        let mut registry = TaskSurfaceRegistry::<TestHostKey>::default();

        registry.begin_conversation(local.clone(), 1);
        let local_admit = registry
            .admit_conversation(local.clone(), 1, &page(3, "local-only"))
            .expect("local admit");
        assert!(local_admit.changed);

        registry.begin_conversation(remote.clone(), 2);
        let remote_admit = registry
            .admit_conversation(remote.clone(), 2, &page(5, "remote-only"))
            .expect("remote admit");
        assert!(remote_admit.changed);

        assert_eq!(registry.latest_snippet(local.clone()), Some("local-only"));
        assert_eq!(registry.latest_snippet(remote.clone()), Some("remote-only"));
        assert_eq!(registry.conversation_after_sequence(local.clone()), 3);
        assert_eq!(registry.conversation_after_sequence(remote.clone()), 5);

        registry.retain_tasks(&[local.clone()]);
        assert!(registry.state(local).is_some());
        assert!(registry.state(remote).is_none());
    }

    #[test]
    fn host_qualified_selection_and_query_schedule_keep_key_owners() {
        let shared = TaskId::new();
        let local = host_key("desk", shared);
        let remote = host_key("cloud", shared);
        let mut workspace = None;
        apply_workspace_selection(
            &mut workspace,
            local.clone(),
            WorkspaceSelectionGesture::Plain,
        )
        .unwrap();
        apply_workspace_selection(
            &mut workspace,
            remote.clone(),
            WorkspaceSelectionGesture::Toggle,
        )
        .unwrap();
        let workspace = workspace.expect("workspace");
        assert_eq!(workspace.pane_count(), 2);
        assert_eq!(workspace.focused_task(), Some(remote.clone()));

        let mut registry = TaskSurfaceRegistry::<TestHostKey>::default();
        let schedule = registry.conversation_query_schedule(&workspace, 1);
        assert_eq!(schedule.len(), 2);
        assert_eq!(schedule[0].task_id, remote);
        assert_eq!(schedule[0].priority, ConversationQueryPriority::Interactive);
        assert_eq!(schedule[1].task_id, local);
        assert_eq!(schedule[1].priority, ConversationQueryPriority::Background);
    }

    #[test]
    fn terminal_admit_rejects_mismatched_canonical_task_for_host_key() {
        let shared = TaskId::new();
        let other = TaskId::new();
        let local = host_key("local", shared);
        let mut registry = TaskSurfaceRegistry::<TestHostKey>::default();
        let mut projection = surface_with_terminal_lines(&["mismatch"])
            .latest_terminal()
            .cloned()
            .expect("projection");
        projection.task_id = other;

        assert_eq!(
            registry.admit_terminal(local.clone(), &projection),
            Err(SurfaceAdmissionError::WrongTask)
        );
        assert!(registry.state(local).is_none());
    }

    #[test]
    fn terminal_admit_separates_same_raw_task_id_across_host_owners() {
        let shared = TaskId::new();
        let local = host_key("local", shared);
        let remote = host_key("remote", shared);
        let mut registry = TaskSurfaceRegistry::<TestHostKey>::default();
        let mut projection = surface_with_terminal_lines(&["shared-raw"])
            .latest_terminal()
            .cloned()
            .expect("projection");
        projection.task_id = shared;

        registry
            .admit_terminal(local.clone(), &projection)
            .expect("admit under local owner");
        registry
            .admit_terminal(remote.clone(), &projection)
            .expect("admit under remote owner");

        assert!(registry.terminal_is_interactive(local.clone()));
        assert!(registry.terminal_is_interactive(remote.clone()));
        assert_eq!(
            registry.terminal_tail(local.clone(), 1),
            vec!["shared-raw".to_string()]
        );
        assert_eq!(
            registry.terminal_tail(remote.clone(), 1),
            vec!["shared-raw".to_string()]
        );
        assert_eq!(
            registry
                .state(local)
                .and_then(|s| s.latest_terminal().map(|p| p.task_id)),
            Some(shared)
        );
        assert_eq!(
            registry
                .state(remote)
                .and_then(|s| s.latest_terminal().map(|p| p.task_id)),
            Some(shared),
            "domain TaskId on projection is unchanged; owners stay distinct"
        );
    }

    #[test]
    fn terminal_admit_rejects_mismatched_canonical_task_for_local_task_id() {
        let expected = TaskId::new();
        let other = TaskId::new();
        let mut registry = TaskSurfaceRegistry::default();
        let mut projection = surface_with_terminal_lines(&["local-mismatch"])
            .latest_terminal()
            .cloned()
            .expect("projection");
        projection.task_id = other;

        assert_eq!(
            registry.admit_terminal(expected, &projection),
            Err(SurfaceAdmissionError::WrongTask)
        );
        assert!(registry.state(expected).is_none());
    }
}
