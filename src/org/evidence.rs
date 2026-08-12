//! DevAgent EvidenceBundle contract and local TaskDraft/metadata intake.
//! Default Connect projection is metadata-only; raw transcript text is accepted
//! only under an explicit E2E authorization. Portal/DevAgent export transport
//! is outside this host adapter and is not claimed here.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::org::TaskScope;
use crate::org::error::OrgError;
use crate::org::identity::PortalTenantId;
use crate::org::ids::{EvidenceBundleId, TaskDraftId};

pub const EVIDENCE_BUNDLE_VERSION: u16 = 1;
pub const MAX_EVIDENCE_SEGMENTS: usize = 32;
pub const MAX_EVIDENCE_MEDIA_REFS: usize = 32;
pub const MAX_EVIDENCE_CRITERIA: usize = 32;
pub const MAX_EVIDENCE_STEPS: usize = 32;
pub const MAX_EVIDENCE_LABELS: usize = 16;
pub const MAX_EVIDENCE_TEXT_BYTES: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceBundle {
    pub manifest_version: u16,
    pub bundle_id: EvidenceBundleId,
    pub capture_started_at_ms: i64,
    pub capture_ended_at_ms: i64,
    pub timezone: String,
    pub source_device: String,
    pub source_user: String,
    pub transcript_segments: Vec<EvidenceSegment>,
    pub media_refs: Vec<EvidenceMediaRef>,
    pub proposed_title: String,
    pub proposed_summary: String,
    pub acceptance_criteria: Vec<String>,
    pub steps: Vec<String>,
    pub privacy_labels: Vec<String>,
    pub redactions: Vec<String>,
    pub content_hash_hex: String,
    pub signature_hex: String,
    pub signer: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceSegment {
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    pub redacted: bool,
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceMediaRef {
    pub label: String,
    pub digest_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskDraft {
    pub draft_id: TaskDraftId,
    pub bundle_id: EvidenceBundleId,
    pub title: String,
    pub summary: String,
    pub acceptance_criteria: Vec<String>,
    pub selected_evidence: Vec<String>,
    pub scope: TaskScope,
    pub reviewed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceAccessClass {
    MetadataOnly,
    AuthorizedE2ERaw,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceMetadataProjection {
    pub draft: TaskDraft,
    pub capture_started_at_ms: i64,
    pub capture_ended_at_ms: i64,
    pub timezone: String,
    pub privacy_labels: Vec<String>,
    pub redactions: Vec<String>,
    pub content_hash_hex: String,
    pub signer: String,
    pub raw_content_included: bool,
}

#[derive(Debug, Default)]
pub struct EvidenceIntake {
    imported: std::collections::BTreeSet<String>,
    trusted_signers: std::collections::BTreeSet<String>,
    tenant_id: Option<PortalTenantId>,
    e2e_raw_authorized: bool,
}

impl EvidenceIntake {
    pub fn new(trusted_signers: impl IntoIterator<Item = String>) -> Self {
        Self {
            imported: std::collections::BTreeSet::new(),
            trusted_signers: trusted_signers.into_iter().collect(),
            tenant_id: None,
            e2e_raw_authorized: false,
        }
    }

    pub fn bind_tenant(&mut self, tenant_id: PortalTenantId) {
        self.tenant_id = Some(tenant_id);
    }

    pub fn authorize_e2e_raw(&mut self, authorized: bool) {
        self.e2e_raw_authorized = authorized;
    }

    pub fn trust_signer(&mut self, signer: impl Into<String>) -> Result<(), OrgError> {
        let signer = signer.into();
        if signer.trim().is_empty() {
            return Err(OrgError::EmptyIdentity);
        }
        if self.trusted_signers.len() >= MAX_EVIDENCE_LABELS
            && !self.trusted_signers.contains(&signer)
        {
            return Err(OrgError::BoundExceeded);
        }
        self.trusted_signers.insert(signer);
        Ok(())
    }

    pub fn validate(&self, bundle: &EvidenceBundle) -> Result<String, OrgError> {
        if bundle.manifest_version != EVIDENCE_BUNDLE_VERSION {
            return Err(OrgError::StalePolicy);
        }
        if bundle.capture_ended_at_ms < bundle.capture_started_at_ms {
            return Err(OrgError::BoundExceeded);
        }
        if bundle.timezone.trim().is_empty()
            || bundle.source_device.trim().is_empty()
            || bundle.source_user.trim().is_empty()
            || bundle.signer.trim().is_empty()
        {
            return Err(OrgError::EmptyIdentity);
        }
        if bundle.transcript_segments.len() > MAX_EVIDENCE_SEGMENTS
            || bundle.media_refs.len() > MAX_EVIDENCE_MEDIA_REFS
            || bundle.acceptance_criteria.len() > MAX_EVIDENCE_CRITERIA
            || bundle.steps.len() > MAX_EVIDENCE_STEPS
            || bundle.privacy_labels.len() > MAX_EVIDENCE_LABELS
            || bundle.redactions.len() > MAX_EVIDENCE_LABELS
            || bundle.proposed_title.len() > MAX_EVIDENCE_TEXT_BYTES
            || bundle.proposed_summary.len() > MAX_EVIDENCE_TEXT_BYTES
        {
            return Err(OrgError::BoundExceeded);
        }
        for item in bundle
            .acceptance_criteria
            .iter()
            .chain(bundle.steps.iter())
            .chain(bundle.privacy_labels.iter())
            .chain(bundle.redactions.iter())
        {
            if item.len() > MAX_EVIDENCE_TEXT_BYTES {
                return Err(OrgError::BoundExceeded);
            }
        }
        for (index, segment) in bundle.transcript_segments.iter().enumerate() {
            if segment.ended_at_ms < segment.started_at_ms
                || segment.started_at_ms < bundle.capture_started_at_ms
                || segment.ended_at_ms > bundle.capture_ended_at_ms
            {
                return Err(OrgError::BoundExceeded);
            }
            if index > 0
                && segment.started_at_ms < bundle.transcript_segments[index - 1].started_at_ms
            {
                return Err(OrgError::BoundExceeded);
            }
            match &segment.text {
                Some(text) if text.len() > MAX_EVIDENCE_TEXT_BYTES => {
                    return Err(OrgError::BoundExceeded);
                }
                Some(_) if !self.e2e_raw_authorized && !segment.redacted => {
                    return Err(OrgError::ProhibitedField);
                }
                _ => {}
            }
        }
        for media in &bundle.media_refs {
            if media.label.trim().is_empty()
                || media.digest_hex.trim().is_empty()
                || media.label.len() > MAX_EVIDENCE_TEXT_BYTES
            {
                return Err(OrgError::UntrustedSigner);
            }
            if media.digest_hex.len() != 64
                || !media
                    .digest_hex
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(OrgError::TamperedEvidence);
            }
        }
        let expected = compute_bundle_hash(bundle);
        if expected != bundle.content_hash_hex {
            return Err(OrgError::TamperedEvidence);
        }
        if !self.trusted_signers.contains(&bundle.signer) {
            return Err(OrgError::UntrustedSigner);
        }
        if bundle.signature_hex != expected {
            return Err(OrgError::TamperedEvidence);
        }
        Ok(expected)
    }

    pub fn ingest(&mut self, bundle: &EvidenceBundle) -> Result<TaskDraft, OrgError> {
        self.ingest_for_tenant(None, bundle)
            .map(|projection| projection.draft)
    }

    pub fn ingest_for_tenant(
        &mut self,
        tenant_id: Option<&PortalTenantId>,
        bundle: &EvidenceBundle,
    ) -> Result<EvidenceMetadataProjection, OrgError> {
        self.validate(bundle)?;
        if let (Some(bound), Some(expected)) = (self.tenant_id.as_ref(), tenant_id) {
            if bound != expected {
                return Err(OrgError::CrossTenant);
            }
        } else if self.tenant_id.is_some() && tenant_id.is_none() {
            return Err(OrgError::CrossTenant);
        }
        if !self.imported.insert(bundle.bundle_id.to_string()) {
            return Err(OrgError::Replay);
        }
        let draft = TaskDraft {
            draft_id: TaskDraftId::new(),
            bundle_id: bundle.bundle_id,
            title: bundle.proposed_title.clone(),
            summary: bundle.proposed_summary.clone(),
            acceptance_criteria: bundle.acceptance_criteria.clone(),
            selected_evidence: bundle
                .media_refs
                .iter()
                .map(|media| media.digest_hex.clone())
                .collect(),
            scope: TaskScope::personal(),
            reviewed: false,
        };
        Ok(self.project_metadata(bundle, draft))
    }

    pub fn project_metadata(
        &self,
        bundle: &EvidenceBundle,
        draft: TaskDraft,
    ) -> EvidenceMetadataProjection {
        EvidenceMetadataProjection {
            draft,
            capture_started_at_ms: bundle.capture_started_at_ms,
            capture_ended_at_ms: bundle.capture_ended_at_ms,
            timezone: bundle.timezone.clone(),
            privacy_labels: bundle.privacy_labels.clone(),
            redactions: bundle.redactions.clone(),
            content_hash_hex: bundle.content_hash_hex.clone(),
            signer: bundle.signer.clone(),
            raw_content_included: false,
        }
    }

    pub fn raw_evidence<'a>(
        &self,
        access: EvidenceAccessClass,
        bundle: &'a EvidenceBundle,
    ) -> Result<&'a [EvidenceSegment], OrgError> {
        if !matches!(access, EvidenceAccessClass::AuthorizedE2ERaw) || !self.e2e_raw_authorized {
            return Err(OrgError::ProhibitedField);
        }
        self.validate(bundle)?;
        Ok(&bundle.transcript_segments)
    }

    pub fn create_task(&self, draft: &TaskDraft) -> Result<(), OrgError> {
        if !draft.reviewed {
            return Err(OrgError::ReviewRequired);
        }
        Ok(())
    }
}

pub fn compute_bundle_hash(bundle: &EvidenceBundle) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"evidence.v1");
    hasher.update(bundle.manifest_version.to_le_bytes());
    hash_len_prefixed(&mut hasher, bundle.bundle_id.to_string().as_bytes());
    hasher.update(bundle.capture_started_at_ms.to_le_bytes());
    hasher.update(bundle.capture_ended_at_ms.to_le_bytes());
    hash_len_prefixed(&mut hasher, bundle.timezone.as_bytes());
    hash_len_prefixed(&mut hasher, bundle.source_device.as_bytes());
    hash_len_prefixed(&mut hasher, bundle.source_user.as_bytes());
    hasher.update((bundle.transcript_segments.len() as u64).to_le_bytes());
    for segment in &bundle.transcript_segments {
        hasher.update(segment.started_at_ms.to_le_bytes());
        hasher.update(segment.ended_at_ms.to_le_bytes());
        hasher.update([u8::from(segment.redacted)]);
        match &segment.text {
            Some(text) => {
                hasher.update([1]);
                hash_len_prefixed(&mut hasher, text.as_bytes());
            }
            None => hasher.update([0]),
        }
    }
    hasher.update((bundle.media_refs.len() as u64).to_le_bytes());
    for media in &bundle.media_refs {
        hash_len_prefixed(&mut hasher, media.label.as_bytes());
        hash_len_prefixed(&mut hasher, media.digest_hex.as_bytes());
    }
    hash_len_prefixed(&mut hasher, bundle.proposed_title.as_bytes());
    hash_len_prefixed(&mut hasher, bundle.proposed_summary.as_bytes());
    hash_string_list(&mut hasher, &bundle.acceptance_criteria);
    hash_string_list(&mut hasher, &bundle.steps);
    hash_string_list(&mut hasher, &bundle.privacy_labels);
    hash_string_list(&mut hasher, &bundle.redactions);
    hash_len_prefixed(&mut hasher, bundle.signer.as_bytes());
    hex_encode(&hasher.finalize())
}

fn hash_string_list(hasher: &mut Sha256, items: &[String]) {
    hasher.update((items.len() as u64).to_le_bytes());
    for item in items {
        hash_len_prefixed(hasher, item.as_bytes());
    }
}

fn hash_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
