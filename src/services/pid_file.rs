use crate::persistence;
use crate::services::platform_service;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
use std::sync::MutexGuard;

static PID_FILE_ACCESS_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
static TEST_PID_FILE_OVERRIDE_LOCK: Mutex<()> = Mutex::new(());
#[cfg(test)]
static TEST_PID_FILE_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

const LEDGER_VERSION: u32 = 1;

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

#[cfg(test)]
pub(crate) struct TestPidFileGuard {
    _lock: MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for TestPidFileGuard {
    fn drop(&mut self) {
        if let Ok(mut override_path) = TEST_PID_FILE_OVERRIDE.lock() {
            *override_path = None;
        }
    }
}

#[cfg(test)]
pub(crate) fn use_test_pid_file(path: PathBuf) -> TestPidFileGuard {
    let lock = TEST_PID_FILE_OVERRIDE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Ok(mut override_path) = TEST_PID_FILE_OVERRIDE.lock() {
        *override_path = Some(path);
    }
    TestPidFileGuard { _lock: lock }
}

fn pid_file_path() -> Result<PathBuf, String> {
    #[cfg(test)]
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
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(_) => return ManagedProcessLedgerFile::default(),
    };
    match serde_json::from_str::<StoredLedgerFile>(&contents) {
        Ok(StoredLedgerFile::Current(mut ledger)) => {
            ledger.version = LEDGER_VERSION;
            ledger
        }
        Ok(StoredLedgerFile::LegacyPids(pids)) => ManagedProcessLedgerFile {
            version: LEDGER_VERSION,
            sessions: pids
                .into_iter()
                .map(|pid| {
                    let entry = ManagedProcessRecord::legacy(pid);
                    (entry.session_id.clone(), entry)
                })
                .collect(),
        },
        Err(_) => ManagedProcessLedgerFile::default(),
    }
}

fn write_ledger_to_path(path: &Path, ledger: &ManagedProcessLedgerFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create PID ledger directory: {error}"))?;
    }
    let temp_path = path.with_extension("json.tmp");
    let contents = serde_json::to_string_pretty(ledger)
        .map_err(|error| format!("Failed to serialize PID ledger: {error}"))?;
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

fn cleanup_orphaned_processes_with_path<F, G>(
    path: &Path,
    mut identify_process: F,
    mut kill_process_tree: G,
) where
    F: FnMut(u32) -> Option<platform_service::ProcessIdentity>,
    G: FnMut(u32) -> Result<(), String>,
{
    let mut ledger = read_ledger_from_path(path);
    if ledger.sessions.is_empty() {
        return;
    }

    let mut retained = BTreeMap::new();
    for (session_id, entry) in ledger.sessions {
        let root_live = tracked_process_identity_state_with(
            &root_process_identity(&entry),
            &mut identify_process,
        ) == TrackedProcessState::VerifiedRunning;
        let Some(active_entry) = active_processes_in_record_with(&entry, &mut identify_process)
        else {
            continue;
        };

        if root_live {
            let _ = kill_process_tree(entry.pid);
        } else {
            for descendant in &active_entry.descendant_processes {
                let _ = kill_process_tree(descendant.pid);
            }
        }

        if let Some(active_after_kill) =
            active_processes_in_record_with(&entry, &mut identify_process)
        {
            retained.insert(session_id, active_after_kill);
        }
    }

    ledger.sessions = retained;
    let _ = write_ledger_to_path(path, &ledger);
}

pub fn track_session_process(record: ManagedProcessRecord) -> Result<(), String> {
    mutate_ledger(|ledger| {
        ledger.version = LEDGER_VERSION;
        ledger.sessions.insert(record.session_id.clone(), record);
    })
    .map(|_| ())
}

pub fn sync_session_descendant_processes(
    session_id: &str,
    root_pid: u32,
    descendants: Vec<platform_service::ProcessIdentity>,
) -> Result<(), String> {
    let normalized = normalize_descendant_processes(root_pid, descendants);
    mutate_ledger_if_changed(|ledger| {
        let Some(entry) = ledger.sessions.get_mut(session_id) else {
            return false;
        };
        if entry.pid != root_pid || entry.descendant_processes == normalized {
            return false;
        }
        entry.descendant_processes = normalized;
        true
    })
    .map(|_| ())
}

pub fn release_session_root(
    session_id: &str,
    root_pid: u32,
    surviving_descendants: Vec<platform_service::ProcessIdentity>,
) -> Result<(), String> {
    let normalized = normalize_descendant_processes(root_pid, surviving_descendants);
    mutate_ledger_if_changed(|ledger| {
        let Some(entry) = ledger.sessions.get(session_id) else {
            return false;
        };
        if entry.pid != root_pid {
            return false;
        }
        if normalized.is_empty() {
            ledger.sessions.remove(session_id);
        } else if let Some(entry) = ledger.sessions.get_mut(session_id) {
            entry.descendant_processes = normalized;
        }
        true
    })
    .map(|_| ())
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

pub fn cleanup_orphaned_processes() {
    let path = match pid_file_path() {
        Ok(path) => path,
        Err(_) => return,
    };
    let _guard = PID_FILE_ACCESS_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    cleanup_orphaned_processes_with_path(
        &path,
        platform_service::capture_process_identity,
        platform_service::kill_process_tree,
    );
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
    fn cleanup_orphaned_processes_kills_running_pids_and_prunes_reused_entries() {
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
        let mut killed = Vec::new();
        cleanup_orphaned_processes_with_path(
            &path,
            |pid| running.borrow().get(&pid).cloned(),
            |pid| {
                killed.push(pid);
                running.borrow_mut().remove(&pid);
                Ok(())
            },
        );

        killed.sort_unstable();
        assert_eq!(killed, vec![11, 33]);
        assert!(read_ledger_from_path(&path).sessions.is_empty());
    }

    #[test]
    fn cleanup_orphaned_processes_keeps_entries_that_still_refuse_to_die() {
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
        cleanup_orphaned_processes_with_path(
            &path,
            |_| Some(running.clone()),
            |_| Err("still running".to_string()),
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
    fn cleanup_orphaned_processes_kills_surviving_descendants_when_root_is_gone() {
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
        let mut killed = Vec::new();
        cleanup_orphaned_processes_with_path(
            &path,
            |pid| running.borrow().get(&pid).cloned(),
            |pid| {
                killed.push(pid);
                running.borrow_mut().remove(&pid);
                Ok(())
            },
        );

        killed.sort_unstable();
        assert_eq!(killed, vec![21, 22]);
        assert!(read_ledger_from_path(&path).sessions.is_empty());
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
}
