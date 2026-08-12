//! Host-owned SSH launch and prompt contract.
//!
//! This module is deliberately not a runtime.  Until the Task 3 supervisor
//! adapter exists, [`ssh_runtime_outcome`] is the only production entry point
//! and it fails closed with a typed unavailable state.  The pure contract is
//! exercised by unit tests using a cfg(test)-only host issuer; no caller can
//! forge a binding in a production build.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[cfg(test)]
use std::time::Duration;

use base64::Engine;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
#[cfg(test)]
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::domain::id::{ResourceId, TaskId};
use crate::process::identity::ManagedProcessIdentity;

#[cfg(test)]
use crate::process::identity::ManagedProcessId;

use super::credentials::{
    CredentialError, CredentialKind, CredentialRef, CredentialResolver, CredentialSecret,
    KeyIdentity, KeyMaterialStore, PinnedFile, RetainedKey,
};

pub(crate) const MAX_ID_BYTES: usize = 128;
pub(crate) const MAX_HOST_BYTES: usize = 255;
pub(crate) const MAX_USERNAME_BYTES: usize = 128;
pub(crate) const MAX_PROXY_JUMP_BYTES: usize = 512;
pub(crate) const MAX_KNOWN_HOSTS_PATH_BYTES: usize = 4 * 1024;
pub(crate) const MAX_ENV_ENTRIES: usize = 8;
pub(crate) const MAX_ENV_KEY_BYTES: usize = 32;
pub(crate) const MAX_ENV_VALUE_BYTES: usize = 128;
pub(crate) const MAX_ARGS: usize = 32;
pub(crate) const MAX_ARG_BYTES: usize = 2_048;
pub(crate) const MAX_PROMPT_BYTES: usize = 8 * 1024;
pub(crate) const MAX_PROMPT_TAIL_BYTES: usize = 16 * 1024;
pub(crate) const MAX_PROMPT_ATTEMPTS: u8 = 4;
pub(crate) const MAX_CANCELLATION_ENTRIES: usize = 4_096;
pub(crate) const MAX_RUNTIME_SECONDS: u64 = 15 * 60;

/// The production runtime is intentionally unavailable until the Task 3
/// supervisor supplies a process identity and host connection adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshUnavailableReason {
    TaskSupervisorAdapterMissing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshRuntimeOutcome {
    Unavailable { reason: SshUnavailableReason },
}

impl SshRuntimeOutcome {
    pub const fn unavailable() -> Self {
        Self::Unavailable {
            reason: SshUnavailableReason::TaskSupervisorAdapterMissing,
        }
    }
}

pub fn ssh_runtime_outcome() -> SshRuntimeOutcome {
    SshRuntimeOutcome::unavailable()
}

/// Internal stamp used for cancellation and leases.  The underlying fields
/// are private and never serialized or exposed to an untrusted caller.
struct BindingInner {
    task_id: TaskId,
    resource_id: ResourceId,
    runtime_generation: u64,
    action_epoch: u64,
    provider_process: ManagedProcessIdentity,
    launch_nonce: [u8; 16],
    key: [u8; 32],
}

#[derive(Clone)]
struct BindingClaim(Arc<BindingInner>);

impl BindingClaim {
    fn same(&self, other: &Self) -> bool {
        self.0.key == other.0.key
            && self.0.task_id == other.0.task_id
            && self.0.resource_id == other.0.resource_id
            && self.0.runtime_generation == other.0.runtime_generation
            && self.0.action_epoch == other.0.action_epoch
            && self.0.launch_nonce == other.0.launch_nonce
            && self
                .0
                .provider_process
                .matches_root(&other.0.provider_process)
    }

    fn key(&self) -> [u8; 32] {
        self.0.key
    }

    fn generation(&self) -> u64 {
        self.0.runtime_generation
    }

    fn action_epoch(&self) -> u64 {
        self.0.action_epoch
    }
}

/// Opaque, non-cloneable, non-serializable host authority.  The only
/// constructor is the cfg(test)-only issuer below until the Task 3 union
/// provides the real host-issued path.
pub(crate) struct SshBinding {
    inner: Arc<BindingInner>,
}

impl SshBinding {
    fn claim(&self) -> BindingClaim {
        BindingClaim(Arc::clone(&self.inner))
    }
}

impl PartialEq for SshBinding {
    fn eq(&self, other: &Self) -> bool {
        self.claim().same(&other.claim())
    }
}

impl Eq for SshBinding {}

impl fmt::Debug for SshBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshBinding")
            .field("authority", &"HOST_ISSUED_REDACTED")
            .finish()
    }
}

#[cfg(test)]
struct HostIssuedSshBinding {
    binding: SshBinding,
}

#[cfg(test)]
impl HostIssuedSshBinding {
    fn into_binding(self) -> SshBinding {
        self.binding
    }
}

/// Sealed issuer.  It is intentionally unavailable in non-test builds until
/// the supervisor can pass all five exact identity/epoch components.
#[cfg(test)]
struct HostIssuedSshBindingIssuer;

#[cfg(test)]
impl HostIssuedSshBindingIssuer {
    fn issue(
        task_id: TaskId,
        resource_id: ResourceId,
        runtime_generation: u64,
        action_epoch: u64,
        provider_process: ManagedProcessIdentity,
        launch_nonce: [u8; 16],
    ) -> Result<HostIssuedSshBinding, SshLaunchError> {
        if runtime_generation == 0
            || action_epoch == 0
            || launch_nonce.iter().all(|byte| *byte == 0)
        {
            return Err(SshLaunchError::InvalidField("binding_epoch"));
        }
        let mut hasher = Sha256::new();
        hasher.update(task_id.as_bytes());
        hasher.update(resource_id.as_bytes());
        hasher.update(runtime_generation.to_be_bytes());
        hasher.update(action_epoch.to_be_bytes());
        hasher.update(provider_process.id().pid().to_be_bytes());
        hasher.update(provider_process.id().creation_time_100ns().to_be_bytes());
        hasher.update(
            provider_process
                .canonical_executable()
                .as_os_str()
                .to_string_lossy()
                .as_bytes(),
        );
        hasher.update(launch_nonce);
        let digest = hasher.finalize();
        let mut key = [0u8; 32];
        key.copy_from_slice(&digest);
        Ok(HostIssuedSshBinding {
            binding: SshBinding {
                inner: Arc::new(BindingInner {
                    task_id,
                    resource_id,
                    runtime_generation,
                    action_epoch,
                    provider_process,
                    launch_nonce,
                    key,
                }),
            },
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum AuthConfig {
    Default,
    Agent,
    Password {
        credential_ref: CredentialRef,
    },
    PastedKey {
        credential_ref: CredentialRef,
        passphrase_ref: Option<CredentialRef>,
        password_ref: Option<CredentialRef>,
    },
    KeyPath {
        path: PathBuf,
        passphrase_ref: Option<CredentialRef>,
        password_ref: Option<CredentialRef>,
    },
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthConfig(REDACTED_REFERENCES)")
    }
}

impl AuthConfig {
    #[cfg(test)]
    fn default_auth() -> Self {
        Self::Default
    }

    #[cfg(test)]
    fn agent() -> Self {
        Self::Agent
    }

    #[cfg(test)]
    fn password(reference: impl AsRef<str>) -> Result<Self, SshLaunchError> {
        Ok(Self::Password {
            credential_ref: CredentialRef::parse(reference).map_err(SshLaunchError::Credential)?,
        })
    }

    #[cfg(test)]
    fn pasted_key(reference: impl AsRef<str>) -> Result<Self, SshLaunchError> {
        Ok(Self::PastedKey {
            credential_ref: CredentialRef::parse(reference).map_err(SshLaunchError::Credential)?,
            passphrase_ref: None,
            password_ref: None,
        })
    }

    #[cfg(test)]
    fn pasted_key_with_passphrase(
        key_reference: impl AsRef<str>,
        passphrase_reference: impl AsRef<str>,
    ) -> Result<Self, SshLaunchError> {
        Ok(Self::PastedKey {
            credential_ref: CredentialRef::parse(key_reference)
                .map_err(SshLaunchError::Credential)?,
            passphrase_ref: Some(
                CredentialRef::parse(passphrase_reference).map_err(SshLaunchError::Credential)?,
            ),
            password_ref: None,
        })
    }

    #[cfg(test)]
    fn pasted_key_with_password(
        key_reference: impl AsRef<str>,
        password_reference: impl AsRef<str>,
    ) -> Result<Self, SshLaunchError> {
        Ok(Self::PastedKey {
            credential_ref: CredentialRef::parse(key_reference)
                .map_err(SshLaunchError::Credential)?,
            passphrase_ref: None,
            password_ref: Some(
                CredentialRef::parse(password_reference).map_err(SshLaunchError::Credential)?,
            ),
        })
    }

    #[cfg(test)]
    fn key_path(path: impl Into<PathBuf>) -> Result<Self, SshLaunchError> {
        let path = path.into();
        validate_key_path(&path)?;
        Ok(Self::KeyPath {
            path,
            passphrase_ref: None,
            password_ref: None,
        })
    }

    #[cfg(test)]
    fn key_path_with_password(
        path: impl Into<PathBuf>,
        password_reference: impl AsRef<str>,
    ) -> Result<Self, SshLaunchError> {
        let path = path.into();
        validate_key_path(&path)?;
        Ok(Self::KeyPath {
            path,
            passphrase_ref: None,
            password_ref: Some(
                CredentialRef::parse(password_reference).map_err(SshLaunchError::Credential)?,
            ),
        })
    }

    fn references(&self) -> impl Iterator<Item = &CredentialRef> {
        let first = match self {
            Self::Password { credential_ref } | Self::PastedKey { credential_ref, .. } => {
                Some(credential_ref)
            }
            Self::Default | Self::Agent | Self::KeyPath { .. } => None,
        };
        let second = match self {
            Self::PastedKey {
                passphrase_ref: Some(reference),
                ..
            }
            | Self::KeyPath {
                passphrase_ref: Some(reference),
                ..
            } => Some(reference),
            _ => None,
        };
        let third = match self {
            Self::PastedKey {
                password_ref: Some(reference),
                ..
            }
            | Self::KeyPath {
                password_ref: Some(reference),
                ..
            } => Some(reference),
            _ => None,
        };
        first.into_iter().chain(second).chain(third)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SshKnownHostPolicy {
    Prompt,
    Strict,
}

struct SshLaunchRequest {
    binding: BindingClaim,
    #[cfg(test)]
    issued_binding: SshBinding,
    connection_id: String,
    host: String,
    port: u16,
    username: String,
    auth: AuthConfig,
    deadline: Instant,
    proxy_jump: Option<String>,
    known_hosts_path: Option<PathBuf>,
    known_host_policy: SshKnownHostPolicy,
    env: BTreeMap<String, String>,
}

impl fmt::Debug for SshLaunchRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshLaunchRequest")
            .field("binding", &"HOST_ISSUED_REDACTED")
            .field("connection_id", &self.connection_id)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("auth", &self.auth)
            .field("deadline", &"BOUNDED")
            .finish()
    }
}

#[cfg(test)]
impl SshLaunchRequest {
    fn new(
        issued: HostIssuedSshBinding,
        connection_id: impl AsRef<str>,
        host: impl AsRef<str>,
        port: u16,
        username: impl AsRef<str>,
        auth: AuthConfig,
        timeout: Duration,
    ) -> Result<Self, SshLaunchError> {
        Self::new_with_deadline(
            issued,
            connection_id,
            host,
            port,
            username,
            auth,
            Instant::now()
                .checked_add(timeout)
                .ok_or(SshLaunchError::DeadlineExpired)?,
        )
    }

    fn new_with_deadline(
        issued: HostIssuedSshBinding,
        connection_id: impl AsRef<str>,
        host: impl AsRef<str>,
        port: u16,
        username: impl AsRef<str>,
        auth: AuthConfig,
        deadline: Instant,
    ) -> Result<Self, SshLaunchError> {
        let connection_id = bounded_text("connection_id", connection_id.as_ref(), MAX_ID_BYTES)?;
        let host = validate_host(host.as_ref())?;
        let username = validate_username(username.as_ref())?;
        if port == 0 {
            return Err(SshLaunchError::InvalidField("port"));
        }
        if deadline > Instant::now() + Duration::from_secs(MAX_RUNTIME_SECONDS) {
            return Err(SshLaunchError::DeadlineTooFar);
        }
        for reference in auth.references() {
            validate_reference(reference)?;
        }
        let issued_binding = issued.into_binding();
        let claim = issued_binding.claim();
        Ok(Self {
            binding: claim,
            #[cfg(test)]
            issued_binding,
            connection_id,
            host,
            port,
            username,
            auth,
            deadline,
            proxy_jump: None,
            known_hosts_path: None,
            known_host_policy: SshKnownHostPolicy::Prompt,
            env: BTreeMap::new(),
        })
    }

    fn with_network_policy(
        mut self,
        proxy_jump: Option<String>,
        known_hosts_path: Option<PathBuf>,
        known_host_policy: SshKnownHostPolicy,
    ) -> Result<Self, SshLaunchError> {
        if let Some(proxy_jump) = proxy_jump.as_deref() {
            validate_proxy_jump(proxy_jump)?;
        }
        if let Some(path) = known_hosts_path.as_deref() {
            validate_known_hosts_path(path)?;
        }
        self.proxy_jump = proxy_jump;
        self.known_hosts_path = known_hosts_path;
        self.known_host_policy = known_host_policy;
        Ok(self)
    }

    fn with_environment(mut self, env: BTreeMap<String, String>) -> Result<Self, SshLaunchError> {
        validate_env(&env)?;
        self.env = env;
        Ok(self)
    }
}

#[derive(Clone)]
struct CancellationState {
    cancelled: Arc<Mutex<BTreeMap<[u8; 32], Instant>>>,
}

pub(crate) struct CancellationToken {
    state: CancellationState,
}

impl Clone for CancellationToken {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

impl CancellationToken {
    #[cfg(test)]
    fn new() -> Self {
        Self {
            state: CancellationState {
                cancelled: Arc::new(Mutex::new(BTreeMap::new())),
            },
        }
    }

    fn cancel(&self, binding: &BindingClaim) -> Result<(), SshLaunchError> {
        let mut cancelled = self
            .state
            .cancelled
            .lock()
            .map_err(|_| SshLaunchError::CancellationLedgerUnavailable)?;
        if !cancelled.contains_key(&binding.key()) && cancelled.len() >= MAX_CANCELLATION_ENTRIES {
            return Err(SshLaunchError::CapacityExceeded);
        }
        cancelled.insert(binding.key(), Instant::now());
        Ok(())
    }

    fn is_cancelled(&self, binding: &BindingClaim) -> bool {
        self.state
            .cancelled
            .lock()
            .map(|cancelled| cancelled.contains_key(&binding.key()))
            .unwrap_or(true)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SshLaunchError {
    Credential(CredentialError),
    InvalidField(&'static str),
    ArgumentTooLarge { bytes: usize },
    EnvironmentTooLarge { entries: usize },
    PromptTooLarge { bytes: usize },
    DeadlineExpired,
    DeadlineTooFar,
    Cancelled,
    StaleBinding,
    AlreadyConsumed,
    PreSpawnConsumed,
    AttemptLimit,
    CapacityExceeded,
    CancellationLedgerUnavailable,
    UnsupportedRuntime,
}

impl fmt::Display for SshLaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Credential(error) => error.fmt(formatter),
            Self::InvalidField(field) => write!(formatter, "invalid SSH {field}"),
            Self::ArgumentTooLarge { bytes } => {
                write!(formatter, "SSH argument exceeds bound ({bytes} bytes)")
            }
            Self::EnvironmentTooLarge { entries } => write!(
                formatter,
                "SSH environment exceeds bound ({entries} entries)"
            ),
            Self::PromptTooLarge { bytes } => {
                write!(formatter, "SSH prompt exceeds bound ({bytes} bytes)")
            }
            Self::DeadlineExpired => formatter.write_str("SSH launch deadline expired"),
            Self::DeadlineTooFar => formatter.write_str("SSH launch deadline exceeds bound"),
            Self::Cancelled => formatter.write_str("SSH launch was cancelled"),
            Self::StaleBinding => formatter.write_str("SSH request binding is stale"),
            Self::AlreadyConsumed => formatter.write_str("SSH input lease was already consumed"),
            Self::PreSpawnConsumed => {
                formatter.write_str("SSH pre-spawn authority was already consumed")
            }
            Self::AttemptLimit => formatter.write_str("SSH prompt attempt limit exceeded"),
            Self::CapacityExceeded => formatter.write_str("SSH bounded ledger is full"),
            Self::CancellationLedgerUnavailable => {
                formatter.write_str("SSH cancellation ledger unavailable")
            }
            Self::UnsupportedRuntime => formatter.write_str("SSH runtime adapter unavailable"),
        }
    }
}

impl std::error::Error for SshLaunchError {}

impl From<CredentialError> for SshLaunchError {
    fn from(error: CredentialError) -> Self {
        match error {
            CredentialError::DeadlineExpired => Self::DeadlineExpired,
            CredentialError::UnsupportedRuntime => Self::UnsupportedRuntime,
            other => Self::Credential(other),
        }
    }
}

#[derive(Clone, Serialize)]
pub(crate) enum SafeAuth {
    Default,
    Agent,
    Password {
        credential_ref: CredentialRef,
    },
    PastedKey {
        credential_ref: CredentialRef,
        key_index: u32,
        passphrase_ref: Option<CredentialRef>,
        password_ref: Option<CredentialRef>,
    },
    KeyPath {
        path: PathBuf,
        passphrase_ref: Option<CredentialRef>,
        password_ref: Option<CredentialRef>,
    },
}

impl fmt::Debug for SafeAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SafeAuth(REDACTED_REFERENCES)")
    }
}

impl SafeAuth {
    fn is_non_secret(&self) -> bool {
        true
    }
}

#[derive(Clone)]
pub(crate) struct SshCommand {
    program: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
}

impl SshCommand {
    pub(crate) fn program(&self) -> &str {
        &self.program
    }

    pub(crate) fn args(&self) -> &[String] {
        &self.args
    }

    pub(crate) fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }
}

impl fmt::Debug for SshCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshCommand")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("cwd", &"REDACTED_PATH")
            .finish()
    }
}

impl Serialize for SshCommand {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("SshCommand", 4)?;
        state.serialize_field("program", &self.program)?;
        state.serialize_field("args", &self.args)?;
        // Environment values are intentionally absent from the wire shape.
        state.serialize_field("envKeys", &self.env.keys().collect::<Vec<_>>())?;
        state.serialize_field("cwd", &self.cwd)?;
        state.end()
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct SshConnectSnapshot {
    auth: SafeAuth,
    command: SnapshotCommand,
    runtime_generation: u64,
    action_epoch: u64,
}

#[derive(Clone, Serialize)]
struct SnapshotCommand {
    program: String,
    args: Vec<String>,
    env_keys: Vec<String>,
}

impl SshConnectSnapshot {
    fn auth(&self) -> &SafeAuth {
        &self.auth
    }
}

#[derive(Clone, Serialize)]
pub(crate) enum SshLaunchEvent {
    Prepared { auth: SafeAuth },
    CredentialPrompt { kind: InputRequestKind },
    HostKeyPrompt,
    Cancelled,
    Uncertain { code: &'static str },
}

impl fmt::Debug for SshLaunchEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prepared { .. } => formatter.write_str("Prepared(REDACTED)"),
            Self::CredentialPrompt { kind } => formatter
                .debug_struct("CredentialPrompt")
                .field("kind", kind)
                .finish(),
            Self::HostKeyPrompt => formatter.write_str("HostKeyPrompt"),
            Self::Cancelled => formatter.write_str("Cancelled"),
            Self::Uncertain { code } => formatter
                .debug_struct("Uncertain")
                .field("code", code)
                .finish(),
        }
    }
}

pub(crate) struct SshLaunchPlan {
    binding: BindingClaim,
    cancellation: CancellationToken,
    command: SshCommand,
    snapshot: SshConnectSnapshot,
    events: Vec<SshLaunchEvent>,
    username: String,
    host: String,
    port: u16,
    password_ref: Option<CredentialRef>,
    passphrase_ref: Option<CredentialRef>,
    retained_key: Option<RetainedKey>,
    pre_spawn_consumed: bool,
    guards: LaunchGuards,
    deadline: Instant,
}

#[derive(Clone)]
struct LaunchGuards {
    ssh: Arc<PinnedFile>,
    key: Option<Arc<PinnedFile>>,
    known_hosts: Option<Arc<PinnedFile>>,
}

impl LaunchGuards {
    fn revalidate(&self, deadline: Instant) -> Result<(), SshLaunchError> {
        if deadline <= Instant::now() {
            return Err(SshLaunchError::DeadlineExpired);
        }
        self.ssh.revalidate_until(deadline)?;
        if let Some(key) = self.key.as_ref() {
            key.revalidate_until(deadline)?;
        }
        if let Some(known_hosts) = self.known_hosts.as_ref() {
            known_hosts.revalidate_until(deadline)?;
        }
        if deadline <= Instant::now() {
            return Err(SshLaunchError::DeadlineExpired);
        }
        Ok(())
    }
}

impl fmt::Debug for SshLaunchPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshLaunchPlan")
            .field("binding", &"HOST_ISSUED_REDACTED")
            .field("command", &self.command)
            .field("snapshot", &"REDACTED")
            .field("events", &self.events)
            .field("retained_key", &self.retained_key)
            .finish()
    }
}

impl SshLaunchPlan {
    pub(crate) fn command(&self) -> &SshCommand {
        &self.command
    }

    pub(crate) fn snapshot(&self) -> &SshConnectSnapshot {
        &self.snapshot
    }

    pub(crate) fn events(&self) -> &[SshLaunchEvent] {
        &self.events
    }

    pub(crate) fn retained_key(&self) -> Option<&RetainedKey> {
        self.retained_key.as_ref()
    }

    /// Establishes the last contract boundary before a future supervisor may
    /// spawn the child.  No process is spawned here until that supervisor
    /// adapter exists; the returned authority retains all canonical guards.
    pub(crate) fn pre_spawn(&mut self) -> Result<SshPreSpawn, SshLaunchError> {
        if self.pre_spawn_consumed {
            return Err(SshLaunchError::PreSpawnConsumed);
        }
        if self.cancellation.is_cancelled(&self.binding) {
            return Err(SshLaunchError::Cancelled);
        }
        if self.deadline <= Instant::now() {
            return Err(SshLaunchError::DeadlineExpired);
        }
        self.guards.revalidate(self.deadline)?;
        self.pre_spawn_consumed = true;
        // The future supervisor owns this authority through settlement.  Move
        // it only after all pre-spawn checks pass so dropping the plan cannot
        // remove the `-i` material while the child may still need it.
        let retained_key = self.retained_key.take();
        Ok(SshPreSpawn {
            command: self.command.clone(),
            binding: self.binding.clone(),
            cancellation: self.cancellation.clone(),
            guards: self.guards.clone(),
            deadline: self.deadline,
            retained_key,
        })
    }

    fn prompt_matcher(&self) -> PromptMatcher {
        PromptMatcher {
            binding: self.binding.clone(),
            username: self.username.clone(),
            host: self.host.clone(),
            port: self.port,
            password_ref: self.password_ref.clone(),
            passphrase_ref: self.passphrase_ref.clone(),
            key_path: self.guards.key.as_ref().map(|guard| guard.path_string()),
            guards: self.guards.clone(),
            deadline: self.deadline,
            cancellation: self.cancellation.clone(),
            tail: Zeroizing::new(Vec::new()),
            password_line: Zeroizing::new(Vec::new()),
            password_candidate: Zeroizing::new(Vec::new()),
            password_visible_lines: VecDeque::new(),
            password_candidate_start: None,
            password_target_seen: false,
            password_match_pending: false,
            password_invalid: false,
            password_rejected: false,
            invalid_line: false,
            host_key_started: false,
            host_key_bytes: 0,
            host_key_display: None,
            host_key_fingerprint: None,
            emitted: false,
            attempts: 0,
        }
    }
}

/// Retained authority handed to the future Task 3 supervisor at its immediate
/// pre-spawn boundary.  This contract deliberately has no spawn method while
/// the production supervisor adapter is unavailable.
pub(crate) struct SshPreSpawn {
    command: SshCommand,
    binding: BindingClaim,
    cancellation: CancellationToken,
    guards: LaunchGuards,
    deadline: Instant,
    retained_key: Option<RetainedKey>,
}

impl SshPreSpawn {
    pub(crate) fn command(&self) -> &SshCommand {
        &self.command
    }

    pub(crate) fn revalidate(&self) -> Result<(), SshLaunchError> {
        if self.cancellation.is_cancelled(&self.binding) {
            return Err(SshLaunchError::Cancelled);
        }
        if self.deadline <= Instant::now() {
            return Err(SshLaunchError::DeadlineExpired);
        }
        self.guards.revalidate(self.deadline)
    }
}

impl fmt::Debug for SshPreSpawn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshPreSpawn")
            .field("command", &self.command)
            .field("authority", &"RETAINED_REDACTED")
            .finish()
    }
}

pub(crate) enum LaunchOutcome {
    Ready(Box<SshLaunchPlan>),
    Cancelled,
}

pub(crate) struct SshUncertainty {
    code: &'static str,
}

impl SshUncertainty {
    pub(crate) fn ambiguous_dispatch() -> Self {
        Self {
            code: "ambiguous_dispatch",
        }
    }

    pub(crate) fn code(&self) -> &'static str {
        self.code
    }
}

fn pin_path(
    path: &Path,
    field: &'static str,
    deadline: Instant,
) -> Result<Arc<PinnedFile>, SshLaunchError> {
    if deadline <= Instant::now() {
        return Err(SshLaunchError::DeadlineExpired);
    }
    validate_key_path(path).map_err(|_| SshLaunchError::InvalidField(field))?;
    if deadline <= Instant::now() {
        return Err(SshLaunchError::DeadlineExpired);
    }
    Ok(Arc::new(PinnedFile::open_until(path, deadline)?))
}

fn pin_ssh_executable(deadline: Instant) -> Result<Arc<PinnedFile>, SshLaunchError> {
    pin_ssh_executable_uncached(deadline)
}

fn pin_ssh_executable_uncached(deadline: Instant) -> Result<Arc<PinnedFile>, SshLaunchError> {
    #[cfg(test)]
    let path = std::env::var_os("COMSPEC")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .ok_or(SshLaunchError::UnsupportedRuntime)?;
    #[cfg(all(not(test), windows))]
    let path = {
        let root = std::env::var_os("SystemRoot").ok_or(SshLaunchError::UnsupportedRuntime)?;
        super::credentials::bounded_system_path(&root, &["System32", "OpenSSH", "ssh.exe"])?
    };
    #[cfg(all(not(test), unix))]
    let path = {
        let preferred = Path::new("/usr/bin/ssh");
        if preferred.is_file() {
            preferred.to_path_buf()
        } else {
            PathBuf::from("/bin/ssh")
        }
    };
    #[cfg(all(not(test), not(any(unix, windows))))]
    let path = PathBuf::from("ssh");
    Ok(Arc::new(PinnedFile::open_until(&path, deadline)?))
}

fn ensure_request_active(
    request: &SshLaunchRequest,
    cancellation: &CancellationToken,
    guards: &LaunchGuards,
) -> Result<(), SshLaunchError> {
    if request.deadline <= Instant::now() {
        return Err(SshLaunchError::DeadlineExpired);
    }
    if cancellation.is_cancelled(&request.binding) {
        return Err(SshLaunchError::Cancelled);
    }
    guards.revalidate(request.deadline)
}

fn build_ssh_launch_plan(
    request: &SshLaunchRequest,
    credentials: &dyn CredentialResolver,
    key_store: Option<&KeyMaterialStore>,
    cancellation: &CancellationToken,
) -> Result<LaunchOutcome, SshLaunchError> {
    super::credentials::ensure_supported_runtime()?;
    if request.deadline <= Instant::now() {
        return Err(SshLaunchError::DeadlineExpired);
    }
    if cancellation.is_cancelled(&request.binding) {
        return Ok(LaunchOutcome::Cancelled);
    }
    let ssh_authority = pin_ssh_executable(request.deadline)?;
    let known_hosts_guard = request
        .known_hosts_path
        .as_deref()
        .map(|path| pin_path(path, "known_hosts_path", request.deadline))
        .transpose()?;
    let mut retained_key = None;
    let mut key_guard = None;
    let mut configured_key_path = None;
    let safe_auth = match &request.auth {
        AuthConfig::Default => SafeAuth::Default,
        AuthConfig::Agent => SafeAuth::Agent,
        AuthConfig::Password { credential_ref } => {
            let secret = resolve_expected(credentials, credential_ref, CredentialKind::Password)?;
            secret.validate()?;
            SafeAuth::Password {
                credential_ref: credential_ref.clone(),
            }
        }
        AuthConfig::PastedKey {
            credential_ref,
            passphrase_ref,
            password_ref,
        } => {
            let store =
                key_store.ok_or(SshLaunchError::Credential(CredentialError::MissingKeyStore))?;
            let secret = resolve_expected(credentials, credential_ref, CredentialKind::PrivateKey)?;
            let identity = KeyIdentity::issue(&request.connection_id, credential_ref)?;
            let retained = store.materialize_until(&identity, &secret, request.deadline)?;
            retained.revalidate_until(request.deadline)?;
            key_guard = Some(Arc::new(PinnedFile::open_until(
                retained.path(),
                request.deadline,
            )?));
            retained_key = Some(retained);
            if let Some(reference) = passphrase_ref {
                let passphrase =
                    resolve_expected(credentials, reference, CredentialKind::Passphrase)?;
                passphrase.validate()?;
            }
            SafeAuth::PastedKey {
                credential_ref: credential_ref.clone(),
                key_index: identity.index(),
                passphrase_ref: passphrase_ref.clone(),
                password_ref: password_ref.clone(),
            }
        }
        AuthConfig::KeyPath {
            path,
            passphrase_ref,
            password_ref,
        } => {
            let guard = pin_path(path, "key_path", request.deadline)?;
            configured_key_path = Some(guard.path_string());
            key_guard = Some(guard);
            if let Some(reference) = passphrase_ref {
                let passphrase =
                    resolve_expected(credentials, reference, CredentialKind::Passphrase)?;
                passphrase.validate()?;
            }
            SafeAuth::KeyPath {
                path: PathBuf::from(
                    configured_key_path
                        .as_deref()
                        .ok_or(SshLaunchError::InvalidField("key_path"))?,
                ),
                passphrase_ref: passphrase_ref.clone(),
                password_ref: password_ref.clone(),
            }
        }
    };

    let guards = LaunchGuards {
        ssh: ssh_authority,
        key: key_guard,
        known_hosts: known_hosts_guard,
    };
    if let Err(error) = ensure_request_active(request, cancellation, &guards) {
        drop(retained_key);
        return if error == SshLaunchError::Cancelled {
            Ok(LaunchOutcome::Cancelled)
        } else {
            Err(error)
        };
    }
    let target = format!("{}@{}", request.username, request.host);
    let mut args = vec!["-p".to_string(), request.port.to_string()];
    if request.known_host_policy == SshKnownHostPolicy::Prompt {
        args.extend(["-o".to_string(), "StrictHostKeyChecking=ask".to_string()]);
    } else {
        args.extend(["-o".to_string(), "StrictHostKeyChecking=yes".to_string()]);
    }
    if let Some(path) = guards.known_hosts.as_ref() {
        args.extend([
            "-o".to_string(),
            format!("UserKnownHostsFile={}", path.path().display()),
        ]);
    }
    if let Some(proxy_jump) = request.proxy_jump.as_ref() {
        args.extend(["-J".to_string(), proxy_jump.clone()]);
    }
    if let Some(retained) = retained_key.as_ref() {
        args.extend(["-i".to_string(), retained.path_string()]);
    } else if let Some(path) = configured_key_path {
        args.extend(["-i".to_string(), path]);
    }
    args.extend(["--".to_string(), target]);
    validate_args(&args)?;
    let command = SshCommand {
        program: guards.ssh.path_string(),
        args: args.clone(),
        env: request.env.clone(),
        cwd: None,
    };
    let snapshot = SshConnectSnapshot {
        auth: safe_auth.clone(),
        command: SnapshotCommand {
            program: command.program.clone(),
            args,
            env_keys: command.env.keys().cloned().collect(),
        },
        runtime_generation: request.binding.generation(),
        action_epoch: request.binding.action_epoch(),
    };
    let password_ref = match &request.auth {
        AuthConfig::Password { credential_ref } => Some(credential_ref.clone()),
        AuthConfig::PastedKey { password_ref, .. } | AuthConfig::KeyPath { password_ref, .. } => {
            password_ref.clone()
        }
        _ => None,
    };
    let passphrase_ref = match &request.auth {
        AuthConfig::PastedKey { passphrase_ref, .. }
        | AuthConfig::KeyPath { passphrase_ref, .. } => passphrase_ref.clone(),
        _ => None,
    };
    if let Err(error) = ensure_request_active(request, cancellation, &guards) {
        drop(retained_key);
        return if error == SshLaunchError::Cancelled {
            Ok(LaunchOutcome::Cancelled)
        } else {
            Err(error)
        };
    }
    Ok(LaunchOutcome::Ready(Box::new(SshLaunchPlan {
        binding: request.binding.clone(),
        cancellation: cancellation.clone(),
        command,
        snapshot,
        events: vec![SshLaunchEvent::Prepared { auth: safe_auth }],
        username: request.username.clone(),
        host: request.host.clone(),
        port: request.port,
        password_ref,
        passphrase_ref,
        retained_key,
        pre_spawn_consumed: false,
        guards,
        deadline: request.deadline,
    })))
}

fn resolve_expected(
    credentials: &dyn CredentialResolver,
    reference: &CredentialRef,
    expected: CredentialKind,
) -> Result<CredentialSecret, SshLaunchError> {
    let secret = credentials.resolve(reference)?;
    if secret.kind() != expected {
        return Err(SshLaunchError::Credential(CredentialError::WrongKind {
            expected,
            actual: secret.kind(),
        }));
    }
    Ok(secret)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum InputRequestKind {
    Password,
    Passphrase,
}

pub(crate) struct InputDelivery {
    bytes: Zeroizing<Vec<u8>>,
}

impl InputDelivery {
    fn new(secret: &CredentialSecret) -> Result<Self, SshLaunchError> {
        secret.validate()?;
        let mut bytes = Zeroizing::new(Vec::with_capacity(secret.bytes().len() + 1));
        bytes.extend_from_slice(secret.bytes());
        bytes.push(b'\r');
        Ok(Self { bytes })
    }

    #[cfg(test)]
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for InputDelivery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("InputDelivery(REDACTED)")
    }
}

struct LeaseState {
    consumed: AtomicBool,
    attempts: AtomicU8,
    binding: BindingClaim,
    deadline: Instant,
    cancellation: CancellationToken,
}

impl LeaseState {
    fn ensure_binding(&self, binding: &BindingClaim) -> Result<(), SshLaunchError> {
        if !self.binding.same(binding) {
            return Err(SshLaunchError::StaleBinding);
        }
        Ok(())
    }

    fn ensure_live(&self, binding: &BindingClaim) -> Result<(), SshLaunchError> {
        self.ensure_binding(binding)?;
        if self.cancellation.is_cancelled(binding) {
            return Err(SshLaunchError::Cancelled);
        }
        if self.deadline <= Instant::now() {
            return Err(SshLaunchError::DeadlineExpired);
        }
        Ok(())
    }

    fn ensure_active(&self, binding: &BindingClaim) -> Result<(), SshLaunchError> {
        self.ensure_binding(binding)?;
        if self.consumed.load(Ordering::Acquire) {
            return Err(SshLaunchError::AlreadyConsumed);
        }
        if self.cancellation.is_cancelled(binding) {
            return Err(SshLaunchError::Cancelled);
        }
        if self.deadline <= Instant::now() {
            return Err(SshLaunchError::DeadlineExpired);
        }
        Ok(())
    }

    fn consume(&self, binding: &BindingClaim) -> Result<(), SshLaunchError> {
        self.ensure_active(binding)?;
        if self.consumed.load(Ordering::Acquire) {
            return Err(SshLaunchError::AlreadyConsumed);
        }
        let attempts = self.attempts.fetch_add(1, Ordering::AcqRel);
        if attempts >= MAX_PROMPT_ATTEMPTS {
            return Err(SshLaunchError::AttemptLimit);
        }
        self.consumed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| SshLaunchError::AlreadyConsumed)
    }
}

pub(crate) struct SshInputRequest {
    binding: BindingClaim,
    kind: InputRequestKind,
    credential_ref: CredentialRef,
    lease: Arc<LeaseState>,
    guards: LaunchGuards,
}

impl fmt::Debug for SshInputRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshInputRequest")
            .field("kind", &self.kind)
            .field("credential_ref", &self.credential_ref)
            .field("binding", &"HOST_ISSUED_REDACTED")
            .finish()
    }
}

impl SshInputRequest {
    fn new(
        binding: BindingClaim,
        kind: InputRequestKind,
        credential_ref: CredentialRef,
        deadline: Instant,
        cancellation: CancellationToken,
        guards: LaunchGuards,
    ) -> Self {
        Self {
            lease: Arc::new(LeaseState {
                consumed: AtomicBool::new(false),
                attempts: AtomicU8::new(0),
                binding: binding.clone(),
                deadline,
                cancellation,
            }),
            binding,
            kind,
            credential_ref,
            guards,
        }
    }

    #[cfg(test)]
    fn kind(&self) -> InputRequestKind {
        self.kind
    }

    fn accepts_binding(&self, binding: &SshBinding) -> bool {
        self.binding.same(&binding.claim())
    }

    #[cfg(test)]
    fn resolve(
        &self,
        credentials: &dyn CredentialResolver,
        binding: &SshBinding,
    ) -> Result<InputDelivery, SshLaunchError> {
        let claim = binding.claim();
        self.lease.ensure_active(&claim)?;
        let secret = credentials.resolve(&self.credential_ref)?;
        let expected = match self.kind {
            InputRequestKind::Password => CredentialKind::Password,
            InputRequestKind::Passphrase => CredentialKind::Passphrase,
        };
        if secret.kind() != expected {
            return Err(SshLaunchError::Credential(CredentialError::WrongKind {
                expected,
                actual: secret.kind(),
            }));
        }
        // Resolver code is host-owned and may block or revoke this request.
        // Recheck all authority immediately after it returns, before bytes are
        // copied into the delivery buffer and again before returning it.
        self.lease.ensure_live(&claim)?;
        self.guards.revalidate(self.lease.deadline)?;
        self.lease.consume(&claim)?;
        let delivery = InputDelivery::new(&secret)?;
        self.lease.ensure_live(&claim)?;
        self.guards.revalidate(self.lease.deadline)?;
        Ok(delivery)
    }
}

pub(crate) struct SshHostKeyPrompt {
    binding: BindingClaim,
    lease: Arc<LeaseState>,
    guards: LaunchGuards,
    host: String,
    fingerprint: String,
}

impl fmt::Debug for SshHostKeyPrompt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshHostKeyPrompt")
            .field("binding", &"HOST_ISSUED_REDACTED")
            .field("answer", &"REDACTED")
            .finish()
    }
}

impl SshHostKeyPrompt {
    fn new(
        binding: BindingClaim,
        deadline: Instant,
        cancellation: CancellationToken,
        guards: LaunchGuards,
        host: String,
        fingerprint: String,
    ) -> Self {
        Self {
            lease: Arc::new(LeaseState {
                consumed: AtomicBool::new(false),
                attempts: AtomicU8::new(0),
                binding: binding.clone(),
                deadline,
                cancellation,
            }),
            binding,
            guards,
            host,
            fingerprint,
        }
    }

    #[cfg(test)]
    fn host(&self) -> &str {
        &self.host
    }

    #[cfg(test)]
    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    fn accepts_binding(&self, binding: &SshBinding) -> bool {
        self.binding.same(&binding.claim())
    }

    #[cfg(test)]
    fn accept(&self, binding: &SshBinding) -> Result<InputDelivery, SshLaunchError> {
        self.answer(binding, b"yes\r")
    }

    #[cfg(test)]
    fn reject(&self, binding: &SshBinding) -> Result<InputDelivery, SshLaunchError> {
        self.answer(binding, b"no\r")
    }

    #[cfg(test)]
    fn answer(&self, binding: &SshBinding, answer: &[u8]) -> Result<InputDelivery, SshLaunchError> {
        let claim = binding.claim();
        self.lease.ensure_active(&claim)?;
        self.guards.revalidate(self.lease.deadline)?;
        self.lease.consume(&claim)?;
        let mut bytes = Zeroizing::new(answer.to_vec());
        let delivery = InputDelivery {
            bytes: Zeroizing::new(std::mem::take(&mut bytes)),
        };
        self.lease.ensure_live(&claim)?;
        self.guards.revalidate(self.lease.deadline)?;
        Ok(delivery)
    }
}

pub(crate) enum PromptMatch {
    Input(SshInputRequest),
    HostKey(SshHostKeyPrompt),
    Ignore,
}

impl fmt::Debug for PromptMatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(input) => formatter.debug_tuple("Input").field(input).finish(),
            Self::HostKey(prompt) => formatter.debug_tuple("HostKey").field(prompt).finish(),
            Self::Ignore => formatter.write_str("Ignore"),
        }
    }
}

pub(crate) struct PromptMatcher {
    binding: BindingClaim,
    username: String,
    host: String,
    port: u16,
    password_ref: Option<CredentialRef>,
    passphrase_ref: Option<CredentialRef>,
    key_path: Option<String>,
    guards: LaunchGuards,
    deadline: Instant,
    cancellation: CancellationToken,
    tail: Zeroizing<Vec<u8>>,
    password_line: Zeroizing<Vec<u8>>,
    password_candidate: Zeroizing<Vec<u8>>,
    password_visible_lines: VecDeque<Zeroizing<Vec<u8>>>,
    password_candidate_start: Option<usize>,
    password_target_seen: bool,
    password_match_pending: bool,
    password_invalid: bool,
    password_rejected: bool,
    invalid_line: bool,
    host_key_started: bool,
    host_key_bytes: usize,
    host_key_display: Option<String>,
    host_key_fingerprint: Option<String>,
    emitted: bool,
    attempts: u8,
}

impl fmt::Debug for PromptMatcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptMatcher")
            .field("binding", &"HOST_ISSUED_REDACTED")
            .field("username", &self.username)
            .field("host", &self.host)
            .field("tail", &"REDACTED")
            .field("attempts", &self.attempts)
            .finish()
    }
}

impl PromptMatcher {
    fn observe(&mut self, chunk: &[u8]) -> Result<PromptMatch, SshLaunchError> {
        if chunk.len() > MAX_PROMPT_BYTES {
            self.tail.clear();
            self.invalid_line = true;
            return Err(SshLaunchError::PromptTooLarge { bytes: chunk.len() });
        }
        if self.emitted {
            return Err(SshLaunchError::AlreadyConsumed);
        }
        if self.cancellation.is_cancelled(&self.binding) {
            return Err(SshLaunchError::Cancelled);
        }
        if self.deadline <= Instant::now() {
            return Err(SshLaunchError::DeadlineExpired);
        }
        self.guards.revalidate(self.deadline)?;

        let target = format!("{}@{}", self.username, self.host);
        let password_prompt = format!("{target}'s password:");
        let host_key_question =
            "Are you sure you want to continue connecting (yes/no/[fingerprint])?";
        let host_key_prompt = format!(
            "The authenticity of host '{}' can't be established. {host_key_question}",
            self.host
        );
        let passphrase_prompt = self
            .key_path
            .as_ref()
            .map(|path| format!("Enter passphrase for key '{path}':"));

        // Preserve the legacy auto-submit matcher as a bounded normalized
        // stream.  It accepts only the exact configured `user@host's
        // password:` grammar, while allowing CR/LF line wrapping and ASCII
        // case differences.  Only the last three visible lines participate;
        // banners are ignored, but same-line prefixes never become prompts.
        let password_match = self.observe_password_prompt(chunk, &password_prompt)?;
        if self.password_rejected {
            self.invalid_line = true;
            return Ok(PromptMatch::Ignore);
        }

        // OpenSSH emits host-key confirmation over several lines.  Parse line
        // boundaries without discarding a CR/LF-containing chunk, and do not
        // issue a HostKey request until both the exact configured host display
        // and a verified 32-byte SHA256 fingerprint are present.
        let mut saw_delimiter = false;
        let mut host_match = false;
        for byte in chunk {
            if matches!(*byte, b'\r' | b'\n') {
                saw_delimiter = true;
                if self.host_key_started {
                    if prompt_matches(&self.tail, host_key_question) {
                        if self.host_key_fingerprint.is_some() {
                            host_match = true;
                            break;
                        }
                    }
                    if let Some(fingerprint) = parse_host_key_fingerprint(&self.tail) {
                        self.host_key_fingerprint = Some(fingerprint);
                    }
                    self.tail.clear();
                } else {
                    if let Some(display) = parse_host_key_header(&self.tail, &self.host, self.port)
                    {
                        self.host_key_started = true;
                        self.host_key_display = Some(display);
                        self.host_key_bytes = self.tail.len();
                    }
                    self.tail.clear();
                    self.invalid_line = false;
                }
                continue;
            }

            if self.host_key_started {
                self.host_key_bytes = self.host_key_bytes.saturating_add(1);
                if self.host_key_bytes > MAX_PROMPT_TAIL_BYTES {
                    self.tail.clear();
                    self.invalid_line = true;
                    return Err(SshLaunchError::PromptTooLarge {
                        bytes: self.host_key_bytes,
                    });
                }
            }
            let bytes = self.tail.len().saturating_add(1);
            if bytes > MAX_PROMPT_TAIL_BYTES {
                self.tail.clear();
                self.invalid_line = true;
                return Err(SshLaunchError::PromptTooLarge { bytes });
            }
            self.tail.push(*byte);
        }

        if host_match {
            return self.emit_host_key_prompt();
        }
        if self.host_key_started
            && self.host_key_fingerprint.is_some()
            && prompt_matches(&self.tail, host_key_question)
        {
            return self.emit_host_key_prompt();
        }
        if self.host_key_started {
            return Ok(PromptMatch::Ignore);
        }
        if saw_delimiter && !password_match {
            // Delimited lines may be a bounded login banner.  The password
            // matcher separately rejects same-line prompt-shaped prefixes;
            // do not poison the stream merely because the final line ended.
            return Ok(PromptMatch::Ignore);
        }

        if self.invalid_line {
            return Ok(PromptMatch::Ignore);
        }
        let kind = if password_match {
            Some(InputRequestKind::Password)
        } else if passphrase_prompt
            .as_deref()
            .is_some_and(|prompt| prompt_matches(&self.tail, prompt))
        {
            Some(InputRequestKind::Passphrase)
        } else {
            if !is_prompt_prefix(
                &self.tail,
                &password_prompt,
                &host_key_prompt,
                passphrase_prompt.as_deref(),
            ) && !host_key_prefix_matches(&self.tail, &self.host, self.port)
            {
                self.tail.clear();
                self.invalid_line = true;
            }
            return Ok(PromptMatch::Ignore);
        };
        if self.attempts >= MAX_PROMPT_ATTEMPTS {
            return Err(SshLaunchError::AttemptLimit);
        }
        self.attempts += 1;
        self.emitted = true;
        let Some(kind) = kind else {
            return Ok(PromptMatch::Ignore);
        };
        let reference = match kind {
            InputRequestKind::Password => self.password_ref.clone(),
            InputRequestKind::Passphrase => self.passphrase_ref.clone(),
        };
        let Some(reference) = reference else {
            return Err(if kind == InputRequestKind::Password {
                SshLaunchError::InvalidField("password_reference")
            } else {
                SshLaunchError::InvalidField("passphrase_reference")
            });
        };
        Ok(PromptMatch::Input(SshInputRequest::new(
            self.binding.clone(),
            kind,
            reference,
            self.deadline,
            self.cancellation.clone(),
            self.guards.clone(),
        )))
    }

    fn emit_host_key_prompt(&mut self) -> Result<PromptMatch, SshLaunchError> {
        if self.attempts >= MAX_PROMPT_ATTEMPTS {
            return Err(SshLaunchError::AttemptLimit);
        }
        self.attempts += 1;
        self.emitted = true;
        Ok(PromptMatch::HostKey(SshHostKeyPrompt::new(
            self.binding.clone(),
            self.deadline,
            self.cancellation.clone(),
            self.guards.clone(),
            self.host.clone(),
            self.host_key_fingerprint
                .clone()
                .ok_or(SshLaunchError::InvalidField("host_key_fingerprint"))?,
        )))
    }

    fn observe_password_prompt(
        &mut self,
        chunk: &[u8],
        expected: &str,
    ) -> Result<bool, SshLaunchError> {
        if self.password_invalid {
            return Ok(false);
        }
        let expected = expected.to_ascii_lowercase();
        let expected = expected.as_bytes();
        let target_prefix_len = expected
            .strip_suffix(b" password:")
            .map_or(0, |prefix| prefix.len());

        for byte in chunk {
            if matches!(*byte, b'\r' | b'\n') {
                self.commit_password_line(expected, target_prefix_len);
                self.password_line.clear();
                if self.password_match_pending {
                    return Ok(true);
                }
                continue;
            }
            self.password_match_pending = false;
            if self.password_line.len() >= MAX_PROMPT_TAIL_BYTES {
                self.password_invalid = true;
                return Err(SshLaunchError::PromptTooLarge {
                    bytes: self.password_line.len().saturating_add(1),
                });
            }
            self.password_line.push(byte.to_ascii_lowercase());
        }

        let line = normalize_prompt_line(&self.password_line);
        if self.password_invalid {
            return Ok(false);
        }
        if line.is_empty() {
            return Ok(self.password_match_pending);
        }
        if line.as_slice() != expected
            && line
                .windows(b"'s password:".len())
                .any(|window| window == b"'s password:")
        {
            self.password_rejected = true;
            return Ok(false);
        }
        if !self.password_target_seen {
            return Ok(line.as_slice() == expected && self.password_visible_lines.is_empty());
        }
        let mut candidate = self.password_candidate.to_vec();
        if !candidate.is_empty() {
            candidate.push(b' ');
        }
        candidate.extend_from_slice(&line);
        Ok(candidate.as_slice() == expected)
    }

    fn commit_password_line(&mut self, expected: &[u8], target_prefix_len: usize) {
        let line = normalize_prompt_line(&self.password_line);
        if line.is_empty() || self.password_invalid {
            return;
        }
        self.password_match_pending = false;
        if line.as_slice() != expected
            && line
                .windows(b"'s password:".len())
                .any(|window| window == b"'s password:")
        {
            self.password_rejected = true;
            return;
        }
        if self.password_visible_lines.len() == 3 {
            if self.password_candidate_start == Some(0) {
                self.password_candidate.clear();
                self.password_target_seen = false;
                self.password_candidate_start = None;
            } else if let Some(start) = self.password_candidate_start.as_mut() {
                *start -= 1;
            }
            self.password_visible_lines.pop_front();
        }
        self.password_visible_lines
            .push_back(Zeroizing::new(line.clone()));

        // A complete line is the only unambiguous prompt.  This check comes
        // before wrapped-line state so a banner can be followed by the exact
        // target and still produce one request when its newline arrives.
        if line.as_slice() == expected {
            self.password_candidate.clear();
            self.password_candidate_start = None;
            self.password_target_seen = false;
            self.password_match_pending = true;
            return;
        }

        if !self.password_target_seen {
            // Unrelated visible lines are valid bounded banners.  A line
            // that is not an expected prefix simply starts no candidate.
            self.password_candidate.clear();
            self.password_candidate_start = None;
            if expected.starts_with(&line) {
                self.password_candidate.extend_from_slice(&line);
                self.password_candidate_start = Some(self.password_visible_lines.len() - 1);
                if target_prefix_len != 0
                    && line.len() >= target_prefix_len
                    && line[..target_prefix_len] == expected[..target_prefix_len]
                {
                    self.password_target_seen = true;
                }
            }
            return;
        }

        let mut candidate = self.password_candidate.to_vec();
        if !candidate.is_empty() {
            candidate.push(b' ');
        }
        candidate.extend_from_slice(&line);
        if expected.starts_with(&candidate) {
            if candidate.as_slice() == expected {
                self.password_candidate.clear();
                self.password_candidate_start = None;
                self.password_target_seen = false;
                self.password_match_pending = true;
            } else {
                self.password_candidate.clear();
                self.password_candidate.extend_from_slice(&candidate);
            }
        } else {
            self.password_candidate.clear();
            self.password_candidate_start = None;
            self.password_target_seen = false;
            if expected.starts_with(&line) {
                self.password_candidate.extend_from_slice(&line);
                self.password_candidate_start = Some(self.password_visible_lines.len() - 1);
                if target_prefix_len != 0
                    && line.len() >= target_prefix_len
                    && line[..target_prefix_len] == expected[..target_prefix_len]
                {
                    self.password_target_seen = true;
                }
            }
        }
    }
}

fn normalize_prompt_line(value: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(value.len());
    let mut pending_space = false;
    for byte in value.iter().copied() {
        if byte.is_ascii_whitespace() {
            pending_space = !normalized.is_empty();
        } else {
            if pending_space {
                normalized.push(b' ');
            }
            normalized.push(byte.to_ascii_lowercase());
            pending_space = false;
        }
    }
    normalized
}

fn parse_host_key_header(value: &[u8], expected_host: &str, expected_port: u16) -> Option<String> {
    let value = std::str::from_utf8(value).ok()?;
    let prefix = "The authenticity of host '";
    let suffix = "' can't be established.";
    let inner = value.strip_prefix(prefix)?.strip_suffix(suffix)?;
    let (host_display, address) = inner.rsplit_once(" (")?;
    let address = address.strip_suffix(')')?;
    if address.is_empty() {
        return None;
    }
    let bracketed_address = address.starts_with('[') || address.ends_with(']');
    let address = if bracketed_address {
        address.strip_prefix('[')?.strip_suffix(']')?
    } else {
        address
    };
    let parsed_address = address.parse::<std::net::IpAddr>().ok()?;
    if address.is_empty() {
        return None;
    }

    let direct = host_display == expected_host;
    let bracketed = host_display == format!("[{expected_host}]:{expected_port}");
    // OpenSSH's direct host form carries a plain address; its non-default
    // port form carries the exact bracketed host/port and may bracket IPv6.
    if (direct && bracketed_address)
        || (!direct && !bracketed)
        || (bracketed && parsed_address.is_ipv6() && !bracketed_address)
    {
        return None;
    }
    if direct || bracketed {
        Some(host_display.to_string())
    } else {
        None
    }
}

fn parse_host_key_fingerprint(value: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(value).ok()?;
    if value.is_empty() || value.len() > MAX_ID_BYTES {
        return None;
    }

    // OpenSSH prints one exact key-type line.  Permit the optional terminal
    // sentence period once, but do not trim or search within a banner: the
    // caller must have already isolated the current complete output line.
    let line = value.strip_suffix('.').unwrap_or(value);
    if line.contains('.') {
        return None;
    }
    let (key_type, fingerprint) = line.split_once(" key fingerprint is ")?;
    if !matches!(
        key_type,
        "DSA" | "ECDSA" | "ECDSA-SK" | "ED25519" | "ED25519-SK" | "RSA"
    ) {
        return None;
    }
    if fingerprint.len() > MAX_ID_BYTES || !fingerprint.starts_with("SHA256:") {
        return None;
    }
    let payload = &fingerprint["SHA256:".len()..];
    if payload.is_empty() || payload.contains('=') {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(payload)
        .ok()?;
    if decoded.len() != 32 {
        return None;
    }
    let canonical = base64::engine::general_purpose::STANDARD_NO_PAD.encode(decoded);
    if canonical != payload {
        return None;
    }
    Some(format!("SHA256:{}", canonical))
}

fn host_key_prefix_matches(value: &[u8], expected_host: &str, expected_port: u16) -> bool {
    let direct = format!("The authenticity of host '{expected_host}");
    let bracketed = format!("The authenticity of host '[{expected_host}]:{expected_port}");
    prompt_prefix_or_display_suffix(value, direct.as_bytes())
        || prompt_prefix_or_display_suffix(value, bracketed.as_bytes())
}

fn prompt_prefix_or_display_suffix(value: &[u8], expected_prefix: &[u8]) -> bool {
    if expected_prefix.starts_with(value) {
        return true;
    }
    if !value.starts_with(expected_prefix) {
        return false;
    }
    value[expected_prefix.len()..].iter().copied().all(|byte| {
        byte == b'\''
            || byte == b' '
            || byte == b'('
            || byte == b')'
            || byte == b'['
            || byte == b']'
            || byte == b'.'
            || byte == b':'
            || byte.is_ascii_hexdigit()
    })
}

fn is_prompt_prefix(
    value: &[u8],
    password: &str,
    host_key: &str,
    passphrase: Option<&str>,
) -> bool {
    [Some(password), Some(host_key), passphrase]
        .into_iter()
        .flatten()
        .any(|prompt| prompt.as_bytes().starts_with(value))
}

fn prompt_matches(value: &[u8], expected: &str) -> bool {
    let expected = expected.as_bytes();
    if value == expected {
        return true;
    }
    value
        .strip_prefix(expected)
        .is_some_and(|suffix| !suffix.is_empty() && suffix.iter().all(|byte| *byte == b' '))
}

fn validate_reference(reference: &CredentialRef) -> Result<(), SshLaunchError> {
    CredentialRef::parse(reference.as_str())
        .map(|_| ())
        .map_err(SshLaunchError::Credential)
}

fn bounded_text(field: &'static str, value: &str, max: usize) -> Result<String, SshLaunchError> {
    if value.is_empty()
        || value.len() > max
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b'\0')
    {
        return Err(SshLaunchError::InvalidField(field));
    }
    Ok(value.to_string())
}

fn validate_host(value: &str) -> Result<String, SshLaunchError> {
    let value = bounded_text("host", value, MAX_HOST_BYTES)?;
    if value.starts_with('-') || value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(SshLaunchError::InvalidField("host"));
    }
    Ok(value)
}

fn validate_username(value: &str) -> Result<String, SshLaunchError> {
    let value = bounded_text("username", value, MAX_USERNAME_BYTES)?;
    if value.starts_with('-') || value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(SshLaunchError::InvalidField("username"));
    }
    Ok(value)
}

fn validate_proxy_jump(value: &str) -> Result<(), SshLaunchError> {
    if value.is_empty()
        || value.len() > MAX_PROXY_JUMP_BYTES
        || value.starts_with('-')
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(SshLaunchError::InvalidField("proxy_jump"));
    }
    Ok(())
}

fn validate_key_path(path: &Path) -> Result<(), SshLaunchError> {
    if !path.is_absolute() || native_path_length(path) > MAX_KNOWN_HOSTS_PATH_BYTES {
        return Err(SshLaunchError::InvalidField("key_path"));
    }
    let rendered = path.to_string_lossy();
    if rendered.starts_with('-')
        || rendered
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(SshLaunchError::InvalidField("key_path"));
    }
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| SshLaunchError::InvalidField("key_path"))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(SshLaunchError::InvalidField("key_path"));
    }
    Ok(())
}

fn native_path_length(path: &Path) -> usize {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        return path.as_os_str().as_bytes().len();
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        return path
            .as_os_str()
            .encode_wide()
            .count()
            .saturating_mul(std::mem::size_of::<u16>());
    }
    #[cfg(not(any(unix, windows)))]
    {
        path.as_os_str().len()
    }
}

fn validate_known_hosts_path(path: &Path) -> Result<(), SshLaunchError> {
    validate_key_path(path).map_err(|_| SshLaunchError::InvalidField("known_hosts_path"))
}

fn validate_env(env: &BTreeMap<String, String>) -> Result<(), SshLaunchError> {
    if env.len() > MAX_ENV_ENTRIES {
        return Err(SshLaunchError::EnvironmentTooLarge { entries: env.len() });
    }
    for (key, value) in env {
        if !matches!(key.as_str(), "TERM" | "COLORTERM" | "LANG" | "LC_ALL") {
            return Err(SshLaunchError::InvalidField("environment_key"));
        }
        if key.len() > MAX_ENV_KEY_BYTES
            || value.is_empty()
            || value.len() > MAX_ENV_VALUE_BYTES
            || value.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(SshLaunchError::InvalidField("environment_value"));
        }
        let allowed = match key.as_str() {
            "TERM" => matches!(
                value.as_str(),
                "dumb" | "xterm" | "xterm-256color" | "screen" | "screen-256color"
            ),
            "COLORTERM" => matches!(value.as_str(), "truecolor" | "24bit"),
            "LANG" | "LC_ALL" => matches!(value.as_str(), "C" | "C.UTF-8" | "en_US.UTF-8"),
            _ => false,
        };
        if !allowed {
            return Err(SshLaunchError::InvalidField("environment_value"));
        }
    }
    Ok(())
}

fn validate_args(args: &[String]) -> Result<(), SshLaunchError> {
    if args.len() > MAX_ARGS {
        return Err(SshLaunchError::ArgumentTooLarge { bytes: args.len() });
    }
    for arg in args {
        if arg.len() > MAX_ARG_BYTES || arg.bytes().any(|byte| byte == 0 || byte.is_ascii_control())
        {
            return Err(SshLaunchError::ArgumentTooLarge { bytes: arg.len() });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use std::collections::BTreeMap;
    use std::io::Write;
    use zeroize::Zeroizing;

    const PASSWORD_REF: &str = "credential:test-password";
    const KEY_REF: &str = "credential:test-private-key";
    const PASSPHRASE_REF: &str = "credential:test-passphrase";
    const PASSWORD: &str = "fixture-password-never-journaled";
    const PRIVATE_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\r\nfixture-key-material\r\n-----END OPENSSH PRIVATE KEY-----";
    const PASSPHRASE: &str = "fixture-passphrase-never-journaled";

    fn valid_host_key_fingerprint() -> String {
        format!(
            "SHA256:{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode([0u8; 32])
        )
    }

    #[derive(Default)]
    struct FixtureCredentials {
        values: BTreeMap<CredentialRef, (CredentialKind, Zeroizing<Vec<u8>>)>,
    }

    impl FixtureCredentials {
        fn with_password(mut self) -> Self {
            self.values.insert(
                CredentialRef::parse(PASSWORD_REF).unwrap(),
                (
                    CredentialKind::Password,
                    Zeroizing::new(PASSWORD.as_bytes().to_vec()),
                ),
            );
            self
        }

        fn with_key(mut self) -> Self {
            self.values.insert(
                CredentialRef::parse(KEY_REF).unwrap(),
                (
                    CredentialKind::PrivateKey,
                    Zeroizing::new(PRIVATE_KEY.as_bytes().to_vec()),
                ),
            );
            self
        }

        fn with_passphrase(mut self) -> Self {
            self.values.insert(
                CredentialRef::parse(PASSPHRASE_REF).unwrap(),
                (
                    CredentialKind::Passphrase,
                    Zeroizing::new(PASSPHRASE.as_bytes().to_vec()),
                ),
            );
            self
        }
    }

    impl CredentialResolver for FixtureCredentials {
        fn resolve(&self, reference: &CredentialRef) -> Result<CredentialSecret, CredentialError> {
            let (kind, value) = self
                .values
                .get(reference)
                .ok_or_else(|| CredentialError::MissingReference(reference.clone()))?;
            CredentialSecret::from_bytes(*kind, value.as_slice())
        }
    }

    fn issued(generation: u64, action_epoch: u64) -> HostIssuedSshBinding {
        let executable = std::env::current_exe().expect("test executable");
        let process = ManagedProcessIdentity::new(
            ManagedProcessId::new(std::process::id().max(1), 1).unwrap(),
            executable,
        )
        .expect("process identity");
        HostIssuedSshBindingIssuer::issue(
            TaskId::new(),
            ResourceId::new(),
            generation,
            action_epoch,
            process,
            [generation as u8; 16],
        )
        .expect("issued binding")
    }

    fn request(auth: AuthConfig, generation: u64) -> SshLaunchRequest {
        SshLaunchRequest::new(
            issued(generation, 1),
            "connection-fixture",
            "example.test",
            2222,
            "deploy",
            auth,
            Duration::from_secs(30),
        )
        .expect("request")
    }

    fn build(
        request: &SshLaunchRequest,
        credentials: &FixtureCredentials,
        store: Option<&KeyMaterialStore>,
        cancellation: &CancellationToken,
    ) -> LaunchOutcome {
        build_ssh_launch_plan(request, credentials, store, cancellation).expect("launch contract")
    }

    #[test]
    fn binding_is_host_issued_and_exact_process_epoch_fenced() {
        let left = issued(7, 3).into_binding();
        let right = issued(7, 3).into_binding();
        assert_ne!(
            left, right,
            "distinct task/resource/nonce authority must not alias"
        );
        assert!(!format!("{left:?}").contains("task"));
        assert!(!format!("{left:?}").contains("generation"));
    }

    #[test]
    fn password_lease_is_single_use_and_stale_is_rejected_without_delivery() {
        let credentials = FixtureCredentials::default().with_password();
        let cancellation = CancellationToken::new();
        let password_request = request(AuthConfig::password(PASSWORD_REF).unwrap(), 7);
        let LaunchOutcome::Ready(plan) =
            build(&password_request, &credentials, None, &cancellation)
        else {
            panic!("ready")
        };
        let mut matcher = plan.prompt_matcher();
        let PromptMatch::Input(input) =
            matcher.observe(b"deploy@example.test's password:").unwrap()
        else {
            panic!("input")
        };
        assert!(input.accepts_binding(password_request.binding_as_test()));
        assert_eq!(
            input
                .resolve(&credentials, password_request.binding_as_test())
                .unwrap()
                .bytes(),
            format!("{PASSWORD}\r").as_bytes()
        );
        assert!(matches!(
            input.resolve(&credentials, password_request.binding_as_test()),
            Err(SshLaunchError::AlreadyConsumed)
        ));
        let stale = SshLaunchRequest::new(
            issued(8, 1),
            "connection-fixture",
            "example.test",
            22,
            "deploy",
            AuthConfig::agent(),
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(!input.accepts_binding(stale.binding_as_test()));
        assert!(matches!(
            input.resolve(&credentials, stale.binding_as_test()),
            Err(SshLaunchError::StaleBinding)
        ));
    }

    #[test]
    fn exact_prompt_accepts_bounded_fragmentation_but_not_line_delimiters() {
        let credentials = FixtureCredentials::default().with_password();
        let request = request(AuthConfig::password(PASSWORD_REF).unwrap(), 7);
        let LaunchOutcome::Ready(plan) =
            build(&request, &credentials, None, &CancellationToken::new())
        else {
            panic!("ready")
        };
        let mut matcher = plan.prompt_matcher();
        assert!(matches!(
            matcher.observe(b"deploy@example.test's pass"),
            Ok(PromptMatch::Ignore)
        ));
        assert!(matches!(
            matcher.observe(b"word:"),
            Ok(PromptMatch::Input(_))
        ));

        let LaunchOutcome::Ready(plan) =
            build(&request, &credentials, None, &CancellationToken::new())
        else {
            panic!("ready")
        };
        let mut matcher = plan.prompt_matcher();
        assert!(matches!(
            matcher.observe(b"deploy@example.test's pass\r\nword:"),
            Ok(PromptMatch::Ignore)
        ));

        let LaunchOutcome::Ready(plan) =
            build(&request, &credentials, None, &CancellationToken::new())
        else {
            panic!("ready")
        };
        let mut matcher = plan.prompt_matcher();
        assert!(matches!(
            matcher.observe(b"deploy@example.test's password: "),
            Ok(PromptMatch::Input(_))
        ));
    }

    #[test]
    fn key_and_known_hosts_mutation_is_rejected_before_prompt_delivery() {
        let temp = tempfile::tempdir().unwrap();
        let key_path = temp.path().join("id_ed25519");
        let known_hosts = temp.path().join("known_hosts");
        std::fs::write(&key_path, b"configured key fixture").unwrap();
        std::fs::write(&known_hosts, b"known-host fixture\n").unwrap();
        let configured_request = request(AuthConfig::key_path(key_path.clone()).unwrap(), 1)
            .with_network_policy(None, Some(known_hosts.clone()), SshKnownHostPolicy::Strict)
            .unwrap();
        let cancellation = CancellationToken::new();
        let LaunchOutcome::Ready(plan) = build(
            &configured_request,
            &FixtureCredentials::default(),
            None,
            &cancellation,
        ) else {
            panic!("ready")
        };
        std::fs::write(&known_hosts, b"mutated known-host fixture\n").unwrap();
        let mut matcher = plan.prompt_matcher();
        assert!(matches!(
            matcher.observe(
                b"The authenticity of host 'example.test' can't be established. Are you sure you want to continue connecting (yes/no/[fingerprint])?"
            ),
            Err(SshLaunchError::Credential(CredentialError::InvalidPath))
        ));

        let pasted_request = request(
            AuthConfig::pasted_key_with_passphrase(KEY_REF, PASSPHRASE_REF).unwrap(),
            2,
        );
        let store = KeyMaterialStore::new(temp.path().join("keys")).unwrap();
        let credentials = FixtureCredentials::default().with_key().with_passphrase();
        let LaunchOutcome::Ready(plan) = build(
            &pasted_request,
            &credentials,
            Some(&store),
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };
        let path = plan.retained_key().unwrap().path().to_path_buf();
        std::fs::write(&path, b"replacement key bytes").unwrap();
        let mut matcher = plan.prompt_matcher();
        assert!(matches!(
            matcher.observe(format!("Enter passphrase for key '{}':", path.display()).as_bytes()),
            Err(SshLaunchError::Credential(CredentialError::InvalidPath))
        ));
    }

    #[test]
    fn password_match_requires_exact_legacy_target_and_host_key_prompt_shape() {
        let password_request = request(AuthConfig::password(PASSWORD_REF).unwrap(), 7);
        let LaunchOutcome::Ready(plan) = build(
            &password_request,
            &FixtureCredentials::default().with_password(),
            None,
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };
        let mut password_matcher = plan.prompt_matcher();
        assert!(matches!(
            password_matcher.observe(b"deploy authenticated via example.test password:"),
            Ok(PromptMatch::Ignore)
        ));

        let host_request = request(AuthConfig::agent(), 8);
        let LaunchOutcome::Ready(host_plan) = build(
            &host_request,
            &FixtureCredentials::default(),
            None,
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };
        let mut host_matcher = host_plan.prompt_matcher();
        assert!(matches!(
            host_matcher
                .observe(b"The authenticity of host 'example.test' can't be established. yes/no?"),
            Ok(PromptMatch::Ignore)
        ));
    }

    #[test]
    fn prompt_grammar_rejects_wrong_host_banner_prefix_and_multiline_current_lines() {
        let credentials = FixtureCredentials::default().with_password();
        let request = request(AuthConfig::password(PASSWORD_REF).unwrap(), 7);

        let cases = [
            b"banner\ndeploy@example.test's password:".as_slice(),
            b"prefix deploy@example.test's password:".as_slice(),
            b"deploy@example.test's password:\n".as_slice(),
            b"deploy@other.example.test's password:".as_slice(),
            b"The authenticity of host 'other.example.test' can't be established. Are you sure you want to continue connecting (yes/no/[fingerprint])?".as_slice(),
        ];
        for chunk in cases {
            let LaunchOutcome::Ready(plan) =
                build(&request, &credentials, None, &CancellationToken::new())
            else {
                panic!("ready")
            };
            let mut matcher = plan.prompt_matcher();
            if chunk == b"deploy@example.test's password:\n" {
                assert!(matches!(matcher.observe(chunk), Ok(PromptMatch::Input(_))));
            } else {
                assert!(
                    matches!(matcher.observe(chunk), Ok(PromptMatch::Ignore)),
                    "{chunk:?}"
                );
            }
        }

        let LaunchOutcome::Ready(plan) =
            build(&request, &credentials, None, &CancellationToken::new())
        else {
            panic!("ready")
        };
        let mut matcher = plan.prompt_matcher();
        assert!(matches!(
            matcher.observe(b"banner fragment without newline"),
            Ok(PromptMatch::Ignore)
        ));
        assert!(matches!(
            matcher.observe(b"deploy@example.test's password:"),
            Ok(PromptMatch::Ignore)
        ));
    }

    #[test]
    fn passphrase_prompt_is_bound_to_the_retained_key_identity() {
        let temp = tempfile::tempdir().unwrap();
        let store = KeyMaterialStore::new(temp.path()).unwrap();
        let credentials = FixtureCredentials::default().with_key().with_passphrase();
        let request = request(
            AuthConfig::pasted_key_with_passphrase(KEY_REF, PASSPHRASE_REF).unwrap(),
            7,
        );
        let LaunchOutcome::Ready(plan) = build(
            &request,
            &credentials,
            Some(&store),
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };
        let key_path = plan.retained_key().unwrap().path_string();
        let wrong = format!("Enter passphrase for key '{}-wrong':", key_path);
        let exact = format!("Enter passphrase for key '{key_path}':");
        let mut matcher = plan.prompt_matcher();
        assert!(matches!(
            matcher.observe(wrong.as_bytes()),
            Ok(PromptMatch::Ignore)
        ));
        assert!(matches!(matcher.observe(b"\n"), Ok(PromptMatch::Ignore)));
        assert!(matches!(
            matcher.observe(exact.as_bytes()),
            Ok(PromptMatch::Input(_))
        ));
    }

    #[test]
    fn resolver_completion_rechecks_deadline_and_cancellation_before_plan_delivery() {
        struct DelayedResolver;

        impl CredentialResolver for DelayedResolver {
            fn resolve(
                &self,
                _reference: &CredentialRef,
            ) -> Result<CredentialSecret, CredentialError> {
                std::thread::sleep(Duration::from_millis(20));
                Ok(CredentialSecret::password(PASSWORD))
            }
        }

        let deadline_request = SshLaunchRequest::new_with_deadline(
            issued(7, 1),
            "connection",
            "example.test",
            22,
            "deploy",
            AuthConfig::password(PASSWORD_REF).unwrap(),
            Instant::now() + Duration::from_millis(1),
        )
        .unwrap();
        assert!(matches!(
            build_ssh_launch_plan(
                &deadline_request,
                &DelayedResolver,
                None,
                &CancellationToken::new(),
            ),
            Err(SshLaunchError::DeadlineExpired)
        ));

        struct CancelAfterResolve {
            cancellation: CancellationToken,
            binding: BindingClaim,
        }

        impl CredentialResolver for CancelAfterResolve {
            fn resolve(
                &self,
                _reference: &CredentialRef,
            ) -> Result<CredentialSecret, CredentialError> {
                self.cancellation.cancel(&self.binding).unwrap();
                Ok(CredentialSecret::password(PASSWORD))
            }
        }

        let cancellation = CancellationToken::new();
        let cancelled_request = request(AuthConfig::password(PASSWORD_REF).unwrap(), 8);
        let resolver = CancelAfterResolve {
            cancellation: cancellation.clone(),
            binding: cancelled_request.binding.clone(),
        };
        assert!(matches!(
            build_ssh_launch_plan(&cancelled_request, &resolver, None, &cancellation),
            Ok(LaunchOutcome::Cancelled)
        ));
    }

    #[test]
    fn input_delivery_rechecks_lease_after_resolver_before_secret_delivery() {
        struct CancelAfterResolve {
            cancellation: CancellationToken,
            binding: BindingClaim,
        }

        impl CredentialResolver for CancelAfterResolve {
            fn resolve(
                &self,
                _reference: &CredentialRef,
            ) -> Result<CredentialSecret, CredentialError> {
                self.cancellation.cancel(&self.binding).unwrap();
                Ok(CredentialSecret::password(PASSWORD))
            }
        }

        let cancellation = CancellationToken::new();
        let request = request(AuthConfig::password(PASSWORD_REF).unwrap(), 11);
        let LaunchOutcome::Ready(plan) = build(
            &request,
            &FixtureCredentials::default().with_password(),
            None,
            &cancellation,
        ) else {
            panic!("ready")
        };
        let mut matcher = plan.prompt_matcher();
        let PromptMatch::Input(input) =
            matcher.observe(b"deploy@example.test's password:").unwrap()
        else {
            panic!("input")
        };
        let resolver = CancelAfterResolve {
            cancellation: cancellation.clone(),
            binding: request.binding.clone(),
        };
        assert!(matches!(
            input.resolve(&resolver, request.binding_as_test()),
            Err(SshLaunchError::Cancelled)
        ));
    }

    #[test]
    fn consumed_leases_remain_already_consumed_after_cancel() {
        let credentials = FixtureCredentials::default().with_password();
        let cancellation = CancellationToken::new();
        let password_request = request(AuthConfig::password(PASSWORD_REF).unwrap(), 7);
        let LaunchOutcome::Ready(plan) =
            build(&password_request, &credentials, None, &cancellation)
        else {
            panic!("ready")
        };
        let mut matcher = plan.prompt_matcher();
        let PromptMatch::Input(input) =
            matcher.observe(b"deploy@example.test's password:").unwrap()
        else {
            panic!("input")
        };
        input
            .resolve(&credentials, password_request.binding_as_test())
            .unwrap();
        cancellation.cancel(&password_request.binding).unwrap();
        assert!(matches!(
            input.resolve(&credentials, password_request.binding_as_test()),
            Err(SshLaunchError::AlreadyConsumed)
        ));

        let host_request = request(AuthConfig::agent(), 8);
        let host_cancellation = CancellationToken::new();
        let LaunchOutcome::Ready(host_plan) =
            build(&host_request, &credentials, None, &host_cancellation)
        else {
            panic!("ready")
        };
        let mut host_matcher = host_plan.prompt_matcher();
        assert!(matches!(
            host_matcher
                .observe(
                    format!(
                        "The authenticity of host 'example.test (192.0.2.1)' can't be established.\r\nED25519 key fingerprint is {}.\r\n",
                        valid_host_key_fingerprint()
                    )
                    .as_bytes()
                ),
            Ok(PromptMatch::Ignore)
        ));
        assert!(matches!(
            host_matcher
                .observe(b"Are you sure you want to continue connecting (yes/no/[fingerprint])?"),
            Ok(PromptMatch::HostKey(_))
        ));
        host_cancellation.cancel(&host_request.binding).unwrap();
        assert!(matches!(
            host_matcher
                .observe(b"The authenticity of host 'example.test (192.0.2.1)' can't be established. Are you sure you want to continue connecting (yes/no/[fingerprint])?"),
            Err(SshLaunchError::AlreadyConsumed)
        ));
    }

    #[test]
    fn host_key_and_passphrase_order_are_typed_and_single_use() {
        let temp = tempfile::tempdir().unwrap();
        let store = KeyMaterialStore::new(temp.path()).unwrap();
        let credentials = FixtureCredentials::default().with_key().with_passphrase();
        let passphrase_request = request(
            AuthConfig::pasted_key_with_passphrase(KEY_REF, PASSPHRASE_REF).unwrap(),
            9,
        );
        let LaunchOutcome::Ready(plan) = build(
            &passphrase_request,
            &credentials,
            Some(&store),
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };
        let key_path = plan.retained_key().unwrap().path_string();
        let mut matcher = plan.prompt_matcher();
        let passphrase_prompt = format!("Enter passphrase for key '{key_path}':");
        let PromptMatch::Input(passphrase) = matcher.observe(passphrase_prompt.as_bytes()).unwrap()
        else {
            panic!("passphrase")
        };
        assert_eq!(passphrase.kind(), InputRequestKind::Passphrase);
        assert_eq!(
            passphrase
                .resolve(&credentials, passphrase_request.binding_as_test())
                .unwrap()
                .bytes(),
            format!("{PASSPHRASE}\r").as_bytes()
        );
        assert!(matches!(
            matcher.observe(passphrase_prompt.as_bytes()),
            Err(SshLaunchError::AlreadyConsumed)
        ));

        let host_key_request = request(AuthConfig::agent(), 10);
        let LaunchOutcome::Ready(plan) = build(
            &host_key_request,
            &credentials,
            None,
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };
        let mut matcher = plan.prompt_matcher();
        let PromptMatch::HostKey(prompt) = ({
            assert!(matches!(
                matcher
                    .observe(
                        format!(
                            "The authenticity of host 'example.test (192.0.2.1)' can't be established.\r\nED25519 key fingerprint is {}.\r\n",
                            valid_host_key_fingerprint()
                        )
                        .as_bytes()
                    ),
                Ok(PromptMatch::Ignore)
            ));
            matcher
                .observe(b"Are you sure you want to continue connecting (yes/no/[fingerprint])?")
                .unwrap()
        }) else {
            panic!("host key")
        };
        assert_eq!(
            prompt
                .accept(host_key_request.binding_as_test())
                .unwrap()
                .bytes(),
            b"yes\r"
        );
        assert!(matches!(
            prompt.accept(host_key_request.binding_as_test()),
            Err(SshLaunchError::AlreadyConsumed)
        ));
    }

    #[test]
    fn agent_and_default_auth_never_add_identity_or_secret_arguments() {
        for auth in [AuthConfig::agent(), AuthConfig::default_auth()] {
            let request = request(auth, 1);
            let LaunchOutcome::Ready(plan) = build(
                &request,
                &FixtureCredentials::default(),
                None,
                &CancellationToken::new(),
            ) else {
                panic!("ready")
            };
            assert!(!plan.command.args().contains(&"-i".to_string()));
            assert!(plan.snapshot.auth().is_non_secret());
        }
    }

    #[test]
    fn configured_key_path_is_a_bounded_argument_only() {
        let temp = tempfile::tempdir().unwrap();
        let key_path = temp.path().join("id_ed25519");
        std::fs::write(&key_path, b"configured key fixture").unwrap();
        let request = request(AuthConfig::key_path(key_path.clone()).unwrap(), 1);
        let LaunchOutcome::Ready(plan) = build(
            &request,
            &FixtureCredentials::default(),
            None,
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };
        let canonical_key_path = std::fs::canonicalize(&key_path).unwrap();
        assert!(plan.command.args().windows(2).any(|args| {
            args[0] == "-i" && args[1] == canonical_key_path.to_string_lossy().as_ref()
        }));
        assert!(Path::new(&plan.command.program).is_absolute());
        assert!(!format!("{plan:?}").contains("fixture-key-material"));
    }

    #[test]
    fn configured_key_auth_can_fall_back_to_password_without_secret_in_plan() {
        let temp = tempfile::tempdir().unwrap();
        let key_path = temp.path().join("id_ed25519");
        std::fs::write(&key_path, b"configured key fixture").unwrap();
        let request = request(
            AuthConfig::key_path_with_password(&key_path, PASSWORD_REF).unwrap(),
            12,
        );
        let credentials = FixtureCredentials::default().with_password();
        let LaunchOutcome::Ready(plan) =
            build(&request, &credentials, None, &CancellationToken::new())
        else {
            panic!("ready")
        };
        let mut matcher = plan.prompt_matcher();
        let PromptMatch::Input(input) =
            matcher.observe(b"deploy@example.test's password:").unwrap()
        else {
            panic!("password fallback")
        };
        assert_eq!(input.kind(), InputRequestKind::Password);
        assert_eq!(
            input
                .resolve(&credentials, request.binding_as_test())
                .unwrap()
                .bytes(),
            format!("{PASSWORD}\r").as_bytes()
        );
        let rendered = format!("{plan:?}");
        assert!(!rendered.contains(PASSWORD));
        assert!(!serde_json::to_string(plan.snapshot())
            .unwrap()
            .contains(PASSWORD));
    }

    #[test]
    fn pasted_key_authority_survives_plan_drop_until_supervisor_settlement() {
        let temp = tempfile::tempdir().unwrap();
        let store = KeyMaterialStore::new(temp.path()).unwrap();
        let credentials = FixtureCredentials::default().with_key();
        let request = request(AuthConfig::pasted_key(KEY_REF).unwrap(), 17);
        let LaunchOutcome::Ready(mut plan) = build(
            &request,
            &credentials,
            Some(&store),
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };
        let key_path = plan.retained_key().unwrap().path().to_path_buf();
        let authority = plan.pre_spawn().expect("pre-spawn authority");
        drop(plan);
        assert!(
            key_path.exists(),
            "the supervisor authority must retain pasted-key material after plan drop"
        );
        drop(authority);
        assert!(
            !key_path.exists(),
            "settlement must release pasted-key material"
        );
    }

    #[test]
    fn pre_spawn_is_single_use_and_cannot_issue_second_command_authority() {
        let temp = tempfile::tempdir().unwrap();
        let store = KeyMaterialStore::new(temp.path()).unwrap();
        let credentials = FixtureCredentials::default().with_key();
        let request = request(AuthConfig::pasted_key(KEY_REF).unwrap(), 22);
        let LaunchOutcome::Ready(mut plan) = build(
            &request,
            &credentials,
            Some(&store),
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };

        let authority = plan.pre_spawn().expect("first pre-spawn authority");
        assert!(
            matches!(plan.pre_spawn(), Err(SshLaunchError::PreSpawnConsumed)),
            "a launch plan must not mint a second command authority"
        );
        drop(authority);
    }

    #[test]
    fn pasted_key_password_fallback_keeps_secret_out_of_plan_and_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let store = KeyMaterialStore::new(temp.path()).unwrap();
        let credentials = FixtureCredentials::default().with_key().with_password();
        let request = request(
            AuthConfig::pasted_key_with_password(KEY_REF, PASSWORD_REF).unwrap(),
            15,
        );
        let LaunchOutcome::Ready(plan) = build(
            &request,
            &credentials,
            Some(&store),
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };

        let mut matcher = plan.prompt_matcher();
        let PromptMatch::Input(input) =
            matcher.observe(b"deploy@example.test's password:").unwrap()
        else {
            panic!("password fallback")
        };
        assert_eq!(input.kind(), InputRequestKind::Password);
        assert_eq!(
            input
                .resolve(&credentials, request.binding_as_test())
                .unwrap()
                .bytes(),
            format!("{PASSWORD}\r").as_bytes()
        );
        assert!(!format!("{plan:?}").contains(PASSWORD));
        assert!(!serde_json::to_string(plan.snapshot())
            .unwrap()
            .contains(PASSWORD));
    }

    #[test]
    fn pre_spawn_authority_revalidates_retained_path_guards_immediately_before_spawn() {
        let temp = tempfile::tempdir().unwrap();
        let key_path = temp.path().join("id_ed25519");
        let known_hosts = temp.path().join("known_hosts");
        std::fs::write(&key_path, b"configured key fixture").unwrap();
        std::fs::write(&known_hosts, b"known-host fixture\n").unwrap();
        let request = request(AuthConfig::key_path(key_path).unwrap(), 13)
            .with_network_policy(None, Some(known_hosts.clone()), SshKnownHostPolicy::Strict)
            .unwrap();
        let LaunchOutcome::Ready(mut plan) = build(
            &request,
            &FixtureCredentials::default(),
            None,
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };
        let authority = plan.pre_spawn().expect("pre-spawn authority");
        std::fs::write(&known_hosts, b"known-host replacement\n").unwrap();
        assert!(matches!(
            authority.revalidate(),
            Err(SshLaunchError::Credential(CredentialError::InvalidPath))
        ));
    }

    #[test]
    fn realistic_multiline_host_key_output_emits_one_typed_prompt() {
        let request = request(AuthConfig::agent(), 14);
        let LaunchOutcome::Ready(plan) = build(
            &request,
            &FixtureCredentials::default(),
            None,
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };
        let mut matcher = plan.prompt_matcher();
        assert!(matches!(
            matcher.observe(
                format!(
                    "The authenticity of host 'example.test (192.0.2.1)' can't be established.\r\nED25519 key fingerprint is {}\r\n",
                    valid_host_key_fingerprint()
                )
                .as_bytes()
            ),
            Ok(PromptMatch::Ignore)
        ));
        assert!(matches!(
            matcher
                .observe(b"Are you sure you want to continue connecting (yes/no/[fingerprint])?"),
            Ok(PromptMatch::HostKey(_))
        ));
    }

    #[test]
    fn realistic_host_key_header_with_ip_binds_configured_host_and_fingerprint() {
        let request = request(AuthConfig::agent(), 16);
        let LaunchOutcome::Ready(plan) = build(
            &request,
            &FixtureCredentials::default(),
            None,
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };
        let mut matcher = plan.prompt_matcher();
        assert!(matches!(
            matcher.observe(
                format!(
                    "The authenticity of host 'example.test (192.0.2.1)' can't be established.\r\nED25519 key fingerprint is {}.\r\n",
                    valid_host_key_fingerprint()
                )
                .as_bytes()
            ),
            Ok(PromptMatch::Ignore)
        ));
        let PromptMatch::HostKey(prompt) = matcher
            .observe(b"Are you sure you want to continue connecting (yes/no/[fingerprint])?")
            .unwrap()
        else {
            panic!("host key")
        };
        assert_eq!(prompt.host(), "example.test");
        assert_eq!(prompt.fingerprint(), valid_host_key_fingerprint());
    }

    #[test]
    fn host_key_requires_exact_sha256_payload_and_configured_host_display() {
        let fingerprint = valid_host_key_fingerprint();
        for (header, expected_host) in [
            (
                "The authenticity of host 'example.test (192.0.2.1)' can't be established.",
                "example.test",
            ),
            (
                "The authenticity of host 'example.test (2001:db8::1)' can't be established.",
                "example.test",
            ),
            (
                "The authenticity of host '[example.test]:2222 ([2001:db8::1])' can't be established.",
                "example.test",
            ),
        ] {
            let request = request(AuthConfig::agent(), 18);
            let LaunchOutcome::Ready(plan) = build(
                &request,
                &FixtureCredentials::default(),
                None,
                &CancellationToken::new(),
            ) else {
                panic!("ready")
            };
            let mut matcher = plan.prompt_matcher();
            let chunk = format!("{header}\r\nED25519 key fingerprint is {fingerprint}.\r\n");
            assert!(matches!(
                matcher.observe(chunk.as_bytes()),
                Ok(PromptMatch::Ignore)
            ));
            let PromptMatch::HostKey(prompt) = matcher
                .observe(b"Are you sure you want to continue connecting (yes/no/[fingerprint])?")
                .unwrap()
            else {
                panic!("host key")
            };
            assert_eq!(prompt.host(), expected_host);
            assert_eq!(prompt.fingerprint(), fingerprint);
        }

        for malformed in [
            "SHA256:",
            "SHA256:fixture",
            "SHA256:!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!",
        ] {
            let request = request(AuthConfig::agent(), 19);
            let LaunchOutcome::Ready(plan) = build(
                &request,
                &FixtureCredentials::default(),
                None,
                &CancellationToken::new(),
            ) else {
                panic!("ready")
            };
            let mut matcher = plan.prompt_matcher();
            let chunk = format!(
                "The authenticity of host 'example.test (192.0.2.1)' can't be established.\r\nED25519 key fingerprint is {malformed}.\r\n"
            );
            assert!(matches!(
                matcher.observe(chunk.as_bytes()),
                Ok(PromptMatch::Ignore)
            ));
            assert!(matches!(
                matcher.observe(
                    b"Are you sure you want to continue connecting (yes/no/[fingerprint])?"
                ),
                Ok(PromptMatch::Ignore)
            ));
        }

        let request = request(AuthConfig::agent(), 20);
        let LaunchOutcome::Ready(plan) = build(
            &request,
            &FixtureCredentials::default(),
            None,
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };
        let mut matcher = plan.prompt_matcher();
        let wrong_host = format!(
            "The authenticity of host 'other.example.test (192.0.2.1)' can't be established.\r\nED25519 key fingerprint is {fingerprint}.\r\n"
        );
        assert!(matches!(
            matcher.observe(wrong_host.as_bytes()),
            Ok(PromptMatch::Ignore)
        ));
    }

    #[test]
    fn host_header_requires_exact_ip_and_direct_or_bracketed_port_binding() {
        let valid_direct =
            b"The authenticity of host 'example.test (192.0.2.1)' can't be established.";
        let valid_ipv6 =
            b"The authenticity of host 'example.test (2001:db8::1)' can't be established.";
        let valid_bracketed =
            b"The authenticity of host '[example.test]:2222 ([2001:db8::1])' can't be established.";
        assert_eq!(
            parse_host_key_header(valid_direct, "example.test", 22),
            Some("example.test".to_string())
        );
        assert_eq!(
            parse_host_key_header(valid_ipv6, "example.test", 22),
            Some("example.test".to_string())
        );
        assert_eq!(
            parse_host_key_header(valid_bracketed, "example.test", 2222),
            Some("[example.test]:2222".to_string())
        );

        for malformed in [
            b"The authenticity of host 'example.test' can't be established.".as_slice(),
            b"The authenticity of host 'example.test (not-an-ip)' can't be established.",
            b"The authenticity of host '[example.test]:2200 ([2001:db8::1])' can't be established.",
            b"The authenticity of host '[example.test]:2222 (2001:db8::1)' can't be established.",
            b"The authenticity of host 'example.test (192.0.2.1' can't be established.",
            b"The authenticity of host 'example.test (192.0.2.1) extra' can't be established.",
        ] {
            assert_eq!(
                parse_host_key_header(malformed, "example.test", 2222),
                None,
                "malformed host header must be rejected: {malformed:?}"
            );
        }
    }

    #[test]
    fn host_key_fingerprint_requires_exact_key_type_line_and_canonical_payload() {
        let fingerprint = valid_host_key_fingerprint();
        assert_eq!(
            parse_host_key_fingerprint(
                format!("ED25519 key fingerprint is {fingerprint}.").as_bytes()
            ),
            Some(fingerprint.clone())
        );
        assert_eq!(
            parse_host_key_fingerprint(
                format!("ED25519 key fingerprint is {fingerprint}").as_bytes()
            ),
            Some(fingerprint.clone())
        );

        let padded = base64::engine::general_purpose::STANDARD.encode([0u8; 32]);
        for malformed in [
            format!("banner ED25519 key fingerprint is {fingerprint}"),
            format!("unknown key fingerprint is {fingerprint}"),
            format!("ED25519 key fingerprint is {fingerprint}.."),
            format!("ED25519 key fingerprint is SHA256:{padded}"),
            format!("The key fingerprint is: {fingerprint}"),
        ] {
            assert_eq!(
                parse_host_key_fingerprint(malformed.as_bytes()),
                None,
                "accepted malformed fingerprint line: {malformed}"
            );
        }
    }

    #[test]
    fn password_prompt_is_case_insensitive_bounded_and_wrap_safe_with_leading_crlf() {
        let request = request(AuthConfig::password(PASSWORD_REF).unwrap(), 21);
        let credentials = FixtureCredentials::default().with_password();
        let LaunchOutcome::Ready(plan) =
            build(&request, &credentials, None, &CancellationToken::new())
        else {
            panic!("ready")
        };
        let mut matcher = plan.prompt_matcher();
        assert!(matches!(
            matcher.observe(b"\r\nDePlOy@ExAmPlE.TeSt's\r\n"),
            Ok(PromptMatch::Ignore)
        ));
        let PromptMatch::Input(input) = matcher.observe(b"PaSsWoRd:").unwrap() else {
            panic!("wrapped password prompt")
        };
        assert_eq!(input.kind(), InputRequestKind::Password);
        assert_eq!(
            input
                .resolve(&credentials, request.binding_as_test())
                .unwrap()
                .bytes(),
            format!("{PASSWORD}\r").as_bytes()
        );

        let LaunchOutcome::Ready(plan) =
            build(&request, &credentials, None, &CancellationToken::new())
        else {
            panic!("ready")
        };
        let mut matcher = plan.prompt_matcher();
        assert!(matches!(
            matcher.observe(b"\r\nDePlOy@ExAmPlE.TeSt's\r\nPaSsWoRd:"),
            Ok(PromptMatch::Input(_))
        ));
    }

    #[test]
    fn password_prompt_keeps_last_three_visible_lines_and_emits_before_trailing_newline() {
        let request = request(AuthConfig::password(PASSWORD_REF).unwrap(), 23);
        let credentials = FixtureCredentials::default().with_password();
        let LaunchOutcome::Ready(plan) =
            build(&request, &credentials, None, &CancellationToken::new())
        else {
            panic!("ready")
        };
        let mut matcher = plan.prompt_matcher();
        assert!(matches!(
            matcher.observe(b"warning banner\r\nstatus\r\n"),
            Ok(PromptMatch::Ignore)
        ));
        assert!(matches!(
            matcher.observe(b"deploy@example.test's password:\r\n"),
            Ok(PromptMatch::Input(_))
        ));
        assert!(matches!(
            matcher.observe(b"deploy@example.test's password:"),
            Err(SshLaunchError::AlreadyConsumed)
        ));
    }

    #[test]
    fn configured_key_and_known_hosts_paths_require_existing_non_reparse_files() {
        let temp = tempfile::tempdir().unwrap();
        let missing_key = temp.path().join("missing-key");
        assert!(matches!(
            AuthConfig::key_path(missing_key),
            Err(SshLaunchError::InvalidField("key_path"))
        ));

        let missing_known_hosts = temp.path().join("missing-known-hosts");
        assert!(matches!(
            request(AuthConfig::agent(), 1).with_network_policy(
                None,
                Some(missing_known_hosts),
                SshKnownHostPolicy::Strict,
            ),
            Err(SshLaunchError::InvalidField("known_hosts_path"))
        ));
    }

    #[test]
    fn args_have_option_terminator_and_reject_injection_values() {
        assert!(SshLaunchRequest::new(
            issued(1, 1),
            "connection",
            "-bad",
            22,
            "deploy",
            AuthConfig::agent(),
            Duration::from_secs(1)
        )
        .is_err());
        assert!(SshLaunchRequest::new(
            issued(1, 1),
            "connection",
            "host",
            22,
            "-bad",
            AuthConfig::agent(),
            Duration::from_secs(1)
        )
        .is_err());
        let request = request(AuthConfig::agent(), 1);
        let LaunchOutcome::Ready(plan) = build(
            &request,
            &FixtureCredentials::default(),
            None,
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };
        assert_eq!(
            plan.command.args().last().map(String::as_str),
            Some("deploy@example.test")
        );
        assert!(plan.command.args().contains(&"--".to_string()));
    }

    #[test]
    fn proxy_jump_and_known_hosts_policy_are_bounded_explicit_options() {
        let temp = tempfile::tempdir().unwrap();
        let known_hosts = temp.path().join("known_hosts");
        std::fs::write(&known_hosts, b"known-host fixture\n").unwrap();
        let invalid = request(AuthConfig::agent(), 1).with_network_policy(
            Some("-oProxyCommand=bad".to_string()),
            None,
            SshKnownHostPolicy::Prompt,
        );
        assert!(matches!(
            invalid,
            Err(SshLaunchError::InvalidField("proxy_jump"))
        ));
        let request = request(AuthConfig::agent(), 1)
            .with_network_policy(
                Some("jump.example.test".to_string()),
                Some(known_hosts),
                SshKnownHostPolicy::Strict,
            )
            .unwrap();
        let LaunchOutcome::Ready(plan) = build(
            &request,
            &FixtureCredentials::default(),
            None,
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };
        assert!(plan
            .command
            .args()
            .windows(2)
            .any(|args| args[0] == "-J" && args[1] == "jump.example.test"));
        assert!(plan
            .command
            .args()
            .contains(&"StrictHostKeyChecking=yes".to_string()));
        assert!(plan
            .command
            .args()
            .iter()
            .any(|arg| arg.starts_with("UserKnownHostsFile=")));
    }

    #[test]
    fn environment_is_allowlisted_and_serialization_omits_values() {
        let env_request = request(AuthConfig::agent(), 1).with_environment(BTreeMap::from([(
            "PASSWORD".to_string(),
            PASSWORD.to_string(),
        )]));
        assert!(matches!(
            env_request,
            Err(SshLaunchError::InvalidField("environment_key"))
        ));
        let secret_value = request(AuthConfig::agent(), 1)
            .with_environment(BTreeMap::from([("TERM".to_string(), PASSWORD.to_string())]));
        assert!(matches!(
            secret_value,
            Err(SshLaunchError::InvalidField("environment_value"))
        ));
        let request = request(AuthConfig::agent(), 1)
            .with_environment(BTreeMap::from([("TERM".to_string(), "xterm".to_string())]))
            .unwrap();
        let LaunchOutcome::Ready(plan) = build(
            &request,
            &FixtureCredentials::default(),
            None,
            &CancellationToken::new(),
        ) else {
            panic!("ready")
        };
        let json = serde_json::to_string(plan.snapshot()).unwrap();
        assert!(!json.contains("xterm"));
        assert!(!json.contains(PASSWORD));
        let command_json = serde_json::to_string(plan.command()).unwrap();
        assert!(command_json.contains("envKeys"));
        assert!(!command_json.contains("xterm"));
        assert!(!command_json.contains(PASSWORD));
    }

    #[test]
    fn cancelled_and_expired_requests_fail_closed() {
        let request = request(AuthConfig::agent(), 1);
        let cancellation = CancellationToken::new();
        cancellation.cancel(&request.binding).unwrap();
        assert!(matches!(
            build(
                &request,
                &FixtureCredentials::default(),
                None,
                &cancellation
            ),
            LaunchOutcome::Cancelled
        ));
        let expired = SshLaunchRequest::new_with_deadline(
            issued(2, 1),
            "connection",
            "host",
            22,
            "user",
            AuthConfig::agent(),
            Instant::now() - Duration::from_secs(1),
        )
        .unwrap();
        assert!(matches!(
            build_ssh_launch_plan(
                &expired,
                &FixtureCredentials::default(),
                None,
                &CancellationToken::new()
            ),
            Err(SshLaunchError::DeadlineExpired)
        ));
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn unsupported_macos_launch_holds_before_executable_or_known_hosts_pin() {
        let temp = tempfile::tempdir().expect("root");
        let known_hosts = temp.path().join("known_hosts");
        std::fs::write(&known_hosts, b"known-host fixture\n").expect("known hosts");
        let request = request(AuthConfig::agent(), 1)
            .with_network_policy(None, Some(known_hosts.clone()), SshKnownHostPolicy::Strict)
            .expect("network policy");
        std::fs::remove_file(&known_hosts).expect("remove known hosts");

        assert!(matches!(
            build_ssh_launch_plan(
                &request,
                &FixtureCredentials::default(),
                None,
                &CancellationToken::new(),
            ),
            Err(SshLaunchError::UnsupportedRuntime)
        ));
    }

    #[test]
    fn cancellation_ledger_rejects_entries_beyond_the_bound() {
        let executable = std::env::current_exe().expect("test executable");
        let process = ManagedProcessIdentity::new(
            ManagedProcessId::new(std::process::id().max(1), 1).unwrap(),
            executable,
        )
        .expect("process identity");
        let cancellation = CancellationToken::new();
        for index in 1..=MAX_CANCELLATION_ENTRIES {
            let mut launch_nonce = [0u8; 16];
            launch_nonce[..8].copy_from_slice(&(index as u64).to_be_bytes());
            let issued = HostIssuedSshBindingIssuer::issue(
                TaskId::new(),
                ResourceId::new(),
                1,
                1,
                process.clone(),
                launch_nonce,
            )
            .expect("binding");
            let binding = issued.into_binding();
            cancellation.cancel(&binding.claim()).expect("capacity");
        }
        let mut launch_nonce = [0u8; 16];
        launch_nonce[..8].copy_from_slice(&((MAX_CANCELLATION_ENTRIES as u64) + 1).to_be_bytes());
        let issued = HostIssuedSshBindingIssuer::issue(
            TaskId::new(),
            ResourceId::new(),
            1,
            1,
            process,
            launch_nonce,
        )
        .expect("binding");
        assert!(matches!(
            cancellation.cancel(&issued.into_binding().claim()),
            Err(SshLaunchError::CapacityExceeded)
        ));
    }

    #[test]
    fn fake_executable_contract_is_argument_only_and_no_secret_bytes_cross_boundary() {
        let credentials = FixtureCredentials::default().with_password();
        let request = request(AuthConfig::password(PASSWORD_REF).unwrap(), 1);
        let LaunchOutcome::Ready(plan) =
            build(&request, &credentials, None, &CancellationToken::new())
        else {
            panic!("ready")
        };
        assert!(plan.command.args().contains(&"--".to_string()));
        assert!(!plan.command.args().iter().any(|arg| arg.contains(PASSWORD)));
        assert!(!format!("{plan:?}").contains(PASSWORD));
        assert!(!serde_json::to_string(plan.events())
            .unwrap()
            .contains(PASSWORD));
    }

    #[test]
    fn malformed_and_oversize_resolver_material_never_enters_errors() {
        let malformed = FixtureCredentials {
            values: BTreeMap::from([(
                CredentialRef::parse(PASSWORD_REF).unwrap(),
                (
                    CredentialKind::Password,
                    Zeroizing::new(b"bad\nmaterial".to_vec()),
                ),
            )]),
        };
        let request = request(AuthConfig::password(PASSWORD_REF).unwrap(), 1);
        let error =
            match build_ssh_launch_plan(&request, &malformed, None, &CancellationToken::new()) {
                Err(error) => error,
                Ok(_) => panic!("malformed resolver material unexpectedly launched"),
            };
        assert!(!format!("{error:?}").contains("bad"));

        let oversized = FixtureCredentials {
            values: BTreeMap::from([(
                CredentialRef::parse(PASSWORD_REF).unwrap(),
                (
                    CredentialKind::Password,
                    Zeroizing::new(vec![b'x'; super::super::credentials::MAX_SECRET_BYTES + 1]),
                ),
            )]),
        };
        let error =
            match build_ssh_launch_plan(&request, &oversized, None, &CancellationToken::new()) {
                Err(error) => error,
                Ok(_) => panic!("oversized resolver material unexpectedly launched"),
            };
        assert!(matches!(
            error,
            SshLaunchError::Credential(CredentialError::SecretTooLarge)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn fake_executable_preserves_host_key_then_passphrase_prompt_order() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("fake-ssh-prompt.ps1");
        let mut child = std::process::Command::new("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(fixture)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("fake SSH executable");
        child
            .stdin
            .as_mut()
            .expect("fake stdin")
            .write_all(b"yes\nfixture-passphrase\n")
            .expect("prompt replies");
        let output = child.wait_with_output().expect("fake output");
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).expect("prompt output");
        assert!(output.find("authenticity").unwrap() < output.find("passphrase").unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn fake_executable_preserves_host_key_then_passphrase_prompt_order() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("fake-ssh-prompt.sh");
        let mut child = std::process::Command::new("sh")
            .arg(fixture)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("fake SSH executable");
        child
            .stdin
            .as_mut()
            .expect("fake stdin")
            .write_all(b"yes\nfixture-passphrase\n")
            .expect("prompt replies");
        let output = child.wait_with_output().expect("fake output");
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).expect("prompt output");
        assert!(output.find("authenticity").unwrap() < output.find("passphrase").unwrap());
    }

    trait TestBindingAccess {
        fn binding_as_test(&self) -> &SshBinding;
    }

    impl TestBindingAccess for SshLaunchRequest {
        fn binding_as_test(&self) -> &SshBinding {
            &self.issued_binding
        }
    }
}
