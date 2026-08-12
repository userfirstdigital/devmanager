//! Composer-owned Prompt Library picker. Provider slash commands stay separate.

use super::history::RecentHistoryRecord;
use super::library::filter_saved_prompts;
use crate::domain::id::{PromptChainLinkId, PromptHistoryId, PromptId, PromptVersionId};
use crate::prompts::model::{PromptVersion, SavedPrompt};
use crate::prompts::projection::PromptChainLinkRecord;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptPickerSource {
    Saved,
    Recent,
    Chain,
    ProviderCommands,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptPickerHit {
    pub source: PromptPickerSource,
    pub title: String,
    pub prompt_id: Option<PromptId>,
    pub prompt_version_id: Option<PromptVersionId>,
    pub history_id: Option<PromptHistoryId>,
    pub chain_link_id: Option<PromptChainLinkId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptPicker {
    pub source: PromptPickerSource,
    pub query: String,
    pub hits: Vec<PromptPickerHit>,
    pub notice: Option<String>,
}

pub fn open_picker(
    source: PromptPickerSource,
    saved: &[SavedPrompt],
    history: &[RecentHistoryRecord],
    links: &[PromptChainLinkRecord],
    versions: &[PromptVersion],
    query: &str,
) -> PromptPicker {
    let hits = match source {
        PromptPickerSource::Saved => filter_saved_prompts(saved, query)
            .into_iter()
            .map(|row| PromptPickerHit {
                source: PromptPickerSource::Saved,
                title: row.title,
                prompt_id: Some(row.id),
                prompt_version_id: Some(row.current_version_id),
                history_id: None,
                chain_link_id: None,
            })
            .collect(),
        PromptPickerSource::Recent => history
            .iter()
            .filter(|row| {
                query.trim().is_empty() || row.body.to_lowercase().contains(&query.to_lowercase())
            })
            .map(|row| PromptPickerHit {
                source: PromptPickerSource::Recent,
                title: row.body.chars().take(80).collect(),
                prompt_id: None,
                prompt_version_id: None,
                history_id: Some(row.id),
                chain_link_id: None,
            })
            .collect(),
        PromptPickerSource::Chain => links
            .iter()
            .map(|link| {
                let title = saved
                    .iter()
                    .find(|prompt| prompt.id == link.prompt_id())
                    .map(|prompt| prompt.title.clone())
                    .unwrap_or_else(|| "Chain link".into());
                PromptPickerHit {
                    source: PromptPickerSource::Chain,
                    title,
                    prompt_id: Some(link.prompt_id()),
                    prompt_version_id: Some(link.prompt_version_id()),
                    history_id: None,
                    chain_link_id: Some(link.id()),
                }
            })
            .collect(),
        PromptPickerSource::ProviderCommands => Vec::new(),
    };
    let _ = versions;
    PromptPicker {
        source,
        query: query.to_string(),
        hits,
        notice: None,
    }
}
