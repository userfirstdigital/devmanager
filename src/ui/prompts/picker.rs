//! Composer-owned Prompt Library picker. Provider slash commands stay separate.

use super::history::RecentHistoryRecord;
use super::library::filter_saved_prompts;
use crate::domain::id::{PromptChainLinkId, PromptHistoryId, PromptId, PromptVersionId};
use crate::prompts::model::{PromptVersion, SavedPrompt};
use crate::prompts::organization::{OrgPrompt, OrgPromptVersion};
use crate::prompts::projection::PromptChainLinkRecord;

/// A picker is a presentation window, not a second search index. Keep the
/// result set bounded so opening it never turns a keystroke into a large body
/// transfer or an unbounded native view.
pub const MAX_PICKER_HITS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptPickerSource {
    Saved,
    Recent,
    Chain,
    Organization,
    ProviderCommands,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptPickerHit {
    pub source: PromptPickerSource,
    pub title: String,
    pub prompt_id: Option<PromptId>,
    pub prompt_version_id: Option<PromptVersionId>,
    pub organization_prompt_id: Option<crate::org::OrgPromptId>,
    pub organization_prompt_version_id: Option<crate::org::OrgPromptVersionId>,
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
                organization_prompt_id: None,
                organization_prompt_version_id: None,
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
                organization_prompt_id: None,
                organization_prompt_version_id: None,
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
                    organization_prompt_id: None,
                    organization_prompt_version_id: None,
                    history_id: None,
                    chain_link_id: Some(link.id()),
                }
            })
            .collect(),
        PromptPickerSource::Organization => Vec::new(),
        PromptPickerSource::ProviderCommands => Vec::new(),
    };
    let _ = versions;
    let mut picker = PromptPicker {
        source,
        query: query.to_string(),
        hits,
        notice: None,
    };
    picker.hits.truncate(MAX_PICKER_HITS);
    picker
}

/// Open the read-only organization catalog picker. Organization IDs stay
/// separate from local prompt IDs; selecting one never silently creates a
/// personal prompt or sends provider input.
pub fn open_organization_picker(
    prompts: &[OrgPrompt],
    versions: &[OrgPromptVersion],
    query: &str,
) -> PromptPicker {
    let needle = query.trim().to_lowercase();
    let mut hits = Vec::new();
    for prompt in prompts {
        let version = versions
            .iter()
            .find(|version| version.version_id == prompt.current_version_id);
        let matches = needle.is_empty()
            || prompt.name.to_lowercase().contains(&needle)
            || prompt.namespace.to_lowercase().contains(&needle)
            || version.is_some_and(|version| {
                version.title.to_lowercase().contains(&needle)
                    || version
                        .tags
                        .iter()
                        .any(|tag| tag.to_lowercase().contains(&needle))
            });
        if !matches {
            continue;
        }
        hits.push(PromptPickerHit {
            source: PromptPickerSource::Organization,
            title: version
                .map(|version| version.title.clone())
                .unwrap_or_else(|| prompt.name.clone()),
            prompt_id: None,
            prompt_version_id: None,
            organization_prompt_id: Some(prompt.prompt_id),
            organization_prompt_version_id: Some(prompt.current_version_id),
            history_id: None,
            chain_link_id: None,
        });
        if hits.len() == MAX_PICKER_HITS {
            break;
        }
    }
    PromptPicker {
        source: PromptPickerSource::Organization,
        query: query.to_string(),
        hits,
        notice: Some("Organization prompts are published read-only; choose a version to preview or insert manually".into()),
    }
}
