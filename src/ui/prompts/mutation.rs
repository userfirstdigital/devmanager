//! Owner-gated prompt mutation executor.
//!
//! Durable writes travel the existing owner-granted host / command-bus path
//! ([`CommandBus::execute_with_owner_grant`] or [`HostClient::execute_command`]
//! after a local grant check). Read-only and ungranted callers fail closed
//! before a mutation envelope is sent.
//!
//! Chain positions remain a dense 0-based prefix. This module does not
//! implement or remap chain indexing.

use std::fmt;

use crate::client::HostClient;
use crate::domain::command::{Command, CommandEnvelope, CommandReceipt};
use crate::domain::id::{PromptId, PromptVersionId};
use crate::domain::query::QueryError;
use crate::host::IpcError;
use crate::kernel::{CommandBus, StoreError};
use crate::prompts::model::{
    ArchivePrompt, InsertPromptChainLink, MovePromptChainLink, PromptChain, PromptChainCommand,
    PromptCommand, PromptVersion, RemovePromptChainLink, RestorePrompt, SavedPrompt,
    UpdatePromptChainLinkVersion,
};
use crate::prompts::projection::{
    OwnerDeviceCapability, PromptChainLinkRecord, PromptLibraryQuery, PromptProjectionReply,
};

use super::{PromptLibraryAction, PromptLibraryLoadState, PromptLibrarySession};

/// Chain link positions stay `0..n-1`. Callers must not treat this as an
/// indexing implementation.
pub const PROMPT_CHAIN_INDEX_BASE: u32 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMutationAuthority<'a> {
    OwnerGranted(&'a OwnerDeviceCapability),
    ReadOnly,
    Ungranted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMutationError {
    Ungranted,
    ReadOnly,
    UnsupportedCommand,
    Host,
    Query,
    Store,
}

impl fmt::Display for PromptMutationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ungranted => write!(f, "prompt mutation requires an owner grant"),
            Self::ReadOnly => write!(f, "prompt mutation is unavailable to read-only callers"),
            Self::UnsupportedCommand => {
                write!(f, "prompt mutation requires a PromptLibrary command")
            }
            Self::Host => write!(f, "prompt mutation host error"),
            Self::Query => write!(f, "prompt library query error"),
            Self::Store => write!(f, "prompt mutation store error"),
        }
    }
}

impl std::error::Error for PromptMutationError {}

pub struct PromptMutationExecutor;

impl PromptMutationExecutor {
    pub fn execute(
        bus: &mut CommandBus,
        authority: PromptMutationAuthority<'_>,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, PromptMutationError> {
        let grant = require_prompt_mutation_grant(authority, &envelope)?;
        bus.execute_with_owner_grant(grant, envelope)
            .map_err(map_store_error)
    }
}

pub fn require_prompt_mutation_grant<'a>(
    authority: PromptMutationAuthority<'a>,
    envelope: &CommandEnvelope,
) -> Result<&'a OwnerDeviceCapability, PromptMutationError> {
    let grant = match authority {
        PromptMutationAuthority::OwnerGranted(grant) => grant,
        PromptMutationAuthority::ReadOnly => return Err(PromptMutationError::ReadOnly),
        PromptMutationAuthority::Ungranted => return Err(PromptMutationError::Ungranted),
    };
    if !matches!(
        envelope.command,
        Command::PromptLibrary(_) | Command::PromptChain(_)
    ) {
        return Err(PromptMutationError::UnsupportedCommand);
    }
    if !grant.binds_client(envelope.client_id) {
        return Err(PromptMutationError::Ungranted);
    }
    let _ = PROMPT_CHAIN_INDEX_BASE;
    Ok(grant)
}

/// Query the active personal prompt library through the existing HostClient
/// protocol seam. This is a read; it does not mint a mutation receipt.
pub async fn query_active_session(
    client: &mut HostClient,
    query: PromptLibraryQuery,
) -> Result<PromptProjectionReply, PromptMutationError> {
    match client.query_prompt_library(query).await {
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(error)) => Err(map_query_error(error)),
        Err(error) => Err(map_ipc_error(error)),
    }
}

/// Hydrate load/revision on the active [`PromptLibrarySession`] from a
/// HostClient query. Item bodies stay on the host projection; this does not
/// reconstruct saved rows from metadata-only fields.
pub async fn hydrate_active_session(
    client: &mut HostClient,
    session: &mut PromptLibrarySession,
    query: PromptLibraryQuery,
) -> Result<PromptProjectionReply, PromptMutationError> {
    session.load = PromptLibraryLoadState::Loading;
    match query_active_session(client, query).await {
        Ok(reply) => {
            apply_host_reply_to_session(session, &reply)?;
            Ok(reply)
        }
        Err(error) => {
            session.load = PromptLibraryLoadState::Error {
                message: error.to_string(),
            };
            Err(error)
        }
    }
}

/// Owner-gated PromptLibrary mutation through HostClient. Ungranted and
/// read-only callers fail closed before `execute_command`.
pub async fn mutate_active_session(
    client: &mut HostClient,
    authority: PromptMutationAuthority<'_>,
    envelope: CommandEnvelope,
) -> Result<CommandReceipt, PromptMutationError> {
    let _grant = require_prompt_mutation_grant(authority, &envelope)?;
    client
        .execute_command(envelope)
        .await
        .map_err(map_ipc_error)
}

pub fn apply_host_reply_to_session(
    session: &mut PromptLibrarySession,
    reply: &PromptProjectionReply,
) -> Result<(), PromptMutationError> {
    match reply {
        PromptProjectionReply::MetadataPage(page) => {
            session.library_revision = page.library_revision();
            session.saved = page
                .items()
                .iter()
                .filter_map(saved_prompt_from_metadata)
                .collect();
            session.load = if session.saved.is_empty() {
                PromptLibraryLoadState::Empty
            } else {
                PromptLibraryLoadState::Ready
            };
            Ok(())
        }
        PromptProjectionReply::VersionPage(page) => {
            if let Some(version) = prompt_version_from_page(page) {
                if !session
                    .versions
                    .iter()
                    .any(|existing| existing.id == version.id)
                {
                    session.versions.push(version);
                }
            }
            session.load = PromptLibraryLoadState::Ready;
            Ok(())
        }
        PromptProjectionReply::SearchPage(_) => {
            session.load = PromptLibraryLoadState::Ready;
            Ok(())
        }
        PromptProjectionReply::ChainPage(page) => {
            let (chains, links) = chains_from_page(page);
            session.chains = chains;
            session.links = links;
            session.refresh_suggested_next();
            session.load = if session.chains.is_empty() && session.links.is_empty() {
                PromptLibraryLoadState::Empty
            } else {
                PromptLibraryLoadState::Ready
            };
            Ok(())
        }
        PromptProjectionReply::DiffPage(_) | PromptProjectionReply::HistoryPage(_) => {
            session.load = PromptLibraryLoadState::Ready;
            Ok(())
        }
        PromptProjectionReply::MutationSettlement(settlement) => {
            if !settlement.settled() {
                session.load = PromptLibraryLoadState::Error {
                    message: "prompt mutation settlement was not verified".into(),
                };
                return Err(PromptMutationError::Store);
            }
            session.load = PromptLibraryLoadState::Ready;
            Ok(())
        }
    }
}

/// Map a UI mutation to the existing owner-granted prompt command envelope.
/// Chain edits reuse the durable [`PromptChainCommand`] model and carry the
/// chain revision fence captured by the UI at activation time.
pub fn prompt_mutation_command(
    action: &PromptLibraryAction,
) -> Result<Command, PromptMutationError> {
    match action {
        PromptLibraryAction::CreatePrompt { prompt, version } => Ok(Command::PromptLibrary(
            PromptCommand::CreatePrompt(crate::prompts::model::CreatePrompt {
                prompt_id: prompt.id,
                prompt_version_id: version.id,
                title: prompt.title.clone(),
                description: prompt.description.clone(),
                tags: prompt.tags.clone(),
                variables: version.variables.clone(),
                body: version.body.clone(),
                created_at_ms: version.created_at_ms,
            }),
        )),
        PromptLibraryAction::ArchivePrompt {
            prompt_id,
            expected_revision,
            archived_at_ms,
        } => Ok(Command::PromptLibrary(PromptCommand::ArchivePrompt(
            ArchivePrompt {
                prompt_id: *prompt_id,
                archived_at_ms: *archived_at_ms,
                expected_revision: *expected_revision,
            },
        ))),
        PromptLibraryAction::RestorePrompt {
            prompt_id,
            expected_revision,
        } => Ok(Command::PromptLibrary(PromptCommand::RestorePrompt(
            RestorePrompt {
                prompt_id: *prompt_id,
                expected_revision: *expected_revision,
            },
        ))),
        PromptLibraryAction::InsertChainLinkBetween {
            chain_id,
            before_link_id,
            link,
            expected_revision,
            ..
        } => Ok(Command::PromptChain(
            PromptChainCommand::InsertPromptChainLink(InsertPromptChainLink {
                chain_id: *chain_id,
                link_id: link.id(),
                prompt_id: link.prompt_id(),
                prompt_version_id: Some(link.prompt_version_id()),
                before_link_id: Some(*before_link_id),
                expected_revision: *expected_revision,
            }),
        )),
        PromptLibraryAction::ReorderChainLink {
            chain_id,
            link_id,
            before_link_id,
            expected_revision,
        } => Ok(Command::PromptChain(
            PromptChainCommand::MovePromptChainLink(MovePromptChainLink {
                chain_id: *chain_id,
                link_id: *link_id,
                before_link_id: *before_link_id,
                expected_revision: *expected_revision,
            }),
        )),
        PromptLibraryAction::RemoveChainLink {
            chain_id,
            link_id,
            expected_revision,
        } => Ok(Command::PromptChain(
            PromptChainCommand::RemovePromptChainLink(RemovePromptChainLink {
                chain_id: *chain_id,
                link_id: *link_id,
                expected_revision: *expected_revision,
            }),
        )),
        PromptLibraryAction::UpdateLinkToCurrent {
            chain_id,
            link_id,
            expected_revision,
            ..
        } => Ok(Command::PromptChain(
            PromptChainCommand::UpdatePromptChainLinkVersion(UpdatePromptChainLinkVersion {
                chain_id: *chain_id,
                link_id: *link_id,
                expected_revision: *expected_revision,
            }),
        )),
        _ => Err(PromptMutationError::UnsupportedCommand),
    }
}

#[derive(serde::Deserialize)]
struct MetadataItemView {
    id: PromptId,
    title: String,
    description: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    current_version_id: PromptVersionId,
    revision: u64,
    archived_at_ms: Option<i64>,
}

fn saved_prompt_from_metadata(
    item: &crate::prompts::projection::PromptMetadataItem,
) -> Option<SavedPrompt> {
    let value = serde_json::to_value(item).ok()?;
    let view: MetadataItemView = serde_json::from_value(value).ok()?;
    Some(SavedPrompt {
        id: view.id,
        title: view.title,
        description: view.description,
        tags: view.tags,
        current_version_id: view.current_version_id,
        revision: view.revision,
        archived_at_ms: view.archived_at_ms,
    })
}

fn prompt_version_from_page(
    page: &crate::prompts::projection::PromptVersionPage,
) -> Option<PromptVersion> {
    #[derive(serde::Deserialize)]
    struct VersionTimeView {
        created_at_ms: i64,
    }
    let created_at_ms = serde_json::to_value(page)
        .ok()
        .and_then(|value| serde_json::from_value::<VersionTimeView>(value).ok())
        .map(|view| view.created_at_ms)?;
    let body = String::from_utf8_lossy(page.chunk().bytes()).into_owned();
    PromptVersion::new(
        page.version_id(),
        page.prompt_id(),
        page.version(),
        body,
        created_at_ms,
    )
    .ok()
}

#[derive(serde::Deserialize)]
struct ChainRecordView {
    chain: PromptChain,
    #[serde(default)]
    links: Vec<PromptChainLinkRecord>,
}

fn chains_from_page(
    page: &crate::prompts::projection::PromptChainPage,
) -> (Vec<PromptChain>, Vec<PromptChainLinkRecord>) {
    let mut chains = Vec::new();
    let mut links = Vec::new();
    for record in page.chains() {
        let Ok(value) = serde_json::to_value(record) else {
            continue;
        };
        let Ok(view) = serde_json::from_value::<ChainRecordView>(value) else {
            continue;
        };
        chains.push(view.chain);
        links.extend(view.links);
    }
    (chains, links)
}

fn map_store_error(_error: StoreError) -> PromptMutationError {
    PromptMutationError::Store
}

fn map_ipc_error(error: IpcError) -> PromptMutationError {
    match error {
        IpcError::UnsupportedCapability | IpcError::Unauthorized => PromptMutationError::Ungranted,
        _ => PromptMutationError::Host,
    }
}

fn map_query_error(error: QueryError) -> PromptMutationError {
    match error {
        QueryError::UnsupportedCapability | QueryError::Unauthorized => {
            PromptMutationError::Ungranted
        }
        QueryError::Unavailable {
            reason: "owner_device_session",
        } => PromptMutationError::Ungranted,
        _ => PromptMutationError::Query,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::{PromptChainId, PromptChainLinkId, PromptId};
    use crate::ui::prompts::PromptLibraryAction;

    #[test]
    fn prompt_mutations_require_typed_host_commands_and_reject_local_chain_success() {
        let prompt_id = PromptId::new();
        let command = prompt_mutation_command(&PromptLibraryAction::ArchivePrompt {
            prompt_id,
            expected_revision: 2,
            archived_at_ms: 1_725_000_000_000,
        })
        .expect("archive is a PromptLibrary command");
        assert!(matches!(
            command,
            Command::PromptLibrary(PromptCommand::ArchivePrompt(_))
        ));

        let err = prompt_mutation_command(&PromptLibraryAction::RemoveChainLink {
            chain_id: PromptChainId::new(),
            link_id: PromptChainLinkId::new(),
            expected_revision: 1,
        });
        assert!(matches!(
            err,
            Ok(Command::PromptChain(
                PromptChainCommand::RemovePromptChainLink(_)
            ))
        ));
    }
}
