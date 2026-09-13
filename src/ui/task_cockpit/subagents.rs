//! Subagent navigation derived only from admitted, provider-correlated facts.
use crate::domain::{SemanticJournalPage, SemanticJournalPayload};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentTab {
    pub id: String,
    pub label: String,
    pub active: bool,
    pub completed: bool,
    pub foldable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubagentTabAction {
    Select(String),
    ToggleCompleted,
}

pub fn catalog(page: &SemanticJournalPage) -> Vec<SubagentTab> {
    let parent_end = page
        .facts
        .iter()
        .filter(|fact| fact.subagent_id.is_none())
        .filter_map(|fact| match &fact.payload {
            SemanticJournalPayload::SessionState { state }
                if state == "ready" || state == "ended" || state.starts_with("ended: ") =>
            {
                Some(fact.sequence)
            }
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let mut entries = BTreeMap::<String, (u64, u64, SubagentTab)>::new();
    for fact in &page.facts {
        let Some(id) = &fact.subagent_id else {
            continue;
        };
        let entry = entries.entry(id.clone()).or_insert_with(|| {
            (
                fact.sequence,
                0,
                SubagentTab {
                    id: id.clone(),
                    label: "Subagent".into(),
                    active: false,
                    completed: false,
                    foldable: false,
                },
            )
        });
        if let SemanticJournalPayload::PlanStep {
            step_id,
            title,
            status,
        } = &fact.payload
        {
            if step_id.starts_with("subagent:") && fact.sequence >= entry.1 {
                entry.1 = fact.sequence;
                entry.2.label = title.chars().take(48).collect();
                entry.2.active = status == "active";
                entry.2.completed = status == "completed";
                entry.2.foldable = status == "completed" && parent_end > fact.sequence;
            }
        }
    }
    let mut entries = entries.into_values().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.0);
    entries.into_iter().map(|entry| entry.2).collect()
}

pub fn scoped_page(page: &SemanticJournalPage, selected: Option<&str>) -> SemanticJournalPage {
    let mut scoped = page.clone();
    scoped.facts.retain(|fact| match selected {
        Some(id) => fact.subagent_id.as_deref() == Some(id),
        None => fact.subagent_id.is_none()
            || matches!(&fact.payload, SemanticJournalPayload::ApprovalRequest { .. } | SemanticJournalPayload::ApprovalResult { .. } | SemanticJournalPayload::Question { .. } | SemanticJournalPayload::Error { .. })
            || matches!(&fact.payload, SemanticJournalPayload::PlanStep { step_id, .. } if step_id.starts_with("subagent:")),
    });
    scoped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, PrivacyClass, SemanticJournalFact};
    fn fact(
        sequence: u64,
        id: Option<&str>,
        payload: SemanticJournalPayload,
    ) -> SemanticJournalFact {
        SemanticJournalFact {
            id: EventId::new(),
            sequence,
            occurred_at_ms: None,
            subagent_id: id.map(str::to_owned),
            provider: "claude_code".into(),
            schema_version: 1,
            kind: "test".into(),
            visibility: "task".into(),
            privacy_class: PrivacyClass::LocalOnly,
            redacted: false,
            payload,
        }
    }
    fn page(facts: Vec<SemanticJournalFact>) -> SemanticJournalPage {
        SemanticJournalPage {
            oldest_sequence: 1,
            cursor_rolled_over: false,
            after_sequence: 0,
            through_sequence: 9,
            high_water: 9,
            encoded_bytes: 0,
            next_sequence: None,
            facts,
        }
    }
    fn child(sequence: u64, id: &str, status: &str) -> SemanticJournalFact {
        fact(
            sequence,
            Some(id),
            SemanticJournalPayload::PlanStep {
                step_id: format!("subagent:{id}"),
                title: "Explore".into(),
                status: status.into(),
            },
        )
    }
    #[test]
    fn finished_children_fold_only_after_parent_turn_end_and_live_children_stay_visible() {
        let mut page = page(vec![
            child(1, "session-a:child", "completed"),
            child(2, "session-b:child", "active"),
        ]);
        assert!(!catalog(&page)[0].foldable);
        page.facts.push(fact(
            3,
            None,
            SemanticJournalPayload::SessionState {
                state: "ready".into(),
            },
        ));
        let tabs = catalog(&page);
        assert_eq!(tabs.len(), 2);
        assert!(tabs[0].foldable);
        assert!(!tabs[0].active);
        assert!(tabs[1].active);
        assert!(!tabs[1].foldable);
        page.facts.push(child(4, "session-a:child", "active"));
        assert!(!catalog(&page)[0].foldable);
    }
    #[test]
    fn scope_keeps_parent_attention_but_never_mix_child_output_or_invent_membership() {
        let page = page(vec![
            fact(
                1,
                None,
                SemanticJournalPayload::AssistantText {
                    text: "parent".into(),
                },
            ),
            child(2, "child-a", "active"),
            fact(
                3,
                Some("child-a"),
                SemanticJournalPayload::AssistantText { text: "a".into() },
            ),
            fact(
                4,
                Some("child-b"),
                SemanticJournalPayload::AssistantText { text: "b".into() },
            ),
            fact(
                5,
                Some("child-a"),
                SemanticJournalPayload::Question {
                    question_id: "q".into(),
                    prompt: "Continue?".into(),
                    options: vec![],
                },
            ),
        ]);
        assert_eq!(
            scoped_page(&page, None)
                .facts
                .iter()
                .map(|fact| fact.sequence)
                .collect::<Vec<_>>(),
            [1, 2, 5]
        );
        assert_eq!(
            scoped_page(&page, Some("child-a"))
                .facts
                .iter()
                .map(|fact| fact.sequence)
                .collect::<Vec<_>>(),
            [2, 3, 5]
        );
        assert!(scoped_page(&page, Some("foreign")).facts.is_empty());
    }
}
