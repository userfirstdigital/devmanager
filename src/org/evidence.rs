//! DevAgent EvidenceBundle contract and local TaskDraft intake. Portal and
//! DevAgent export remain external HOLDs.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::org::TaskScope;
use crate::org::error::OrgError;
use crate::org::ids::{EvidenceBundleId, TaskDraftId};

pub const EVIDENCE_BUNDLE_VERSION: u16 = 1;

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

pub struct EvidenceIntake {
    imported: std::collections::BTreeSet<String>,
    trusted_signers: std::collections::BTreeSet<String>,
}

impl EvidenceIntake {
    pub fn new(trusted_signers: impl IntoIterator<Item = String>) -> Self {
        Self {
            imported: std::collections::BTreeSet::new(),
            trusted_signers: trusted_signers.into_iter().collect(),
        }
    }

    pub fn validate(&self, bundle: &EvidenceBundle) -> Result<String, OrgError> {
        if bundle.manifest_version != EVIDENCE_BUNDLE_VERSION {
            return Err(OrgError::StalePolicy);
        }
        if bundle.capture_ended_at_ms < bundle.capture_started_at_ms {
            return Err(OrgError::BoundExceeded);
        }
        if bundle.timezone.trim().is_empty() {
            return Err(OrgError::EmptyIdentity);
        }
        for segment in &bundle.transcript_segments {
            if !segment.redacted && segment.text.is_some() {
                return Err(OrgError::ProhibitedField);
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
        self.validate(bundle)?;
        if !self.imported.insert(bundle.bundle_id.to_string()) {
            return Err(OrgError::Replay);
        }
        Ok(TaskDraft {
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
        })
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
    hasher.update(bundle.manifest_version.to_le_bytes());
    hasher.update(bundle.bundle_id.to_string().as_bytes());
    hasher.update(bundle.capture_started_at_ms.to_le_bytes());
    hasher.update(bundle.capture_ended_at_ms.to_le_bytes());
    hasher.update(bundle.timezone.as_bytes());
    hasher.update(bundle.source_device.as_bytes());
    hasher.update(bundle.source_user.as_bytes());
    hasher.update(bundle.proposed_title.as_bytes());
    hasher.update(bundle.proposed_summary.as_bytes());
    for media in &bundle.media_refs {
        hasher.update(media.digest_hex.as_bytes());
    }
    hex_encode(&hasher.finalize())
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
