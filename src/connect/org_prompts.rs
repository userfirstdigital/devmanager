//! Connect projection cache adapter for published organization prompts.

pub use crate::prompts::{
    ComposerInsertion, OrganizationPromptProjection, OrganizationPromptSnapshot,
};

use crate::org::{HostMembership, OrgError, OrgPromptVersionId, SyncOutcome};

/// Typed Connect adapter for Portal-authoritative organization prompts.
/// Selection copies one exact immutable version and never sends or advances.
pub struct OrganizationPromptAdapter {
    projection: OrganizationPromptProjection,
}

impl OrganizationPromptAdapter {
    pub fn new() -> Self {
        Self {
            projection: OrganizationPromptProjection::new(),
        }
    }

    pub fn projection(&self) -> &OrganizationPromptProjection {
        &self.projection
    }

    pub fn sync_snapshot(
        &mut self,
        membership: &HostMembership,
        snapshot: OrganizationPromptSnapshot,
        now_ms: i64,
        entitlement_expires_at_ms: i64,
    ) -> Result<SyncOutcome, OrgError> {
        self.projection.apply_authoritative_snapshot(
            membership,
            snapshot,
            now_ms,
            entitlement_expires_at_ms,
        )
    }

    pub fn put_in_composer(
        &self,
        version_id: OrgPromptVersionId,
        now_ms: i64,
    ) -> Result<ComposerInsertion, OrgError> {
        let insertion = self.projection.put_in_composer(version_id, now_ms)?;
        if insertion.sent || insertion.advanced {
            return Err(OrgError::AutoLaunchForbidden);
        }
        Ok(insertion)
    }

    pub fn mutate_old_version(
        &mut self,
        version_id: OrgPromptVersionId,
        body: &str,
    ) -> Result<(), OrgError> {
        self.projection.mutate_old_version(version_id, body)
    }
}

impl Default for OrganizationPromptAdapter {
    fn default() -> Self {
        Self::new()
    }
}
