//! Metadata-only evidence references for the organization bridge.
//!
//! Raw transcript/media content stays in the local DevAgent boundary.  This
//! module carries only a bounded, redacted reference that can be attached to a
//! task without making Connect a transcript transport.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::id::TaskId;
use crate::org::{EvidenceBundleId, OrgError};

pub const MAX_EVIDENCE_REFERENCES: usize = 128;
pub const MAX_EVIDENCE_HASH_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceReference {
    pub bundle_id: EvidenceBundleId,
    pub task_id: TaskId,
    pub content_hash: String,
    pub segment_count: u16,
    pub media_count: u16,
    pub redacted: bool,
}

#[derive(Debug, Default)]
pub struct EvidenceReferenceStore {
    references: BTreeMap<EvidenceBundleId, EvidenceReference>,
}

impl EvidenceReferenceStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, reference: EvidenceReference) -> Result<EvidenceReference, OrgError> {
        if !reference.redacted
            || reference.content_hash.len() != MAX_EVIDENCE_HASH_BYTES
            || !reference
                .content_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(OrgError::ProhibitedField);
        }
        if self.references.len() >= MAX_EVIDENCE_REFERENCES
            && !self.references.contains_key(&reference.bundle_id)
        {
            return Err(OrgError::BoundExceeded);
        }
        if let Some(existing) = self.references.get(&reference.bundle_id) {
            if existing != &reference {
                return Err(OrgError::LastWriteWinsForbidden);
            }
            return Ok(existing.clone());
        }
        self.references
            .insert(reference.bundle_id, reference.clone());
        Ok(reference)
    }

    pub fn get(&self, bundle_id: EvidenceBundleId) -> Option<&EvidenceReference> {
        self.references.get(&bundle_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_reference_never_accepts_unredacted_content() {
        let mut store = EvidenceReferenceStore::new();
        let reference = EvidenceReference {
            bundle_id: EvidenceBundleId::new(),
            task_id: TaskId::new(),
            content_hash: "b".repeat(64),
            segment_count: 2,
            media_count: 1,
            redacted: false,
        };
        assert_eq!(store.record(reference), Err(OrgError::ProhibitedField));
    }

    #[test]
    fn identical_evidence_reference_is_idempotent() {
        let mut store = EvidenceReferenceStore::new();
        let reference = EvidenceReference {
            bundle_id: EvidenceBundleId::new(),
            task_id: TaskId::new(),
            content_hash: "c".repeat(64),
            segment_count: 1,
            media_count: 0,
            redacted: true,
        };
        assert_eq!(store.record(reference.clone()), Ok(reference.clone()));
        assert_eq!(store.record(reference.clone()), Ok(reference));
    }
}
