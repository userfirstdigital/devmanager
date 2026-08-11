use crate::domain::id::{CommandId, PromptChainId, PromptChainLinkId, PromptVersionId};

use super::model::{
    PromptChain, PromptChainCommand, PromptChainLink, PromptChainLinkContext,
    PromptChainMutationReceipt, PromptVersion,
};
use super::store::{PromptStore, PromptStoreError};

/// The manual prompt-chain application/query seam.
///
/// This service deliberately exposes only mutations represented by
/// [`PromptChainCommand`] and read-only sequence projections. There is no
/// execution, cursor, scheduler, completion, or automatic-advance state.
pub struct PromptChainService<'store> {
    store: &'store mut PromptStore,
}

impl<'store> PromptChainService<'store> {
    pub fn new(store: &'store mut PromptStore) -> Self {
        Self { store }
    }

    pub fn apply(
        &mut self,
        command_id: CommandId,
        command: PromptChainCommand,
    ) -> Result<PromptChainMutationReceipt, PromptStoreError> {
        self.store.execute_chain(command_id, command)
    }

    pub fn chain(&self, chain_id: PromptChainId) -> Result<Option<PromptChain>, PromptStoreError> {
        self.store.get_chain(chain_id)
    }

    pub fn links(&self, chain_id: PromptChainId) -> Result<Vec<PromptChainLink>, PromptStoreError> {
        self.store.list_chain_links(chain_id)
    }

    pub fn link_context(
        &self,
        chain_id: PromptChainId,
        link_id: PromptChainLinkId,
    ) -> Result<Option<PromptChainLinkContext>, PromptStoreError> {
        self.store.get_chain_link_context(chain_id, link_id)
    }

    pub fn version(
        &self,
        version_id: PromptVersionId,
    ) -> Result<Option<PromptVersion>, PromptStoreError> {
        self.store.get_version(version_id)
    }
}
