use super::model::{
    AppConfig, ConfigCommand, ConfigError, ConfigErrorKind, ConfigRevision, Nullable, Project,
    ProjectFolder, RunCommand, SSHConnection, MAX_CONFIG_BYTES,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use crate::domain::ProjectId;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AtomicWriteFailure {
    BeforeDirectorySync,
    BeforeFlush,
    BeforeReplace,
    InstallAfterBackup,
    ExternalWriterBeforeReplace,
    ExternalWriterAfterAdmission,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileFingerprint {
    pub identity: FileIdentity,
    pub digest: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FileIdentity {
    Windows { volume: u32, index: u64 },
    Unix { device: u64, inode: u64 },
}

#[derive(Clone, PartialEq, Eq)]
pub struct ImportPreviewToken {
    source: PathBuf,
    source_fingerprint: FileFingerprint,
    expected_destination_revision: ConfigRevision,
}

impl fmt::Debug for ImportPreviewToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImportPreviewToken")
            .field("source", &"<opaque>")
            .field("source_fingerprint", &"<opaque>")
            .field(
                "expected_destination_revision",
                &self.expected_destination_revision,
            )
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ImportTokenKey {
    source: PathBuf,
    source_fingerprint: FileFingerprint,
    expected_destination_revision: ConfigRevision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSnapshot {
    pub config: AppConfig,
    pub revision: ConfigRevision,
    pub fingerprint: Option<FileFingerprint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigMigrationResult {
    pub migrated: bool,
    pub source_version: Option<u32>,
    pub target_version: u32,
}

impl ConfigMigrationResult {
    const NONE: Self = Self {
        migrated: false,
        source_version: None,
        target_version: super::model::CURRENT_CONFIG_VERSION,
    };
}

/// Opaque, already-resolved authority for the isolated canonical config.
/// Callers cannot select a destination leaf or a nested namespace through the
/// runtime store API.
pub struct ConfigAuthority {
    root: PathBuf,
    root_identity: FileIdentity,
}

/// Host-only input seam for Phase 6.2 workspace binding.
///
/// A caller cannot construct or clone this issuer, choose a project id, or
/// extract a raw path through a public API.  The host obtains it from the
/// loaded ConfigStore snapshot, and Phase 6.2 consumes the crate-private root
/// pairs when constructing its own `WorkspaceProjectRoots` authority.  Legacy
/// string ids are mapped to random host-issued `ProjectId`s and remain tied to
/// this config revision/fingerprint plus the host action/runtime generations.
/// Active configured folders (non-archived) are retained alongside roots so a
/// Task may target sibling or external repositories without client path
/// authority.
#[allow(dead_code)]
pub(crate) struct ConfigWorkspaceIssuer {
    projects: Vec<(ProjectId, PathBuf, String)>,
    folders: Vec<ConfigWorkspaceFolderTarget>,
    config_revision: ConfigRevision,
    snapshot_fingerprint: Option<FileFingerprint>,
    action_epoch: u64,
    runtime_generation: u64,
}

/// One active configured folder retained by [`ConfigWorkspaceIssuer`].
/// Paths stay host-private; the opaque folder config id is the only selector
/// identity a client may later present.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ConfigWorkspaceFolderTarget {
    pub(crate) project_id: ProjectId,
    pub(crate) project_config_id: String,
    pub(crate) folder_config_id: String,
    pub(crate) label: String,
    pub(crate) path: PathBuf,
}

impl fmt::Debug for ConfigWorkspaceFolderTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ConfigWorkspaceFolderTarget(REDACTED)")
    }
}

impl fmt::Debug for ConfigWorkspaceIssuer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ConfigWorkspaceIssuer(REDACTED)")
    }
}

#[allow(dead_code)]
impl ConfigWorkspaceIssuer {
    /// Return the exact input shape consumed by the Phase 6.2
    /// `WorkspaceProjectRoots::try_from_pairs` adapter.  This is crate-private
    /// by design: paths remain host-owned and never become a public/raw-path
    /// authority or a client-forgeable id issuer.
    pub(crate) fn workspace_project_roots(&self) -> Vec<(ProjectId, PathBuf)> {
        self.projects
            .iter()
            .map(|(project_id, root, _)| (*project_id, root.clone()))
            .collect()
    }

    pub(crate) fn workspace_project_config_ids(&self) -> Vec<(String, ProjectId)> {
        self.projects
            .iter()
            .map(|(project_id, _, configured_id)| (configured_id.clone(), *project_id))
            .collect()
    }

    /// Active (non-archived) configured folders retained with this issuer.
    /// Folder config ids are unique within a Project; cross-project duplicates
    /// are retained and later selected by Task ProjectId. Callers must not
    /// invent or reorder these targets.
    pub(crate) fn workspace_project_folders(
        &self,
    ) -> Vec<(ProjectId, String, String, String, PathBuf)> {
        self.folders
            .iter()
            .map(|folder| {
                (
                    folder.project_id,
                    folder.project_config_id.clone(),
                    folder.folder_config_id.clone(),
                    folder.label.clone(),
                    folder.path.clone(),
                )
            })
            .collect()
    }

    pub(crate) fn project_id_for_config_id(&self, config_id: &str) -> Option<ProjectId> {
        self.projects
            .iter()
            .find(|(_, _, configured_id)| configured_id == config_id.trim())
            .map(|(project_id, _, _)| *project_id)
    }

    pub(crate) fn config_revision(&self) -> ConfigRevision {
        self.config_revision
    }

    pub(crate) fn action_epoch(&self) -> u64 {
        self.action_epoch
    }

    pub(crate) fn runtime_generation(&self) -> u64 {
        self.runtime_generation
    }

    /// A host must re-check the live snapshot before admitting a workspace
    /// action.  Revision equality alone is insufficient when a file is
    /// replaced outside the store, so the fingerprint is fenced as well.
    pub(crate) fn validate_current_snapshot(
        &self,
        snapshot: &ConfigSnapshot,
    ) -> Result<(), ConfigError> {
        if self.config_revision != snapshot.revision
            || self.snapshot_fingerprint != snapshot.fingerprint
        {
            return Err(ConfigError::new(
                ConfigErrorKind::ExternalChange,
                "workspace issuer snapshot is stale",
            ));
        }
        Ok(())
    }
}

impl ConfigAuthority {
    pub(crate) fn from_host_paths(
        paths: &crate::config::paths::ResolvedAppPaths,
    ) -> Result<Self, ConfigError> {
        let root = absolute_path(&paths.root)?;
        let config = absolute_path(&paths.config)?;
        if config != root.join("config.json") {
            return Err(ConfigError::new(
                ConfigErrorKind::ProtectedPath,
                "host configuration authority must name the canonical config.json leaf",
            ));
        }
        fs::create_dir_all(&root).map_err(|_error| {
            ConfigError::new(
                ConfigErrorKind::Io,
                "host configuration root could not be created",
            )
        })?;
        let root_handle = open_root_handle_without_expected(&root)?;
        Ok(Self {
            root,
            root_identity: root_handle.identity.clone(),
        })
    }

    /// Test-only fixture boundary. Production wiring must obtain this
    /// authority from its host-owned profile resolver.
    #[cfg(test)]
    pub(crate) fn from_test_fixture_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = absolute_path(path.as_ref())?;
        let (root, root_identity) = approved_isolated_root(&path)?;
        Ok(Self {
            root,
            root_identity,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPreview {
    pub token: ImportPreviewToken,
    pub project_count: usize,
    pub ssh_host_count: usize,
    pub revision: ConfigRevision,
    pub valid: bool,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportPreview {
    pub revision: ConfigRevision,
    pub byte_count: usize,
    pub valid: bool,
}

pub struct ConfigStore {
    path: PathBuf,
    root: PathBuf,
    root_identity: FileIdentity,
    lock_path: PathBuf,
    snapshot: ConfigSnapshot,
    write_failure: Option<AtomicWriteFailure>,
    consumed_import_tokens: HashSet<ImportTokenKey>,
    consumed_import_order: VecDeque<ImportTokenKey>,
    migration: ConfigMigrationResult,
}

const MAX_CONSUMED_IMPORT_TOKENS: usize = 1024;
const CONFIG_OPERATION_LIMIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
struct OperationDeadline {
    deadline: Instant,
}

#[derive(Clone, Copy)]
enum ConfigDecodeMode {
    Strict,
    LegacyMigration,
}

impl OperationDeadline {
    fn new() -> Self {
        Self {
            deadline: Instant::now() + CONFIG_OPERATION_LIMIT,
        }
    }

    fn check(self) -> Result<(), ConfigError> {
        if Instant::now() >= self.deadline {
            Err(ConfigError::new(
                ConfigErrorKind::AtomicWrite,
                "configuration operation exceeded its deadline",
            ))
        } else {
            Ok(())
        }
    }

    fn remaining(self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

impl ConfigStore {
    pub fn open(authority: ConfigAuthority) -> Result<Self, ConfigError> {
        Self::open_authority(authority, false)
    }

    /// Open the canonical host profile and perform its one-time legacy
    /// migration.  The host binary receives only this validated store; callers
    /// cannot provide a raw config path or construct a workspace authority.
    pub fn open_host(paths: &crate::config::paths::ResolvedAppPaths) -> Result<Self, ConfigError> {
        let authority = ConfigAuthority::from_host_paths(paths)?;
        Self::open_with_legacy_migration(authority)
    }

    /// Read a validated current or unique recovery candidate without mutating
    /// the profile.  Startup uses this only to keep known project rows visible
    /// while the canonical store is repaired; no default config is synthesized.
    pub(crate) fn recover_host_snapshot(
        paths: &crate::config::paths::ResolvedAppPaths,
    ) -> Result<ConfigSnapshot, ConfigError> {
        let authority = ConfigAuthority::from_host_paths(paths)?;
        Self::recover_read_only(authority)
    }

    pub(crate) fn open_with_legacy_migration(
        authority: ConfigAuthority,
    ) -> Result<Self, ConfigError> {
        Self::open_authority(authority, true)
    }

    #[cfg(test)]
    pub(crate) fn open_test_fixture(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Self::open_authority(ConfigAuthority::from_test_fixture_path(path)?, false)
    }

    #[cfg(test)]
    pub(crate) fn open_legacy_fixture(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Self::open_authority(ConfigAuthority::from_test_fixture_path(path)?, true)
    }

    fn open_authority(
        authority: ConfigAuthority,
        migrate_legacy: bool,
    ) -> Result<Self, ConfigError> {
        let deadline = OperationDeadline::new();
        let root = authority.root;
        let root_identity = authority.root_identity;
        let path = root.join("config.json");
        deadline.check()?;
        let root_handle = open_root_handle(&root, &root_identity)?;
        validate_path_with_protected_alias_classification(&path, &root)?;
        reject_store_path(&path, &root)?;
        let lock_path = lock_path_for(&root);
        let mut migration = ConfigMigrationResult::NONE;
        let snapshot = match read_snapshot(
            &path,
            &root,
            &root_handle,
            deadline,
            ConfigDecodeMode::Strict,
        ) {
            Ok(snapshot) => snapshot,
            Err(error)
                if migrate_legacy
                    && matches!(
                        error.kind(),
                        ConfigErrorKind::Parse | ConfigErrorKind::Validation
                    ) =>
            {
                // A legacy read is only a migration candidate. Acquire the
                // same lock used by writes before re-reading or replacing it,
                // then re-check strict storage so a concurrent writer wins.
                let _lock = acquire_config_lock(&root_handle, &root, &lock_path, deadline)?;
                match read_snapshot(
                    &path,
                    &root,
                    &root_handle,
                    deadline,
                    ConfigDecodeMode::Strict,
                ) {
                    Ok(snapshot) => snapshot,
                    Err(strict_error)
                        if matches!(
                            strict_error.kind(),
                            ConfigErrorKind::Parse | ConfigErrorKind::Validation
                        ) =>
                    {
                        let legacy = read_snapshot(
                            &path,
                            &root,
                            &root_handle,
                            deadline,
                            ConfigDecodeMode::LegacyMigration,
                        )?;
                        migration = ConfigMigrationResult {
                            migrated: true,
                            source_version: legacy.config.source_version(),
                            target_version: super::model::CURRENT_CONFIG_VERSION,
                        };
                        let mut migrated = legacy.config;
                        migrated.version = super::model::CURRENT_CONFIG_VERSION;
                        let _ = migrated.settings_mut();
                        migrated.materialize_for_write();
                        let bytes = migrated.to_json_bytes()?;
                        atomic_write(
                            &path,
                            &bytes,
                            None,
                            legacy.fingerprint.as_ref(),
                            &root,
                            &root_handle,
                            deadline,
                        )?;
                        read_snapshot(
                            &path,
                            &root,
                            &root_handle,
                            deadline,
                            ConfigDecodeMode::Strict,
                        )?
                    }
                    Err(strict_error) => return Err(strict_error),
                }
            }
            Err(error) => return Err(error),
        };
        deadline.check()?;
        Ok(Self {
            path,
            root,
            root_identity,
            lock_path,
            snapshot,
            write_failure: None,
            consumed_import_tokens: HashSet::new(),
            consumed_import_order: VecDeque::new(),
            migration,
        })
    }

    pub fn snapshot(&self) -> &ConfigSnapshot {
        &self.snapshot
    }

    pub fn migration_result(&self) -> ConfigMigrationResult {
        self.migration
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Issue the sealed host-to-workspace binding input from this loaded
    /// snapshot. Arbitrary configured ids receive random opaque identities that
    /// are persisted in canonical config metadata. The identity is keyed by
    /// the configured id, never by its filesystem path, so changing a root or
    /// reopening the process does not silently mint a new authority.
    pub(crate) fn issue_workspace_authority(
        &mut self,
        expected_revision: ConfigRevision,
        action_epoch: u64,
        runtime_generation: u64,
    ) -> Result<ConfigWorkspaceIssuer, ConfigError> {
        if expected_revision != self.snapshot.revision {
            return Err(ConfigError::new(
                ConfigErrorKind::RevisionConflict,
                "workspace issuer expected revision does not match",
            ));
        }

        let configured_projects = self
            .snapshot
            .config
            .projects
            .iter()
            .map(|project| (project.id.clone(), project.root_path.clone()))
            .collect::<Vec<_>>();
        let configured_ids = configured_projects
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<BTreeSet<_>>();

        let mut mapping = self.snapshot.config.workspace_project_ids().clone();
        mapping.retain(|config_id, _| configured_ids.contains(config_id.as_str()));
        let mut mapping_changed = false;
        for (config_id, _) in &configured_projects {
            if mapping.contains_key(config_id) {
                continue;
            }
            let project_id = ProjectId::new().to_string();
            mapping.insert(config_id.clone(), project_id);
            mapping_changed = true;
        }
        if mapping.len() != self.snapshot.config.workspace_project_ids().len() {
            mapping_changed = true;
        }
        if mapping_changed {
            let mut config = self.snapshot.config.clone();
            config.set_workspace_project_ids(mapping);
            self.replace_config(config)?;
        }

        let mut projects = Vec::with_capacity(configured_projects.len());
        let mut folders = Vec::new();
        // Folder config ids are unique within a Project only (matches
        // validate_project). Cross-project duplicates such as A/api and B/api
        // are valid; Task scope later selects by ProjectId.
        let mut folder_ids = BTreeSet::<(ProjectId, String)>::new();
        for project in self
            .snapshot
            .config
            .projects
            .iter()
            .filter(|project| !project.archived.as_ref().copied().unwrap_or(false))
        {
            let project_id = self
                .snapshot
                .config
                .workspace_project_ids()
                .get(&project.id)
                .and_then(|opaque_id| ProjectId::parse(opaque_id).ok())
                .ok_or_else(|| {
                    ConfigError::new(
                        ConfigErrorKind::Validation,
                        "workspace identity mapping is invalid",
                    )
                })?;
            if let Some((_, root)) = configured_projects
                .iter()
                .find(|(config_id, _)| config_id == &project.id)
            {
                projects.push((project_id, PathBuf::from(root), project.id.clone()));
            }
            for folder in project
                .folders
                .iter()
                .filter(|folder| !folder.archived.as_ref().copied().unwrap_or(false))
            {
                let folder_config_id = folder.id.trim();
                if folder_config_id.is_empty()
                    || crate::domain::cockpit::validate_folder_config_id(folder_config_id).is_err()
                {
                    return Err(ConfigError::new(
                        ConfigErrorKind::Validation,
                        "configured folder selector identity is invalid",
                    ));
                }
                if !folder_ids.insert((project_id, folder_config_id.to_string())) {
                    return Err(ConfigError::new(
                        ConfigErrorKind::Validation,
                        "configured folder selector identity is ambiguous",
                    ));
                }
                let label = if folder.name.trim().is_empty() {
                    folder.id.clone()
                } else {
                    folder.name.clone()
                };
                folders.push(ConfigWorkspaceFolderTarget {
                    project_id,
                    project_config_id: project.id.clone(),
                    folder_config_id: folder_config_id.to_string(),
                    label,
                    path: PathBuf::from(folder.folder_path.trim()),
                });
            }
        }

        Ok(ConfigWorkspaceIssuer {
            projects,
            folders,
            config_revision: self.snapshot.revision,
            snapshot_fingerprint: self.snapshot.fingerprint.clone(),
            action_epoch,
            runtime_generation,
        })
    }

    #[cfg(test)]
    pub(crate) fn temp_path(&self) -> PathBuf {
        temp_path_for(&self.path)
    }

    #[cfg(test)]
    pub(crate) fn inject_write_failure(&mut self, failure: AtomicWriteFailure) {
        self.write_failure = Some(failure);
    }

    pub fn replace_config(
        &mut self,
        mut config: AppConfig,
    ) -> Result<&ConfigSnapshot, ConfigError> {
        let deadline = OperationDeadline::new();
        config.validate()?;
        deadline.check()?;
        let root_handle = self.open_root_for_io(deadline)?;
        validate_final_path(&self.path, &self.root)?;
        let _lock = acquire_config_lock(&root_handle, &self.root, &self.lock_path, deadline)?;
        let expected =
            self.ensure_current_locked(self.snapshot.revision, &root_handle, deadline)?;
        config.revision = self.snapshot.revision.checked_add(1).ok_or_else(|| {
            ConfigError::new(
                ConfigErrorKind::Validation,
                "configuration revision overflowed",
            )
        })?;
        config.materialize_for_write();
        let snapshot = self.persist_locked(config, &root_handle, expected.as_ref(), deadline)?;
        self.snapshot = snapshot;
        Ok(&self.snapshot)
    }

    pub(crate) fn write_external_config(
        path: &Path,
        config: &AppConfig,
    ) -> Result<(), ConfigError> {
        let deadline = OperationDeadline::new();
        let path = absolute_path(path)?;
        validate_external_transfer_path(&path, deadline)?;
        deadline.check()?;
        let bytes = config.to_redacted_json_bytes()?;
        deadline.check()?;
        let parent = path.parent().ok_or_else(|| {
            ConfigError::new(
                ConfigErrorKind::ProtectedPath,
                "external configuration path has no parent",
            )
        })?;
        let root_handle = open_root_handle_without_expected(parent)?;
        deadline.check()?;
        // Use the same held-parent, compare-and-swap writer as the active
        // store.  This keeps transfer writes recoverable and prevents a
        // Windows destination replacement from silently overwriting a
        // post-check writer.
        let expected = destination_fingerprint(&path, parent, &root_handle, deadline)?;
        atomic_write(
            &path,
            &bytes,
            None,
            expected.as_ref(),
            parent,
            &root_handle,
            deadline,
        )
    }

    pub fn execute(
        &mut self,
        expected_revision: ConfigRevision,
        command: ConfigCommand,
    ) -> Result<&ConfigSnapshot, ConfigError> {
        let deadline = OperationDeadline::new();
        command.validate()?;
        deadline.check()?;
        let root_handle = self.open_root_for_io(deadline)?;
        validate_final_path(&self.path, &self.root)?;
        let _lock = acquire_config_lock(&root_handle, &self.root, &self.lock_path, deadline)?;
        let expected = self.ensure_current_locked(expected_revision, &root_handle, deadline)?;
        let mut next = self.snapshot.config.clone();
        apply_command(&mut next, command)?;
        next.revision = expected_revision.checked_add(1).ok_or_else(|| {
            ConfigError::new(
                ConfigErrorKind::Validation,
                "configuration revision overflowed",
            )
        })?;
        next.materialize_for_write();

        let snapshot = self.persist_locked(next, &root_handle, expected.as_ref(), deadline)?;
        self.snapshot = snapshot;
        Ok(&self.snapshot)
    }

    pub fn preview_import(&self, path: impl AsRef<Path>) -> Result<ImportPreview, ConfigError> {
        let deadline = OperationDeadline::new();
        let path = self.validate_transfer_path(path.as_ref())?;
        let root_handle = self.open_root_for_io(deadline)?;
        validate_final_path(&path, &self.root)?;
        let imported = read_existing_snapshot(&path, &self.root, &root_handle, deadline)?;
        let source_fingerprint = imported.fingerprint.clone().ok_or_else(|| {
            ConfigError::new(
                ConfigErrorKind::Io,
                "import source fingerprint could not be captured",
            )
        })?;
        Ok(ImportPreview {
            token: ImportPreviewToken {
                source: path,
                source_fingerprint,
                expected_destination_revision: self.snapshot.revision,
            },
            project_count: imported.config.projects.len(),
            ssh_host_count: imported.config.ssh_connections.len(),
            revision: imported.revision,
            valid: true,
            summary: format!(
                "{} projects and {} SSH hosts",
                imported.config.projects.len(),
                imported.config.ssh_connections.len()
            ),
        })
    }

    pub fn preview_export(&self, path: impl AsRef<Path>) -> Result<ExportPreview, ConfigError> {
        let deadline = OperationDeadline::new();
        let _path = self.validate_transfer_path(path.as_ref())?;
        let _root_handle = self.open_root_for_io(deadline)?;
        deadline.check()?;
        let bytes = self.snapshot.config.to_redacted_json_bytes()?;
        deadline.check()?;
        Ok(ExportPreview {
            revision: self.snapshot.revision,
            byte_count: bytes.len(),
            valid: true,
        })
    }

    pub fn export_to(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let deadline = OperationDeadline::new();
        let path = self.validate_transfer_path(path.as_ref())?;
        let root_handle = self.open_root_for_io(deadline)?;
        validate_final_path(&path, &self.root)?;
        let _lock = acquire_config_lock(&root_handle, &self.root, &self.lock_path, deadline)?;
        let _expected =
            self.ensure_current_locked(self.snapshot.revision, &root_handle, deadline)?;
        let path = self.validate_transfer_path(&path)?;
        validate_final_path(&path, &self.root)?;
        let bytes = self.snapshot.config.to_redacted_json_bytes()?;
        let destination = destination_fingerprint(&path, &self.root, &root_handle, deadline)?;
        atomic_write(
            &path,
            &bytes,
            None,
            destination.as_ref(),
            &self.root,
            &root_handle,
            deadline,
        )
    }

    /// Export the current canonical snapshot to a user-selected destination.
    ///
    /// `export_to` intentionally accepts only destinations beneath the store's
    /// isolated root because it is also used by host-internal transfers.  The
    /// desktop export dialog is an explicitly external transfer, so it uses
    /// this seam instead.  Both paths still share the same strict bytes,
    /// held-parent compare-and-swap writer, active-store lock, and durability
    /// protocol; no legacy model is involved.
    pub(crate) fn export_external_to(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let deadline = OperationDeadline::new();
        let path = absolute_path(path.as_ref())?;
        validate_external_transfer_path(&path, deadline)?;
        let active_root_handle = self.open_root_for_io(deadline)?;
        validate_final_path(&self.path, &self.root)?;
        let _lock =
            acquire_config_lock(&active_root_handle, &self.root, &self.lock_path, deadline)?;
        let _expected =
            self.ensure_current_locked(self.snapshot.revision, &active_root_handle, deadline)?;
        let bytes = self.snapshot.config.to_redacted_json_bytes()?;
        deadline.check()?;
        let parent = path.parent().ok_or_else(|| {
            ConfigError::new(
                ConfigErrorKind::ProtectedPath,
                "configuration transfer path has no parent",
            )
        })?;
        let destination_root_handle = open_root_handle_without_expected(parent)?;
        deadline.check()?;
        let destination =
            destination_fingerprint(&path, parent, &destination_root_handle, deadline)?;
        atomic_write(
            &path,
            &bytes,
            None,
            destination.as_ref(),
            parent,
            &destination_root_handle,
            deadline,
        )
    }

    pub fn import_replace(
        &mut self,
        preview: ImportPreview,
    ) -> Result<&ConfigSnapshot, ConfigError> {
        let deadline = OperationDeadline::new();
        let token = preview.token;
        let key = ImportTokenKey {
            source: token.source.clone(),
            source_fingerprint: token.source_fingerprint.clone(),
            expected_destination_revision: token.expected_destination_revision,
        };
        if self.consumed_import_tokens.contains(&key) {
            return Err(ConfigError::new(
                ConfigErrorKind::PreviewReplay,
                "import preview has already been consumed",
            ));
        }

        let root_handle = self.open_root_for_io(deadline)?;
        let _lock = acquire_config_lock(&root_handle, &self.root, &self.lock_path, deadline)?;
        let expected = self.ensure_current_locked(
            token.expected_destination_revision,
            &root_handle,
            deadline,
        )?;
        let path = self.validate_transfer_path(&token.source)?;
        validate_final_path(&path, &self.root)?;
        if path != token.source {
            return Err(ConfigError::new(
                ConfigErrorKind::ExternalChange,
                "import source path changed after preview",
            ));
        }
        let current_source = destination_fingerprint(&path, &self.root, &root_handle, deadline)?;
        if current_source.as_ref() != Some(&token.source_fingerprint) {
            return Err(ConfigError::new(
                ConfigErrorKind::ExternalChange,
                "import source changed after preview",
            ));
        }
        let imported = read_existing_snapshot(&path, &self.root, &root_handle, deadline)?;
        if imported.fingerprint.as_ref() != Some(&token.source_fingerprint) {
            return Err(ConfigError::new(
                ConfigErrorKind::ExternalChange,
                "import source changed after preview",
            ));
        }
        let mut next = imported.config;
        next.revision = token
            .expected_destination_revision
            .checked_add(1)
            .ok_or_else(|| {
                ConfigError::new(
                    ConfigErrorKind::Validation,
                    "configuration revision overflowed",
                )
            })?;
        next.materialize_for_write();
        let snapshot = self.persist_locked(next, &root_handle, expected.as_ref(), deadline)?;
        self.snapshot = snapshot;
        if self.consumed_import_tokens.insert(key.clone()) {
            self.consumed_import_order.push_back(key);
            while self.consumed_import_order.len() > MAX_CONSUMED_IMPORT_TOKENS {
                if let Some(expired) = self.consumed_import_order.pop_front() {
                    self.consumed_import_tokens.remove(&expired);
                }
            }
        }
        Ok(&self.snapshot)
    }

    fn persist_locked(
        &mut self,
        mut config: AppConfig,
        root_handle: &RootHandle,
        expected_destination: Option<&FileFingerprint>,
        deadline: OperationDeadline,
    ) -> Result<ConfigSnapshot, ConfigError> {
        config.materialize_for_write();
        let bytes = config.to_json_bytes()?;
        let failure = self.write_failure.take();
        atomic_write(
            &self.path,
            &bytes,
            failure,
            expected_destination,
            &self.root,
            root_handle,
            deadline,
        )?;
        read_snapshot(
            &self.path,
            &self.root,
            root_handle,
            deadline,
            ConfigDecodeMode::Strict,
        )
    }

    fn ensure_current_locked(
        &self,
        expected_revision: ConfigRevision,
        root_handle: &RootHandle,
        deadline: OperationDeadline,
    ) -> Result<Option<FileFingerprint>, ConfigError> {
        validate_final_path(&self.path, &self.root)?;
        if expected_revision != self.snapshot.revision {
            return Err(ConfigError::new(
                ConfigErrorKind::RevisionConflict,
                "expected configuration revision does not match",
            ));
        }

        let disk = match read_snapshot(
            &self.path,
            &self.root,
            root_handle,
            deadline,
            ConfigDecodeMode::Strict,
        ) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let current =
                    destination_fingerprint(&self.path, &self.root, root_handle, deadline)?;
                if current != self.snapshot.fingerprint {
                    return Err(ConfigError::new(
                        ConfigErrorKind::ExternalChange,
                        "configuration changed outside this store",
                    ));
                }
                return Err(error);
            }
        };
        if disk.revision != self.snapshot.revision || disk.fingerprint != self.snapshot.fingerprint
        {
            return Err(ConfigError::new(
                ConfigErrorKind::ExternalChange,
                "configuration changed outside this store",
            ));
        }
        Ok(disk.fingerprint)
    }

    fn validate_transfer_path(&self, path: &Path) -> Result<PathBuf, ConfigError> {
        let path = absolute_path(path)?;
        reject_transfer_path(&path, &self.root)?;
        validate_path_with_protected_alias_classification(&path, &self.root)?;
        if same_path_or_file(&path, &self.path) {
            return Err(ConfigError::new(
                ConfigErrorKind::PathAlias,
                "import or export path aliases the active configuration",
            ));
        }
        Ok(path)
    }

    fn open_root_for_io(&self, deadline: OperationDeadline) -> Result<RootHandle, ConfigError> {
        deadline.check()?;
        let handle = open_root_handle(&self.root, &self.root_identity)?;
        deadline.check()?;
        Ok(handle)
    }

    fn recover_read_only(authority: ConfigAuthority) -> Result<ConfigSnapshot, ConfigError> {
        let deadline = OperationDeadline::new();
        let root = authority.root;
        let root_identity = authority.root_identity;
        let path = root.join("config.json");
        let root_handle = open_root_handle(&root, &root_identity)?;
        validate_path_with_protected_alias_classification(&path, &root)?;
        reject_store_path(&path, &root)?;

        let mut first_error = None;
        if fs::symlink_metadata(&path).is_ok() {
            match read_snapshot(
                &path,
                &root,
                &root_handle,
                deadline,
                ConfigDecodeMode::Strict,
            ) {
                Ok(snapshot) if snapshot.fingerprint.is_some() => return Ok(snapshot),
                Ok(_) => {}
                Err(error) => first_error = Some(error),
            }
        }

        let destination_prefix = ".config.json.";
        let mut candidates = fs::read_dir(&root)
            .map_err(|_| {
                ConfigError::new(
                    ConfigErrorKind::Io,
                    "configuration recovery directory could not be read",
                )
            })?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|candidate| {
                candidate
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with(destination_prefix) && name.ends_with(".tmp")
                    })
            })
            .collect::<Vec<_>>();
        candidates.sort();

        let mut recovered = None;
        for candidate in candidates {
            deadline.check()?;
            match read_snapshot(
                &candidate,
                &root,
                &root_handle,
                deadline,
                ConfigDecodeMode::Strict,
            ) {
                Ok(snapshot) if snapshot.fingerprint.is_some() => {
                    if recovered
                        .as_ref()
                        .is_none_or(|current: &ConfigSnapshot| snapshot.revision > current.revision)
                    {
                        recovered = Some(snapshot);
                    }
                }
                Ok(_) | Err(_) => {}
            }
        }
        recovered.ok_or_else(|| {
            first_error.unwrap_or_else(|| {
                ConfigError::new(
                    ConfigErrorKind::NotFound,
                    "no validated configuration recovery copy is available",
                )
            })
        })
    }

    /// Re-read the strict canonical snapshot immediately before a host
    /// workspace admission.  This checks revision and file identity/digest
    /// under the same path validation used by writes, so a post-issuance file
    /// replacement fails closed rather than reviving stale roots.
    pub(crate) fn validate_workspace_issuer_current(
        &self,
        issuer: &ConfigWorkspaceIssuer,
    ) -> Result<(), ConfigError> {
        let deadline = OperationDeadline::new();
        let root_handle = self.open_root_for_io(deadline)?;
        let snapshot = read_snapshot(
            &self.path,
            &self.root,
            &root_handle,
            deadline,
            ConfigDecodeMode::Strict,
        )?;
        issuer.validate_current_snapshot(&snapshot)
    }
}

pub(crate) fn read_external_config(path: &Path) -> Result<AppConfig, ConfigError> {
    let deadline = OperationDeadline::new();
    let path = absolute_path(path)?;
    validate_external_transfer_path(&path, deadline)?;
    deadline.check()?;
    let parent = path.parent().ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "external configuration path has no parent",
        )
    })?;
    let root_handle = open_root_handle_without_expected(parent)?;
    let file = open_existing_relative_file(&root_handle, parent, &path).map_err(|error| {
        ConfigError::new(
            if error.kind() == io::ErrorKind::NotFound {
                ConfigErrorKind::NotFound
            } else {
                ConfigErrorKind::Io
            },
            "external configuration file could not be read",
        )
    })?;
    let metadata = bound_file_metadata(&file).ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::PathAlias,
            "external configuration identity could not be proven",
        )
    })?;
    if metadata.is_reparse_point || !metadata.is_regular_file || metadata.link_count != 1 {
        return Err(ConfigError::new(
            ConfigErrorKind::PathAlias,
            "external configuration is not a plain unaliased file",
        ));
    }
    deadline.check()?;
    let mut bytes = Vec::new();
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::Io,
                "external configuration file could not be read",
            )
        })?;
    deadline.check()?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "external configuration exceeds the size limit",
        ));
    }
    let contents = std::str::from_utf8(&bytes).map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::Parse,
            "external configuration is not UTF-8",
        )
    })?;
    let result =
        AppConfig::from_json_str(contents).or_else(|_| AppConfig::from_legacy_json_str(contents));
    deadline.check()?;
    result
}

fn validate_external_transfer_path(
    path: &Path,
    deadline: OperationDeadline,
) -> Result<(), ConfigError> {
    deadline.check()?;
    if is_secret_path(path) || is_production_namespace(path) {
        return Err(ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "protected configuration transfer path is not supported",
        ));
    }
    let name = lower_file_name(path);
    if matches!(
        name.as_str(),
        ".config.lock" | "session.json" | "remote.json"
    ) {
        return Err(ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "protected configuration transfer path is not supported",
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration transfer path has no parent",
        )
    })?;
    let metadata = fs::symlink_metadata(parent).map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration transfer parent could not be verified",
        )
    })?;
    deadline.check()?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ConfigError::new(
            ConfigErrorKind::PathAlias,
            "configuration transfer parent is not a plain directory",
        ));
    }
    let canonical_parent = fs::canonicalize(parent).map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration transfer parent could not be resolved",
        )
    })?;
    deadline.check()?;
    if !paths_equal(&canonical_parent, parent) {
        return Err(ConfigError::new(
            ConfigErrorKind::PathAlias,
            "configuration transfer parent is aliased",
        ));
    }
    let result = match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || file_link_count(path, &metadata).is_some_and(|count| count != 1) =>
        {
            Err(ConfigError::new(
                ConfigErrorKind::PathAlias,
                "configuration transfer destination is not a plain unaliased file",
            ))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration transfer destination could not be verified",
        )),
    };
    deadline.check()?;
    result
}

fn apply_command(config: &mut AppConfig, command: ConfigCommand) -> Result<(), ConfigError> {
    match command {
        ConfigCommand::CreateProject { project } => {
            ensure_new_id(
                config.projects.iter().map(|item| item.id.as_str()),
                &project.id,
            )?;
            config.projects.push(project);
        }
        ConfigCommand::UpdateProject { project } => {
            let target = config
                .projects
                .iter_mut()
                .find(|item| item.id == project.id)
                .ok_or_else(|| not_found("project"))?;
            *target = project;
        }
        ConfigCommand::ReorderProject {
            project_id,
            new_index,
        } => reorder(&mut config.projects, &project_id, new_index, "project")?,
        ConfigCommand::ArchiveProject { project_id } => {
            let target = find_project_mut(config, &project_id)?;
            target.archived = Nullable::Value(true);
        }
        ConfigCommand::CreateFolder { project_id, folder } => {
            let project = find_project_mut(config, &project_id)?;
            ensure_new_id(
                project.folders.iter().map(|item| item.id.as_str()),
                &folder.id,
            )?;
            project.folders.push(folder);
        }
        ConfigCommand::UpdateFolder { project_id, folder } => {
            let project = find_project_mut(config, &project_id)?;
            let target = project
                .folders
                .iter_mut()
                .find(|item| item.id == folder.id)
                .ok_or_else(|| not_found("folder"))?;
            *target = folder;
        }
        ConfigCommand::ReorderFolder {
            project_id,
            folder_id,
            new_index,
        } => {
            let project = find_project_mut(config, &project_id)?;
            reorder(&mut project.folders, &folder_id, new_index, "folder")?;
        }
        ConfigCommand::ArchiveFolder {
            project_id,
            folder_id,
        } => {
            let project = find_project_mut(config, &project_id)?;
            let target = project
                .folders
                .iter_mut()
                .find(|item| item.id == folder_id)
                .ok_or_else(|| not_found("folder"))?;
            target.archived = Nullable::Value(true);
        }
        ConfigCommand::CreateCommand {
            project_id,
            folder_id,
            command,
        } => {
            let folder = find_folder_mut(config, &project_id, &folder_id)?;
            ensure_new_id(
                folder.commands.iter().map(|item| item.id.as_str()),
                &command.id,
            )?;
            folder.commands.push(command);
        }
        ConfigCommand::UpdateCommand {
            project_id,
            folder_id,
            command,
        } => {
            let folder = find_folder_mut(config, &project_id, &folder_id)?;
            let target = folder
                .commands
                .iter_mut()
                .find(|item| item.id == command.id)
                .ok_or_else(|| not_found("command"))?;
            *target = command;
        }
        ConfigCommand::ReorderCommand {
            project_id,
            folder_id,
            command_id,
            new_index,
        } => {
            let folder = find_folder_mut(config, &project_id, &folder_id)?;
            reorder(&mut folder.commands, &command_id, new_index, "command")?;
        }
        ConfigCommand::ArchiveCommand {
            project_id,
            folder_id,
            command_id,
        } => {
            let folder = find_folder_mut(config, &project_id, &folder_id)?;
            let target = folder
                .commands
                .iter_mut()
                .find(|item| item.id == command_id)
                .ok_or_else(|| not_found("command"))?;
            target.archived = Nullable::Value(true);
        }
        ConfigCommand::CreateSsh { connection } => {
            ensure_new_id(
                config.ssh_connections.iter().map(|item| item.id.as_str()),
                &connection.id,
            )?;
            config.ssh_connections.push(connection);
        }
        ConfigCommand::UpdateSsh { connection } => {
            let target = config
                .ssh_connections
                .iter_mut()
                .find(|item| item.id == connection.id)
                .ok_or_else(|| not_found("SSH host"))?;
            *target = connection;
        }
        ConfigCommand::ReorderSsh {
            connection_id,
            new_index,
        } => reorder(
            &mut config.ssh_connections,
            &connection_id,
            new_index,
            "SSH host",
        )?,
        ConfigCommand::ArchiveSsh { connection_id } => {
            let target = config
                .ssh_connections
                .iter_mut()
                .find(|item| item.id == connection_id)
                .ok_or_else(|| not_found("SSH host"))?;
            target.archived = Nullable::Value(true);
        }
        ConfigCommand::PatchSettings { patch } => config.apply_settings_patch(&patch),
    }
    Ok(())
}

fn find_project_mut<'a>(
    config: &'a mut AppConfig,
    id: &str,
) -> Result<&'a mut Project, ConfigError> {
    config
        .projects
        .iter_mut()
        .find(|item| item.id == id)
        .ok_or_else(|| not_found("project"))
}

fn find_folder_mut<'a>(
    config: &'a mut AppConfig,
    project_id: &str,
    folder_id: &str,
) -> Result<&'a mut ProjectFolder, ConfigError> {
    find_project_mut(config, project_id)?
        .folders
        .iter_mut()
        .find(|item| item.id == folder_id)
        .ok_or_else(|| not_found("folder"))
}

fn ensure_new_id<'a>(ids: impl Iterator<Item = &'a str>, id: &str) -> Result<(), ConfigError> {
    if super::model::validate_id(id).is_err() || ids.into_iter().any(|existing| existing == id) {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "new configuration item has an invalid or duplicate ID",
        ));
    }
    Ok(())
}

fn reorder<T>(items: &mut Vec<T>, id: &str, new_index: usize, kind: &str) -> Result<(), ConfigError>
where
    T: HasConfigId,
{
    let old_index = items
        .iter()
        .position(|item| item.config_id() == id)
        .ok_or_else(|| not_found(kind))?;
    if new_index >= items.len() {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "reorder target is outside the collection",
        ));
    }
    let item = items.remove(old_index);
    items.insert(new_index, item);
    Ok(())
}

trait HasConfigId {
    fn config_id(&self) -> &str;
}

impl HasConfigId for Project {
    fn config_id(&self) -> &str {
        &self.id
    }
}

impl HasConfigId for ProjectFolder {
    fn config_id(&self) -> &str {
        &self.id
    }
}

impl HasConfigId for RunCommand {
    fn config_id(&self) -> &str {
        &self.id
    }
}

impl HasConfigId for SSHConnection {
    fn config_id(&self) -> &str {
        &self.id
    }
}

fn not_found(_kind: &str) -> ConfigError {
    ConfigError::new(
        ConfigErrorKind::NotFound,
        "configuration item was not found",
    )
}

struct BoundFileMetadata {
    identity: FileIdentity,
    link_count: u64,
    is_regular_file: bool,
    is_reparse_point: bool,
}

#[cfg(windows)]
struct RootHandle {
    handle: windows::Win32::Foundation::HANDLE,
    identity: FileIdentity,
}

#[cfg(windows)]
impl Drop for RootHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(unix)]
struct RootHandle {
    file: fs::File,
    identity: FileIdentity,
}

#[cfg(not(any(windows, unix)))]
struct RootHandle {
    path: PathBuf,
    identity: FileIdentity,
}

fn open_root_handle(path: &Path, expected: &FileIdentity) -> Result<RootHandle, ConfigError> {
    let handle = open_root_handle_without_expected(path)?;
    if &handle.identity != expected {
        return Err(ConfigError::new(
            ConfigErrorKind::PathAlias,
            "approved isolated root identity changed",
        ));
    }
    Ok(handle)
}

#[cfg(windows)]
fn open_root_handle_without_expected(path: &Path) -> Result<RootHandle, ConfigError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAGS_AND_ATTRIBUTES,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let share = FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_GENERIC_READ.0
                | windows::Win32::Storage::FileSystem::FILE_GENERIC_WRITE.0
                | 0x0001_0000
                | 0x0000_0040,
            share,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(
                FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0,
            ),
            None,
        )
    }
    .map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "approved isolated root could not be opened without following links",
        )
    })?;

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let result = unsafe { GetFileInformationByHandle(handle, &mut information) };
    if result.is_err()
        || information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
        || information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
        }
        return Err(ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "approved isolated root is not a plain directory",
        ));
    }

    Ok(RootHandle {
        handle,
        identity: windows_identity(&information),
    })
}

#[cfg(unix)]
fn open_root_handle_without_expected(path: &Path) -> Result<RootHandle, ConfigError> {
    use std::os::unix::fs::OpenOptionsExt;

    let flags = unix_directory_flags().map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration root is on an unsupported Unix target",
        )
    })?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
        .map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::ProtectedPath,
                "approved isolated root could not be opened without following links",
            )
        })?;
    let metadata = file.metadata().map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "approved isolated root metadata could not be read",
        )
    })?;
    if !metadata.is_dir() {
        return Err(ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "approved isolated root is not a directory",
        ));
    }
    use std::os::unix::fs::MetadataExt;
    Ok(RootHandle {
        identity: FileIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        },
        file,
    })
}

#[cfg(not(any(windows, unix)))]
fn open_root_handle_without_expected(path: &Path) -> Result<RootHandle, ConfigError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "approved isolated root could not be opened",
        )
    })?;
    let identity = file_identity(path, &metadata).ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "approved isolated root identity could not be proven",
        )
    })?;
    Ok(RootHandle {
        path: path.to_path_buf(),
        identity,
    })
}

fn read_snapshot(
    path: &Path,
    root: &Path,
    root_handle: &RootHandle,
    deadline: OperationDeadline,
    decode_mode: ConfigDecodeMode,
) -> Result<ConfigSnapshot, ConfigError> {
    deadline.check()?;
    let file = match open_existing_relative_file(root_handle, root, path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let config = AppConfig::default();
            return Ok(ConfigSnapshot {
                revision: config.revision,
                config,
                fingerprint: None,
            });
        }
        Err(error) if is_no_follow_error(&error) => {
            return Err(ConfigError::new(
                ConfigErrorKind::PathAlias,
                "configuration final path is an aliased link",
            ));
        }
        Err(_) => {
            return Err(ConfigError::new(
                ConfigErrorKind::Io,
                "configuration file could not be read",
            ));
        }
    };
    let metadata = bound_file_metadata(&file).ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::PathAlias,
            "configuration file identity could not be proven",
        )
    })?;
    if metadata.is_reparse_point || !metadata.is_regular_file {
        return Err(ConfigError::new(
            ConfigErrorKind::PathAlias,
            "configuration final path is not a plain file",
        ));
    }
    if metadata.link_count != 1 {
        return Err(ConfigError::new(
            ConfigErrorKind::PathAlias,
            "configuration file identity is aliased",
        ));
    }

    let mut bytes = Vec::new();
    (&file)
        .take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            ConfigError::new(ConfigErrorKind::Io, "configuration file could not be read")
        })?;
    deadline.check()?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "input exceeds the configuration size limit",
        ));
    }
    let contents = std::str::from_utf8(&bytes)
        .map_err(|_| ConfigError::new(ConfigErrorKind::Parse, "configuration is not UTF-8"))?;
    let config = match decode_mode {
        ConfigDecodeMode::Strict => AppConfig::from_json_str(contents),
        ConfigDecodeMode::LegacyMigration => AppConfig::from_legacy_json_str(contents),
    }?;
    deadline.check()?;
    let revision = config.revision;
    Ok(ConfigSnapshot {
        config,
        revision,
        fingerprint: Some(FileFingerprint {
            identity: metadata.identity,
            digest: Sha256::digest(&bytes).into(),
        }),
    })
}

fn read_existing_snapshot(
    path: &Path,
    root: &Path,
    root_handle: &RootHandle,
    deadline: OperationDeadline,
) -> Result<ConfigSnapshot, ConfigError> {
    validate_final_path(path, root)?;
    let snapshot = read_snapshot(path, root, root_handle, deadline, ConfigDecodeMode::Strict)?;
    if snapshot.fingerprint.is_none() {
        return Err(ConfigError::new(
            ConfigErrorKind::NotFound,
            "import source was not found",
        ));
    }
    Ok(snapshot)
}

struct DestinationAdmission {
    fingerprint: Option<FileFingerprint>,
    held_file: Option<fs::File>,
}

fn destination_admission(
    path: &Path,
    root: &Path,
    root_handle: &RootHandle,
    deadline: OperationDeadline,
) -> Result<DestinationAdmission, ConfigError> {
    deadline.check()?;
    validate_final_path(path, root)?;
    let file = match open_destination_file(root_handle, root, path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(DestinationAdmission {
                fingerprint: None,
                held_file: None,
            })
        }
        Err(_) => {
            return Err(ConfigError::new(
                ConfigErrorKind::ExternalChange,
                "destination could not be read for replacement admission",
            ));
        }
    };
    let metadata = bound_file_metadata(&file).ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::PathAlias,
            "destination identity could not be proven",
        )
    })?;
    if metadata.is_reparse_point || !metadata.is_regular_file || metadata.link_count != 1 {
        return Err(ConfigError::new(
            ConfigErrorKind::PathAlias,
            "destination identity is aliased or not a plain file",
        ));
    }
    let mut bytes = Vec::new();
    (&file)
        .take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::ExternalChange,
                "destination could not be read for replacement admission",
            )
        })?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::new(
            ConfigErrorKind::ExternalChange,
            "destination exceeds the configuration size limit",
        ));
    }
    deadline.check()?;
    Ok(DestinationAdmission {
        fingerprint: Some(FileFingerprint {
            identity: metadata.identity,
            digest: Sha256::digest(bytes).into(),
        }),
        held_file: Some(file),
    })
}

fn destination_fingerprint(
    path: &Path,
    root: &Path,
    root_handle: &RootHandle,
    deadline: OperationDeadline,
) -> Result<Option<FileFingerprint>, ConfigError> {
    Ok(destination_admission(path, root, root_handle, deadline)?.fingerprint)
}

fn relative_components(root: &Path, path: &Path) -> Result<Vec<std::ffi::OsString>, ConfigError> {
    let root_components = root
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_os_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let path_components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_os_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if path_components.len() <= root_components.len()
        || !root_components
            .iter()
            .zip(&path_components)
            .all(|(left, right)| path_components_equal(left, right))
    {
        return Err(ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration path is not a child of the approved root",
        ));
    }
    Ok(path_components[root_components.len()..].to_vec())
}

fn path_components_equal(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

fn relative_leaf(components: &[std::ffi::OsString]) -> &std::ffi::OsStr {
    components
        .last()
        .map(std::ffi::OsString::as_os_str)
        .expect("validated relative path must have a final component")
}

fn is_no_follow_error(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        return error.raw_os_error() == Some(40);
    }
    #[cfg(windows)]
    {
        let _ = error;
        false
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = error;
        false
    }
}

#[cfg(windows)]
fn windows_identity(
    information: &windows::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION,
) -> FileIdentity {
    FileIdentity::Windows {
        volume: information.dwVolumeSerialNumber,
        index: ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64,
    }
}

#[cfg(windows)]
fn bound_file_metadata(file: &fs::File) -> Option<BoundFileMetadata> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(
            windows::Win32::Foundation::HANDLE(file.as_raw_handle() as *mut _),
            &mut information,
        )
        .ok()?;
    }
    Some(BoundFileMetadata {
        identity: windows_identity(&information),
        link_count: u64::from(information.nNumberOfLinks),
        is_regular_file: information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0,
        is_reparse_point: information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0,
    })
}

#[cfg(unix)]
fn bound_file_metadata(file: &fs::File) -> Option<BoundFileMetadata> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata().ok()?;
    Some(BoundFileMetadata {
        identity: FileIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        },
        link_count: metadata.nlink(),
        is_regular_file: metadata.is_file(),
        is_reparse_point: metadata.file_type().is_symlink(),
    })
}

#[cfg(not(any(windows, unix)))]
fn bound_file_metadata(file: &fs::File) -> Option<BoundFileMetadata> {
    let metadata = file.metadata().ok()?;
    Some(BoundFileMetadata {
        identity: FileIdentity::Windows {
            volume: 0,
            index: 0,
        },
        link_count: 1,
        is_regular_file: metadata.is_file(),
        is_reparse_point: false,
    })
}

#[cfg(windows)]
struct RelativeParent {
    handle: windows::Win32::Foundation::HANDLE,
    owned: bool,
}

#[cfg(windows)]
impl Drop for RelativeParent {
    fn drop(&mut self) {
        if self.owned {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(unix)]
struct RelativeParent {
    file: fs::File,
}

#[cfg(not(any(windows, unix)))]
struct RelativeParent {
    path: PathBuf,
}

fn open_relative_parent(
    root: &RootHandle,
    root_path: &Path,
    path: &Path,
) -> Result<RelativeParent, ConfigError> {
    let components = relative_components(root_path, path)?;
    let parent_components = &components[..components.len() - 1];

    #[cfg(windows)]
    {
        return windows_open_relative_parent(root, parent_components);
    }
    #[cfg(unix)]
    {
        return unix_open_relative_parent(root, parent_components);
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = root;
        let mut parent = root_path.to_path_buf();
        for component in parent_components {
            parent.push(component);
        }
        return Ok(RelativeParent { path: parent });
    }
}

fn open_existing_relative_file(
    root: &RootHandle,
    root_path: &Path,
    path: &Path,
) -> io::Result<fs::File> {
    let components = relative_components(root_path, path)
        .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error.to_string()))?;
    let parent = open_relative_parent(root, root_path, path)
        .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error.to_string()))?;
    let leaf = relative_leaf(&components);

    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::FILE_GENERIC_READ;
        return windows_open_relative_file(
            &parent,
            leaf,
            FILE_GENERIC_READ.0,
            0x0000_0007,
            1,
            0x0020_0060,
            0x0000_0080,
        );
    }
    #[cfg(unix)]
    {
        return unix_open_relative_file(&parent, leaf, unix_read_file_flags()?, 0);
    }
    #[cfg(not(any(windows, unix)))]
    {
        return fs::OpenOptions::new()
            .read(true)
            .open(Path::new(&parent.path).join(leaf));
    }
}

fn open_destination_file(root: &RootHandle, root_path: &Path, path: &Path) -> io::Result<fs::File> {
    let components = relative_components(root_path, path)
        .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error.to_string()))?;
    let parent = open_relative_parent(root, root_path, path)
        .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error.to_string()))?;
    let leaf = relative_leaf(&components);

    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::FILE_GENERIC_READ;
        // Hold the destination open with read-only sharing.  A
        // non-cooperating writer cannot replace, delete, or mutate this exact
        // file between the identity check and the held-handle rename.
        return windows_open_relative_file(
            &parent,
            leaf,
            FILE_GENERIC_READ.0 | 0x0001_0000,
            0x0000_0001,
            1,
            0x0020_0060,
            0x0000_0080,
        );
    }
    #[cfg(not(windows))]
    {
        open_existing_relative_file(root, root_path, path)
    }
}

fn create_relative_temp_file(
    root: &RootHandle,
    root_path: &Path,
    path: &Path,
) -> io::Result<(RelativeParent, fs::File)> {
    let components = relative_components(root_path, path)
        .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error.to_string()))?;
    let parent = open_relative_parent(root, root_path, path)
        .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error.to_string()))?;
    let leaf = relative_leaf(&components);

    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::{FILE_GENERIC_READ, FILE_GENERIC_WRITE};
        let file = windows_open_relative_file(
            &parent,
            leaf,
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0 | 0x0001_0000,
            0x0000_0007,
            2,
            0x0020_0060,
            0x0000_0100,
        )?;
        return Ok((parent, file));
    }
    #[cfg(unix)]
    {
        let file = unix_open_relative_file(&parent, leaf, unix_create_file_flags()?, 0o600)?;
        return Ok((parent, file));
    }
    #[cfg(not(any(windows, unix)))]
    {
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(Path::new(&parent.path).join(leaf))?;
        return Ok((parent, file));
    }
}

fn remove_relative_file(root: &RootHandle, root_path: &Path, path: &Path) -> io::Result<()> {
    let components = relative_components(root_path, path)
        .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error.to_string()))?;
    let parent = open_relative_parent(root, root_path, path)
        .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error.to_string()))?;
    let leaf = relative_leaf(&components);

    #[cfg(windows)]
    {
        return windows_remove_relative_file(&parent, leaf);
    }
    #[cfg(unix)]
    {
        return unix_remove_relative_file(&parent, leaf);
    }
    #[cfg(not(any(windows, unix)))]
    {
        return fs::remove_file(Path::new(&parent.path).join(leaf));
    }
}

#[cfg(not(windows))]
fn rename_relative_file(
    source: &fs::File,
    parent: &RelativeParent,
    destination_path: &Path,
    destination: &std::ffi::OsStr,
    temporary: &std::ffi::OsStr,
    replace_if_exists: bool,
) -> io::Result<()> {
    #[cfg(windows)]
    {
        let _ = (destination_path, temporary);
        return windows_rename_relative_file(source, parent, destination, replace_if_exists);
    }
    #[cfg(unix)]
    {
        let _ = (destination_path, replace_if_exists);
        return unix_rename_relative_file(parent, temporary, destination);
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (source, destination_path, replace_if_exists);
        return fs::rename(parent.path.join(temporary), parent.path.join(destination));
    }
}

fn sync_relative_parent(parent: &RelativeParent) -> io::Result<()> {
    #[cfg(windows)]
    {
        unsafe { windows::Win32::Storage::FileSystem::FlushFileBuffers(parent.handle) }
            .map_err(|error| io::Error::from_raw_os_error(error.code().0))
    }
    #[cfg(unix)]
    {
        parent.file.sync_all()
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = parent;
        Ok(())
    }
}

#[cfg(windows)]
#[repr(C)]
struct NtUnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

#[cfg(windows)]
#[repr(C)]
struct NtObjectAttributes {
    length: u32,
    root_directory: windows::Win32::Foundation::HANDLE,
    object_name: *mut NtUnicodeString,
    attributes: u32,
    security_descriptor: *mut core::ffi::c_void,
    security_quality_of_service: *mut core::ffi::c_void,
}

#[cfg(windows)]
#[repr(C)]
struct NtIoStatusBlock {
    status: i32,
    information: usize,
}

#[cfg(windows)]
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(
        file_handle: *mut windows::Win32::Foundation::HANDLE,
        desired_access: u32,
        object_attributes: *mut NtObjectAttributes,
        io_status_block: *mut NtIoStatusBlock,
        allocation_size: *mut i64,
        file_attributes: u32,
        share_access: u32,
        create_disposition: u32,
        create_options: u32,
        ea_buffer: *mut core::ffi::c_void,
        ea_length: u32,
    ) -> i32;

    fn RtlNtStatusToDosError(status: i32) -> u32;

    fn NtSetInformationFile(
        file_handle: windows::Win32::Foundation::HANDLE,
        io_status_block: *mut NtIoStatusBlock,
        file_information: *mut core::ffi::c_void,
        length: u32,
        file_information_class: u32,
    ) -> i32;
}

#[cfg(windows)]
fn nt_error(status: i32) -> io::Error {
    let code = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(code as i32)
}

#[cfg(windows)]
fn nt_open_child(
    parent: windows::Win32::Foundation::HANDLE,
    name: &std::ffi::OsStr,
    desired_access: u32,
    share_access: u32,
    create_disposition: u32,
    create_options: u32,
    file_attributes: u32,
) -> io::Result<windows::Win32::Foundation::HANDLE> {
    use std::os::windows::ffi::OsStrExt;

    let mut name: Vec<u16> = name.encode_wide().collect();
    if name.iter().any(|unit| *unit == 0) || name.len() > (u16::MAX as usize / 2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "relative configuration path component is invalid",
        ));
    }
    let mut unicode_name = NtUnicodeString {
        length: (name.len() * 2) as u16,
        maximum_length: (name.len() * 2) as u16,
        buffer: name.as_mut_ptr(),
    };
    let mut attributes = NtObjectAttributes {
        length: std::mem::size_of::<NtObjectAttributes>() as u32,
        root_directory: parent,
        object_name: &mut unicode_name,
        attributes: 0x0000_0040,
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut handle = windows::Win32::Foundation::HANDLE(std::ptr::null_mut());
    let mut status = NtIoStatusBlock {
        status: 0,
        information: 0,
    };
    let mut allocation_size = 0i64;
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access,
            &mut attributes,
            &mut status,
            &mut allocation_size,
            file_attributes,
            share_access,
            create_disposition,
            create_options,
            std::ptr::null_mut(),
            0,
        )
    };
    if status < 0 {
        Err(nt_error(status))
    } else {
        Ok(handle)
    }
}

#[cfg(windows)]
fn windows_handle_metadata(
    handle: windows::Win32::Foundation::HANDLE,
) -> io::Result<BoundFileMetadata> {
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle, &mut information) }
        .map_err(|error| io::Error::from_raw_os_error(error.code().0))?;
    Ok(BoundFileMetadata {
        identity: windows_identity(&information),
        link_count: u64::from(information.nNumberOfLinks),
        is_regular_file: information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0,
        is_reparse_point: information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0,
    })
}

#[cfg(windows)]
fn windows_open_relative_parent(
    root: &RootHandle,
    components: &[std::ffi::OsString],
) -> Result<RelativeParent, ConfigError> {
    let mut current = root.handle;
    let mut owned = false;
    for component in components {
        let next = nt_open_child(
            current,
            component,
            0x0013_01DF,
            0x0000_0007,
            1,
            0x0020_0021,
            0x0000_0010,
        )
        .map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::ProtectedPath,
                "configuration parent could not be opened without following links",
            )
        })?;
        let metadata = windows_handle_metadata(next).map_err(|_| {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(next);
            }
            ConfigError::new(
                ConfigErrorKind::ProtectedPath,
                "configuration parent identity could not be proven",
            )
        })?;
        if metadata.is_regular_file || metadata.is_reparse_point {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(next);
            }
            return Err(ConfigError::new(
                ConfigErrorKind::ProtectedPath,
                "configuration parent is not a plain directory",
            ));
        }
        if owned {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(current);
            }
        }
        current = next;
        owned = true;
    }
    Ok(RelativeParent {
        handle: current,
        owned,
    })
}

#[cfg(windows)]
fn windows_open_relative_file(
    parent: &RelativeParent,
    name: &std::ffi::OsStr,
    desired_access: u32,
    share_access: u32,
    create_disposition: u32,
    create_options: u32,
    file_attributes: u32,
) -> io::Result<fs::File> {
    let handle = nt_open_child(
        parent.handle,
        name,
        desired_access,
        share_access,
        create_disposition,
        create_options,
        file_attributes,
    )?;
    unsafe {
        Ok(std::os::windows::io::FromRawHandle::from_raw_handle(
            handle.0 as *mut _,
        ))
    }
}

#[cfg(windows)]
fn windows_rename_relative_file(
    source: &fs::File,
    parent: &RelativeParent,
    destination: &std::ffi::OsStr,
    replace_if_exists: bool,
) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Storage::FileSystem::FILE_RENAME_INFO;

    let name: Vec<u16> = destination.encode_wide().collect();
    // FILE_RENAME_INFO has trailing padding after its one-element array on
    // 64-bit Windows.  The kernel expects the variable-length payload to end
    // at the actual FileName offset, not at the padded Rust struct size.
    let name_offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let size = name_offset + name.len() * std::mem::size_of::<u16>();
    let mut buffer = vec![0u8; size];
    let info = buffer.as_mut_ptr() as *mut FILE_RENAME_INFO;
    unsafe {
        (*info).Anonymous.ReplaceIfExists = replace_if_exists;
        (*info).RootDirectory = parent.handle;
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        let mut io_status = NtIoStatusBlock {
            status: 0,
            information: 0,
        };
        let status = NtSetInformationFile(
            windows::Win32::Foundation::HANDLE(source.as_raw_handle() as *mut _),
            &mut io_status,
            info.cast(),
            size as u32,
            10, // FileRenameInformation in the native NT information classes.
        );
        if status < 0 {
            Err(nt_error(status))
        } else {
            Ok(())
        }
    }
}

#[cfg(windows)]
fn windows_remove_relative_file(parent: &RelativeParent, name: &std::ffi::OsStr) -> io::Result<()> {
    use windows::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
    };

    let file = windows_open_relative_file(
        parent,
        name,
        windows::Win32::Storage::FileSystem::FILE_GENERIC_READ.0 | 0x0001_0000,
        0x0000_0007,
        1,
        0x0020_0060,
        0x0000_0080,
    )?;
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    use std::os::windows::io::AsRawHandle;
    unsafe {
        SetFileInformationByHandle(
            windows::Win32::Foundation::HANDLE(file.as_raw_handle() as *mut _),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
        .map_err(|error| io::Error::from_raw_os_error(error.code().0))
    }
}

#[cfg(unix)]
fn unix_open_relative_parent(
    root: &RootHandle,
    components: &[std::ffi::OsString],
) -> Result<RelativeParent, ConfigError> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let flags = unix_directory_flags().map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration parent is on an unsupported Unix target",
        )
    })?;
    let mut current = root.file.try_clone().map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::Io,
            "configuration root handle could not be cloned",
        )
    })?;
    for component in components {
        let fd = unix_open_at(current.as_raw_fd(), component, flags, 0).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::ProtectedPath,
                "configuration parent could not be opened without following links",
            )
        })?;
        current = unsafe { fs::File::from_raw_fd(fd) };
    }
    Ok(RelativeParent { file: current })
}

#[cfg(unix)]
fn unix_open_relative_file(
    parent: &RelativeParent,
    name: &std::ffi::OsStr,
    flags: i32,
    mode: u32,
) -> io::Result<fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let fd = unix_open_at(parent.file.as_raw_fd(), name, flags, mode)?;
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn unix_rename_relative_file(
    parent: &RelativeParent,
    temporary: &std::ffi::OsStr,
    destination: &std::ffi::OsStr,
) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    let temporary = std::ffi::CString::new(temporary.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid temp name"))?;
    let destination = std::ffi::CString::new(destination.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid destination name"))?;
    let result = unsafe {
        renameat(
            parent.file.as_raw_fd(),
            temporary.as_ptr(),
            parent.file.as_raw_fd(),
            destination.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn unix_remove_relative_file(parent: &RelativeParent, name: &std::ffi::OsStr) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid temp name"))?;
    let result = unsafe { unlinkat(parent.file.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn unix_open_at(
    parent: std::os::fd::RawFd,
    name: &std::ffi::OsStr,
    flags: i32,
    mode: u32,
) -> io::Result<std::os::fd::RawFd> {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid path component"))?;
    let fd = unsafe { openat(parent, name.as_ptr(), flags, mode) };
    if fd >= 0 {
        Ok(fd)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn unix_directory_flags() -> io::Result<i32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        Ok(0x0001_0000 | 0x0002_0000 | 0x0008_0000)
    }
    #[cfg(target_os = "macos")]
    {
        Ok(0x0010_0000 | 0x0000_0100 | 0x0100_0000)
    }
    #[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
    {
        Ok(0x0001_0000 | 0x0000_0100 | 0x0010_0000)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported Unix target has no secure configuration open flags",
        ))
    }
}

#[cfg(unix)]
fn unix_read_file_flags() -> io::Result<i32> {
    Ok(unix_directory_flags()? & !unix_directory_only_flag()?)
}

#[cfg(unix)]
fn unix_create_file_flags() -> io::Result<i32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        Ok(unix_read_file_flags()? | 0x0001 | 0x0040 | 0x0080)
    }
    #[cfg(target_os = "macos")]
    {
        Ok(unix_read_file_flags()? | 0x0001 | 0x0002_00 | 0x0000_800)
    }
    #[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
    {
        Ok(unix_read_file_flags()? | 0x0001 | 0x0000_200 | 0x0000_800)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported Unix target has no secure configuration create flags",
        ))
    }
}

#[cfg(unix)]
fn unix_lock_file_flags() -> io::Result<i32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        Ok(unix_read_file_flags()? | 0x0002 | 0x0040)
    }
    #[cfg(target_os = "macos")]
    {
        Ok(unix_read_file_flags()? | 0x0002 | 0x0000_200)
    }
    #[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
    {
        Ok(unix_read_file_flags()? | 0x0002 | 0x0000_200)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported Unix target has no secure configuration lock flags",
        ))
    }
}

#[cfg(unix)]
fn unix_directory_only_flag() -> io::Result<i32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        Ok(0x0001_0000)
    }
    #[cfg(target_os = "macos")]
    {
        Ok(0x0010_0000)
    }
    #[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
    {
        Ok(0x0001_0000)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported Unix target has no secure directory flag",
        ))
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn openat(
        dirfd: std::os::fd::RawFd,
        pathname: *const std::ffi::c_char,
        flags: i32,
        mode: u32,
    ) -> std::os::fd::RawFd;
    fn renameat(
        olddirfd: std::os::fd::RawFd,
        oldpath: *const std::ffi::c_char,
        newdirfd: std::os::fd::RawFd,
        newpath: *const std::ffi::c_char,
    ) -> i32;
    fn unlinkat(dirfd: std::os::fd::RawFd, pathname: *const std::ffi::c_char, flags: i32) -> i32;
}

fn file_identity(path: &Path, metadata: &fs::Metadata) -> Option<FileIdentity> {
    #[cfg(windows)]
    {
        let _ = metadata;
        return windows_file_identity(path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return Some(FileIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        });
    }
    #[allow(unreachable_code)]
    {
        let _ = (path, metadata);
        None
    }
}

#[cfg(windows)]
fn windows_file_identity(path: &Path) -> Option<FileIdentity> {
    windows_file_metadata(path).map(|(identity, _)| identity)
}

#[cfg(windows)]
fn windows_file_link_count(path: &Path) -> Option<u32> {
    windows_file_metadata(path).map(|(_, link_count)| link_count)
}

#[cfg(windows)]
fn windows_file_metadata(path: &Path) -> Option<(FileIdentity, u32)> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_NORMAL,
        FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let share = FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            share,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(FILE_ATTRIBUTE_NORMAL.0 | FILE_FLAG_BACKUP_SEMANTICS.0),
            None,
        )
    }
    .ok()?;

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let result = unsafe { GetFileInformationByHandle(handle, &mut information) };
    let _ = unsafe { CloseHandle(handle) };
    result.ok()?;
    Some((
        FileIdentity::Windows {
            volume: information.dwVolumeSerialNumber,
            index: ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64,
        },
        information.nNumberOfLinks,
    ))
}

fn file_link_count(path: &Path, metadata: &fs::Metadata) -> Option<u64> {
    #[cfg(windows)]
    {
        let _ = metadata;
        return windows_file_link_count(path).map(u64::from);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = path;
        return Some(metadata.nlink());
    }
    #[allow(unreachable_code)]
    {
        let _ = (path, metadata);
        None
    }
}

fn atomic_write(
    path: &Path,
    bytes: &[u8],
    failure: Option<AtomicWriteFailure>,
    expected_destination: Option<&FileFingerprint>,
    root: &Path,
    root_handle: &RootHandle,
    deadline: OperationDeadline,
) -> Result<(), ConfigError> {
    // Validate the exact bytes that are about to become the canonical leaf
    // before touching the destination.  Typed values and legacy migration
    // both retain bounded opaque maps, but those maps must still obey the
    // strict wire shape used by every subsequent load.  A post-swap read is
    // too late: it could leave a valid original replaced by bytes the store
    // can no longer open.
    deadline.check()?;
    let candidate = std::str::from_utf8(bytes).map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::Parse,
            "configuration candidate is not UTF-8",
        )
    })?;
    AppConfig::from_json_str(candidate)?;
    deadline.check()?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "configuration exceeds the size limit",
        ));
    }
    validate_final_path(path, root)?;
    let temp = unique_temp_path_for(path)?;
    #[cfg(not(windows))]
    let temp_name = temp.file_name().ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::AtomicWrite,
            "temporary configuration path has no final component",
        )
    })?;
    let destination_name = path.file_name().ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::AtomicWrite,
            "configuration path has no final component",
        )
    })?;
    let mut created = false;
    let result = (|| -> Result<(), ConfigError> {
        deadline.check()?;
        recover_stale_temps(path, root, root_handle, deadline)?;
        let (parent, mut file) =
            create_relative_temp_file(root_handle, root, &temp).map_err(|_| {
                ConfigError::new(ConfigErrorKind::AtomicWrite, "atomic temp creation failed")
            })?;
        created = true;
        deadline.check()?;
        if matches!(failure, Some(AtomicWriteFailure::BeforeDirectorySync)) {
            return Err(ConfigError::new(
                ConfigErrorKind::AtomicWrite,
                "configuration directory durability could not be proven",
            ));
        }
        sync_relative_parent(&parent).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::AtomicWrite,
                "configuration directory durability could not be proven",
            )
        })?;
        deadline.check()?;
        file.write_all(bytes).map_err(|_| {
            ConfigError::new(ConfigErrorKind::AtomicWrite, "atomic temp write failed")
        })?;
        deadline.check()?;
        if matches!(failure, Some(AtomicWriteFailure::BeforeFlush)) {
            return Err(ConfigError::new(
                ConfigErrorKind::AtomicWrite,
                "atomic temp flush failed",
            ));
        }
        file.flush().map_err(|_| {
            ConfigError::new(ConfigErrorKind::AtomicWrite, "atomic temp flush failed")
        })?;
        deadline.check()?;
        file.sync_all().map_err(|_| {
            ConfigError::new(ConfigErrorKind::AtomicWrite, "atomic temp sync failed")
        })?;
        deadline.check()?;
        if matches!(failure, Some(AtomicWriteFailure::BeforeReplace)) {
            return Err(ConfigError::new(
                ConfigErrorKind::AtomicWrite,
                "atomic replacement was injected to fail",
            ));
        }
        if matches!(
            failure,
            Some(AtomicWriteFailure::ExternalWriterBeforeReplace)
        ) {
            fs::write(path, b"external writer mutation").map_err(|_| {
                ConfigError::new(
                    ConfigErrorKind::ExternalChange,
                    "external writer could not update destination",
                )
            })?;
        }
        deadline.check()?;
        let admission = destination_admission(path, root, root_handle, deadline)?;
        if admission.fingerprint.as_ref() != expected_destination {
            return Err(ConfigError::new(
                ConfigErrorKind::ExternalChange,
                "destination changed during replacement",
            ));
        }
        if matches!(
            failure,
            Some(AtomicWriteFailure::ExternalWriterAfterAdmission)
        ) {
            let _ = fs::write(path, b"external writer mutation");
            return Err(ConfigError::new(
                ConfigErrorKind::ExternalChange,
                "destination changed after replacement admission",
            ));
        }
        #[cfg(windows)]
        let rename_result = rename_destination_windows_cas(
            &file,
            &parent,
            path,
            destination_name,
            admission,
            failure,
            root,
            root_handle,
        );
        #[cfg(not(windows))]
        let rename_result = {
            let replace_if_exists = admission.fingerprint.is_some();
            let _held_destination = admission.held_file;
            rename_relative_file(
                &file,
                &parent,
                path,
                destination_name,
                temp_name,
                replace_if_exists,
            )
        };
        rename_result.map_err(|error| {
            ConfigError::new(
                ConfigErrorKind::AtomicWrite,
                if error.raw_os_error() == Some(87) {
                    "atomic rename rejected the relative destination"
                } else if error.raw_os_error() == Some(5) {
                    "atomic rename was denied by the destination handle"
                } else if error.raw_os_error() == Some(2) {
                    "atomic rename could not find the relative destination"
                } else if error.raw_os_error() == Some(3) {
                    "atomic rename could not find the held parent"
                } else if error.raw_os_error() == Some(32) {
                    "atomic rename hit a sharing violation"
                } else if error.raw_os_error() == Some(183) {
                    "atomic rename found an existing destination"
                } else {
                    "atomic rename failed"
                },
            )
        })?;
        deadline.check()?;
        sync_relative_parent(&parent).map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::AtomicWrite,
                "configuration directory durability could not be proven",
            )
        })?;
        Ok(())
    })();
    if created && result.is_err() {
        if remove_relative_file(root_handle, root, &temp).is_err() {
            return Err(ConfigError::new(
                ConfigErrorKind::AtomicWrite,
                "atomic temporary cleanup failed",
            ));
        }
    }
    result
}

#[cfg(windows)]
fn rename_destination_windows_cas(
    source: &fs::File,
    parent: &RelativeParent,
    path: &Path,
    destination: &std::ffi::OsStr,
    admission: DestinationAdmission,
    failure: Option<AtomicWriteFailure>,
    root: &Path,
    root_handle: &RootHandle,
) -> io::Result<()> {
    let Some(held_destination) = admission.held_file else {
        // A missing destination is admitted with ReplaceIfExists=false.  A
        // concurrent creator therefore causes an atomic collision instead of
        // silently replacing the creator's file.
        return windows_rename_relative_file(source, parent, destination, false);
    };

    let backup = unique_temp_path_for_kind(path, "backup").map_err(|_| {
        io::Error::new(
            io::ErrorKind::Other,
            "temporary destination backup name could not be generated",
        )
    })?;
    let backup_name = backup.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Other,
            "temporary destination backup has no final component",
        )
    })?;

    // Rename the exact admitted destination handle out of the canonical leaf.
    // The handle was opened without delete/write sharing, so a non-cooperating
    // post-check replacement cannot race this move.  The canonical leaf is
    // then installed with ReplaceIfExists=false, making a concurrent creator
    // fail closed rather than get overwritten.
    windows_rename_relative_file(&held_destination, parent, backup_name, false)?;
    let installed = if matches!(failure, Some(AtomicWriteFailure::InstallAfterBackup)) {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "atomic install was injected to fail after backup admission",
        ))
    } else {
        windows_rename_relative_file(source, parent, destination, false)
    };
    if let Err(install_error) = installed {
        // The only original is still held by this handle, now naming the
        // backup. Restore it before returning the install error. If restore
        // itself fails, leave the backup in place and surface that failure so
        // recovery can handle it on the next operation.
        let restored = windows_rename_relative_file(&held_destination, parent, destination, false);
        if let Err(restore_error) = restored {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("atomic install failed ({install_error}); original restore failed ({restore_error})"),
            ));
        }
        return Err(install_error);
    }

    drop(held_destination);
    remove_relative_file(root_handle, root, &backup)
}

fn recover_stale_temps(
    destination_path: &Path,
    root: &Path,
    root_handle: &RootHandle,
    deadline: OperationDeadline,
) -> Result<(), ConfigError> {
    let parent = destination_path.parent().ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::AtomicWrite,
            "configuration path has no recoverable parent",
        )
    })?;
    let destination_name = destination_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    let prefix = format!(".{destination_name}.");
    deadline.check()?;
    let entries = fs::read_dir(parent).map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::AtomicWrite,
            "configuration temporary files could not be discovered",
        )
    })?;

    for entry in entries {
        deadline.check()?;
        let entry = entry.map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::AtomicWrite,
                "configuration temporary files could not be discovered",
            )
        })?;
        let candidate = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(".tmp") {
            continue;
        }

        let is_recovery_backup = name.ends_with(".backup.tmp");
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) => {
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(ConfigError::new(
                        ConfigErrorKind::PathAlias,
                        "stale configuration temp is not a plain file",
                    ));
                }
                let file =
                    open_existing_relative_file(root_handle, root, &candidate).map_err(|_| {
                        ConfigError::new(
                            ConfigErrorKind::PathAlias,
                            "stale configuration temp could not be verified",
                        )
                    })?;
                let bound = bound_file_metadata(&file).ok_or_else(|| {
                    ConfigError::new(
                        ConfigErrorKind::PathAlias,
                        "stale configuration temp identity could not be proven",
                    )
                })?;
                if bound.link_count != 1 || bound.is_reparse_point {
                    return Err(ConfigError::new(
                        ConfigErrorKind::PathAlias,
                        "stale configuration temp is aliased",
                    ));
                }
                if is_recovery_backup {
                    let canonical = read_snapshot(
                        destination_path,
                        root,
                        root_handle,
                        deadline,
                        ConfigDecodeMode::Strict,
                    )?;
                    if canonical.fingerprint.is_some() {
                        // Once the canonical file is valid, an old backup is
                        // no longer the only recovery copy and can be
                        // cleaned up even if its payload is stale.
                        remove_relative_file(root_handle, root, &candidate).map_err(|_| {
                            ConfigError::new(
                                ConfigErrorKind::AtomicWrite,
                                "stale configuration recovery backup could not be removed",
                            )
                        })?;
                    } else {
                        // With no canonical file, the backup is the last copy
                        // of the original after the destination was moved
                        // aside. Validate its canonical payload before it can
                        // be restored; an arbitrary attacker-created
                        // `*.backup.tmp` must never become active config.
                        read_snapshot(
                            &candidate,
                            root,
                            root_handle,
                            deadline,
                            ConfigDecodeMode::Strict,
                        )?;
                        let parent = open_relative_parent(root_handle, root, destination_path)
                            .map_err(|_| {
                                ConfigError::new(
                                    ConfigErrorKind::AtomicWrite,
                                    "stale configuration recovery parent could not be opened",
                                )
                            })?;
                        let destination = destination_path.file_name().ok_or_else(|| {
                            ConfigError::new(
                                ConfigErrorKind::AtomicWrite,
                                "configuration destination has no final component",
                            )
                        })?;
                        #[cfg(windows)]
                        let restore =
                            windows_rename_relative_file(&file, &parent, destination, false);
                        #[cfg(not(windows))]
                        let restore = rename_relative_file(
                            &file,
                            &parent,
                            destination_path,
                            destination,
                            candidate.file_name().ok_or_else(|| {
                                io::Error::new(
                                    io::ErrorKind::Other,
                                    "stale recovery backup has no final component",
                                )
                            })?,
                            false,
                        );
                        restore.map_err(|_| {
                            ConfigError::new(
                                ConfigErrorKind::AtomicWrite,
                                "stale configuration recovery backup could not be restored",
                            )
                        })?;
                        sync_relative_parent(&parent).map_err(|_| {
                            ConfigError::new(
                                ConfigErrorKind::AtomicWrite,
                                "stale configuration recovery durability could not be proven",
                            )
                        })?;
                    }
                } else {
                    remove_relative_file(root_handle, root, &candidate).map_err(|_| {
                        ConfigError::new(
                            ConfigErrorKind::AtomicWrite,
                            "stale configuration temp could not be removed",
                        )
                    })?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(ConfigError::new(
                    ConfigErrorKind::PathAlias,
                    "stale configuration temp could not be verified",
                ));
            }
        }
    }
    Ok(())
}

fn acquire_config_lock(
    root: &RootHandle,
    root_path: &Path,
    lock_path: &Path,
    deadline: OperationDeadline,
) -> Result<ConfigFileLock, ConfigError> {
    loop {
        deadline.check().map_err(|_| {
            ConfigError::new(
                ConfigErrorKind::LockTimeout,
                "configuration lock could not be acquired before the deadline",
            )
        })?;
        match try_acquire_config_lock(root, root_path, lock_path) {
            Ok(Some(lock)) => return Ok(lock),
            Ok(None) if !deadline.remaining().is_zero() => {
                thread::sleep(Duration::from_millis(5).min(deadline.remaining()));
            }
            Ok(None) => {
                return Err(ConfigError::new(
                    ConfigErrorKind::LockTimeout,
                    "configuration lock could not be acquired before the deadline",
                ));
            }
            Err(error) => return Err(error),
        }
    }
}

fn lock_path_for(path: &Path) -> PathBuf {
    path.join(".config.lock")
}

#[cfg(windows)]
struct ConfigFileLock {
    handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl Drop for ConfigFileLock {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
fn try_acquire_config_lock(
    root: &RootHandle,
    root_path: &Path,
    lock_path: &Path,
) -> Result<Option<ConfigFileLock>, ConfigError> {
    use windows::Win32::Storage::FileSystem::{FILE_GENERIC_READ, FILE_GENERIC_WRITE};
    let components = relative_components(root_path, lock_path)?;
    let parent = open_relative_parent(root, root_path, lock_path)?;
    let handle = match nt_open_child(
        parent.handle,
        relative_leaf(&components),
        FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0 | 0x0001_0000,
        0,
        3,
        0x0020_0060,
        0x0000_0100,
    ) {
        Ok(handle) => handle,
        Err(error) if matches!(error.raw_os_error(), Some(32 | 33)) => return Ok(None),
        Err(_) => {
            return Err(ConfigError::new(
                ConfigErrorKind::Io,
                "configuration lock could not be opened",
            ));
        }
    };
    let metadata = windows_handle_metadata(handle).map_err(|_| {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
        }
        ConfigError::new(
            ConfigErrorKind::PathAlias,
            "configuration lock identity could not be proven",
        )
    })?;
    if metadata.is_reparse_point {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
        }
        return Err(ConfigError::new(
            ConfigErrorKind::PathAlias,
            "configuration lock is a link",
        ));
    }
    if !metadata.is_regular_file {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
        }
        return Err(ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration lock is not a plain file",
        ));
    }
    if metadata.link_count != 1 {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
        }
        return Err(ConfigError::new(
            ConfigErrorKind::PathAlias,
            "configuration lock has an aliased identity",
        ));
    }

    use windows::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
    };
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    let result = unsafe {
        SetFileInformationByHandle(
            handle,
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if result.is_err() {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
        }
        return Err(ConfigError::new(
            ConfigErrorKind::Io,
            "configuration lock could not be armed for cleanup",
        ));
    }
    Ok(Some(ConfigFileLock { handle }))
}

#[cfg(unix)]
struct ConfigFileLock {
    file: fs::File,
}

#[cfg(unix)]
fn try_acquire_config_lock(
    root: &RootHandle,
    root_path: &Path,
    lock_path: &Path,
) -> Result<Option<ConfigFileLock>, ConfigError> {
    use std::os::fd::AsRawFd;

    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    const LOCK_UN: i32 = 8;
    let lock_flags = unix_lock_file_flags().map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration lock is on an unsupported Unix target",
        )
    })?;
    let root_fd = root.file.as_raw_fd();
    let root_result = unsafe { flock(root_fd, LOCK_EX | LOCK_NB) };
    if root_result != 0 {
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(11 | 35)) {
            return Ok(None);
        }
        return Err(ConfigError::new(
            ConfigErrorKind::Io,
            "configuration root lock could not be acquired",
        ));
    }

    let components = relative_components(root_path, lock_path)?;
    let parent = match open_relative_parent(root, root_path, lock_path) {
        Ok(parent) => parent,
        Err(error) => {
            unsafe {
                let _ = flock(root_fd, LOCK_UN);
            }
            return Err(error);
        }
    };
    let file = match unix_open_relative_file(&parent, relative_leaf(&components), lock_flags, 0o600)
    {
        Ok(file) => file,
        Err(error) if is_no_follow_error(&error) => {
            unsafe {
                let _ = flock(root_fd, LOCK_UN);
            }
            return Err(ConfigError::new(
                ConfigErrorKind::PathAlias,
                "configuration lock is a link",
            ));
        }
        Err(_) => {
            unsafe {
                let _ = flock(root_fd, LOCK_UN);
            }
            return Err(ConfigError::new(
                ConfigErrorKind::Io,
                "configuration lock could not be opened",
            ));
        }
    };
    let metadata = bound_file_metadata(&file).ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::PathAlias,
            "configuration lock identity could not be proven",
        )
    });
    let metadata = match metadata {
        Ok(metadata) => metadata,
        Err(error) => {
            unsafe {
                let _ = flock(root_fd, LOCK_UN);
            }
            return Err(error);
        }
    };
    if metadata.is_reparse_point || !metadata.is_regular_file || metadata.link_count != 1 {
        unsafe {
            let _ = flock(root_fd, LOCK_UN);
        }
        return Err(ConfigError::new(
            if metadata.is_reparse_point || metadata.link_count != 1 {
                ConfigErrorKind::PathAlias
            } else {
                ConfigErrorKind::ProtectedPath
            },
            "configuration lock identity is aliased or not a plain file",
        ));
    }
    let result = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if result == 0 {
        Ok(Some(ConfigFileLock { file }))
    } else {
        let error = io::Error::last_os_error();
        unsafe {
            let _ = flock(root_fd, LOCK_UN);
        }
        if matches!(error.raw_os_error(), Some(11 | 35)) {
            Ok(None)
        } else {
            Err(ConfigError::new(
                ConfigErrorKind::Io,
                "configuration lock could not be acquired",
            ))
        }
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

#[cfg(not(any(windows, unix)))]
struct ConfigFileLock {
    file: fs::File,
}

#[cfg(not(any(windows, unix)))]
fn try_acquire_config_lock(
    _root: &RootHandle,
    _root_path: &Path,
    _lock_path: &Path,
) -> Result<Option<ConfigFileLock>, ConfigError> {
    Err(ConfigError::new(
        ConfigErrorKind::Io,
        "configuration locking is unsupported on this platform",
    ))
}

#[cfg(test)]
fn temp_path_for(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    let process_id = std::process::id();
    path.with_file_name(format!(".{name}.{process_id}.tmp"))
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);
static PROCESS_TEMP_TOKEN: OnceLock<[u8; 16]> = OnceLock::new();

fn process_temp_token() -> Result<u128, ConfigError> {
    if let Some(token) = PROCESS_TEMP_TOKEN.get() {
        return Ok(u128::from_le_bytes(*token));
    }

    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::AtomicWrite,
            "cryptographic temporary nonce could not be generated",
        )
    })?;
    let _ = PROCESS_TEMP_TOKEN.set(bytes);
    let token = PROCESS_TEMP_TOKEN.get().copied().unwrap_or(bytes);
    Ok(u128::from_le_bytes(token))
}

fn unique_temp_path_for(path: &Path) -> Result<PathBuf, ConfigError> {
    unique_temp_path_for_kind(path, "write")
}

fn unique_temp_path_for_kind(path: &Path, kind: &str) -> Result<PathBuf, ConfigError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    let process_id = std::process::id();
    let sequence = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    Ok(path.with_file_name(format!(
        ".{name}.{process_id}.{:x}.{sequence}.{kind}.tmp",
        process_temp_token()?,
    )))
}

fn absolute_path(path: &Path) -> Result<PathBuf, ConfigError> {
    if path.as_os_str().is_empty() {
        return Err(ConfigError::new(
            ConfigErrorKind::Validation,
            "configuration path is empty",
        ));
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| ConfigError::new(ConfigErrorKind::Io, "current directory is unavailable"))?
            .join(path)
    };
    let path = normalize_path(&path);
    reject_unsupported_path_operations(&path)?;
    Ok(path)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn reject_store_path(path: &Path, _root: &Path) -> Result<(), ConfigError> {
    let name = lower_file_name(path);
    if matches!(
        name.as_str(),
        ".config.lock" | "session.json" | "remote.json"
    ) || is_secret_path(path)
    {
        return Err(ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "protected configuration path is not supported",
        ));
    }

    if let Some(parent) = path.parent() {
        let config_path = parent.join("config.json");
        if config_path != path && same_path_or_file(path, &config_path) {
            return Err(ConfigError::new(
                ConfigErrorKind::PathAlias,
                "configuration path aliases the profile configuration",
            ));
        }
    }
    Ok(())
}

fn reject_transfer_path(path: &Path, protected_root: &Path) -> Result<(), ConfigError> {
    let name = lower_file_name(path);
    if !is_path_within(protected_root, path) {
        return Err(ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration transfer path is outside the isolated root",
        ));
    }
    if matches!(
        name.as_str(),
        ".config.lock" | "session.json" | "remote.json"
    ) || is_secret_path(path)
        || is_production_namespace(path)
    {
        return Err(ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "protected import or export path is not supported",
        ));
    }
    Ok(())
}

fn validate_path_with_protected_alias_classification(
    path: &Path,
    root: &Path,
) -> Result<(), ConfigError> {
    match validate_final_path(path, root) {
        Ok(()) => Ok(()),
        Err(error)
            if error.kind() == ConfigErrorKind::PathAlias
                && is_protected_alias(path, [Some(root)].into_iter()) =>
        {
            Err(ConfigError::new(
                ConfigErrorKind::ProtectedPath,
                "protected import or export path is not supported",
            ))
        }
        Err(error) => Err(error),
    }
}

fn reject_unsupported_path_operations(path: &Path) -> Result<(), ConfigError> {
    #[cfg(windows)]
    {
        use std::path::Prefix;

        for component in path.components() {
            if let Component::Prefix(prefix) = component {
                if matches!(prefix.kind(), Prefix::DeviceNS(_)) {
                    return Err(ConfigError::new(
                        ConfigErrorKind::ProtectedPath,
                        "unsupported configuration path operation",
                    ));
                }
            }
            let Component::Normal(name) = component else {
                continue;
            };
            let name = name.to_string_lossy();
            let stem = name
                .split('.')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            if name.chars().any(|character| {
                matches!(character, '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            }) || matches!(
                stem.as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
            ) {
                return Err(ConfigError::new(
                    ConfigErrorKind::ProtectedPath,
                    "unsupported configuration path operation",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn approved_isolated_root(path: &Path) -> Result<(PathBuf, FileIdentity), ConfigError> {
    let leaf = path.file_name().ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration path must name the canonical config.json leaf",
        )
    })?;
    if leaf != std::ffi::OsStr::new("config.json")
        && !(cfg!(windows) && leaf.to_string_lossy().eq_ignore_ascii_case("config.json"))
    {
        return Err(ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration path must name the canonical config.json leaf",
        ));
    }
    let root = path.parent().ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration path is outside the approved isolated root",
        )
    })?;
    if root.file_name().is_none_or(|name| {
        !name
            .to_string_lossy()
            .eq_ignore_ascii_case(ISOLATED_NAMESPACE)
    }) {
        return Err(ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration path is outside the approved isolated root",
        ));
    }
    let root = root.to_path_buf();
    let metadata = fs::symlink_metadata(&root).map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "approved isolated root could not be verified",
        )
    })?;
    if !metadata.is_dir() {
        return Err(ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "approved isolated root is not a directory",
        ));
    }
    let canonical = fs::canonicalize(&root).map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "approved isolated root could not be resolved",
        )
    })?;
    if !paths_equal(&canonical, &root) {
        return Err(ConfigError::new(
            ConfigErrorKind::PathAlias,
            "approved isolated root identity could not be proven",
        ));
    }
    let root_handle = open_root_handle_without_expected(&canonical)?;
    Ok((canonical, root_handle.identity.clone()))
}

fn validate_final_path(path: &Path, root: &Path) -> Result<(), ConfigError> {
    if !is_path_within(root, path) {
        return Err(ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration path is outside the approved isolated root",
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration path has no verifiable parent",
        )
    })?;
    let parent_metadata = fs::symlink_metadata(parent).map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration parent identity could not be proven",
        )
    })?;
    if !parent_metadata.is_dir() || file_identity(parent, &parent_metadata).is_none() {
        return Err(ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration parent identity could not be proven",
        ));
    }
    let canonical_parent = fs::canonicalize(parent).map_err(|_| {
        ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration parent could not be resolved",
        )
    })?;
    if !paths_equal(&canonical_parent, parent) {
        let kind = if is_path_within(root, &canonical_parent) {
            ConfigErrorKind::PathAlias
        } else {
            ConfigErrorKind::ProtectedPath
        };
        return Err(ConfigError::new(
            kind,
            "configuration parent is an aliased path",
        ));
    }
    if !is_path_within(root, &canonical_parent) {
        return Err(ConfigError::new(
            ConfigErrorKind::ProtectedPath,
            "configuration parent is outside the approved isolated root",
        ));
    }

    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(ConfigError::new(
                    ConfigErrorKind::PathAlias,
                    "configuration final path is a symlink",
                ));
            }
            if !metadata.is_file() {
                return Err(ConfigError::new(
                    ConfigErrorKind::ProtectedPath,
                    "configuration final path is not a file",
                ));
            }
            let canonical_path = fs::canonicalize(path).map_err(|_| {
                ConfigError::new(
                    ConfigErrorKind::ProtectedPath,
                    "configuration final path could not be resolved",
                )
            })?;
            if !paths_equal(&canonical_path, path) {
                let kind = if is_path_within(root, &canonical_path) {
                    ConfigErrorKind::PathAlias
                } else {
                    ConfigErrorKind::ProtectedPath
                };
                return Err(ConfigError::new(
                    kind,
                    "configuration final path is an aliased path",
                ));
            }
            if !is_path_within(root, &canonical_path) {
                return Err(ConfigError::new(
                    ConfigErrorKind::ProtectedPath,
                    "configuration final path is outside the approved isolated root",
                ));
            }
            if file_identity(
                path,
                &fs::metadata(path).map_err(|_| {
                    ConfigError::new(
                        ConfigErrorKind::ProtectedPath,
                        "configuration final identity could not be proven",
                    )
                })?,
            )
            .is_none()
                || file_link_count(path, &metadata) != Some(1)
            {
                return Err(ConfigError::new(
                    ConfigErrorKind::PathAlias,
                    "configuration final identity is aliased or unverifiable",
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => {
            return Err(ConfigError::new(
                ConfigErrorKind::ProtectedPath,
                "configuration final identity could not be proven",
            ));
        }
    }
    Ok(())
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    path_key(left) == path_key(right)
}

fn path_key(path: &Path) -> String {
    let mut value = path.to_string_lossy().replace('\\', "/");
    if let Some(stripped) = value.strip_prefix("//?/") {
        value = stripped.to_string();
    }
    if cfg!(windows) {
        value.make_ascii_lowercase();
    }
    value
}

fn is_path_within(root: &Path, path: &Path) -> bool {
    let root = path_key(root);
    let path = path_key(path);
    path == root || path.starts_with(&(root + "/"))
}

fn lower_file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn is_secret_path(path: &Path) -> bool {
    let name = lower_file_name(path);
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let normalized_name: String = name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect();
    name.contains("secret")
        || name.contains("password")
        || name.contains("private-key")
        || name.contains("private_key")
        || normalized_name.contains("privatekey")
        || matches!(extension.as_str(), "pem" | "ppk" | "key")
}

fn is_protected_alias<'a>(path: &Path, roots: impl Iterator<Item = Option<&'a Path>>) -> bool {
    let mut visited = HashSet::new();
    roots.flatten().map(normalize_path).any(|root| {
        if !root.exists() || !visited.insert(root.clone()) {
            return false;
        }
        let mut pending = vec![(root, 0usize)];
        let mut scanned = 0usize;
        while let Some((directory, depth)) = pending.pop() {
            let Ok(entries) = fs::read_dir(&directory) else {
                return true;
            };
            for entry in entries {
                scanned += 1;
                if scanned > MAX_ALIAS_ENTRIES {
                    return true;
                }
                let Ok(entry) = entry else {
                    return true;
                };
                let candidate = entry.path();
                if candidate == path {
                    continue;
                }
                let protected_name = {
                    let name = lower_file_name(&candidate);
                    matches!(name.as_str(), "session.json" | "remote.json")
                        || is_secret_path(&candidate)
                        || is_production_namespace(&candidate)
                };
                if protected_name && same_path_or_file(path, &candidate) {
                    return true;
                }
                if depth < MAX_ALIAS_DEPTH
                    && fs::symlink_metadata(&candidate)
                        .map(|metadata| metadata.file_type().is_dir())
                        .unwrap_or(false)
                {
                    pending.push((candidate, depth + 1));
                }
            }
        }
        false
    })
}

const MAX_ALIAS_DEPTH: usize = 16;
const MAX_ALIAS_ENTRIES: usize = 10_000;

const ISOLATED_NAMESPACE: &str = "com.userfirst.devmanager-native-next-dev";

fn is_production_namespace(path: &Path) -> bool {
    path.components().any(|component| {
        let value = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        value == "com.userfirst.devmanager"
            || (value.starts_with("com.userfirst.devmanager-") && value != ISOLATED_NAMESPACE)
    })
}

fn same_path_or_file(left: &Path, right: &Path) -> bool {
    if paths_equal(left, right) {
        return true;
    }
    let left_canonical = resolved_path(left);
    let right_canonical = resolved_path(right);
    if left_canonical.is_some() && left_canonical == right_canonical {
        return true;
    }
    match (fs::metadata(left), fs::metadata(right)) {
        (Ok(left_metadata), Ok(right_metadata)) => {
            match (
                file_identity(left, &left_metadata),
                file_identity(right, &right_metadata),
            ) {
                (Some(left_identity), Some(right_identity)) => left_identity == right_identity,
                _ => false,
            }
        }
        _ => false,
    }
}

fn resolved_path(path: &Path) -> Option<PathBuf> {
    if let Ok(path) = fs::canonicalize(path) {
        return Some(path);
    }

    let mut suffix = Vec::new();
    let mut existing = path.to_path_buf();
    while !existing.exists() {
        suffix.push(existing.file_name()?.to_os_string());
        existing.pop();
    }
    let mut resolved = fs::canonicalize(existing).ok()?;
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    Some(resolved)
}
