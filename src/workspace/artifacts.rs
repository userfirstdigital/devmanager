//! Task artifact promotion for host-owned browser downloads.

use crate::browser::{BrowserIoError, BrowserStagedDownload};
use crate::domain::artifact::{ArtifactContentRef, ArtifactFacts, ArtifactKind, PrivacyClass};
use crate::domain::id::TaskId;

pub fn promote_browser_download(
    task_id: TaskId,
    staged: &BrowserStagedDownload,
) -> Result<ArtifactFacts, BrowserIoError> {
    if staged.file_name.is_empty() || staged.sha256_hex.len() != 64 {
        return Err(BrowserIoError::InvalidRequest);
    }
    let mut sha256 = [0u8; 32];
    for (index, chunk) in staged.sha256_hex.as_bytes().chunks(2).enumerate() {
        let hex = std::str::from_utf8(chunk).map_err(|_| BrowserIoError::InvalidRequest)?;
        sha256[index] = u8::from_str_radix(hex, 16).map_err(|_| BrowserIoError::InvalidRequest)?;
    }
    let content = ArtifactContentRef::content_addressed(staged.sha256_hex.clone())
        .map_err(|_| BrowserIoError::InvalidRequest)?;
    ArtifactFacts::new(
        task_id,
        ArtifactKind::Evidence,
        staged.file_name.clone(),
        content,
        sha256,
        PrivacyClass::LocalOnly,
        1,
    )
    .map_err(|_| BrowserIoError::InvalidRequest)
}
