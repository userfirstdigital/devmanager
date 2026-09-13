use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use url::Url;

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

/// A repository-relative path kept as the bytes emitted by Git where the host
/// platform can represent them. Display conversion is deliberately lossy and
/// is never used for command arguments.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepoPath(Vec<u8>);

impl RepoPath {
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn from_path(path: PathBuf) -> Self {
        #[cfg(unix)]
        {
            return Self(path.as_os_str().as_bytes().to_vec());
        }

        #[cfg(not(unix))]
        {
            Self(path.to_string_lossy().as_bytes().to_vec())
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn display_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.0)
    }

    pub(crate) fn to_path_buf(&self) -> PathBuf {
        #[cfg(unix)]
        {
            return PathBuf::from(OsString::from_vec(self.0.clone()));
        }

        #[cfg(not(unix))]
        {
            PathBuf::from(self.display_lossy().as_ref())
        }
    }

    pub(crate) fn to_os_string(&self) -> OsString {
        self.to_path_buf().into_os_string()
    }

    pub fn validate_relative(&self) -> Result<(), String> {
        if self.0.is_empty() {
            return Err("repository path is empty".to_string());
        }
        if self.0.contains(&0) {
            return Err("repository path contains NUL".to_string());
        }

        let path = self.to_path_buf();
        if path.as_os_str().is_empty() {
            return Err("repository path is empty".to_string());
        }
        for component in path.components() {
            match component {
                Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                    return Err(format!(
                        "repository path is not contained: {}",
                        self.display_lossy()
                    ));
                }
                Component::CurDir => {
                    return Err(format!(
                        "repository path must name an exact entry: {}",
                        self.display_lossy()
                    ));
                }
                Component::Normal(_) => {}
            }
        }
        Ok(())
    }
}

impl From<&str> for RepoPath {
    fn from(value: &str) -> Self {
        Self::from_bytes(value.as_bytes().to_vec())
    }
}

impl From<String> for RepoPath {
    fn from(value: String) -> Self {
        Self::from_bytes(value.into_bytes())
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.display_lossy())
    }
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceIdentity {
    cwd: PathBuf,
    id: String,
}

impl fmt::Debug for WorkspaceIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WorkspaceIdentity(REDACTED)")
    }
}

impl WorkspaceIdentity {
    pub(crate) fn from_canonical_root(cwd: PathBuf) -> Self {
        let mut hasher = Sha256::new();
        update_os_string_digest(&mut hasher, &cwd.as_os_str().to_os_string());
        let id = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self { cwd, id }
    }

    #[cfg(test)]
    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CommitId(String);

pub type ObjectId = CommitId;

impl CommitId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for CommitId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for CommitId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BranchName(String);

impl BranchName {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_branch_name(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_branch_name(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("branch name must be non-empty".to_string());
    }
    if value == "@" || value.starts_with('-') {
        return Err("branch name is ambiguous or option-like".to_string());
    }
    if value.contains("..") || value.contains("@{") {
        return Err("branch name contains a forbidden ref sequence".to_string());
    }
    if value.ends_with('/') || value.ends_with('.') {
        return Err("branch name must not end with '/' or '.'".to_string());
    }
    if value.chars().any(|character| {
        character.is_control()
            || character.is_whitespace()
            || matches!(character, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    }) {
        return Err("branch name contains a forbidden character".to_string());
    }
    for component in value.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.starts_with('.')
            || component.to_ascii_lowercase().ends_with(".lock")
        {
            return Err("branch name contains an invalid component".to_string());
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum GitCapability {
    Stage,
    Unstage,
    Commit,
    Push,
    CreatePullRequest,
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RemoteTransport {
    Local,
    File,
    Https,
    Ssh,
}

impl fmt::Debug for RemoteTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Local => "Local",
            Self::File => "File",
            Self::Https => "Https",
            Self::Ssh => "Ssh",
        })
    }
}

impl fmt::Display for RemoteTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Local => "local",
            Self::File => "file",
            Self::Https => "https",
            Self::Ssh => "ssh",
        })
    }
}

/// The exact transport and endpoint a mutation is allowed to use.
///
/// Network endpoints are normalized without user-info. Local endpoints are
/// created by the Git command layer only after canonical containment and file
/// identity have been checked.
#[derive(Clone)]
pub(crate) struct RemoteEndpointLease {
    path: PathBuf,
    identity: String,
    handles: Arc<Vec<fs::File>>,
    ancestors: Arc<Vec<(PathBuf, String)>>,
}

impl fmt::Debug for RemoteEndpointLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteEndpointLease")
            .field("path", &"<endpoint>")
            .field("identity", &"<identity>")
            .field("handle_count", &self.handles.len())
            .field("ancestor_count", &self.ancestors.len())
            .finish()
    }
}

impl RemoteEndpointLease {
    pub(crate) fn new(
        path: PathBuf,
        identity: String,
        handles: Arc<Vec<fs::File>>,
        ancestors: Arc<Vec<(PathBuf, String)>>,
    ) -> Self {
        Self {
            path,
            identity,
            handles,
            ancestors,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn identity(&self) -> &str {
        &self.identity
    }

    pub(crate) fn handles(&self) -> &Arc<Vec<fs::File>> {
        &self.handles
    }

    pub(crate) fn ancestors(&self) -> &Arc<Vec<(PathBuf, String)>> {
        &self.ancestors
    }
}

#[derive(Clone)]
pub struct RemotePolicy {
    transport: RemoteTransport,
    endpoint: String,
    identity: Option<String>,
    endpoint_lease: Option<Arc<RemoteEndpointLease>>,
}

impl PartialEq for RemotePolicy {
    fn eq(&self, other: &Self) -> bool {
        self.transport == other.transport
            && self.endpoint == other.endpoint
            && self.identity == other.identity
    }
}

impl Eq for RemotePolicy {}

impl Hash for RemotePolicy {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.transport.hash(state);
        self.endpoint.hash(state);
        self.identity.hash(state);
    }
}

impl fmt::Debug for RemotePolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemotePolicy")
            .field("transport", &self.transport)
            .field("endpoint", &"<remote>")
            .field("identity", &self.identity.as_ref().map(|_| "<identity>"))
            .finish()
    }
}

impl RemotePolicy {
    pub fn https(value: &str) -> Result<Self, String> {
        Self::network(RemoteTransport::Https, value, "https", false)
    }

    pub fn ssh(value: &str) -> Result<Self, String> {
        if value.starts_with("ssh://") {
            Self::network(RemoteTransport::Ssh, value, "ssh", true)
        } else if valid_scp_like_remote(value) {
            Ok(Self {
                transport: RemoteTransport::Ssh,
                endpoint: value.to_string(),
                identity: None,
                endpoint_lease: None,
            })
        } else {
            Err("SSH remote must be an ssh URL or an exact SCP-style endpoint".to_string())
        }
    }

    pub fn transport(&self) -> RemoteTransport {
        self.transport
    }

    /// Returns the normalized endpoint to the in-crate Git executor only.
    /// Credentials are never retained; keeping this accessor crate-private
    /// prevents raw local paths/remote URLs from becoming transport data.
    pub(crate) fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(crate) fn local(
        transport: RemoteTransport,
        canonical_endpoint: String,
        identity: String,
    ) -> Result<Self, String> {
        Self::local_with_lease(transport, canonical_endpoint, identity, None)
    }

    pub(crate) fn local_with_lease(
        transport: RemoteTransport,
        canonical_endpoint: String,
        identity: String,
        endpoint_lease: Option<Arc<RemoteEndpointLease>>,
    ) -> Result<Self, String> {
        if !matches!(transport, RemoteTransport::Local | RemoteTransport::File) {
            return Err("local remote policy has an invalid transport".to_string());
        }
        Ok(Self {
            transport,
            endpoint: canonical_endpoint,
            identity: Some(identity),
            endpoint_lease,
        })
    }

    pub(crate) fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }

    pub(crate) fn endpoint_lease(&self) -> Option<&Arc<RemoteEndpointLease>> {
        self.endpoint_lease.as_ref()
    }

    pub(crate) fn digest_material(&self) -> String {
        format!(
            "{}\0{}\0{}",
            self.transport,
            self.endpoint,
            self.identity.as_deref().unwrap_or("")
        )
    }

    fn network(
        transport: RemoteTransport,
        value: &str,
        scheme: &str,
        allow_username: bool,
    ) -> Result<Self, String> {
        let parsed =
            Url::parse(value).map_err(|_| "remote endpoint is not a valid URL".to_string())?;
        if parsed.scheme() != scheme || parsed.host_str().is_none() {
            return Err(format!("remote endpoint must use {scheme}"));
        }
        if (!allow_username && !parsed.username().is_empty()) || parsed.password().is_some() {
            return Err(if allow_username {
                "remote endpoint must not contain a password".to_string()
            } else {
                "remote endpoint must not contain credentials".to_string()
            });
        }
        if value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(
                "remote endpoint contains invalid whitespace or control characters".to_string(),
            );
        }
        let mut normalized = parsed;
        if !allow_username {
            let _ = normalized.set_username("");
        }
        let _ = normalized.set_password(None);
        Ok(Self {
            transport,
            endpoint: normalized.to_string(),
            identity: None,
            endpoint_lease: None,
        })
    }
}

fn valid_scp_like_remote(value: &str) -> bool {
    if value.is_empty()
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        || value.starts_with('-')
    {
        return false;
    }
    let Some((authority, path)) = value.split_once(':') else {
        return false;
    };
    let Some((user, host)) = authority.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && !host.is_empty()
        && !host.contains('@')
        && !host.starts_with('-')
        && !path.is_empty()
        && !authority.contains('/')
        && !path.starts_with('/')
}

impl fmt::Display for BranchName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FileState {
    #[default]
    Unchanged,
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Unmerged,
    Untracked,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflict,
    Submodule,
    Unknown,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SubmoduleState {
    pub commit_changed: bool,
    pub worktree_modified: bool,
    pub untracked: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusEntry {
    pub path: RepoPath,
    pub original_path: Option<RepoPath>,
    pub kind: StatusKind,
    pub index: FileState,
    pub worktree: FileState,
    pub raw_xy: [u8; 2],
    pub rename_score: Option<u8>,
    pub submodule: Option<SubmoduleState>,
}

impl StatusEntry {
    pub fn is_staged(&self) -> bool {
        self.index != FileState::Unchanged
    }

    pub fn is_unstaged(&self) -> bool {
        self.worktree != FileState::Unchanged
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoFingerprint {
    pub head: Option<ObjectId>,
    pub status_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryStatus {
    pub head: Option<ObjectId>,
    pub branch: Option<BranchName>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub entries: Vec<StatusEntry>,
    pub is_detached: bool,
    pub fingerprint: RepoFingerprint,
}

impl RepositoryStatus {
    pub fn entry(&self, path: &str) -> Option<&StatusEntry> {
        self.entries
            .iter()
            .find(|entry| entry.path.as_bytes() == path.as_bytes())
    }

    pub fn conflicts(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.kind == StatusKind::Conflict)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct StatusPlan {
    workspace: WorkspaceIdentity,
    pub max_bytes: usize,
    arguments: Vec<OsString>,
}

impl fmt::Debug for StatusPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StatusPlan")
            .field("workspace", &"<workspace>")
            .field("max_bytes", &self.max_bytes)
            .field("argument_count", &self.arguments.len())
            .finish()
    }
}

impl StatusPlan {
    pub(crate) fn new(
        workspace: WorkspaceIdentity,
        max_bytes: usize,
        arguments: Vec<OsString>,
    ) -> Self {
        Self {
            workspace,
            max_bytes,
            arguments,
        }
    }

    pub fn workspace(&self) -> &WorkspaceIdentity {
        &self.workspace
    }

    #[cfg(test)]
    pub(crate) fn arguments(&self) -> Vec<String> {
        display_arguments(&self.arguments)
    }

    pub(crate) fn raw_arguments(&self) -> &[OsString] {
        &self.arguments
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct DiffPlan {
    workspace: WorkspaceIdentity,
    pub staged: bool,
    pub max_bytes: usize,
    arguments: Vec<OsString>,
}

impl fmt::Debug for DiffPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiffPlan")
            .field("workspace", &"<workspace>")
            .field("staged", &self.staged)
            .field("max_bytes", &self.max_bytes)
            .field("argument_count", &self.arguments.len())
            .finish()
    }
}

impl DiffPlan {
    pub(crate) fn new(
        workspace: WorkspaceIdentity,
        staged: bool,
        max_bytes: usize,
        arguments: Vec<OsString>,
    ) -> Self {
        Self {
            workspace,
            staged,
            max_bytes,
            arguments,
        }
    }

    pub fn workspace(&self) -> &WorkspaceIdentity {
        &self.workspace
    }

    #[cfg(test)]
    pub(crate) fn arguments(&self) -> Vec<String> {
        display_arguments(&self.arguments)
    }

    pub(crate) fn raw_arguments(&self) -> &[OsString] {
        &self.arguments
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ReviewPlan {
    workspace: WorkspaceIdentity,
    pub staged: bool,
    pub max_bytes: usize,
    arguments: Vec<OsString>,
}

impl fmt::Debug for ReviewPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReviewPlan")
            .field("workspace", &"<workspace>")
            .field("staged", &self.staged)
            .field("max_bytes", &self.max_bytes)
            .field("argument_count", &self.arguments.len())
            .finish()
    }
}

impl ReviewPlan {
    pub(crate) fn new(
        workspace: WorkspaceIdentity,
        staged: bool,
        max_bytes: usize,
        arguments: Vec<OsString>,
    ) -> Self {
        Self {
            workspace,
            staged,
            max_bytes,
            arguments,
        }
    }

    pub fn workspace(&self) -> &WorkspaceIdentity {
        &self.workspace
    }

    #[cfg(test)]
    pub(crate) fn arguments(&self) -> Vec<String> {
        display_arguments(&self.arguments)
    }

    pub(crate) fn raw_arguments(&self) -> &[OsString] {
        &self.arguments
    }
}

pub(crate) trait MutationPlan {
    fn workspace(&self) -> &WorkspaceIdentity;
    fn expected_fingerprint(&self) -> &RepoFingerprint;
    fn capability(&self) -> GitCapability;
    fn arguments_for_digest(&self) -> &[OsString];

    fn remote_policy(&self) -> Option<&RemotePolicy> {
        None
    }

    fn remote_name(&self) -> Option<&str> {
        None
    }

    fn plan_digest(&self) -> String {
        mutation_digest(
            self.workspace(),
            self.capability(),
            self.expected_fingerprint(),
            self.arguments_for_digest(),
            self.remote_policy(),
        )
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct StagePlan {
    workspace: WorkspaceIdentity,
    pub files: Vec<RepoPath>,
    pub expected: RepoFingerprint,
    arguments: Vec<OsString>,
}

impl fmt::Debug for StagePlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StagePlan")
            .field("workspace", &"<workspace>")
            .field("file_count", &self.files.len())
            .field("argument_count", &self.arguments.len())
            .finish()
    }
}

impl StagePlan {
    pub(crate) fn new(
        workspace: WorkspaceIdentity,
        files: Vec<RepoPath>,
        expected: RepoFingerprint,
        arguments: Vec<OsString>,
    ) -> Self {
        Self {
            workspace,
            files,
            expected,
            arguments,
        }
    }

    pub fn workspace(&self) -> &WorkspaceIdentity {
        &self.workspace
    }

    #[cfg(test)]
    pub(crate) fn arguments(&self) -> Vec<String> {
        display_arguments(&self.arguments)
    }

    pub(crate) fn raw_arguments(&self) -> &[OsString] {
        &self.arguments
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct UnstagePlan {
    workspace: WorkspaceIdentity,
    pub files: Vec<RepoPath>,
    pub expected: RepoFingerprint,
    arguments: Vec<OsString>,
}

impl fmt::Debug for UnstagePlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UnstagePlan")
            .field("workspace", &"<workspace>")
            .field("file_count", &self.files.len())
            .field("argument_count", &self.arguments.len())
            .finish()
    }
}

impl UnstagePlan {
    pub(crate) fn new(
        workspace: WorkspaceIdentity,
        files: Vec<RepoPath>,
        expected: RepoFingerprint,
        arguments: Vec<OsString>,
    ) -> Self {
        Self {
            workspace,
            files,
            expected,
            arguments,
        }
    }

    pub fn workspace(&self) -> &WorkspaceIdentity {
        &self.workspace
    }

    #[cfg(test)]
    pub(crate) fn arguments(&self) -> Vec<String> {
        display_arguments(&self.arguments)
    }

    pub(crate) fn raw_arguments(&self) -> &[OsString] {
        &self.arguments
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct CommitPlan {
    workspace: WorkspaceIdentity,
    pub files: Vec<RepoPath>,
    pub message: String,
    pub expected: RepoFingerprint,
    arguments: Vec<OsString>,
}

impl fmt::Debug for CommitPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommitPlan")
            .field("workspace", &"<workspace>")
            .field("file_count", &self.files.len())
            .field("message_bytes", &self.message.len())
            .field("argument_count", &self.arguments.len())
            .finish()
    }
}

impl CommitPlan {
    pub(crate) fn new(
        workspace: WorkspaceIdentity,
        files: Vec<RepoPath>,
        message: String,
        expected: RepoFingerprint,
        arguments: Vec<OsString>,
    ) -> Self {
        Self {
            workspace,
            files,
            message,
            expected,
            arguments,
        }
    }

    pub fn workspace(&self) -> &WorkspaceIdentity {
        &self.workspace
    }

    #[cfg(test)]
    pub(crate) fn arguments(&self) -> Vec<String> {
        display_arguments(&self.arguments)
    }

    pub(crate) fn raw_arguments(&self) -> &[OsString] {
        &self.arguments
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct PushPlan {
    workspace: WorkspaceIdentity,
    pub remote: String,
    pub branch: BranchName,
    pub set_upstream: bool,
    pub expected: RepoFingerprint,
    remote_policy: RemotePolicy,
    arguments: Vec<OsString>,
}

impl PushPlan {
    pub(crate) fn new(
        workspace: WorkspaceIdentity,
        remote: String,
        branch: BranchName,
        set_upstream: bool,
        expected: RepoFingerprint,
        remote_policy: RemotePolicy,
        arguments: Vec<OsString>,
    ) -> Self {
        Self {
            workspace,
            remote,
            branch,
            set_upstream,
            expected,
            remote_policy,
            arguments,
        }
    }

    pub fn remote_policy(&self) -> &RemotePolicy {
        &self.remote_policy
    }

    pub fn workspace(&self) -> &WorkspaceIdentity {
        &self.workspace
    }

    #[cfg(test)]
    pub(crate) fn arguments(&self) -> Vec<String> {
        display_arguments(&self.arguments)
    }

    pub(crate) fn raw_arguments(&self) -> &[OsString] {
        &self.arguments
    }
}

impl fmt::Debug for PushPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PushPlan")
            .field("workspace", &"<workspace>")
            .field("remote", &"<remote>")
            .field("transport", &self.remote_policy.transport())
            .field("branch", &"<branch>")
            .field("set_upstream", &self.set_upstream)
            .field("argument_count", &self.arguments.len())
            .finish()
    }
}

impl MutationPlan for StagePlan {
    fn workspace(&self) -> &WorkspaceIdentity {
        &self.workspace
    }

    fn expected_fingerprint(&self) -> &RepoFingerprint {
        &self.expected
    }

    fn capability(&self) -> GitCapability {
        GitCapability::Stage
    }

    fn arguments_for_digest(&self) -> &[OsString] {
        &self.arguments
    }
}

impl MutationPlan for UnstagePlan {
    fn workspace(&self) -> &WorkspaceIdentity {
        &self.workspace
    }

    fn expected_fingerprint(&self) -> &RepoFingerprint {
        &self.expected
    }

    fn capability(&self) -> GitCapability {
        GitCapability::Unstage
    }

    fn arguments_for_digest(&self) -> &[OsString] {
        &self.arguments
    }
}

impl MutationPlan for CommitPlan {
    fn workspace(&self) -> &WorkspaceIdentity {
        &self.workspace
    }

    fn expected_fingerprint(&self) -> &RepoFingerprint {
        &self.expected
    }

    fn capability(&self) -> GitCapability {
        GitCapability::Commit
    }

    fn arguments_for_digest(&self) -> &[OsString] {
        &self.arguments
    }
}

impl MutationPlan for PushPlan {
    fn workspace(&self) -> &WorkspaceIdentity {
        &self.workspace
    }

    fn expected_fingerprint(&self) -> &RepoFingerprint {
        &self.expected
    }

    fn capability(&self) -> GitCapability {
        GitCapability::Push
    }

    fn arguments_for_digest(&self) -> &[OsString] {
        &self.arguments
    }

    fn remote_policy(&self) -> Option<&RemotePolicy> {
        Some(&self.remote_policy)
    }

    fn remote_name(&self) -> Option<&str> {
        Some(&self.remote)
    }
}

#[cfg(test)]
fn display_arguments(arguments: &[OsString]) -> Vec<String> {
    arguments
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect()
}

fn mutation_digest(
    workspace: &WorkspaceIdentity,
    capability: GitCapability,
    expected: &RepoFingerprint,
    arguments: &[OsString],
    remote_policy: Option<&RemotePolicy>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(workspace.id.as_bytes());
    hasher.update([match capability {
        GitCapability::Stage => 1,
        GitCapability::Unstage => 2,
        GitCapability::Commit => 3,
        GitCapability::Push => 4,
        GitCapability::CreatePullRequest => 5,
    }]);
    if let Some(head) = &expected.head {
        hasher.update(head.as_str().as_bytes());
    }
    hasher.update(expected.status_digest.as_bytes());
    hasher.update((arguments.len() as u64).to_le_bytes());
    for argument in arguments {
        update_os_string_digest(&mut hasher, argument);
    }
    if let Some(remote_policy) = remote_policy {
        let material = remote_policy.digest_material();
        hasher.update((material.len() as u64).to_le_bytes());
        hasher.update(material.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffSide {
    Old,
    New,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffLineKind {
    Context,
    Add,
    Delete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub content: Vec<u8>,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffHunk {
    pub header: Vec<u8>,
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffChangeKind {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffMarker {
    Binary,
    Truncated,
    NoNewlineAtEndOfFile,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffFile {
    pub old_path: Option<RepoPath>,
    pub new_path: Option<RepoPath>,
    pub old_blob: Option<ObjectId>,
    pub new_blob: Option<ObjectId>,
    pub change: DiffChangeKind,
    pub is_binary: bool,
    pub hunks: Vec<DiffHunk>,
    pub markers: Vec<DiffMarker>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffContinuation {
    pub next_offset: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffDocument {
    pub files: Vec<DiffFile>,
    pub truncated: bool,
    pub bytes_read: usize,
    pub continuation: Option<DiffContinuation>,
    pub markers: Vec<DiffMarker>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewAnchor {
    pub path: RepoPath,
    pub base_blob: ObjectId,
    pub side: DiffSide,
    pub line: u32,
}

impl ReviewAnchor {
    pub fn new(path: RepoPath, base_blob: ObjectId, side: DiffSide, line: u32) -> Self {
        Self {
            path,
            base_blob,
            side,
            line,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        self.path.validate_relative()?;
        if self.line == 0 {
            return Err("review anchor line must be greater than zero".to_string());
        }
        Ok(())
    }
}

/// Hash the platform's exact argument representation. Lossy display text is
/// reserved for diagnostics and must never be the authority binding for a
/// command, because distinct non-UTF-8 arguments can otherwise collide.
pub(crate) fn update_os_string_digest(hasher: &mut Sha256, argument: &OsString) {
    #[cfg(unix)]
    {
        let bytes = argument.as_os_str().as_bytes();
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }

    #[cfg(windows)]
    {
        let bytes = argument
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }

    #[cfg(not(any(unix, windows)))]
    {
        let bytes = argument.to_string_lossy();
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes.as_bytes());
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewComment {
    pub anchor: ReviewAnchor,
    pub body: String,
}

impl ReviewComment {
    pub fn new(anchor: ReviewAnchor, body: impl Into<String>) -> Result<Self, String> {
        anchor.validate()?;
        let body = body.into();
        if body.trim().is_empty() || body.contains('\0') {
            return Err("review comment must be non-empty and NUL-free".to_string());
        }
        if body.len() > 64 * 1024 {
            return Err("review comment exceeds the 64KiB bound".to_string());
        }
        Ok(Self { anchor, body })
    }
}

fn parse_object_id(bytes: &[u8]) -> Option<ObjectId> {
    if bytes.is_empty() || bytes == b"(initial)" || bytes.iter().all(|byte| *byte == b'0') {
        None
    } else {
        Some(ObjectId::from(String::from_utf8_lossy(bytes).into_owned()))
    }
}

fn split_record<'a>(record: &'a [u8], fields: usize) -> Option<Vec<&'a [u8]>> {
    let mut result = Vec::with_capacity(fields);
    let mut start = 0;
    for (index, byte) in record.iter().enumerate() {
        if *byte == b' ' && result.len() + 1 < fields {
            result.push(&record[start..index]);
            start = index + 1;
        }
    }
    result.push(&record[start..]);
    (result.len() == fields).then_some(result)
}

fn state_from_code(code: u8) -> FileState {
    match code {
        b'.' => FileState::Unchanged,
        b'M' => FileState::Modified,
        b'A' => FileState::Added,
        b'D' => FileState::Deleted,
        b'R' => FileState::Renamed,
        b'C' => FileState::Copied,
        b'T' => FileState::TypeChanged,
        b'U' => FileState::Unmerged,
        b'?' => FileState::Untracked,
        _ => FileState::Unknown,
    }
}

fn kind_from_xy(xy: &[u8], submodule: Option<&SubmoduleState>) -> StatusKind {
    if submodule.is_some() {
        return StatusKind::Submodule;
    }
    if xy.contains(&b'U') {
        return StatusKind::Conflict;
    }
    let code = xy.iter().copied().find(|code| *code != b'.');
    match code {
        Some(b'M') => StatusKind::Modified,
        Some(b'A') => StatusKind::Added,
        Some(b'D') => StatusKind::Deleted,
        Some(b'R') => StatusKind::Renamed,
        Some(b'C') => StatusKind::Copied,
        Some(b'T') => StatusKind::TypeChanged,
        _ => StatusKind::Unknown,
    }
}

fn parse_submodule(bytes: &[u8]) -> Option<SubmoduleState> {
    (bytes.first() == Some(&b'S')).then(|| SubmoduleState {
        commit_changed: bytes.get(1) == Some(&b'C'),
        worktree_modified: bytes.get(2) == Some(&b'M'),
        untracked: bytes.get(3) == Some(&b'U'),
    })
}

fn parse_header(line: &[u8], status: &mut HeaderState) -> Result<(), String> {
    let text = String::from_utf8_lossy(line);
    if let Some(value) = text.strip_prefix("# branch.oid ") {
        status.head = parse_object_id(value.as_bytes());
    } else if let Some(value) = text.strip_prefix("# branch.head ") {
        if value == "(detached)" {
            status.is_detached = true;
        } else {
            status.branch = Some(BranchName::new(value.to_string())?);
        }
    } else if let Some(value) = text.strip_prefix("# branch.upstream ") {
        status.upstream = Some(value.to_string());
    } else if let Some(value) = text.strip_prefix("# branch.ab ") {
        for part in value.split_whitespace() {
            if let Some(number) = part.strip_prefix('+') {
                status.ahead = number.parse().unwrap_or(0);
            } else if let Some(number) = part.strip_prefix('-') {
                status.behind = number.parse().unwrap_or(0);
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct HeaderState {
    head: Option<ObjectId>,
    branch: Option<BranchName>,
    upstream: Option<String>,
    ahead: u32,
    behind: u32,
    is_detached: bool,
}

fn parse_record<'a, I>(
    record: &'a [u8],
    records: &mut I,
    entries: &mut Vec<StatusEntry>,
) -> Result<(), String>
where
    I: Iterator<Item = &'a [u8]>,
{
    if record.is_empty() {
        return Ok(());
    }

    match record[0] {
        b'1' => {
            let fields = split_record(record, 9).ok_or("invalid porcelain v2 type 1 record")?;
            let xy = fields[1];
            if xy.len() != 2 {
                return Err("invalid porcelain v2 XY field".to_string());
            }
            let submodule = parse_submodule(fields[2]);
            entries.push(StatusEntry {
                path: RepoPath::from_bytes(fields[8].to_vec()),
                original_path: None,
                kind: kind_from_xy(xy, submodule.as_ref()),
                index: state_from_code(xy[0]),
                worktree: state_from_code(xy[1]),
                raw_xy: [xy[0], xy[1]],
                rename_score: None,
                submodule,
            });
        }
        b'2' => {
            let fields = split_record(record, 10).ok_or("invalid porcelain v2 type 2 record")?;
            let xy = fields[1];
            if xy.len() != 2 {
                return Err("invalid porcelain v2 XY field".to_string());
            }
            let original_path = records
                .next()
                .ok_or("rename/copy record is missing original path")?;
            let score = fields[8]
                .get(1..)
                .and_then(|value| String::from_utf8_lossy(value).parse().ok());
            entries.push(StatusEntry {
                path: RepoPath::from_bytes(fields[9].to_vec()),
                original_path: Some(RepoPath::from_bytes(original_path.to_vec())),
                kind: if fields[8].first() == Some(&b'C') {
                    StatusKind::Copied
                } else {
                    StatusKind::Renamed
                },
                index: state_from_code(xy[0]),
                worktree: state_from_code(xy[1]),
                raw_xy: [xy[0], xy[1]],
                rename_score: score,
                submodule: None,
            });
        }
        b'u' => {
            let fields = split_record(record, 11).ok_or("invalid porcelain v2 unmerged record")?;
            let xy = fields[1];
            if xy.len() != 2 {
                return Err("invalid porcelain v2 XY field".to_string());
            }
            let submodule = parse_submodule(fields[2]);
            entries.push(StatusEntry {
                path: RepoPath::from_bytes(fields[10].to_vec()),
                original_path: None,
                kind: StatusKind::Conflict,
                index: FileState::Unmerged,
                worktree: FileState::Unmerged,
                raw_xy: [xy[0], xy[1]],
                rename_score: None,
                submodule,
            });
        }
        b'?' => entries.push(StatusEntry {
            path: RepoPath::from_bytes(record.get(2..).unwrap_or_default().to_vec()),
            original_path: None,
            kind: StatusKind::Untracked,
            index: FileState::Unchanged,
            worktree: FileState::Untracked,
            raw_xy: [b'.', b'?'],
            rename_score: None,
            submodule: None,
        }),
        b'!' => {}
        other => return Err(format!("unknown porcelain v2 record type: {other}")),
    }
    Ok(())
}

pub fn parse_porcelain_v2_z(input: &[u8]) -> Result<RepositoryStatus, String> {
    parse_porcelain_v2_z_limited(input, input.len())
}

pub fn parse_porcelain_v2_z_limited(
    input: &[u8],
    max_bytes: usize,
) -> Result<RepositoryStatus, String> {
    if input.len() > max_bytes {
        return Err(format!(
            "porcelain v2 output exceeds the {max_bytes}-byte bound"
        ));
    }
    let mut records = input.split(|byte| *byte == 0);
    let mut header = HeaderState::default();
    let mut entries = Vec::new();
    while let Some(segment) = records.next() {
        if segment.is_empty() {
            continue;
        }
        if segment.starts_with(b"# ") {
            let mut remainder = segment;
            while remainder.starts_with(b"# ") {
                if let Some(newline) = remainder.iter().position(|byte| *byte == b'\n') {
                    parse_header(&remainder[..newline], &mut header)?;
                    remainder = &remainder[newline + 1..];
                } else {
                    parse_header(remainder, &mut header)?;
                    remainder = &[];
                }
            }
            if !remainder.is_empty() {
                parse_record(remainder, &mut records, &mut entries)?;
            }
        } else {
            parse_record(segment, &mut records, &mut entries)?;
        }
    }

    let fingerprint = RepoFingerprint {
        head: header.head.clone(),
        status_digest: status_digest(&entries),
    };
    Ok(RepositoryStatus {
        head: header.head,
        branch: header.branch,
        upstream: header.upstream,
        ahead: header.ahead,
        behind: header.behind,
        entries,
        is_detached: header.is_detached,
        fingerprint,
    })
}

fn state_code(state: FileState) -> u8 {
    match state {
        FileState::Unchanged => b'.',
        FileState::Modified => b'M',
        FileState::Added => b'A',
        FileState::Deleted => b'D',
        FileState::Renamed => b'R',
        FileState::Copied => b'C',
        FileState::TypeChanged => b'T',
        FileState::Unmerged => b'U',
        FileState::Untracked => b'?',
        FileState::Unknown => b'!',
    }
}

fn status_digest(entries: &[StatusEntry]) -> String {
    let mut ordered = entries.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.original_path.cmp(&right.original_path))
    });

    let mut hasher = Sha256::new();
    for entry in ordered {
        for bytes in [
            entry.path.as_bytes(),
            entry.original_path.as_ref().map_or(&[], RepoPath::as_bytes),
        ] {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        }
        hasher.update([state_code(entry.index), state_code(entry.worktree)]);
        hasher.update(entry.raw_xy);
        hasher.update([match entry.kind {
            StatusKind::Modified => 1,
            StatusKind::Added => 2,
            StatusKind::Deleted => 3,
            StatusKind::Renamed => 4,
            StatusKind::Copied => 5,
            StatusKind::TypeChanged => 6,
            StatusKind::Untracked => 7,
            StatusKind::Conflict => 8,
            StatusKind::Submodule => 9,
            StatusKind::Unknown => 10,
        }]);
        if let Some(submodule) = &entry.submodule {
            hasher.update([
                submodule.commit_changed as u8,
                submodule.worktree_modified as u8,
                submodule.untracked as u8,
            ]);
        } else {
            hasher.update([0, 0, 0]);
        }
        hasher.update([entry.rename_score.unwrap_or(0)]);
    }
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
