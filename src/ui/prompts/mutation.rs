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
use crate::domain::query::QueryError;
use crate::host::IpcError;
use crate::kernel::{CommandBus, StoreError};
use crate::prompts::projection::{
    OwnerDeviceCapability, PromptLibraryQuery, PromptProjectionReply,
};

use super::{PromptLibraryLoadState, PromptLibrarySession};

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
    if !matches!(envelope.command, Command::PromptLibrary(_)) {
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
            session.load = if page.items().is_empty() {
                PromptLibraryLoadState::Empty
            } else {
                PromptLibraryLoadState::Ready
            };
            Ok(())
        }
        PromptProjectionReply::VersionPage(_)
        | PromptProjectionReply::DiffPage(_)
        | PromptProjectionReply::SearchPage(_)
        | PromptProjectionReply::ChainPage(_)
        | PromptProjectionReply::HistoryPage(_) => {
            session.load = PromptLibraryLoadState::Ready;
            Ok(())
        }
        PromptProjectionReply::MutationSettlement(_) => {
            Err(PromptMutationError::UnsupportedCommand)
        }
    }
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
