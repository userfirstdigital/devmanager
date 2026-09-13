use crate::persistence;
use crate::services::platform_service;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::thread;
use std::time::{Duration, Instant};

static PID_FILE_ACCESS_LOCK: Mutex<()> = Mutex::new(());

static TEST_PID_FILE_OVERRIDE_LOCK: Mutex<()> = Mutex::new(());
static TEST_PID_FILE_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

const LEDGER_VERSION: u32 = 1;
const MAX_LEDGER_FILE_BYTES: usize = 4 * 1024 * 1024;
const MAX_LEDGER_SESSIONS: usize = 1_024;
const MAX_LEDGER_DESCENDANTS_PER_SESSION: usize = 4_096;
const MAX_LEDGER_HOST_STRING_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedProcessRecord {
    pub session_id: String,
    pub pid: u32,
    pub started_at_unix_secs: u64,
    #[serde(default)]
    pub process_name: Option<String>,
    pub session_kind: String,
    pub program: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub command_id: Option<String>,
    #[serde(default)]
    pub tab_id: Option<String>,
    #[serde(default)]
    pub descendant_processes: Vec<TrackedProcessIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedProcessIdentity {
    pub pid: u32,
    pub started_at_unix_secs: u64,
    #[serde(default)]
    pub process_name: Option<String>,
}

impl ManagedProcessRecord {
    fn legacy(pid: u32) -> Self {
        Self {
            session_id: format!("legacy:{pid}"),
            pid,
            started_at_unix_secs: 0,
            process_name: None,
            session_kind: "legacy".to_string(),
            program: String::new(),
            project_id: None,
            command_id: None,
            tab_id: None,
            descendant_processes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedProcessLedgerFile {
    version: u32,
    #[serde(default)]
    sessions: BTreeMap<String, ManagedProcessRecord>,
}

impl Default for ManagedProcessLedgerFile {
    fn default() -> Self {
        Self {
            version: LEDGER_VERSION,
            sessions: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StoredLedgerFile {
    Current(ManagedProcessLedgerFile),
    LegacyPids(HashSet<u32>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackedProcessState {
    Missing,
    VerifiedRunning,
    ReusedPid,
}

/// Result of reconciling process identities left by an earlier DevManager
/// instance.
///
/// A persisted PID ledger is observation only. Once the instance that owned a
/// process Job has exited, the ledger cannot recreate the exact Job/fence
/// capability required to terminate that process safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrphanedProcessReconciliation {
    Clear,
    ExactAuthorityUnavailable {
        retained_sessions: usize,
        retained_processes: usize,
    },
}

#[doc(hidden)]
pub struct TestPidFileGuard {
    _lock: MutexGuard<'static, ()>,
}

impl Drop for TestPidFileGuard {
    fn drop(&mut self) {
        if let Ok(mut override_path) = TEST_PID_FILE_OVERRIDE.lock() {
            *override_path = None;
        }
    }
}

#[doc(hidden)]
pub fn use_test_pid_file(path: PathBuf) -> TestPidFileGuard {
    let lock = TEST_PID_FILE_OVERRIDE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Ok(mut override_path) = TEST_PID_FILE_OVERRIDE.lock() {
        *override_path = Some(path);
    }
    TestPidFileGuard { _lock: lock }
}

fn pid_file_path() -> Result<PathBuf, String> {
    if let Ok(override_path) = TEST_PID_FILE_OVERRIDE.lock() {
        if let Some(path) = override_path.clone() {
            return Ok(path);
        }
    }

    let config_dir = persistence::app_config_dir()
        .map_err(|_| "Could not determine config directory".to_string())?;
    Ok(config_dir.join("running-pids.json"))
}

fn read_ledger_from_path(path: &Path) -> ManagedProcessLedgerFile {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return ManagedProcessLedgerFile::default(),
    };
    let mut contents = String::new();
    let read_limit = u64::try_from(MAX_LEDGER_FILE_BYTES + 1).expect("ledger bound fits u64");
    match file.take(read_limit).read_to_string(&mut contents) {
        Ok(_) if contents.len() <= MAX_LEDGER_FILE_BYTES => {}
        Err(_) => return ManagedProcessLedgerFile::default(),
        Ok(_) => return ManagedProcessLedgerFile::default(),
    }
    match serde_json::from_str::<StoredLedgerFile>(&contents) {
        Ok(StoredLedgerFile::Current(mut ledger)) => {
            ledger.version = LEDGER_VERSION;
            if validate_ledger_bounds(&ledger).is_ok() {
                ledger
            } else {
                ManagedProcessLedgerFile::default()
            }
        }
        Ok(StoredLedgerFile::LegacyPids(pids)) => ManagedProcessLedgerFile {
            version: LEDGER_VERSION,
            sessions: pids
                .into_iter()
                .take(MAX_LEDGER_SESSIONS)
                .map(|pid| {
                    let entry = ManagedProcessRecord::legacy(pid);
                    (entry.session_id.clone(), entry)
                })
                .collect(),
        },
        Err(_) => ManagedProcessLedgerFile::default(),
    }
}

fn validate_optional_string(value: Option<&str>, field: &str) -> Result<(), String> {
    if value.is_some_and(|value| value.len() > MAX_LEDGER_HOST_STRING_BYTES) {
        Err(format!("PID ledger {field} exceeds host string bound"))
    } else {
        Ok(())
    }
}

fn validate_record_bounds(record: &ManagedProcessRecord) -> Result<(), String> {
    for (field, value) in [
        ("session id", record.session_id.as_str()),
        ("session kind", record.session_kind.as_str()),
        ("program", record.program.as_str()),
    ] {
        if value.len() > MAX_LEDGER_HOST_STRING_BYTES {
            return Err(format!("PID ledger {field} exceeds host string bound"));
        }
    }
    validate_optional_string(record.process_name.as_deref(), "process name")?;
    validate_optional_string(record.project_id.as_deref(), "project id")?;
    validate_optional_string(record.command_id.as_deref(), "command id")?;
    validate_optional_string(record.tab_id.as_deref(), "tab id")?;
    if record.descendant_processes.len() > MAX_LEDGER_DESCENDANTS_PER_SESSION {
        return Err("PID ledger descendant set exceeds fixed bound".to_string());
    }
    for descendant in &record.descendant_processes {
        validate_optional_string(
            descendant.process_name.as_deref(),
            "descendant process name",
        )?;
    }
    Ok(())
}

fn validate_ledger_bounds(ledger: &ManagedProcessLedgerFile) -> Result<(), String> {
    if ledger.sessions.len() > MAX_LEDGER_SESSIONS {
        return Err("PID ledger session set exceeds fixed bound".to_string());
    }
    for (session_id, record) in &ledger.sessions {
        if session_id.len() > MAX_LEDGER_HOST_STRING_BYTES || session_id != &record.session_id {
            return Err("PID ledger session key is invalid".to_string());
        }
        validate_record_bounds(record)?;
    }
    Ok(())
}

fn write_ledger_to_path(path: &Path, ledger: &ManagedProcessLedgerFile) -> Result<(), String> {
    validate_ledger_bounds(ledger)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create PID ledger directory: {error}"))?;
    }
    let temp_path = path.with_extension("json.tmp");
    let contents = serde_json::to_string_pretty(ledger)
        .map_err(|error| format!("Failed to serialize PID ledger: {error}"))?;
    if contents.len() > MAX_LEDGER_FILE_BYTES {
        return Err("Serialized PID ledger exceeds fixed file bound".to_string());
    }
    std::fs::write(&temp_path, contents)
        .map_err(|error| format!("Failed to write PID ledger temp file: {error}"))?;
    if let Err(error) = std::fs::rename(&temp_path, path) {
        if path.exists() {
            std::fs::remove_file(path).map_err(|remove_error| {
                format!("Failed to replace PID ledger file: {remove_error}")
            })?;
            std::fs::rename(&temp_path, path).map_err(|rename_error| {
                format!("Failed to replace PID ledger file: {rename_error}")
            })?;
        } else {
            return Err(format!("Failed to replace PID ledger file: {error}"));
        }
    }
    Ok(())
}

fn read_ledger() -> ManagedProcessLedgerFile {
    let path = match pid_file_path() {
        Ok(path) => path,
        Err(_) => return ManagedProcessLedgerFile::default(),
    };
    let _guard = PID_FILE_ACCESS_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    read_ledger_from_path(&path)
}

fn mutate_ledger<R>(f: impl FnOnce(&mut ManagedProcessLedgerFile) -> R) -> Result<R, String> {
    let path = pid_file_path()?;
    let _guard = PID_FILE_ACCESS_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut ledger = read_ledger_from_path(&path);
    let result = f(&mut ledger);
    write_ledger_to_path(&path, &ledger)?;
    Ok(result)
}

fn mutate_ledger_if_changed(
    f: impl FnOnce(&mut ManagedProcessLedgerFile) -> bool,
) -> Result<bool, String> {
    let path = pid_file_path()?;
    let _guard = PID_FILE_ACCESS_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut ledger = read_ledger_from_path(&path);
    let changed = f(&mut ledger);
    if changed {
        write_ledger_to_path(&path, &ledger)?;
    }
    Ok(changed)
}

fn remaining_before(deadline: Instant, operation: &str) -> Result<Duration, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(format!("{operation} exceeded teardown absolute deadline"))
    } else {
        Ok(remaining)
    }
}

fn lock_pid_ledger_before(deadline: Instant) -> Result<MutexGuard<'static, ()>, String> {
    loop {
        let remaining = remaining_before(deadline, "PID ledger lock")?;
        match PID_FILE_ACCESS_LOCK.try_lock() {
            Ok(guard) => {
                remaining_before(deadline, "PID ledger lock")?;
                return Ok(guard);
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                thread::sleep(remaining.min(Duration::from_millis(1)));
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("PID ledger lock poisoned".to_string());
            }
        }
    }
}

fn pid_file_path_before(deadline: Instant) -> Result<PathBuf, String> {
    remaining_before(deadline, "PID ledger path lookup")?;
    let override_path = loop {
        let remaining = remaining_before(deadline, "PID ledger override lookup")?;
        match TEST_PID_FILE_OVERRIDE.try_lock() {
            Ok(path) => break path.clone(),
            Err(std::sync::TryLockError::WouldBlock) => {
                thread::sleep(remaining.min(Duration::from_millis(1)));
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("PID ledger override lock poisoned".to_string());
            }
        }
    };
    if let Some(path) = override_path {
        remaining_before(deadline, "PID ledger path lookup")?;
        return Ok(path);
    }
    let config_dir = persistence::app_config_dir()
        .map_err(|_| "Could not determine config directory".to_string())?;
    remaining_before(deadline, "PID ledger path lookup")?;
    Ok(config_dir.join("running-pids.json"))
}

/// Removes the exact session ledger observation only after the Job-backed
/// registry has authoritatively settled ACTIVE_PROCESS_ZERO and released the
/// matching fence. No PID scan is performed: the Job is the membership truth,
/// while this file is merely durable observability state.
pub(crate) fn release_session_root_after_job_zero(
    session_id: &str,
    root_pid: u32,
    absolute_deadline: Instant,
) -> Result<(), String> {
    if session_id.is_empty() || session_id.len() > MAX_LEDGER_HOST_STRING_BYTES || root_pid == 0 {
        return Err("PID ledger exact release identity is invalid".to_string());
    }
    let path = pid_file_path_before(absolute_deadline)?;
    let _guard = lock_pid_ledger_before(absolute_deadline)?;
    remaining_before(absolute_deadline, "PID ledger exact release read")?;
    let mut ledger = read_ledger_from_path(&path);
    remaining_before(absolute_deadline, "PID ledger exact release read")?;
    let matches_root = ledger
        .sessions
        .get(session_id)
        .is_some_and(|entry| entry.pid == root_pid);
    if matches_root {
        ledger.sessions.remove(session_id);
        remaining_before(absolute_deadline, "PID ledger exact release write")?;
        write_ledger_to_path(&path, &ledger)?;
        remaining_before(absolute_deadline, "PID ledger exact release write")?;
    }
    Ok(())
}

fn tracked_process_identity_state_with<F>(
    identity: &TrackedProcessIdentity,
    identify_process: &mut F,
) -> TrackedProcessState
where
    F: FnMut(u32) -> Option<platform_service::ProcessIdentity>,
{
    let Some(actual_identity) = identify_process(identity.pid) else {
        return TrackedProcessState::Missing;
    };
    if identity.started_at_unix_secs == 0 {
        return TrackedProcessState::ReusedPid;
    }
    if actual_identity.started_at_unix_secs != identity.started_at_unix_secs {
        return TrackedProcessState::ReusedPid;
    }
    match identity.process_name.as_deref() {
        Some(expected_name)
            if actual_identity
                .process_name
                .as_deref()
                .map(|actual_name| !actual_name.eq_ignore_ascii_case(expected_name))
                .unwrap_or(true) =>
        {
            TrackedProcessState::ReusedPid
        }
        _ => TrackedProcessState::VerifiedRunning,
    }
}

fn root_process_identity(entry: &ManagedProcessRecord) -> TrackedProcessIdentity {
    TrackedProcessIdentity {
        pid: entry.pid,
        started_at_unix_secs: entry.started_at_unix_secs,
        process_name: entry.process_name.clone(),
    }
}

fn normalize_descendant_processes(
    root_pid: u32,
    descendants: Vec<platform_service::ProcessIdentity>,
) -> Vec<TrackedProcessIdentity> {
    let mut descendants: Vec<_> = descendants
        .into_iter()
        .filter(|identity| identity.pid != root_pid)
        .map(|identity| TrackedProcessIdentity {
            pid: identity.pid,
            started_at_unix_secs: identity.started_at_unix_secs,
            process_name: identity.process_name,
        })
        .collect();
    descendants.sort_by_key(|identity| identity.pid);
    descendants.dedup_by(|left, right| left.pid == right.pid);
    descendants
}

fn validate_descendant_input(
    session_id: &str,
    descendants: &[platform_service::ProcessIdentity],
) -> Result<(), String> {
    if session_id.len() > MAX_LEDGER_HOST_STRING_BYTES {
        return Err("PID ledger session id exceeds host string bound".to_string());
    }
    if descendants.len() > MAX_LEDGER_DESCENDANTS_PER_SESSION {
        return Err("PID ledger descendant input exceeds fixed bound".to_string());
    }
    for descendant in descendants {
        validate_optional_string(
            descendant.process_name.as_deref(),
            "descendant process name",
        )?;
    }
    Ok(())
}

fn active_processes_in_record_with<F>(
    entry: &ManagedProcessRecord,
    identify_process: &mut F,
) -> Option<ManagedProcessRecord>
where
    F: FnMut(u32) -> Option<platform_service::ProcessIdentity>,
{
    let root_live =
        tracked_process_identity_state_with(&root_process_identity(entry), identify_process)
            == TrackedProcessState::VerifiedRunning;
    let live_descendants = entry
        .descendant_processes
        .iter()
        .filter(|identity| {
            tracked_process_identity_state_with(identity, identify_process)
                == TrackedProcessState::VerifiedRunning
        })
        .cloned()
        .collect::<Vec<_>>();
    if root_live || !live_descendants.is_empty() {
        let mut entry = entry.clone();
        entry.descendant_processes = live_descendants;
        Some(entry)
    } else {
        None
    }
}

fn active_pids_in_record_with<F>(entry: &ManagedProcessRecord, identify_process: &mut F) -> Vec<u32>
where
    F: FnMut(u32) -> Option<platform_service::ProcessIdentity>,
{
    let mut pids = Vec::new();
    if tracked_process_identity_state_with(&root_process_identity(entry), identify_process)
        == TrackedProcessState::VerifiedRunning
    {
        pids.push(entry.pid);
    }
    pids.extend(
        entry
            .descendant_processes
            .iter()
            .filter(|identity| {
                tracked_process_identity_state_with(identity, identify_process)
                    == TrackedProcessState::VerifiedRunning
            })
            .map(|identity| identity.pid),
    );
    pids.sort_unstable();
    pids.dedup();
    pids
}

fn tracked_pids_for_record(entry: &ManagedProcessRecord) -> Vec<u32> {
    let mut pids = Vec::with_capacity(entry.descendant_processes.len() + 1);
    pids.push(entry.pid);
    pids.extend(
        entry
            .descendant_processes
            .iter()
            .map(|identity| identity.pid),
    );
    pids.sort_unstable();
    pids.dedup();
    pids
}

fn active_processes_with<F>(path: &Path, mut identify_process: F) -> Vec<ManagedProcessRecord>
where
    F: FnMut(u32) -> Option<platform_service::ProcessIdentity>,
{
    read_ledger_from_path(path)
        .sessions
        .into_values()
        .filter_map(|entry| active_processes_in_record_with(&entry, &mut identify_process))
        .collect()
}

fn active_pids_with<F>(path: &Path, mut identify_process: F) -> Vec<u32>
where
    F: FnMut(u32) -> Option<platform_service::ProcessIdentity>,
{
    let mut pids: Vec<u32> = read_ledger_from_path(path)
        .sessions
        .into_values()
        .flat_map(|entry| active_pids_in_record_with(&entry, &mut identify_process))
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

fn prune_inactive_entries_with_path<F>(
    path: &Path,
    mut identify_process: F,
) -> Result<usize, String>
where
    F: FnMut(u32) -> Option<platform_service::ProcessIdentity>,
{
    let mut ledger = read_ledger_from_path(path);
    ledger.sessions = ledger
        .sessions
        .into_iter()
        .filter_map(|(session_id, entry)| {
            active_processes_in_record_with(&entry, &mut identify_process)
                .map(|entry| (session_id, entry))
        })
        .collect();
    let remaining = ledger.sessions.len();
    write_ledger_to_path(path, &ledger)?;
    Ok(remaining)
}

fn reconcile_orphaned_process_ledger_with_path<F>(
    path: &Path,
    mut identify_process: F,
) -> Result<OrphanedProcessReconciliation, String>
where
    F: FnMut(u32) -> Option<platform_service::ProcessIdentity>,
{
    let mut ledger = read_ledger_from_path(path);
    if ledger.sessions.is_empty() {
        return Ok(OrphanedProcessReconciliation::Clear);
    }

    let mut retained = BTreeMap::new();
    let mut retained_processes = 0usize;
    for (session_id, entry) in ledger.sessions {
        let Some(active_entry) = active_processes_in_record_with(&entry, &mut identify_process)
        else {
            continue;
        };
        retained_processes = retained_processes
            .checked_add(active_pids_in_record_with(&active_entry, &mut identify_process).len())
            .ok_or_else(|| "Orphan process count overflowed".to_string())?;
        retained.insert(session_id, active_entry);
    }

    ledger.sessions = retained;
    let retained_sessions = ledger.sessions.len();
    write_ledger_to_path(path, &ledger)?;
    if retained_sessions == 0 {
        Ok(OrphanedProcessReconciliation::Clear)
    } else {
        Ok(OrphanedProcessReconciliation::ExactAuthorityUnavailable {
            retained_sessions,
            retained_processes,
        })
    }
}

pub fn track_session_process(record: ManagedProcessRecord) -> Result<(), String> {
    validate_record_bounds(&record)?;
    mutate_ledger(|ledger| -> Result<(), String> {
        ledger.version = LEDGER_VERSION;
        if !ledger.sessions.contains_key(&record.session_id)
            && ledger.sessions.len() >= MAX_LEDGER_SESSIONS
        {
            return Err("PID ledger session capacity exhausted".to_string());
        }
        ledger.sessions.insert(record.session_id.clone(), record);
        Ok(())
    })
    .and_then(|result| result)
}

fn retain_verified_descendant_identities(
    root_pid: u32,
    mut current: Vec<TrackedProcessIdentity>,
    prior: &[TrackedProcessIdentity],
    mut is_verified: impl FnMut(&TrackedProcessIdentity) -> bool,
) -> Vec<TrackedProcessIdentity> {
    for identity in prior {
        if current.len() >= MAX_LEDGER_DESCENDANTS_PER_SESSION {
            break;
        }
        if identity.pid == root_pid || current.iter().any(|entry| entry.pid == identity.pid) {
            continue;
        }
        if is_verified(identity) {
            current.push(identity.clone());
        }
    }
    current.sort_by_key(|identity| identity.pid);
    current.dedup_by(|left, right| left.pid == right.pid);
    current
}

fn merge_descendants_with_verified_priors(
    root_pid: u32,
    descendants: Vec<platform_service::ProcessIdentity>,
    prior: &[TrackedProcessIdentity],
    system: &sysinfo::System,
) -> Vec<TrackedProcessIdentity> {
    let normalized = normalize_descendant_processes(root_pid, descendants);
    retain_verified_descendant_identities(root_pid, normalized, prior, |identity| {
        platform_service::process_matches_identity_with_system(
            system,
            identity.pid,
            identity.started_at_unix_secs,
            identity.process_name.as_deref(),
        )
    })
}

/// Sync descendants using an already-refreshed process snapshot.
///
/// Merges against the ledger entry under the PID-file lock so a concurrent sync
/// cannot lose newly recorded verified descendants.
pub fn sync_session_descendant_processes_with_system(
    session_id: &str,
    root_pid: u32,
    descendants: Vec<platform_service::ProcessIdentity>,
    system: &sysinfo::System,
) -> Result<(), String> {
    validate_descendant_input(session_id, &descendants)?;
    mutate_ledger_if_changed(|ledger| {
        let Some(entry) = ledger.sessions.get_mut(session_id) else {
            return false;
        };
        if entry.pid != root_pid {
            return false;
        }
        let merged = merge_descendants_with_verified_priors(
            root_pid,
            descendants.clone(),
            &entry.descendant_processes,
            system,
        );
        if entry.descendant_processes == merged {
            return false;
        }
        entry.descendant_processes = merged;
        true
    })
    .map(|_| ())
}

pub fn sync_session_descendant_processes(
    session_id: &str,
    root_pid: u32,
    descendants: Vec<platform_service::ProcessIdentity>,
) -> Result<(), String> {
    validate_descendant_input(session_id, &descendants)?;
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    sync_session_descendant_processes_with_system(session_id, root_pid, descendants, &system)
}

/// Release the session root while retaining verified survivors from the current
/// ledger entry under the PID-file lock.
pub fn release_session_root_with_system(
    session_id: &str,
    root_pid: u32,
    surviving_descendants: Vec<platform_service::ProcessIdentity>,
    system: &sysinfo::System,
) -> Result<(), String> {
    validate_descendant_input(session_id, &surviving_descendants)?;
    mutate_ledger_if_changed(|ledger| {
        let Some(entry) = ledger.sessions.get(session_id) else {
            return false;
        };
        if entry.pid != root_pid {
            return false;
        }
        let survivors = merge_descendants_with_verified_priors(
            root_pid,
            surviving_descendants.clone(),
            &entry.descendant_processes,
            system,
        );
        if survivors.is_empty() {
            ledger.sessions.remove(session_id);
        } else if let Some(entry) = ledger.sessions.get_mut(session_id) {
            if entry.descendant_processes == survivors {
                return false;
            }
            entry.descendant_processes = survivors;
        }
        true
    })
    .map(|_| ())
}

pub fn release_session_root(
    session_id: &str,
    root_pid: u32,
    surviving_descendants: Vec<platform_service::ProcessIdentity>,
) -> Result<(), String> {
    validate_descendant_input(session_id, &surviving_descendants)?;
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    release_session_root_with_system(session_id, root_pid, surviving_descendants, &system)
}

pub fn untrack_session_process(session_id: &str, pid: u32) -> Result<(), String> {
    mutate_ledger(|ledger| {
        if ledger
            .sessions
            .get(session_id)
            .map(|entry| entry.pid == pid)
            .unwrap_or(false)
        {
            ledger.sessions.remove(session_id);
        }
    })
    .map(|_| ())
}

pub fn clear_all() {
    let _ = mutate_ledger(|ledger| ledger.sessions.clear());
}

pub fn tracked_processes() -> Vec<ManagedProcessRecord> {
    read_ledger().sessions.into_values().collect()
}

pub fn tracked_process_for_pid(pid: u32) -> Option<ManagedProcessRecord> {
    tracked_processes()
        .into_iter()
        .find(|entry| tracked_pids_for_record(entry).contains(&pid))
}

pub fn tracked_pids() -> HashSet<u32> {
    tracked_processes()
        .into_iter()
        .flat_map(|entry| tracked_pids_for_record(&entry))
        .collect()
}

pub fn active_tracked_processes() -> Vec<ManagedProcessRecord> {
    let path = match pid_file_path() {
        Ok(path) => path,
        Err(_) => return Vec::new(),
    };
    active_processes_with(&path, platform_service::capture_process_identity)
}

pub fn active_tracked_processes_for_session(session_id: &str) -> Vec<ManagedProcessRecord> {
    let path = match pid_file_path() {
        Ok(path) => path,
        Err(_) => return Vec::new(),
    };
    read_ledger_from_path(&path)
        .sessions
        .into_values()
        .filter(|entry| entry.session_id == session_id)
        .filter_map(|entry| {
            active_processes_in_record_with(&entry, &mut platform_service::capture_process_identity)
        })
        .collect()
}

pub fn active_tracked_pids() -> Vec<u32> {
    let path = match pid_file_path() {
        Ok(path) => path,
        Err(_) => return Vec::new(),
    };
    active_pids_with(&path, platform_service::capture_process_identity)
}

pub fn active_tracked_pids_for_session(session_id: &str) -> Vec<u32> {
    let path = match pid_file_path() {
        Ok(path) => path,
        Err(_) => return Vec::new(),
    };
    let mut pids: Vec<u32> = read_ledger_from_path(&path)
        .sessions
        .into_values()
        .filter(|entry| entry.session_id == session_id)
        .flat_map(|entry| {
            active_pids_in_record_with(&entry, &mut platform_service::capture_process_identity)
        })
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

pub fn wait_for_tracked_processes_to_exit(timeout: Duration) -> Vec<ManagedProcessRecord> {
    let started_at = Instant::now();
    loop {
        let active = active_tracked_processes();
        if active.is_empty() || started_at.elapsed() >= timeout {
            return active;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

pub fn wait_for_tracked_pids_to_exit(timeout: Duration) -> Vec<u32> {
    let started_at = Instant::now();
    loop {
        let active = active_tracked_pids();
        if active.is_empty() || started_at.elapsed() >= timeout {
            return active;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

pub fn prune_inactive_entries() -> Result<usize, String> {
    let path = pid_file_path()?;
    let _guard = PID_FILE_ACCESS_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    prune_inactive_entries_with_path(&path, platform_service::capture_process_identity)
}

pub(crate) fn reconcile_orphaned_process_ledger() -> Result<OrphanedProcessReconciliation, String> {
    let path = pid_file_path()?;
    let _guard = PID_FILE_ACCESS_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    reconcile_orphaned_process_ledger_with_path(&path, platform_service::capture_process_identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;

    fn record(session_id: &str, pid: u32, started_at_unix_secs: u64) -> ManagedProcessRecord {
        ManagedProcessRecord {
            session_id: session_id.to_string(),
            pid,
            started_at_unix_secs,
            process_name: Some(format!("proc-{pid}")),
            session_kind: "server".to_string(),
            program: "cmd".to_string(),
            project_id: Some("project-1".to_string()),
            command_id: Some(session_id.to_string()),
            tab_id: None,
            descendant_processes: Vec::new(),
        }
    }

    fn identity(pid: u32, started_at_unix_secs: u64) -> platform_service::ProcessIdentity {
        platform_service::ProcessIdentity {
            pid,
            started_at_unix_secs,
            process_name: Some(format!("proc-{pid}")),
        }
    }

    #[test]
    fn ledger_record_rejects_unbounded_descendants_and_host_strings() {
        let mut too_many_descendants = record("bounded", 1, 1);
        too_many_descendants.descendant_processes = (0..=MAX_LEDGER_DESCENDANTS_PER_SESSION)
            .map(|offset| TrackedProcessIdentity {
                pid: u32::try_from(offset + 2).expect("bounded test pid"),
                started_at_unix_secs: 1,
                process_name: None,
            })
            .collect();
        assert!(validate_record_bounds(&too_many_descendants).is_err());

        let mut oversized_string = record("bounded", 1, 1);
        oversized_string.program = "x".repeat(MAX_LEDGER_HOST_STRING_BYTES + 1);
        assert!(validate_record_bounds(&oversized_string).is_err());
    }

    #[test]
    fn release_session_root_rejects_oversized_descendant_input() {
        let temp = tempfile::tempdir().expect("bounded release ledger root");
        let _guard = use_test_pid_file(temp.path().join("running-pids.json"));
        let descendants = (0..=MAX_LEDGER_DESCENDANTS_PER_SESSION)
            .map(|offset| {
                identity(
                    u32::try_from(offset + 2).expect("bounded test pid"),
                    u64::try_from(offset + 1).expect("bounded test creation time"),
                )
            })
            .collect();

        let error = release_session_root_with_system(
            "bounded-release",
            1,
            descendants,
            &sysinfo::System::new(),
        )
        .expect_err("release input must be bounded before ledger mutation");

        assert!(error.contains("descendant input exceeds fixed bound"));
    }

    #[test]
    fn ledger_reader_rejects_oversized_files_before_json_decode() {
        let temp_dir = std::env::temp_dir().join(format!(
            "devmanager-pid-ledger-bound-tests-{}",
            std::process::id()
        ));
        let path = temp_dir.join("running-pids.json");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).expect("create bounded ledger test dir");
        fs::write(&path, vec![b' '; MAX_LEDGER_FILE_BYTES + 1])
            .expect("write oversized ledger fixture");

        assert!(read_ledger_from_path(&path).sessions.is_empty());
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn orphan_reconciliation_never_reconstructs_termination_authority_from_pids() {
        let temp_dir =
            std::env::temp_dir().join(format!("devmanager-pid-file-tests-{}", std::process::id()));
        let path = temp_dir.join("running-pids.json");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let ledger = ManagedProcessLedgerFile {
            version: LEDGER_VERSION,
            sessions: BTreeMap::from([
                ("server-11".to_string(), record("server-11", 11, 111)),
                ("server-22".to_string(), record("server-22", 22, 222)),
                ("server-33".to_string(), record("server-33", 33, 333)),
            ]),
        };
        write_ledger_to_path(&path, &ledger).unwrap();

        let running = RefCell::new(BTreeMap::from([
            (
                11,
                platform_service::ProcessIdentity {
                    pid: 11,
                    started_at_unix_secs: 111,
                    process_name: Some("proc-11".to_string()),
                },
            ),
            (
                22,
                platform_service::ProcessIdentity {
                    pid: 22,
                    started_at_unix_secs: 999,
                    process_name: Some("proc-22".to_string()),
                },
            ),
            (
                33,
                platform_service::ProcessIdentity {
                    pid: 33,
                    started_at_unix_secs: 333,
                    process_name: Some("proc-33".to_string()),
                },
            ),
        ]));
        let result = reconcile_orphaned_process_ledger_with_path(&path, |pid| {
            running.borrow().get(&pid).cloned()
        })
        .expect("reconcile orphan ledger");

        assert_eq!(
            result,
            OrphanedProcessReconciliation::ExactAuthorityUnavailable {
                retained_sessions: 2,
                retained_processes: 2,
            }
        );
        let retained = read_ledger_from_path(&path).sessions;
        assert_eq!(retained.len(), 2);
        assert!(retained.contains_key("server-11"));
        assert!(retained.contains_key("server-33"));
        assert!(!retained.contains_key("server-22"));
    }

    #[test]
    fn orphan_reconciliation_reports_verified_live_entries_as_authority_unavailable() {
        let temp_dir = std::env::temp_dir().join(format!(
            "devmanager-pid-retain-tests-{}",
            std::process::id()
        ));
        let path = temp_dir.join("running-pids.json");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let ledger = ManagedProcessLedgerFile {
            version: LEDGER_VERSION,
            sessions: BTreeMap::from([("server-44".to_string(), record("server-44", 44, 444))]),
        };
        write_ledger_to_path(&path, &ledger).unwrap();

        let running = platform_service::ProcessIdentity {
            pid: 44,
            started_at_unix_secs: 444,
            process_name: Some("proc-44".to_string()),
        };
        let result = reconcile_orphaned_process_ledger_with_path(&path, |_| Some(running.clone()))
            .expect("reconcile orphan ledger");

        assert_eq!(
            result,
            OrphanedProcessReconciliation::ExactAuthorityUnavailable {
                retained_sessions: 1,
                retained_processes: 1,
            }
        );

        let remaining = read_ledger_from_path(&path)
            .sessions
            .into_values()
            .collect::<Vec<_>>();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].pid, 44);
    }

    #[test]
    fn active_processes_with_filters_non_running_and_reused_entries() {
        let temp_dir = std::env::temp_dir().join(format!(
            "devmanager-pid-active-tests-{}",
            std::process::id()
        ));
        let path = temp_dir.join("running-pids.json");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let ledger = ManagedProcessLedgerFile {
            version: LEDGER_VERSION,
            sessions: BTreeMap::from([
                ("server-5".to_string(), record("server-5", 5, 55)),
                ("server-6".to_string(), record("server-6", 6, 66)),
                ("server-7".to_string(), record("server-7", 7, 77)),
            ]),
        };
        write_ledger_to_path(&path, &ledger).unwrap();

        let mut active = active_processes_with(&path, |pid| match pid {
            5 => Some(platform_service::ProcessIdentity {
                pid,
                started_at_unix_secs: 55,
                process_name: Some("proc-5".to_string()),
            }),
            6 => None,
            7 => Some(platform_service::ProcessIdentity {
                pid,
                started_at_unix_secs: 999,
                process_name: Some("proc-7".to_string()),
            }),
            _ => None,
        });
        active.sort_by(|left, right| left.pid.cmp(&right.pid));

        assert_eq!(
            active
                .into_iter()
                .map(|entry| entry.pid)
                .collect::<Vec<_>>(),
            vec![5]
        );
    }

    #[test]
    fn untrack_session_process_ignores_stale_wait_threads() {
        let temp_dir =
            std::env::temp_dir().join(format!("devmanager-pid-race-tests-{}", std::process::id()));
        let path = temp_dir.join("running-pids.json");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();
        let _guard = use_test_pid_file(path);

        track_session_process(record("server-cmd", 10, 100)).unwrap();
        track_session_process(record("server-cmd", 11, 110)).unwrap();
        untrack_session_process("server-cmd", 10).unwrap();

        let remaining = tracked_processes();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].pid, 11);
    }

    #[test]
    fn release_session_root_keeps_surviving_descendants_tracked() {
        let temp_dir = std::env::temp_dir().join(format!(
            "devmanager-pid-release-tests-{}",
            std::process::id()
        ));
        let path = temp_dir.join("running-pids.json");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();
        let _guard = use_test_pid_file(path);

        track_session_process(record("server-cmd", 10, 100)).unwrap();
        release_session_root("server-cmd", 10, vec![identity(21, 210), identity(22, 220)]).unwrap();

        let remaining = tracked_processes();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].pid, 10);
        assert_eq!(
            remaining[0]
                .descendant_processes
                .iter()
                .map(|identity| identity.pid)
                .collect::<Vec<_>>(),
            vec![21, 22]
        );
    }

    #[test]
    fn release_after_authoritative_job_zero_removes_the_whole_exact_session() {
        let temp_dir = std::env::temp_dir().join(format!(
            "devmanager-pid-job-zero-release-tests-{}",
            std::process::id()
        ));
        let path = temp_dir.join("running-pids.json");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();
        let _guard = use_test_pid_file(path);

        let mut tracked = record("terminal-10", 10, 100);
        tracked.descendant_processes = vec![TrackedProcessIdentity {
            pid: 21,
            started_at_unix_secs: 210,
            process_name: Some("stale-observation".to_string()),
        }];
        track_session_process(tracked).unwrap();

        release_session_root_after_job_zero(
            "terminal-10",
            10,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("authoritative zero releases exact session ledger entry");

        assert!(tracked_processes().is_empty());
    }

    #[test]
    fn orphan_reconciliation_retains_verified_descendants_without_raw_pid_kill() {
        let temp_dir = std::env::temp_dir().join(format!(
            "devmanager-pid-descendant-cleanup-tests-{}",
            std::process::id()
        ));
        let path = temp_dir.join("running-pids.json");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let mut entry = record("server-11", 11, 111);
        entry.descendant_processes = vec![
            TrackedProcessIdentity {
                pid: 21,
                started_at_unix_secs: 210,
                process_name: Some("proc-21".to_string()),
            },
            TrackedProcessIdentity {
                pid: 22,
                started_at_unix_secs: 220,
                process_name: Some("proc-22".to_string()),
            },
        ];
        let ledger = ManagedProcessLedgerFile {
            version: LEDGER_VERSION,
            sessions: BTreeMap::from([("server-11".to_string(), entry)]),
        };
        write_ledger_to_path(&path, &ledger).unwrap();

        let running = RefCell::new(BTreeMap::from([
            (21, identity(21, 210)),
            (22, identity(22, 220)),
        ]));
        let result = reconcile_orphaned_process_ledger_with_path(&path, |pid| {
            running.borrow().get(&pid).cloned()
        })
        .expect("reconcile orphan ledger");

        assert_eq!(
            result,
            OrphanedProcessReconciliation::ExactAuthorityUnavailable {
                retained_sessions: 1,
                retained_processes: 2,
            }
        );
        let retained = read_ledger_from_path(&path)
            .sessions
            .remove("server-11")
            .expect("live descendants remain observable");
        assert_eq!(retained.descendant_processes.len(), 2);
    }

    #[test]
    fn active_processes_with_keeps_records_with_live_descendants() {
        let temp_dir = std::env::temp_dir().join(format!(
            "devmanager-pid-descendant-active-tests-{}",
            std::process::id()
        ));
        let path = temp_dir.join("running-pids.json");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let mut entry = record("server-11", 11, 111);
        entry.descendant_processes = vec![
            TrackedProcessIdentity {
                pid: 21,
                started_at_unix_secs: 210,
                process_name: Some("proc-21".to_string()),
            },
            TrackedProcessIdentity {
                pid: 22,
                started_at_unix_secs: 220,
                process_name: Some("proc-22".to_string()),
            },
        ];
        let ledger = ManagedProcessLedgerFile {
            version: LEDGER_VERSION,
            sessions: BTreeMap::from([("server-11".to_string(), entry)]),
        };
        write_ledger_to_path(&path, &ledger).unwrap();

        let active = active_processes_with(&path, |pid| match pid {
            11 => None,
            21 => Some(identity(21, 210)),
            22 => Some(identity(22, 999)),
            _ => None,
        });

        assert_eq!(active.len(), 1);
        assert_eq!(active[0].pid, 11);
        assert_eq!(
            active[0]
                .descendant_processes
                .iter()
                .map(|identity| identity.pid)
                .collect::<Vec<_>>(),
            vec![21]
        );
    }

    #[test]
    fn sync_retains_verified_detached_descendants_and_rejects_stale_identities() {
        let prior = vec![
            TrackedProcessIdentity {
                pid: 21,
                started_at_unix_secs: 210,
                process_name: Some("worker".to_string()),
            },
            TrackedProcessIdentity {
                pid: 22,
                started_at_unix_secs: 220,
                process_name: Some("stale".to_string()),
            },
        ];
        let current = vec![TrackedProcessIdentity {
            pid: 11,
            started_at_unix_secs: 110,
            process_name: Some("shell-child".to_string()),
        }];

        let retained = retain_verified_descendant_identities(10, current, &prior, |identity| {
            identity.pid == 21
                && identity.started_at_unix_secs == 210
                && identity.process_name.as_deref() == Some("worker")
        });

        assert_eq!(
            retained
                .iter()
                .map(|identity| identity.pid)
                .collect::<Vec<_>>(),
            vec![11, 21]
        );
        assert!(!retained.iter().any(|identity| identity.pid == 22));
    }

    #[test]
    fn sync_with_system_merges_against_current_ledger_entry_under_lock() {
        let temp_dir = std::env::temp_dir().join(format!(
            "devmanager-pid-sync-atomic-tests-{}",
            std::process::id()
        ));
        let path = temp_dir.join("running-pids.json");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();
        let _guard = use_test_pid_file(path);

        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let live = platform_service::process_identity_with_system(&system, std::process::id())
            .expect("test process identity");

        let mut entry = record("session-1", 10, 100);
        entry.descendant_processes = vec![TrackedProcessIdentity {
            pid: live.pid,
            started_at_unix_secs: live.started_at_unix_secs,
            process_name: live.process_name.clone(),
        }];
        track_session_process(entry).unwrap();

        // Empty live walk must still retain the verified prior read under the lock.
        sync_session_descendant_processes_with_system("session-1", 10, Vec::new(), &system)
            .unwrap();

        let remaining = tracked_processes();
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining[0]
                .descendant_processes
                .iter()
                .map(|identity| identity.pid)
                .collect::<Vec<_>>(),
            vec![live.pid]
        );

        // Stale start-time must not be retained.
        let mut stale = record("session-1", 10, 100);
        stale.descendant_processes = vec![TrackedProcessIdentity {
            pid: live.pid,
            started_at_unix_secs: live.started_at_unix_secs.saturating_add(1),
            process_name: live.process_name.clone(),
        }];
        track_session_process(stale).unwrap();
        sync_session_descendant_processes_with_system("session-1", 10, Vec::new(), &system)
            .unwrap();
        assert!(tracked_processes()[0].descendant_processes.is_empty());
    }
}
