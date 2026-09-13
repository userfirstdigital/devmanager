//! Virtualized Saved Prompts list/detail projection.

use super::super::shell::LibrarySection;
use crate::domain::id::{PromptId, PromptVersionId};
use crate::prompts::model::SavedPrompt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedPromptRow {
    pub id: PromptId,
    pub title: String,
    pub tags: Vec<String>,
    pub current_version_id: PromptVersionId,
    pub revision: u64,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualWindow<T> {
    pub offset: usize,
    pub visible: Vec<T>,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryListState {
    pub section: LibrarySection,
    pub total: usize,
    pub offset: usize,
    pub visible: Vec<SavedPromptRow>,
    pub focused_index: usize,
    pub includes_provider_commands: bool,
}

pub fn filter_saved_prompts(prompts: &[SavedPrompt], query: &str) -> Vec<SavedPromptRow> {
    let needle = query.trim().to_lowercase();
    prompts
        .iter()
        .filter(|prompt| {
            if needle.is_empty() {
                return true;
            }
            prompt.title.to_lowercase().contains(&needle)
                || prompt
                    .description
                    .as_deref()
                    .is_some_and(|description| description.to_lowercase().contains(&needle))
                || prompt
                    .tags
                    .iter()
                    .any(|tag| tag.to_lowercase().contains(&needle))
        })
        .map(|prompt| SavedPromptRow {
            id: prompt.id,
            title: prompt.title.clone(),
            tags: prompt.tags.clone(),
            current_version_id: prompt.current_version_id,
            revision: prompt.revision,
            archived: prompt.archived_at_ms.is_some(),
        })
        .collect()
}

pub fn virtualize<T: Clone>(items: &[T], offset: usize, window: usize) -> VirtualWindow<T> {
    let total = items.len();
    let offset = offset.min(total);
    let visible = items.iter().skip(offset).take(window).cloned().collect();
    VirtualWindow {
        offset,
        visible,
        total,
    }
}
