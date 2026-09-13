//! Connect adapter for EvidenceBundle intake into a reviewed TaskDraft
//! metadata projection. Raw transcript/media stay opt-in E2E and are not
//! delivered over a Portal transport from this adapter.

pub use crate::org::{
    EvidenceAccessClass, EvidenceBundle, EvidenceIntake, EvidenceMetadataProjection, TaskDraft,
};

use crate::org::{EvidenceSegment, OrgError, PortalTenantId};

/// Typed EvidenceBundle Connect adapter. The default projection is reviewed
/// TaskDraft metadata only; raw transcript/media stays opt-in E2E.
pub struct EvidenceAdapter {
    intake: EvidenceIntake,
}

impl EvidenceAdapter {
    pub fn new(trusted_signers: impl IntoIterator<Item = String>) -> Self {
        Self {
            intake: EvidenceIntake::new(trusted_signers),
        }
    }

    pub fn bind_tenant(&mut self, tenant_id: PortalTenantId) {
        self.intake.bind_tenant(tenant_id);
    }

    pub fn authorize_e2e_raw(&mut self, authorized: bool) {
        self.intake.authorize_e2e_raw(authorized);
    }

    pub fn ingest(
        &mut self,
        tenant_id: &PortalTenantId,
        bundle: &EvidenceBundle,
    ) -> Result<EvidenceMetadataProjection, OrgError> {
        let projection = self.intake.ingest_for_tenant(Some(tenant_id), bundle)?;
        if projection.raw_content_included {
            return Err(OrgError::ProhibitedField);
        }
        Ok(projection)
    }

    pub fn raw_segments<'a>(
        &self,
        access: EvidenceAccessClass,
        bundle: &'a EvidenceBundle,
    ) -> Result<&'a [EvidenceSegment], OrgError> {
        self.intake.raw_evidence(access, bundle)
    }
}
