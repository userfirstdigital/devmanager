//! Durable per-terminal facts and the per-task terminal strip.
//!
//! `ResourceId` is the only identity. Everything here is a recorded fact.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::resource::{ResourceFacts, ResourceKind};
use crate::domain::{ResourceId, TaskId};

pub const MAX_PLAIN_SHELLS_PER_TASK: usize = 8;
pub const TERMINAL_CWD_DEBOUNCE_MS: i64 = 2_000;
pub const TERMINAL_ACTIVITY_COALESCE_MS: i64 = 30_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalExit {
    pub code: Option<i32>,
    pub summary: String,
    pub at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalFacts {
    pub resource_id: ResourceId,
    pub title: Option<String>,
    pub live_cwd: Option<PathBuf>,
    pub exit: Option<TerminalExit>,
    pub created_at_ms: i64,
    pub last_activity_at_ms: i64,
}

impl TerminalFacts {
    pub fn registered(resource_id: ResourceId, title: Option<String>, created_at_ms: i64) -> Self {
        Self {
            resource_id,
            title,
            live_cwd: None,
            exit: None,
            created_at_ms,
            last_activity_at_ms: created_at_ms,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskTerminalStrip {
    pub order: Vec<ResourceId>,
    pub focused: Option<ResourceId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalStripError {
    Duplicate(ResourceId),
    FocusedNotInOrder(ResourceId),
    NotATerminal(ResourceId),
    ForeignTask(ResourceId),
    TooManyTerminals(usize),
}

impl std::fmt::Display for TerminalStripError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Duplicate(id) => write!(f, "duplicate terminal {id} in strip"),
            Self::FocusedNotInOrder(id) => {
                write!(f, "focused terminal {id} is not in the strip")
            }
            Self::NotATerminal(id) => {
                write!(f, "resource {id} is not a plain shell terminal")
            }
            Self::ForeignTask(id) => {
                write!(f, "resource {id} does not belong to this task")
            }
            Self::TooManyTerminals(count) => write!(
                f,
                "strip holds {count} terminals, more than {MAX_PLAIN_SHELLS_PER_TASK}"
            ),
        }
    }
}

impl std::error::Error for TerminalStripError {}

impl TaskTerminalStrip {
    pub fn validate(
        &self,
        task_id: TaskId,
        resources: &BTreeMap<ResourceId, ResourceFacts>,
    ) -> Result<(), TerminalStripError> {
        if self.order.len() > MAX_PLAIN_SHELLS_PER_TASK {
            return Err(TerminalStripError::TooManyTerminals(self.order.len()));
        }
        let mut seen = BTreeSet::new();
        for id in &self.order {
            if !seen.insert(*id) {
                return Err(TerminalStripError::Duplicate(*id));
            }
            let facts = resources
                .get(id)
                .ok_or(TerminalStripError::ForeignTask(*id))?;
            if facts.task_id != Some(task_id) {
                return Err(TerminalStripError::ForeignTask(*id));
            }
            // The strip holds plain shells only: a provider-owned terminal
            // (a recipe with no launch) is never a user-facing tab.
            if facts.resource_kind != ResourceKind::Terminal || !facts.recipe.is_plain_shell() {
                return Err(TerminalStripError::NotATerminal(*id));
            }
        }
        if let Some(focused) = self.focused {
            if !seen.contains(&focused) {
                return Err(TerminalStripError::FocusedNotInOrder(focused));
            }
        }
        Ok(())
    }

    /// Remove a released terminal. Focus is cleared when it pointed at the
    /// removed terminal; the strip then has no focused terminal at all.
    pub fn remove(&mut self, resource_id: ResourceId) {
        self.order.retain(|id| *id != resource_id);
        if self.focused == Some(resource_id) {
            self.focused = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHELL_CWD: &str = if cfg!(windows) { "C:/Code" } else { "/code" };
    const SHELL_PROGRAM: &str = if cfg!(windows) {
        "C:/Windows/System32/cmd.exe"
    } else {
        "/bin/sh"
    };
    use crate::domain::resource::{
        OwnerKind, ResourceFacts, ResourceKind, ResourceRecipe, TerminalLaunch,
    };
    use crate::domain::{ResourceId, TaskId};
    use std::collections::BTreeMap;

    fn plain_shell_recipe() -> ResourceRecipe {
        ResourceRecipe::Terminal {
            cols: 80,
            rows: 24,
            launch: Some(TerminalLaunch {
                cwd: PathBuf::from(SHELL_CWD),
                program: PathBuf::from(SHELL_PROGRAM),
                args: vec![],
            }),
            title: None,
        }
    }

    /// The strip only ever holds plain shells, so the fixture is one.
    fn terminal_resource(task_id: TaskId) -> ResourceFacts {
        resource_with_recipe(task_id, plain_shell_recipe())
    }

    fn resource_with_recipe(task_id: TaskId, recipe: ResourceRecipe) -> ResourceFacts {
        ResourceFacts::new(
            Some(task_id),
            OwnerKind::Task,
            ResourceKind::Terminal,
            recipe,
            1_725_000_000_000,
        )
        .expect("terminal resource")
    }

    #[test]
    fn strip_rejects_duplicates_and_unknown_focus() {
        let task_id = TaskId::new();
        let a = terminal_resource(task_id);
        let mut resources = BTreeMap::new();
        resources.insert(a.id, a.clone());
        let duplicate = TaskTerminalStrip {
            order: vec![a.id, a.id],
            focused: None,
        };
        assert_eq!(
            duplicate.validate(task_id, &resources),
            Err(TerminalStripError::Duplicate(a.id))
        );
        let stranger = ResourceId::new();
        let bad_focus = TaskTerminalStrip {
            order: vec![a.id],
            focused: Some(stranger),
        };
        assert_eq!(
            bad_focus.validate(task_id, &resources),
            Err(TerminalStripError::FocusedNotInOrder(stranger))
        );
    }

    #[test]
    fn strip_rejects_foreign_and_non_terminal_resources() {
        let task_id = TaskId::new();
        let other_task = TaskId::new();
        let foreign = terminal_resource(other_task);
        let browser = ResourceFacts::new(
            Some(task_id),
            OwnerKind::Task,
            ResourceKind::BrowserContext,
            ResourceRecipe::browser("https://example.test").expect("browser recipe"),
            1_725_000_000_000,
        )
        .expect("browser resource");
        let mut resources = BTreeMap::new();
        resources.insert(foreign.id, foreign.clone());
        resources.insert(browser.id, browser.clone());
        assert_eq!(
            TaskTerminalStrip {
                order: vec![foreign.id],
                focused: None
            }
            .validate(task_id, &resources),
            Err(TerminalStripError::ForeignTask(foreign.id))
        );
        assert_eq!(
            TaskTerminalStrip {
                order: vec![browser.id],
                focused: None
            }
            .validate(task_id, &resources),
            Err(TerminalStripError::NotATerminal(browser.id))
        );
    }

    #[test]
    fn remove_keeps_focus_when_another_terminal_is_removed() {
        let task_id = TaskId::new();
        let a = terminal_resource(task_id);
        let b = terminal_resource(task_id);
        let mut strip = TaskTerminalStrip {
            order: vec![a.id, b.id],
            focused: Some(a.id),
        };
        strip.remove(b.id);
        assert_eq!(strip.order, vec![a.id]);
        assert_eq!(strip.focused, Some(a.id));
    }

    #[test]
    fn remove_clears_focus_when_the_focused_terminal_is_removed() {
        let task_id = TaskId::new();
        let a = terminal_resource(task_id);
        let b = terminal_resource(task_id);
        let mut strip = TaskTerminalStrip {
            order: vec![a.id, b.id],
            focused: Some(a.id),
        };
        strip.remove(a.id);
        assert_eq!(strip.order, vec![b.id]);
        assert_eq!(strip.focused, None);
    }

    #[test]
    fn remove_last_terminal_empties_the_strip() {
        let task_id = TaskId::new();
        let a = terminal_resource(task_id);
        let mut strip = TaskTerminalStrip {
            order: vec![a.id],
            focused: Some(a.id),
        };
        strip.remove(a.id);
        assert!(strip.order.is_empty());
        assert_eq!(strip.focused, None);
    }

    #[test]
    fn strip_limits_are_pinned() {
        assert_eq!(MAX_PLAIN_SHELLS_PER_TASK, 8);
        assert_eq!(TERMINAL_CWD_DEBOUNCE_MS, 2_000);
        assert_eq!(TERMINAL_ACTIVITY_COALESCE_MS, 30_000);
    }

    #[test]
    fn strip_rejects_a_provider_terminal_and_an_oversized_order() {
        let task_id = TaskId::new();
        let provider = resource_with_recipe(task_id, ResourceRecipe::terminal(80, 24));
        let mut resources = BTreeMap::new();
        resources.insert(provider.id, provider.clone());
        assert_eq!(
            TaskTerminalStrip {
                order: vec![provider.id],
                focused: None
            }
            .validate(task_id, &resources),
            Err(TerminalStripError::NotATerminal(provider.id))
        );

        let shells: Vec<ResourceFacts> = (0..=MAX_PLAIN_SHELLS_PER_TASK)
            .map(|_| terminal_resource(task_id))
            .collect();
        for shell in &shells {
            resources.insert(shell.id, shell.clone());
        }
        let order: Vec<ResourceId> = shells.iter().map(|shell| shell.id).collect();
        assert_eq!(
            TaskTerminalStrip {
                order: order.clone(),
                focused: None
            }
            .validate(task_id, &resources),
            Err(TerminalStripError::TooManyTerminals(
                MAX_PLAIN_SHELLS_PER_TASK + 1
            ))
        );
        // Exactly at the bound still passes.
        assert_eq!(
            TaskTerminalStrip {
                order: order[..MAX_PLAIN_SHELLS_PER_TASK].to_vec(),
                focused: None
            }
            .validate(task_id, &resources),
            Ok(())
        );
    }

    #[test]
    fn strip_error_display_names_the_plain_shell_rule_and_the_bound() {
        let id = ResourceId::new();
        assert_eq!(
            TerminalStripError::NotATerminal(id).to_string(),
            format!("resource {id} is not a plain shell terminal")
        );
        assert_eq!(
            TerminalStripError::TooManyTerminals(9).to_string(),
            format!("strip holds 9 terminals, more than {MAX_PLAIN_SHELLS_PER_TASK}")
        );
    }

    #[test]
    fn valid_strip_passes() {
        let task_id = TaskId::new();
        let a = terminal_resource(task_id);
        let b = terminal_resource(task_id);
        let mut resources = BTreeMap::new();
        resources.insert(a.id, a.clone());
        resources.insert(b.id, b.clone());
        let strip = TaskTerminalStrip {
            order: vec![b.id, a.id],
            focused: Some(a.id),
        };
        assert_eq!(strip.validate(task_id, &resources), Ok(()));
    }
}
