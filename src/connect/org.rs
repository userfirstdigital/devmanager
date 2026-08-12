//! Connect adapter for the organization projection. Standalone hosts expose
//! an empty overlay and never advertise organization capability.

pub use crate::org::{
    OperatingMode, OrgDependency, OrgError, OrganizationFact, OrganizationProjection,
    OrganizationSyncState, StandaloneOrganization, SyncOutcome,
};

use crate::org::{
    EvidenceAccessClass, EvidenceBundle, EvidenceMetadataProjection, EvidenceSegment,
    LocalActionAdmissionState, LocalActionCatalogEntry, LocalActionId, LocalActionRequest,
};
use crate::prompts::OrganizationPromptSnapshot;
use crate::protocol::CapabilitySet;

use crate::domain::id::ProjectId;

/// Typed Connect-facing organization adapter. The native host remains
/// authoritative; Portal facts are applied only after identity and revision
/// checks. Sign-in never enrolls a host or Task.
#[derive(Debug, Default)]
pub struct OrganizationAdapter {
    projection: OrganizationProjection,
}

impl OrganizationAdapter {
    pub fn standalone() -> Self {
        Self {
            projection: OrganizationProjection::standalone(),
        }
    }

    pub fn projection(&self) -> &OrganizationProjection {
        &self.projection
    }

    pub fn projection_mut(&mut self) -> &mut OrganizationProjection {
        &mut self.projection
    }

    pub fn advertised_capabilities(&self, base: CapabilitySet) -> CapabilitySet {
        advertised_capabilities(self.projection.mode(), base)
    }

    pub fn apply_authoritative_fact(
        &mut self,
        fact: OrganizationFact,
        now_ms: i64,
    ) -> Result<SyncOutcome, OrgError> {
        self.projection.apply_authoritative_fact(fact, now_ms)
    }

    pub fn apply_prompt_snapshot(
        &mut self,
        snapshot: OrganizationPromptSnapshot,
        now_ms: i64,
        entitlement_expires_at_ms: i64,
    ) -> Result<SyncOutcome, OrgError> {
        self.projection
            .apply_prompt_snapshot(snapshot, now_ms, entitlement_expires_at_ms)
    }

    pub fn bind_local_action_catalog(
        &mut self,
        entries: Vec<LocalActionCatalogEntry>,
    ) -> Result<(), OrgError> {
        self.projection.bind_local_action_catalog(entries)
    }

    pub fn admit_local_action(
        &mut self,
        request: &LocalActionRequest,
        local_host_id: &str,
        local_project: ProjectId,
        local_fingerprint: &str,
        owner_approved: bool,
        now_ms: i64,
    ) -> Result<LocalActionAdmissionState, OrgError> {
        self.projection.admit_local_action(
            request,
            local_host_id,
            local_project,
            local_fingerprint,
            owner_approved,
            now_ms,
        )
    }

    pub fn ingest_evidence(
        &mut self,
        bundle: &EvidenceBundle,
    ) -> Result<EvidenceMetadataProjection, OrgError> {
        self.projection.ingest_evidence(bundle)
    }

    pub fn evidence_raw_segments<'a>(
        &self,
        access: EvidenceAccessClass,
        bundle: &'a EvidenceBundle,
    ) -> Result<&'a [EvidenceSegment], OrgError> {
        self.projection.evidence_raw_segments(access, bundle)
    }

    pub fn local_action_state(
        &self,
        request_id: LocalActionId,
    ) -> Result<Option<&LocalActionAdmissionState>, OrgError> {
        self.projection.local_action_state(request_id)
    }
}

pub fn advertised_capabilities(mode: &OperatingMode, base: CapabilitySet) -> CapabilitySet {
    match mode.organization_capability() {
        Some(capability) => CapabilitySet::from_bits(base.bits() | capability.bit()),
        None => base,
    }
}
