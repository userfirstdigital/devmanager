//! Saved prompt editor: title/description/tags/body and save-as-new-version.

use super::super::shell::PromptLibraryUiError;
use super::version_diff::VersionDiffView;
use crate::prompts::model::{
    PromptVersion, SavedPrompt, MAX_PROMPT_BODY_BYTES, MAX_PROMPT_DESCRIPTION_SCALARS,
    MAX_PROMPT_TAGS, MAX_PROMPT_TAG_SCALARS, MAX_PROMPT_TITLE_SCALARS,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptEditorAction {
    SetTitle(String),
    SetDescription(Option<String>),
    SetTags(Vec<String>),
    SetBody(String),
    SaveAsNewVersion {
        prompt: SavedPrompt,
        version: PromptVersion,
    },
    RestoreByCreatingNewVersion {
        prompt: SavedPrompt,
        version: PromptVersion,
    },
    DiscardUnsaved,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptEditor {
    pub title: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub body: String,
    pub dirty: bool,
    pub confirm_discard: bool,
    pub selected_version: Option<PromptVersion>,
    pub diff: Option<VersionDiffView>,
    pending_save: Option<(SavedPrompt, PromptVersion)>,
}

impl PromptEditor {
    pub fn load(&mut self, prompt: &SavedPrompt, version: &PromptVersion) {
        self.title = prompt.title.clone();
        self.description = prompt.description.clone();
        self.tags = prompt.tags.clone();
        self.body = version.body.clone();
        self.dirty = false;
        self.confirm_discard = false;
        self.selected_version = Some(version.clone());
        self.diff = None;
        self.pending_save = None;
    }

    pub fn select_version(&mut self, version: PromptVersion) {
        self.body = version.body.clone();
        self.selected_version = Some(version);
        self.dirty = false;
        self.confirm_discard = false;
    }

    pub fn apply(&mut self, action: PromptEditorAction) -> Result<(), PromptLibraryUiError> {
        match action {
            PromptEditorAction::SetTitle(title) => {
                if title.chars().count() > MAX_PROMPT_TITLE_SCALARS {
                    return Err(PromptLibraryUiError::CapExceeded);
                }
                self.title = title;
                self.dirty = true;
            }
            PromptEditorAction::SetDescription(description) => {
                if description
                    .as_deref()
                    .is_some_and(|value| value.chars().count() > MAX_PROMPT_DESCRIPTION_SCALARS)
                {
                    return Err(PromptLibraryUiError::CapExceeded);
                }
                self.description = description;
                self.dirty = true;
            }
            PromptEditorAction::SetTags(tags) => {
                if tags.len() > MAX_PROMPT_TAGS
                    || tags
                        .iter()
                        .any(|tag| tag.chars().count() > MAX_PROMPT_TAG_SCALARS)
                {
                    return Err(PromptLibraryUiError::CapExceeded);
                }
                self.tags = tags;
                self.dirty = true;
            }
            PromptEditorAction::SetBody(body) => {
                if body.len() > MAX_PROMPT_BODY_BYTES {
                    return Err(PromptLibraryUiError::CapExceeded);
                }
                self.body = body;
                self.dirty = true;
            }
            PromptEditorAction::SaveAsNewVersion { prompt, version }
            | PromptEditorAction::RestoreByCreatingNewVersion { prompt, version } => {
                if version.body.as_str() != self.body && !self.body.is_empty() {
                    // Caller supplies the exact next immutable version body.
                }
                self.pending_save = Some((prompt, version));
                self.dirty = false;
                self.confirm_discard = false;
            }
            PromptEditorAction::DiscardUnsaved => {
                if self.dirty {
                    self.confirm_discard = true;
                } else {
                    self.confirm_discard = false;
                }
            }
        }
        Ok(())
    }

    pub fn take_saved_version(&mut self) -> Option<(SavedPrompt, PromptVersion)> {
        self.pending_save.take()
    }

    pub fn bounded_preview(&self, max_chars: usize) -> String {
        self.body.chars().take(max_chars).collect()
    }
}
