//! Client-local composer draft wrappers for exact prompt-version insertion.

pub use crate::prompts::ui::composer::{
    apply_put_prompt_version, suggest_provider_commands, ComposerDraft, ComposerInsertionMode,
    DraftProvenance, ExactPromptPayload, ProviderCommandSuggestion, PutPromptVersionInComposer,
};

use crate::domain::id::{AgentSessionId, PromptChainLinkId, PromptVersionId, TaskId};

/// Build the pure client action. It never enters the host action catalog.
pub fn put_prompt_version_in_composer(
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    prompt_version_id: PromptVersionId,
    insertion: ComposerInsertionMode,
    chain_link_id: Option<PromptChainLinkId>,
) -> PutPromptVersionInComposer {
    PutPromptVersionInComposer {
        task_id,
        agent_session_id,
        prompt_version_id,
        insertion,
        chain_link_id,
        sends_provider_input: false,
        advances_chain: false,
    }
}
