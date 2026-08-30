use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

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
    pub latest_terminal: Option<TaskTerminalProjection>,
    pub terminal_attachment: TerminalAttachmentState,
    terminal_query_in_flight: bool,
    pending_user_messages: Vec<PendingUserMessage>,
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
    ConversationSync,
    TerminalInitial,
    TerminalSync,
}

impl TaskSurfaceState {
    pub fn center_loading_state(
        &self,
        showing_terminal: bool,
    ) -> Option<CenterSurfaceLoadingState> {
        if showing_terminal {
            self.terminal_query_in_flight
                .then_some(if self.latest_terminal.is_some() {
                    CenterSurfaceLoadingState::TerminalSync
                } else {
                    CenterSurfaceLoadingState::TerminalInitial
                })
        } else {
            self.conversation_in_flight
                .then_some(if self.conversation_has_content() {
                    CenterSurfaceLoadingState::ConversationSync
                } else {
                    CenterSurfaceLoadingState::ConversationInitial
                })
        }
    }

    pub fn note_terminal_query_started(&mut self) {
        self.terminal_query_in_flight = true;
        if self.latest_terminal.is_none() {
            self.terminal_attachment = TerminalAttachmentState::Starting;
        }
    }

    pub fn note_terminal_reconnecting(&mut self) {
        self.terminal_query_in_flight = false;
        if self.latest_terminal.is_some() {
            self.terminal_attachment = TerminalAttachmentState::StaleReconnecting;
        } else {
            // A missing first projection after a transient transport/query
            // failure is still a recoverable startup state. Only an explicit
            // authoritative unsupported/exited result may label the terminal
            // unavailable; otherwise returning tasks flash a false terminal
            // failure while the host reconnects.
            self.terminal_attachment = TerminalAttachmentState::Starting;
        }
    }

    pub fn note_terminal_unavailable(&mut self) {
        self.terminal_query_in_flight = false;
        self.terminal_attachment = TerminalAttachmentState::Unavailable;
    }

    pub fn note_terminal_exited(&mut self) {
        self.terminal_query_in_flight = false;
        self.terminal_attachment = TerminalAttachmentState::Exited;
    }

    pub fn terminal_query_in_flight(&self) -> bool {
        self.terminal_query_in_flight
    }

    pub fn conversation_has_content(&self) -> bool {
        self.conversation.fact_count() > 0 || !self.pending_user_messages.is_empty()
    }

    pub fn terminal_is_interactive(&self) -> bool {
        self.terminal_attachment == TerminalAttachmentState::Live && self.latest_terminal.is_some()
    }

    pub fn terminal_label(&self) -> &'static str {
        match self.terminal_attachment {
            TerminalAttachmentState::Live => "Terminal is live",
            TerminalAttachmentState::StaleReconnecting => "Reconnecting — last terminal screen",
            TerminalAttachmentState::Starting => "Terminal starting",
            TerminalAttachmentState::Unavailable => "Terminal unavailable",
            TerminalAttachmentState::Exited => "Terminal exited",
        }
    }

    pub fn terminal_empty_message(&self) -> &'static str {
        match self.terminal_attachment {
            TerminalAttachmentState::Live => "Terminal is live; waiting for output.",
            TerminalAttachmentState::StaleReconnecting => "Reconnecting — last terminal screen",
            TerminalAttachmentState::Starting => "Terminal starting…",
            TerminalAttachmentState::Unavailable => "Terminal unavailable",
            TerminalAttachmentState::Exited => "Terminal exited",
        }
    }

    pub fn terminal_tail(&self, max: usize) -> Vec<String> {
        let Some(terminal) = self.latest_terminal.as_ref() else {
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
        state.latest_terminal = Some(projection.clone());
        state.terminal_attachment = TerminalAttachmentState::Live;
        state.terminal_query_in_flight = false;
        Ok(())
    }

    pub fn note_terminal_reconnecting(&mut self, task_id: K) {
        self.ensure_task(task_id).note_terminal_reconnecting();
    }

    pub fn note_terminal_query_started(&mut self, task_id: K) {
        self.ensure_task(task_id).note_terminal_query_started();
    }

    pub fn terminal_is_interactive(&self, task_id: K) -> bool {
        self.state(task_id)
            .is_some_and(TaskSurfaceState::terminal_is_interactive)
    }

    pub fn terminal_label(&self, task_id: K) -> &'static str {
        self.state(task_id)
            .map(TaskSurfaceState::terminal_label)
            .unwrap_or("Terminal unavailable")
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
        assert_eq!(empty.terminal_attachment, TerminalAttachmentState::Starting);
        assert!(
            empty.terminal_query_in_flight(),
            "the empty terminal surface must expose its active load"
        );

        let mut live = surface_with_terminal_lines(&["ready"]);
        live.note_terminal_query_started();
        assert_eq!(live.terminal_attachment, TerminalAttachmentState::Live);
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
            empty.terminal_attachment,
            TerminalAttachmentState::Starting,
            "a transient first-load failure must remain retryable rather than falsely reporting an unsupported terminal"
        );
    }

    #[test]
    fn conversation_loading_distinguishes_first_load_from_cached_sync() {
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
            Some(CenterSurfaceLoadingState::ConversationSync)
        );
        assert!(
            syncing.conversation_has_content(),
            "a refresh must preserve cached content behind a compact syncing indicator"
        );

        let mut terminal = TaskSurfaceState::default();
        terminal.note_terminal_query_started();
        assert_eq!(
            terminal.center_loading_state(true),
            Some(CenterSurfaceLoadingState::TerminalInitial)
        );
        terminal.latest_terminal = surface_with_terminal_lines(&["cached"]).latest_terminal;
        assert_eq!(
            terminal.center_loading_state(true),
            Some(CenterSurfaceLoadingState::TerminalSync)
        );
    }

    #[test]
    fn compact_wire_terminal_reconstructs_text_tail_from_indexed_cells() {
        let mut surface = surface_with_terminal_lines(&["first", "second"]);
        surface
            .latest_terminal
            .as_mut()
            .expect("terminal")
            .screen
            .lines
            .clear();

        assert_eq!(
            surface.terminal_tail(2),
            vec!["first".to_string(), "second".to_string()]
        );
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
        surface.latest_terminal = Some(TaskTerminalProjection {
            accepts_input_without_conversation_id: false,
            task_id,
            terminal_id: TerminalId::new(),
            session_id: TerminalSessionId::new(),
            agent_session_id: AgentSessionId::new(),
            resource_id: ResourceId::new(),
            runtime_generation: 1,
            resource_generation: 1,
            action_epoch: 1,
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
        });
        surface.terminal_attachment = TerminalAttachmentState::Live;
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
        assert!(!registry.conversation_turn_completed(task));

        registry.begin_conversation(task, 1);
        registry
            .admit_conversation(task, 1, &user_page(1, "hello"))
            .expect("admit durable user message");
        assert!(!registry.conversation_turn_completed(task));

        registry.begin_conversation(task, 2);
        registry
            .admit_conversation(task, 2, &page(2, "done"))
            .expect("admit assistant response");
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
        assert!(!working_conversation_poll_due(false, Duration::from_secs(30)));
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
            .latest_terminal
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
            .latest_terminal
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
                .and_then(|s| s.latest_terminal.as_ref().map(|p| p.task_id)),
            Some(shared)
        );
        assert_eq!(
            registry
                .state(remote)
                .and_then(|s| s.latest_terminal.as_ref().map(|p| p.task_id)),
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
            .latest_terminal
            .expect("projection");
        projection.task_id = other;

        assert_eq!(
            registry.admit_terminal(expected, &projection),
            Err(SurfaceAdmissionError::WrongTask)
        );
        assert!(registry.state(expected).is_none());
    }
}
