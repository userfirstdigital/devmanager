//! Linear numbered chain editor with insert-between and manual suggested next.

use super::super::shell::PromptLibraryUiError;
use crate::domain::id::{PromptChainId, PromptChainLinkId, PromptId, PromptVersionId};
use crate::prompts::model::PromptChain;
use crate::prompts::projection::PromptChainLinkRecord;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainLinkView {
    pub link: PromptChainLinkRecord,
    pub title: String,
    pub numbered_position: u32,
    pub connector_visible: bool,
    pub put_in_composer_label: &'static str,
    pub insert_here_label: &'static str,
    pub update_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainGap {
    pub after_link_id: Option<PromptChainLinkId>,
    pub before_link_id: Option<PromptChainLinkId>,
    pub label: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualSuggestedNext {
    pub link_id: PromptChainLinkId,
    pub prompt_id: PromptId,
    pub prompt_version_id: PromptVersionId,
    pub title: String,
    pub automatic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainEditorProjection {
    pub chain: PromptChain,
    pub links: Vec<ChainLinkView>,
    pub gaps: Vec<ChainGap>,
    pub total_links: usize,
    pub suggested_next: Option<ManualSuggestedNext>,
    pub auto_advance: bool,
    pub has_run_button: bool,
    pub has_graph_canvas: bool,
}

pub mod apply_chain_action {
    use super::*;

    pub fn insert_between(
        links: &mut Vec<PromptChainLinkRecord>,
        chain_id: PromptChainId,
        after_link_id: PromptChainLinkId,
        before_link_id: PromptChainLinkId,
        incoming: PromptChainLinkRecord,
    ) -> Result<(), PromptLibraryUiError> {
        if incoming.chain_id() != chain_id {
            return Err(PromptLibraryUiError::NotFound);
        }
        let after = links
            .iter()
            .find(|link| link.id() == after_link_id && link.chain_id() == chain_id)
            .ok_or(PromptLibraryUiError::NotFound)?;
        if after.next_link_id() != Some(before_link_id) {
            return Err(PromptLibraryUiError::AdjacentLinksRequired);
        }
        let before = links
            .iter()
            .find(|link| link.id() == before_link_id && link.chain_id() == chain_id)
            .ok_or(PromptLibraryUiError::NotFound)?;
        if before.previous_link_id() != Some(after_link_id) {
            return Err(PromptLibraryUiError::AdjacentLinksRequired);
        }
        let insert_at = before.position();
        let rebuilt = rebuild(
            links,
            chain_id,
            |existing| existing.id() != incoming.id(),
            Some((insert_at, incoming)),
        )?;
        *links = rebuilt;
        Ok(())
    }

    pub fn reorder(
        links: &mut Vec<PromptChainLinkRecord>,
        chain_id: PromptChainId,
        link_id: PromptChainLinkId,
        before_link_id: Option<PromptChainLinkId>,
    ) -> Result<(), PromptLibraryUiError> {
        let moving = links
            .iter()
            .find(|link| link.id() == link_id && link.chain_id() == chain_id)
            .cloned()
            .ok_or(PromptLibraryUiError::NotFound)?;
        let insert_at = match before_link_id {
            Some(before) => links
                .iter()
                .find(|link| link.id() == before && link.chain_id() == chain_id)
                .map(|link| link.position())
                .ok_or(PromptLibraryUiError::NotFound)?,
            None => links
                .iter()
                .filter(|link| link.chain_id() == chain_id && link.id() != link_id)
                .map(|link| link.position())
                .max()
                .unwrap_or(0)
                .saturating_add(1),
        };
        let rebuilt = rebuild(
            links,
            chain_id,
            |existing| existing.id() != link_id,
            Some((insert_at, moving)),
        )?;
        *links = rebuilt;
        Ok(())
    }

    pub fn remove(
        links: &mut Vec<PromptChainLinkRecord>,
        chain_id: PromptChainId,
        link_id: PromptChainLinkId,
    ) -> Result<(), PromptLibraryUiError> {
        if !links
            .iter()
            .any(|link| link.id() == link_id && link.chain_id() == chain_id)
        {
            return Err(PromptLibraryUiError::NotFound);
        }
        let rebuilt = rebuild(links, chain_id, |existing| existing.id() != link_id, None)?;
        *links = rebuilt;
        Ok(())
    }

    pub fn update_pinned(
        links: &mut Vec<PromptChainLinkRecord>,
        chain_id: PromptChainId,
        link_id: PromptChainLinkId,
        current_version_id: PromptVersionId,
    ) -> Result<(), PromptLibraryUiError> {
        let current = links
            .iter()
            .find(|link| link.id() == link_id && link.chain_id() == chain_id)
            .cloned()
            .ok_or(PromptLibraryUiError::NotFound)?;
        let updated = PromptChainLinkRecord::try_new(
            current.id(),
            current.chain_id(),
            current.position(),
            current.prompt_id(),
            current_version_id,
            current.previous_link_id(),
            current.next_link_id(),
            false,
        )
        .map_err(|_| PromptLibraryUiError::NotFound)?;
        let rebuilt = rebuild(
            links,
            chain_id,
            |existing| existing.id() != link_id,
            Some((current.position(), updated)),
        )?;
        *links = rebuilt;
        Ok(())
    }

    fn rebuild(
        links: &[PromptChainLinkRecord],
        chain_id: PromptChainId,
        keep: impl Fn(&PromptChainLinkRecord) -> bool,
        insert: Option<(u32, PromptChainLinkRecord)>,
    ) -> Result<Vec<PromptChainLinkRecord>, PromptLibraryUiError> {
        let mut others: Vec<PromptChainLinkRecord> = links
            .iter()
            .filter(|link| link.chain_id() != chain_id)
            .cloned()
            .collect();
        let mut owned: Vec<PromptChainLinkRecord> = links
            .iter()
            .filter(|link| link.chain_id() == chain_id && keep(link))
            .cloned()
            .collect();
        owned.sort_by_key(|link| link.position());
        if let Some((position, incoming)) = insert {
            let index = owned
                .iter()
                .position(|link| link.position() >= position)
                .unwrap_or(owned.len());
            owned.insert(index, incoming);
        }
        let mut rebuilt = Vec::with_capacity(owned.len());
        for (index, link) in owned.iter().enumerate() {
            let position =
                u32::try_from(index + 1).map_err(|_| PromptLibraryUiError::CapExceeded)?;
            let previous = index
                .checked_sub(1)
                .and_then(|prev| owned.get(prev))
                .map(|prev| prev.id());
            let next = owned.get(index + 1).map(|next| next.id());
            rebuilt.push(
                PromptChainLinkRecord::try_new(
                    link.id(),
                    chain_id,
                    position,
                    link.prompt_id(),
                    link.prompt_version_id(),
                    previous,
                    next,
                    link.update_available(),
                )
                .map_err(|_| PromptLibraryUiError::NotFound)?,
            );
        }
        others.extend(rebuilt);
        Ok(others)
    }
}
