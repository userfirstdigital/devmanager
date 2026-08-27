//! Local-authority provider settings snapshot/mutation/refresh API.
//!
//! Host-owned only. UI and remote clients must never open
//! [`ProviderSettingsStore`] directly. Mutations require `expected_revision`
//! and seal before publish. Health never projects secrets/home/path/env to
//! remote clients.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::providers::settings::health::{ProviderHealthRow, ProviderHealthStatus};
use crate::providers::settings::health_job::ProviderHealthJobOwner;
use crate::providers::settings::model::{
    ProviderDriverKind, ProviderInstanceConfig, ProviderSettingsDocument, ProviderSettingsError,
};
use crate::providers::settings::profile::ProviderProfileOwner;
use crate::providers::settings::store::ProviderSettingsStoreError;
use crate::providers::ProviderKind;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSettingsSnapshot {
    pub revision: u64,
    pub health_interval_secs: u64,
    pub document: ProviderSettingsDocument,
    pub health: Vec<ProviderHealthRowWire>,
    pub health_in_flight: bool,
    pub health_error: Option<String>,
    /// Composer-facing enabled launchable instances (no secrets).
    pub composer_instances: Vec<ComposerProviderChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderHealthRowWire {
    pub instance_id: String,
    pub driver: String,
    pub status: String,
    pub version: Option<String>,
    pub account_email_masked: Option<String>,
    pub account_email: Option<String>,
    pub subscription_tier: Option<String>,
    pub checked_at_unix_ms: Option<u64>,
    pub error: Option<String>,
    pub reveal_email: bool,
}

impl From<&ProviderHealthRow> for ProviderHealthRowWire {
    fn from(row: &ProviderHealthRow) -> Self {
        Self {
            instance_id: row.instance_id.clone(),
            driver: row.driver.as_str().to_string(),
            status: row.status.as_str().to_string(),
            version: row.version.clone(),
            account_email_masked: row.account_email_masked.clone(),
            // Local authority may reveal; remote must strip before send.
            account_email: row.account_email.clone(),
            subscription_tier: row.subscription_tier.clone(),
            checked_at_unix_ms: row.checked_at_unix_ms,
            error: row.error.clone(),
            reveal_email: row.reveal_email,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ComposerProviderChoice {
    pub instance_id: String,
    pub display_name: String,
    pub driver: String,
    pub provider_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderSettingsMutation {
    ReplaceDocument {
        expected_revision: u64,
        document: ProviderSettingsDocument,
    },
    UpsertInstance {
        expected_revision: u64,
        instance: ProviderInstanceConfig,
    },
    /// Create-only: rejects duplicate ids instead of overwriting.
    AddInstance {
        expected_revision: u64,
        instance: ProviderInstanceConfig,
    },
    RemoveInstance {
        expected_revision: u64,
        instance_id: String,
    },
    SetEnabled {
        expected_revision: u64,
        instance_id: String,
        enabled: bool,
    },
    SetHealthInterval {
        expected_revision: u64,
        interval_secs: u64,
    },
    ResetBuiltin {
        expected_revision: u64,
        instance_id: String,
    },
}

/// Host/local wire request for TaskCockpit ProviderSettings variants.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderSettingsHostRequest {
    Snapshot,
    Refresh { force: bool },
    Mutate(ProviderSettingsMutation),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ProviderSettingsQuery {
    /// Immediate cache read; does not start probes.
    Snapshot,
    /// Deduped manual refresh. Returns generation when admitted.
    Refresh { force: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderSettingsReply {
    Snapshot(ProviderSettingsSnapshot),
    RefreshStarted { generation: u64 },
    RefreshBusy,
    MutationApplied { snapshot: ProviderSettingsSnapshot },
    Error { message: String },
}

#[derive(Debug)]
pub enum ProviderSettingsAuthorityError {
    Store(ProviderSettingsStoreError),
    Settings(ProviderSettingsError),
    StaleRevision { expected: u64, actual: u64 },
    LocalOnly,
    Message(String),
}

impl std::fmt::Display for ProviderSettingsAuthorityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::Settings(e) => write!(f, "{e}"),
            Self::StaleRevision { expected, actual } => {
                write!(f, "stale revision: expected {expected}, got {actual}")
            }
            Self::LocalOnly => write!(f, "provider settings are local-authority only"),
            Self::Message(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ProviderSettingsAuthorityError {}

impl From<ProviderSettingsStoreError> for ProviderSettingsAuthorityError {
    fn from(value: ProviderSettingsStoreError) -> Self {
        match value {
            ProviderSettingsStoreError::Settings(ProviderSettingsError::StaleRevision {
                expected,
                actual,
            }) => Self::StaleRevision { expected, actual },
            other => Self::Store(other),
        }
    }
}

impl From<ProviderSettingsError> for ProviderSettingsAuthorityError {
    fn from(value: ProviderSettingsError) -> Self {
        match value {
            ProviderSettingsError::StaleRevision { expected, actual } => {
                Self::StaleRevision { expected, actual }
            }
            other => Self::Settings(other),
        }
    }
}

/// Host-owned authority over profile settings + health cache/job.
pub struct ProviderSettingsAuthority {
    profile: Arc<ProviderProfileOwner>,
    health_job: ProviderHealthJobOwner,
}

impl ProviderSettingsAuthority {
    pub fn from_profile(profile: Arc<ProviderProfileOwner>) -> Self {
        let health_job = ProviderHealthJobOwner::from_profile(profile.clone());
        health_job
            .health_cache()
            .seed_from_document(&profile.settings.snapshot());
        Self {
            profile,
            health_job,
        }
    }

    pub fn profile(&self) -> &Arc<ProviderProfileOwner> {
        &self.profile
    }

    pub fn health_job(&self) -> &ProviderHealthJobOwner {
        &self.health_job
    }

    pub fn snapshot(&self) -> ProviderSettingsSnapshot {
        let document = self.profile.settings.redacted_snapshot();
        let composer_instances = document
            .instances
            .iter()
            .filter(|instance| instance.enabled && !instance.driver.is_stub())
            .map(|instance| ComposerProviderChoice {
                instance_id: instance.instance_id.to_string(),
                display_name: instance.display_name.clone(),
                driver: instance.driver.as_str().to_string(),
                provider_kind: instance
                    .driver
                    .to_provider_kind()
                    .map(|kind| kind.wire_name().to_string()),
            })
            .collect();
        ProviderSettingsSnapshot {
            revision: document.revision,
            health_interval_secs: document.health_interval_secs,
            health: self
                .profile
                .health
                .snapshot_rows()
                .iter()
                .map(ProviderHealthRowWire::from)
                .collect(),
            health_in_flight: self.profile.health.is_refresh_in_flight(),
            health_error: self.profile.health.last_error(),
            composer_instances,
            document,
        }
    }

    pub fn query(
        &self,
        query: ProviderSettingsQuery,
    ) -> Result<ProviderSettingsReply, ProviderSettingsAuthorityError> {
        match query {
            ProviderSettingsQuery::Snapshot => Ok(ProviderSettingsReply::Snapshot(self.snapshot())),
            ProviderSettingsQuery::Refresh { force } => {
                if !force && !self.health_job.should_schedule() {
                    return Ok(ProviderSettingsReply::RefreshBusy);
                }
                match self.health_job.try_begin_manual_refresh() {
                    Some(generation) => Ok(ProviderSettingsReply::RefreshStarted { generation }),
                    None => Ok(ProviderSettingsReply::RefreshBusy),
                }
            }
        }
    }

    pub fn mutate(
        &self,
        mutation: ProviderSettingsMutation,
    ) -> Result<ProviderSettingsReply, ProviderSettingsAuthorityError> {
        let expected = match &mutation {
            ProviderSettingsMutation::ReplaceDocument {
                expected_revision, ..
            }
            | ProviderSettingsMutation::UpsertInstance {
                expected_revision, ..
            }
            | ProviderSettingsMutation::AddInstance {
                expected_revision, ..
            }
            | ProviderSettingsMutation::RemoveInstance {
                expected_revision, ..
            }
            | ProviderSettingsMutation::SetEnabled {
                expected_revision, ..
            }
            | ProviderSettingsMutation::SetHealthInterval {
                expected_revision, ..
            }
            | ProviderSettingsMutation::ResetBuiltin {
                expected_revision, ..
            } => *expected_revision,
        };
        match mutation {
            ProviderSettingsMutation::ReplaceDocument { document, .. } => {
                document.validate()?;
                self.profile
                    .settings
                    .replace_with_expected_revision(Some(expected), document)?;
            }
            ProviderSettingsMutation::UpsertInstance { instance, .. } => {
                instance.validate()?;
                self.profile.settings.update(|doc| {
                    if doc.revision != expected {
                        return Err(ProviderSettingsError::StaleRevision {
                            expected,
                            actual: doc.revision,
                        });
                    }
                    doc.upsert_instance(instance)
                })?;
            }
            ProviderSettingsMutation::AddInstance { instance, .. } => {
                instance.validate()?;
                self.profile.settings.update(|doc| {
                    if doc.revision != expected {
                        return Err(ProviderSettingsError::StaleRevision {
                            expected,
                            actual: doc.revision,
                        });
                    }
                    doc.add_instance(instance)
                })?;
            }
            ProviderSettingsMutation::RemoveInstance { instance_id, .. } => {
                self.profile.settings.update(|doc| {
                    if doc.revision != expected {
                        return Err(ProviderSettingsError::StaleRevision {
                            expected,
                            actual: doc.revision,
                        });
                    }
                    let _ = doc.remove_custom_instance(&instance_id)?;
                    Ok(())
                })?;
            }
            ProviderSettingsMutation::SetEnabled {
                instance_id,
                enabled,
                ..
            } => {
                self.profile.settings.update(|doc| {
                    if doc.revision != expected {
                        return Err(ProviderSettingsError::StaleRevision {
                            expected,
                            actual: doc.revision,
                        });
                    }
                    let instance = doc.get_mut(&instance_id).ok_or_else(|| {
                        ProviderSettingsError::UnknownInstance(instance_id.clone())
                    })?;
                    if enabled && instance.driver.is_stub() {
                        return Err(ProviderSettingsError::StubCannotEnable(
                            instance.driver.as_str().to_string(),
                        ));
                    }
                    instance.enabled = enabled;
                    Ok(())
                })?;
            }
            ProviderSettingsMutation::SetHealthInterval { interval_secs, .. } => {
                self.profile.settings.update(|doc| {
                    if doc.revision != expected {
                        return Err(ProviderSettingsError::StaleRevision {
                            expected,
                            actual: doc.revision,
                        });
                    }
                    doc.set_health_interval(interval_secs);
                    Ok(())
                })?;
            }
            ProviderSettingsMutation::ResetBuiltin { instance_id, .. } => {
                self.profile.settings.update(|doc| {
                    if doc.revision != expected {
                        return Err(ProviderSettingsError::StaleRevision {
                            expected,
                            actual: doc.revision,
                        });
                    }
                    doc.reset_builtin(&instance_id)
                })?;
            }
        }
        let revision = self.profile.settings.snapshot().revision;
        self.health_job.note_config_revision(revision);
        self.profile
            .health
            .seed_from_document(&self.profile.settings.snapshot());
        Ok(ProviderSettingsReply::MutationApplied {
            snapshot: self.snapshot(),
        })
    }
}

/// Build per-instance probe work items from the document.
pub fn health_probe_plan(
    document: &ProviderSettingsDocument,
) -> Vec<(String, ProviderDriverKind, Option<ProviderKind>)> {
    document
        .instances
        .iter()
        .filter(|instance| !instance.driver.is_stub())
        .map(|instance| {
            (
                instance.instance_id.to_string(),
                instance.driver,
                instance.driver.to_provider_kind(),
            )
        })
        .collect()
}

/// Mark one instance healthy/degraded/error without copying presence across instances.
pub fn publish_instance_health(
    cache: &crate::providers::settings::ProviderHealthCache,
    instance_id: &str,
    driver: ProviderDriverKind,
    status: ProviderHealthStatus,
    version: Option<String>,
    error: Option<String>,
) {
    let mut row = cache
        .snapshot_rows()
        .into_iter()
        .find(|row| row.instance_id == instance_id)
        .unwrap_or_else(|| ProviderHealthRow::unknown(instance_id, driver));
    row.status = status;
    row.version = version;
    row.error = error;
    row.checked_at_unix_ms = Some(crate::providers::settings::health::unix_now_ms());
    cache.upsert_row(row);
}
