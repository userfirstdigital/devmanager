use std::collections::{BTreeMap, BTreeSet};

use crate::domain::{
    SemanticJournalFact, SemanticJournalPage, SemanticJournalPayload, TaskId,
    TaskTerminalProjection,
};

use super::{Axis, PanePresentation, TaskWorkspace, WorkspaceError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationQueryPriority {
    Interactive,
    Background,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConversationQueryPlan {
    pub task_id: TaskId,
    pub priority: ConversationQueryPriority,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceSelectionGesture {
    Plain,
    Toggle,
}

/// Apply the task-list gesture without coupling the recursive model to GPUI.
/// Plain selection preserves an existing multi-pane membership set; Shift-
/// selection is the only gesture that adds or removes panes.
pub fn apply_workspace_selection(
    workspace: &mut Option<TaskWorkspace>,
    task_id: TaskId,
    gesture: WorkspaceSelectionGesture,
) -> Result<(), WorkspaceError> {
    let Some(current) = workspace.as_mut() else {
        *workspace = Some(TaskWorkspace::single(task_id));
        return Ok(());
    };

    match gesture {
        WorkspaceSelectionGesture::Plain => {
            if current.contains_task(task_id) {
                current.focus_task(task_id)
            } else if current.pane_count() <= 1 {
                *workspace = Some(TaskWorkspace::single(task_id));
                Ok(())
            } else {
                // A plain click must not silently change multi-pane membership.
                Ok(())
            }
        }
        WorkspaceSelectionGesture::Toggle if current.contains_task(task_id) => {
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

impl TaskConversationCache {
    pub fn as_page(&self) -> SemanticJournalPage {
        SemanticJournalPage {
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

    pub fn merge_page(&mut self, page: &SemanticJournalPage) {
        self.high_water = self.high_water.max(page.high_water);
        self.through_sequence = self.through_sequence.max(page.through_sequence);
        for fact in &page.facts {
            if self
                .facts
                .iter()
                .any(|existing| existing.sequence == fact.sequence)
            {
                continue;
            }
            self.facts.push(fact.clone());
        }
        self.facts.sort_by_key(|fact| fact.sequence);
        self.next_sequence = page.next_sequence;
        if self.next_sequence.is_none() {
            self.through_sequence = self
                .facts
                .last()
                .map(|fact| fact.sequence)
                .unwrap_or(self.through_sequence)
                .max(page.through_sequence);
        }
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
    pub latest_snippet: Option<String>,
    pub latest_terminal: Option<TaskTerminalProjection>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceAdmissionError {
    MissingSurface,
    StaleGeneration,
    WrongTask,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskSurfaceRegistry {
    surfaces: BTreeMap<TaskId, TaskSurfaceState>,
}

impl TaskSurfaceRegistry {
    pub fn ensure_task(&mut self, task_id: TaskId) -> &mut TaskSurfaceState {
        self.surfaces.entry(task_id).or_default()
    }

    pub fn state(&self, task_id: TaskId) -> Option<&TaskSurfaceState> {
        self.surfaces.get(&task_id)
    }

    pub fn retain_tasks(&mut self, task_ids: &[TaskId]) {
        let valid: BTreeSet<_> = task_ids.iter().copied().collect();
        self.surfaces.retain(|task_id, _| valid.contains(task_id));
    }

    pub fn begin_conversation(&mut self, task_id: TaskId, generation: u64) {
        let state = self.ensure_task(task_id);
        state.conversation_generation = generation;
        state.conversation_in_flight = true;
    }

    pub fn cancel_conversation(&mut self, task_id: TaskId, generation: u64) {
        if let Some(state) = self.surfaces.get_mut(&task_id) {
            if state.conversation_generation == generation {
                state.conversation_in_flight = false;
            }
        }
    }

    pub fn conversation_in_flight(&self, task_id: TaskId) -> bool {
        self.state(task_id)
            .is_some_and(|state| state.conversation_in_flight)
    }

    pub fn conversation_after_sequence(&self, task_id: TaskId) -> u64 {
        self.state(task_id)
            .map(|state| state.conversation.request_after_sequence())
            .unwrap_or(0)
    }

    pub fn conversation_page(&self, task_id: TaskId) -> Option<SemanticJournalPage> {
        self.state(task_id)
            .map(|state| state.conversation.as_page())
    }

    pub fn admit_conversation(
        &mut self,
        task_id: TaskId,
        generation: u64,
        page: &SemanticJournalPage,
    ) -> Result<SemanticJournalPage, SurfaceAdmissionError> {
        let state = self
            .surfaces
            .get_mut(&task_id)
            .ok_or(SurfaceAdmissionError::MissingSurface)?;
        if state.conversation_generation != generation || !state.conversation_in_flight {
            return Err(SurfaceAdmissionError::StaleGeneration);
        }
        state.conversation.merge_page(page);
        state.latest_snippet = state.conversation.latest_snippet().map(ToOwned::to_owned);
        state.conversation_in_flight = false;
        Ok(state.conversation.as_page())
    }

    pub fn admit_terminal(
        &mut self,
        task_id: TaskId,
        projection: &TaskTerminalProjection,
    ) -> Result<(), SurfaceAdmissionError> {
        if projection.task_id != task_id {
            return Err(SurfaceAdmissionError::WrongTask);
        }
        self.ensure_task(task_id).latest_terminal = Some(projection.clone());
        Ok(())
    }

    pub fn latest_snippet(&self, task_id: TaskId) -> Option<&str> {
        self.state(task_id)
            .and_then(|state| state.latest_snippet.as_deref())
    }

    pub fn conversation_tail(&self, task_id: TaskId, max: usize) -> Vec<String> {
        self.state(task_id)
            .map(|state| state.conversation.tail_snippets(max))
            .unwrap_or_default()
    }

    pub fn terminal_tail(&self, task_id: TaskId, max: usize) -> Vec<String> {
        let Some(terminal) = self
            .state(task_id)
            .and_then(|state| state.latest_terminal.as_ref())
        else {
            return Vec::new();
        };
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

    pub fn conversation_query_schedule(
        &self,
        workspace: &TaskWorkspace,
        max_background: usize,
    ) -> Vec<ConversationQueryPlan> {
        let focused = workspace.focused_task();
        let mut schedule = Vec::with_capacity(max_background.saturating_add(1));
        if let Some(task_id) = focused {
            if workspace.presentation(task_id) == Some(PanePresentation::Full)
                && !self.conversation_in_flight(task_id)
            {
                schedule.push(ConversationQueryPlan {
                    task_id,
                    priority: ConversationQueryPriority::Interactive,
                });
            }
        }

        let mut background: Vec<_> = workspace
            .task_ids()
            .into_iter()
            .filter(|task_id| Some(*task_id) != focused)
            .filter_map(|task_id| {
                let pane = workspace.pane_for_task(task_id)?;
                (pane.presentation == PanePresentation::Full
                    && !self.conversation_in_flight(task_id))
                .then_some((pane.last_focused_at, task_id))
            })
            .collect();
        background.sort_by_key(|(last_focused_at, _)| std::cmp::Reverse(*last_focused_at));
        schedule.extend(
            background
                .into_iter()
                .take(max_background)
                .map(|(_, task_id)| ConversationQueryPlan {
                    task_id,
                    priority: ConversationQueryPriority::Background,
                }),
        );
        schedule
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::{
        EventId, PrivacyClass, SemanticJournalFact, SemanticJournalPage, SemanticJournalPayload,
        TaskId,
    };

    use super::*;

    fn page(sequence: u64, text: &str) -> SemanticJournalPage {
        SemanticJournalPage {
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
    fn workspace_query_scheduler_prioritizes_focused_full_and_skips_compact() {
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
        assert!(schedule.iter().all(|plan| plan.task_id != third));
        assert!(schedule.iter().all(|plan| plan.task_id != fourth));
    }
}
