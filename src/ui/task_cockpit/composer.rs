//! Client-local composer draft and exact prompt-version insertion.

use super::super::shell::PromptLibraryUiError;
use crate::domain::id::{AgentSessionId, PromptChainLinkId, PromptVersionId, TaskId};
use crate::prompts::model::PromptVersion;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerInsertionMode {
    ReplaceDraft,
    InsertAtCursor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutPromptVersionInComposer {
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub prompt_version_id: PromptVersionId,
    pub insertion: ComposerInsertionMode,
    pub chain_link_id: Option<PromptChainLinkId>,
    pub sends_provider_input: bool,
    pub advances_chain: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactPromptPayload {
    pub version_id: PromptVersionId,
    pub body: String,
    pub body_sha256: [u8; 32],
}

impl ExactPromptPayload {
    pub fn from_version(version: &PromptVersion) -> Self {
        Self {
            version_id: version.id,
            body: version.body.clone(),
            body_sha256: version.body_sha256,
        }
    }

    pub fn matches(&self, version_id: PromptVersionId) -> bool {
        if self.version_id != version_id {
            return false;
        }
        let digest: [u8; 32] = Sha256::digest(self.body.as_bytes()).into();
        digest == self.body_sha256
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftProvenance {
    pub prompt_version_id: PromptVersionId,
    pub chain_link_id: Option<PromptChainLinkId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerDraft {
    pub task_id: Option<TaskId>,
    pub agent_session_id: Option<AgentSessionId>,
    pub text: String,
    pub cursor: usize,
    pub provenance: Option<DraftProvenance>,
    pub sent: bool,
}

impl Default for ComposerDraft {
    fn default() -> Self {
        Self {
            task_id: None,
            agent_session_id: None,
            text: String::new(),
            cursor: 0,
            provenance: None,
            sent: false,
        }
    }
}

impl ComposerDraft {
    pub fn edit(&mut self, text: String, cursor: usize) {
        self.text = text;
        self.cursor = cursor.min(self.text.len());
        self.provenance = None;
        self.sent = false;
    }

    pub fn mark_sent(&mut self) {
        self.sent = true;
        self.provenance = None;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCommandSuggestion {
    pub label: String,
    pub command: String,
    pub provider_kind: String,
}

pub fn suggest_provider_commands<'a>(
    prefix: &str,
    catalog: &'a [ProviderCommandSuggestion],
) -> Vec<&'a ProviderCommandSuggestion> {
    let needle = prefix.trim();
    catalog
        .iter()
        .filter(|suggestion| needle.is_empty() || suggestion.command.starts_with(needle))
        .collect()
}

pub fn apply_put_prompt_version(
    draft: &mut ComposerDraft,
    action: &PutPromptVersionInComposer,
    payload: &ExactPromptPayload,
) -> Result<(), PromptLibraryUiError> {
    if action.sends_provider_input || action.advances_chain {
        return Err(PromptLibraryUiError::PayloadMismatch);
    }
    if !payload.matches(action.prompt_version_id) {
        return Err(PromptLibraryUiError::PayloadMismatch);
    }
    match action.insertion {
        ComposerInsertionMode::ReplaceDraft => {
            draft.text = payload.body.clone();
            draft.cursor = draft.text.len();
        }
        ComposerInsertionMode::InsertAtCursor => {
            let cursor = draft.cursor.min(draft.text.len());
            draft.text.insert_str(cursor, &payload.body);
            draft.cursor = cursor.saturating_add(payload.body.len());
        }
    }
    draft.task_id = Some(action.task_id);
    draft.agent_session_id = Some(action.agent_session_id);
    draft.provenance = Some(DraftProvenance {
        prompt_version_id: action.prompt_version_id,
        chain_link_id: action.chain_link_id,
    });
    draft.sent = false;
    Ok(())
}
