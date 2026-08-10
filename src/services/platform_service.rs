use std::collections::{BTreeMap, HashSet};
use std::ffi::{c_void, OsStr};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::process::Command;
#[cfg(not(windows))]
use std::thread;
#[cfg(any(not(windows), test))]
use std::time::Duration;
#[cfg(not(windows))]
use std::time::Instant;

pub use crate::process::job::{attach_process_to_managed_job, ManagedProcessJob};
use crate::process::ports::TcpEndpointRecord;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;
#[cfg(windows)]
const CREATE_SUSPENDED: u32 = 0x00000004;
#[cfg(windows)]
pub const MANAGED_PROCESS_CREATION_FLAGS: u32 = CREATE_NO_WINDOW | CREATE_SUSPENDED;

pub fn snapshot_listener_pids(ports: &[u16]) -> Result<BTreeMap<u16, Vec<u32>>, String> {
    let endpoints = snapshot_listener_endpoints(ports)?;
    let mut listeners = BTreeMap::new();
    for (port, rows) in endpoints {
        let pids = listeners.entry(port).or_insert_with(Vec::new);
        for row in rows {
            if !pids.contains(&row.pid()) {
                pids.push(row.pid());
            }
        }
        pids.sort_unstable();
    }
    Ok(listeners)
}

pub fn snapshot_listener_endpoints(
    ports: &[u16],
) -> Result<BTreeMap<u16, Vec<TcpEndpointRecord>>, String> {
    if ports.is_empty() {
        return Ok(BTreeMap::new());
    }

    #[cfg(windows)]
    {
        snapshot_listener_endpoints_windows(ports)
    }

    #[cfg(not(windows))]
    {
        snapshot_listener_endpoints_with_lsof(ports)
    }
}

pub fn find_pid_on_port(port: u16) -> Result<Option<u32>, String> {
    Ok(snapshot_listener_pids(&[port])?
        .remove(&port)
        .and_then(|pids| pids.into_iter().next()))
}

#[cfg(windows)]
fn snapshot_listener_endpoints_windows(
    ports: &[u16],
) -> Result<BTreeMap<u16, Vec<TcpEndpointRecord>>, String> {
    let filter: HashSet<u16> = ports.iter().copied().collect();
    let mut listeners = BTreeMap::new();
    collect_windows_listener_endpoints(AF_INET, &filter, &mut listeners)?;
    collect_windows_listener_endpoints(AF_INET6, &filter, &mut listeners)?;
    for rows in listeners.values_mut() {
        rows.sort_unstable();
        rows.dedup();
    }
    Ok(listeners)
}

#[cfg(windows)]
fn collect_windows_listener_endpoints(
    address_family: u32,
    filter: &HashSet<u16>,
    listeners: &mut BTreeMap<u16, Vec<TcpEndpointRecord>>,
) -> Result<(), String> {
    let mut size = 0u32;
    let first = unsafe {
        GetExtendedTcpTable(
            std::ptr::null_mut(),
            &mut size,
            0,
            address_family,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    if first != ERROR_INSUFFICIENT_BUFFER && first != NO_ERROR {
        return Err(format!(
            "GetExtendedTcpTable size probe failed for AF {address_family}: {first}"
        ));
    }
    if size == 0 {
        return Ok(());
    }

    let mut buffer = vec![0u8; size as usize];
    let result = unsafe {
        GetExtendedTcpTable(
            buffer.as_mut_ptr() as *mut c_void,
            &mut size,
            0,
            address_family,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    if result != NO_ERROR {
        return Err(format!(
            "GetExtendedTcpTable failed for AF {address_family}: {result}"
        ));
    }

    match address_family {
        AF_INET => {
            let table = buffer.as_ptr() as *const MibTcpTableOwnerPid;
            let entry_count = unsafe { (*table).dw_num_entries as usize };
            let rows = unsafe {
                std::slice::from_raw_parts(
                    std::ptr::addr_of!((*table).table) as *const MibTcpRowOwnerPid,
                    entry_count,
                )
            };
            for row in rows {
                let port = windows_port(row.dw_local_port);
                if filter.contains(&port) {
                    let rows = listeners.entry(port).or_default();
                    rows.push(TcpEndpointRecord::tcp(
                        IpAddr::V4(Ipv4Addr::from(row.dw_local_addr.to_be())),
                        port,
                        row.dw_owning_pid,
                    ));
                    if rows.len() > crate::process::ports::MAX_ENDPOINTS_PER_SCAN {
                        return Err(format!(
                            "listener endpoint count exceeds {}",
                            crate::process::ports::MAX_ENDPOINTS_PER_SCAN
                        ));
                    }
                }
            }
        }
        AF_INET6 => {
            let table = buffer.as_ptr() as *const MibTcp6TableOwnerPid;
            let entry_count = unsafe { (*table).dw_num_entries as usize };
            let rows = unsafe {
                std::slice::from_raw_parts(
                    std::ptr::addr_of!((*table).table) as *const MibTcp6RowOwnerPid,
                    entry_count,
                )
            };
            for row in rows {
                let port = windows_port(row.dw_local_port);
                if filter.contains(&port) {
                    let rows = listeners.entry(port).or_default();
                    rows.push(TcpEndpointRecord::tcp(
                        IpAddr::V6(Ipv6Addr::from(row.uc_local_addr)),
                        port,
                        row.dw_owning_pid,
                    ));
                    if rows.len() > crate::process::ports::MAX_ENDPOINTS_PER_SCAN {
                        return Err(format!(
                            "listener endpoint count exceeds {}",
                            crate::process::ports::MAX_ENDPOINTS_PER_SCAN
                        ));
                    }
                }
            }
        }
        _ => {}
    }

    Ok(())
}

#[cfg(not(windows))]
fn snapshot_listener_endpoints_with_lsof(
    ports: &[u16],
) -> Result<BTreeMap<u16, Vec<TcpEndpointRecord>>, String> {
    let filter: HashSet<u16> = ports.iter().copied().collect();
    let output = Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pn"])
        .output()
        .map_err(|error| format!("Failed to run lsof: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }

    let mut listeners = BTreeMap::new();
    let mut current_pid = None;
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|_| "lsof returned non-UTF-8 listener data".to_string())?;
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        if line.len() < 2 {
            return Err("lsof returned a malformed listener record".to_string());
        }
        let (prefix, value) = line.split_at(1);
        match prefix {
            "p" => {
                current_pid = Some(value.trim().parse::<u32>().map_err(|_| {
                    "lsof returned a listener record with an invalid PID".to_string()
                })?);
            }
            "n" => {
                let Some(pid) = current_pid else {
                    return Err("lsof returned an endpoint before its PID".to_string());
                };
                let endpoint = parse_lsof_listener_endpoint(value, pid)
                    .ok_or_else(|| "lsof returned a malformed listener endpoint".to_string())?;
                if filter.contains(&endpoint.port()) {
                    let rows = listeners.entry(endpoint.port()).or_default();
                    rows.push(endpoint);
                    if rows.len() > crate::process::ports::MAX_ENDPOINTS_PER_SCAN {
                        return Err(format!(
                            "listener endpoint count exceeds {}",
                            crate::process::ports::MAX_ENDPOINTS_PER_SCAN
                        ));
                    }
                }
            }
            _ => {
                return Err("lsof returned an unsupported listener record field".to_string());
            }
        }
    }

    for rows in listeners.values_mut() {
        rows.sort_unstable();
        rows.dedup();
    }
    Ok(listeners)
}

#[cfg(not(windows))]
fn parse_lsof_listener_endpoint(value: &str, pid: u32) -> Option<TcpEndpointRecord> {
    let endpoint = value
        .trim()
        .split("->")
        .next()
        .unwrap_or(value)
        .trim_end_matches(" (LISTEN)")
        .trim();
    let (address, port_text) = if let Some(endpoint) = endpoint.strip_prefix('[') {
        let (address, port_text) = endpoint.split_once("]:")?;
        (address, port_text)
    } else {
        endpoint.rsplit_once(':')?
    };
    let port = port_text.trim().parse::<u16>().ok()?;
    let address = match address.trim() {
        "*" | "0.0.0.0" => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        "[::]" | "*:*" => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        address => address.parse().ok()?,
    };
    Some(TcpEndpointRecord::tcp(address, port, pid))
}

#[cfg(not(windows))]
fn parse_lsof_listener_port(value: &str) -> Option<u16> {
    parse_lsof_listener_endpoint(value, 1).map(|endpoint| endpoint.port())
}

#[cfg(windows)]
const AF_INET: u32 = 2;
#[cfg(windows)]
const AF_INET6: u32 = 23;
#[cfg(windows)]
const TCP_TABLE_OWNER_PID_LISTENER: u32 = 3;
#[cfg(windows)]
const NO_ERROR: u32 = 0;
#[cfg(windows)]
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

#[cfg(windows)]
#[repr(C)]
struct MibTcpRowOwnerPid {
    dw_state: u32,
    dw_local_addr: u32,
    dw_local_port: u32,
    dw_remote_addr: u32,
    dw_remote_port: u32,
    dw_owning_pid: u32,
}

#[cfg(windows)]
#[repr(C)]
struct MibTcpTableOwnerPid {
    dw_num_entries: u32,
    table: [MibTcpRowOwnerPid; 1],
}

#[cfg(windows)]
#[repr(C)]
struct MibTcp6RowOwnerPid {
    uc_local_addr: [u8; 16],
    dw_local_scope_id: u32,
    dw_local_port: u32,
    uc_remote_addr: [u8; 16],
    dw_remote_scope_id: u32,
    dw_remote_port: u32,
    dw_state: u32,
    dw_owning_pid: u32,
}

#[cfg(windows)]
#[repr(C)]
struct MibTcp6TableOwnerPid {
    dw_num_entries: u32,
    table: [MibTcp6RowOwnerPid; 1],
}

#[cfg(windows)]
#[link(name = "iphlpapi")]
extern "system" {
    fn GetExtendedTcpTable(
        p_tcp_table: *mut c_void,
        pdw_size: *mut u32,
        b_order: i32,
        ul_af: u32,
        table_class: u32,
        reserved: u32,
    ) -> u32;
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
    fn TerminateProcess(handle: *mut c_void, exit_code: u32) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> *mut c_void;
    fn Thread32First(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
    fn Thread32Next(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
    fn OpenThread(desired_access: u32, inherit_handle: i32, thread_id: u32) -> *mut c_void;
    fn ResumeThread(thread: *mut c_void) -> u32;
    fn GetActiveProcessorCount(group_number: u16) -> u32;
}

#[cfg(windows)]
const PROCESS_TERMINATE: u32 = 0x0001;
#[cfg(windows)]
const SYNCHRONIZE: u32 = 0x00100000;
#[cfg(windows)]
const ALL_PROCESSOR_GROUPS: u16 = 0xffff;
#[cfg(windows)]
const TH32CS_SNAPTHREAD: u32 = 0x00000004;
#[cfg(windows)]
const THREAD_SUSPEND_RESUME: u32 = 0x0002;
#[cfg(windows)]
const INVALID_HANDLE_VALUE: *mut c_void = -1_isize as *mut c_void;
#[cfg(windows)]
const RESUME_THREAD_FAILED: u32 = u32::MAX;

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct ThreadEntry32 {
    size: u32,
    usage_count: u32,
    thread_id: u32,
    owner_process_id: u32,
    base_priority: i32,
    priority_delta: i32,
    flags: u32,
}

/// Logical processors visible to the whole machine (Task Manager denominator).
///
/// On Windows this uses `GetActiveProcessorCount(ALL_PROCESSOR_GROUPS)` so
/// machines with more than 64 logical processors are counted correctly.
pub fn logical_processor_count() -> u32 {
    #[cfg(windows)]
    {
        let count = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
        count.max(1)
    }

    #[cfg(not(windows))]
    {
        std::thread::available_parallelism()
            .map(|count| count.get() as u32)
            .unwrap_or(1)
            .max(1)
    }
}

#[cfg(windows)]
fn windows_port(raw_port: u32) -> u16 {
    u16::from_be((raw_port & 0xffff) as u16)
}

pub fn kill_process_tree(pid: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        // Terminate descendants leaf-first so a parent can't repopulate the tree
        // mid-kill. We ignore per-child errors and rely on the final check of the
        // root PID to determine success.
        let descendants = collect_descendant_process_identities(pid);
        for child in descendants.iter().rev() {
            let _ = windows_terminate_pid(child.pid);
        }
        windows_terminate_pid(pid)
    }

    #[cfg(not(windows))]
    {
        kill_unix_target(pid, true)
    }
}

pub fn kill_process(pid: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        windows_terminate_pid(pid)
    }

    #[cfg(not(windows))]
    {
        kill_unix_target(pid, false)
    }
}

fn claim_suspended_process_with<Job, Attach, Resume>(
    pid: u32,
    attach: Attach,
    resume: Resume,
) -> Result<Option<Job>, String>
where
    Attach: FnOnce(u32) -> Result<Option<Job>, String>,
    Resume: FnOnce(u32) -> Result<(), String>,
{
    let job = attach(pid)?;
    resume(pid)?;
    Ok(job)
}

/// Claims a process created with [`MANAGED_PROCESS_CREATION_FLAGS`] before
/// allowing any of its code to execute. On Windows the returned job must stay
/// alive for as long as the process tree is owned.
pub fn claim_suspended_process(pid: u32) -> Result<Option<ManagedProcessJob>, String> {
    #[cfg(windows)]
    {
        claim_suspended_process_with(pid, attach_process_to_managed_job, resume_suspended_process)
    }

    #[cfg(not(windows))]
    {
        attach_process_to_managed_job(pid)
    }
}

#[cfg(windows)]
fn resume_suspended_process(pid: u32) -> Result<(), String> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(format!(
                "CreateToolhelp32Snapshot failed while resuming process {pid}: {}",
                std::io::Error::last_os_error()
            ));
        }

        let mut entry = ThreadEntry32 {
            size: std::mem::size_of::<ThreadEntry32>() as u32,
            ..ThreadEntry32::default()
        };
        let mut thread_ids = Vec::new();
        let mut has_entry = Thread32First(snapshot, &mut entry) != 0;
        while has_entry {
            if entry.owner_process_id == pid {
                thread_ids.push(entry.thread_id);
            }
            entry.size = std::mem::size_of::<ThreadEntry32>() as u32;
            has_entry = Thread32Next(snapshot, &mut entry) != 0;
        }
        let _ = CloseHandle(snapshot);

        if thread_ids.is_empty() {
            return Err(format!(
                "Cannot resume process {pid}: no process threads were found"
            ));
        }

        let mut resumed_suspended_thread = false;
        for thread_id in thread_ids {
            let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id);
            if thread.is_null() {
                return Err(format!(
                    "OpenThread({thread_id}) failed while resuming process {pid}: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let previous_suspend_count = ResumeThread(thread);
            let resume_error = std::io::Error::last_os_error();
            let _ = CloseHandle(thread);
            if previous_suspend_count == RESUME_THREAD_FAILED {
                return Err(format!(
                    "ResumeThread({thread_id}) failed for process {pid}: {resume_error}"
                ));
            }
            resumed_suspended_thread |= previous_suspend_count > 0;
        }

        if !resumed_suspended_thread {
            return Err(format!(
                "Cannot resume process {pid}: no suspended process thread was found"
            ));
        }
        Ok(())
    }
}

pub fn is_pid_running(pid: u32) -> bool {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    system.process(sysinfo::Pid::from_u32(pid)).is_some()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub started_at_unix_secs: u64,
    pub process_name: Option<String>,
}

pub fn capture_process_identity(pid: u32) -> Option<ProcessIdentity> {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    process_identity_with_system(&system, pid)
}

pub fn process_identity_with_system(system: &sysinfo::System, pid: u32) -> Option<ProcessIdentity> {
    let process = system.process(sysinfo::Pid::from_u32(pid))?;
    Some(ProcessIdentity {
        pid,
        started_at_unix_secs: process.start_time(),
        process_name: normalize_process_name(process.name()),
    })
}

pub fn process_matches_identity(
    pid: u32,
    started_at_unix_secs: u64,
    expected_name: Option<&str>,
) -> bool {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    process_matches_identity_with_system(&system, pid, started_at_unix_secs, expected_name)
}

pub fn process_matches_identity_with_system(
    system: &sysinfo::System,
    pid: u32,
    started_at_unix_secs: u64,
    expected_name: Option<&str>,
) -> bool {
    if started_at_unix_secs == 0 {
        return false;
    }
    let Some(identity) = process_identity_with_system(system, pid) else {
        return false;
    };
    if identity.started_at_unix_secs != started_at_unix_secs {
        return false;
    }
    match expected_name.filter(|name| !name.trim().is_empty()) {
        Some(expected_name) => identity
            .process_name
            .as_deref()
            .map(|actual_name| actual_name.eq_ignore_ascii_case(expected_name))
            .unwrap_or(false),
        None => true,
    }
}

pub fn collect_descendant_process_identities(root_pid: u32) -> Vec<ProcessIdentity> {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    collect_descendant_process_identities_with_system(&system, root_pid)
}

pub fn collect_descendant_process_identities_with_system(
    system: &sysinfo::System,
    root_pid: u32,
) -> Vec<ProcessIdentity> {
    let root_pid = sysinfo::Pid::from_u32(root_pid);
    let mut queue = vec![root_pid];
    let mut visited = HashSet::from([root_pid]);
    let mut descendants = Vec::new();
    let mut cursor = 0;

    while cursor < queue.len() {
        let parent_pid = queue[cursor];
        cursor += 1;

        for (candidate_pid, process) in system.processes() {
            if process.parent() == Some(parent_pid) && visited.insert(*candidate_pid) {
                queue.push(*candidate_pid);
                if let Some(identity) = process_identity_with_system(system, candidate_pid.as_u32())
                {
                    descendants.push(identity);
                }
            }
        }
    }

    descendants.sort_by_key(|identity| identity.pid);
    descendants
}

pub fn get_process_name(pid: u32) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| format!("Failed to run tasklist: {error}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if line.is_empty() || line.contains("No tasks are running") {
            return Ok(None);
        }
        let first = line
            .trim_matches('"')
            .split("\",\"")
            .next()
            .map(|value| value.to_string());
        Ok(first.filter(|value| !value.is_empty()))
    }

    #[cfg(not(windows))]
    {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .map_err(|error| format!("Failed to run ps: {error}"))?;
        if !output.status.success() {
            return Ok(None);
        }
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!name.is_empty()).then_some(name))
    }
}

fn normalize_process_name(name: &OsStr) -> Option<String> {
    let value = name.to_string_lossy().trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::{claim_suspended_process_with, terminate_owned_process_group_with};
    use std::cell::RefCell;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::Duration;

    #[cfg(windows)]
    use super::windows_port;

    #[cfg(windows)]
    #[test]
    fn windows_port_decodes_network_order_port() {
        assert_eq!(windows_port(0x5000), 80);
        assert_eq!(windows_port(0x3614), 5174);
    }

    #[test]
    fn suspended_process_claim_assigns_job_before_resume() {
        let events = RefCell::new(Vec::new());

        let job = claim_suspended_process_with(
            42,
            |pid| {
                events.borrow_mut().push(("assign", pid));
                Ok(Some("job"))
            },
            |pid| {
                events.borrow_mut().push(("resume", pid));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(job, Some("job"));
        assert_eq!(events.into_inner(), [("assign", 42), ("resume", 42)]);
    }

    #[test]
    fn logical_processor_count_is_never_zero() {
        assert!(super::logical_processor_count() > 0);
    }

    #[test]
    fn suspended_process_claim_releases_job_when_resume_fails() {
        #[derive(Debug)]
        struct DropMarker(Arc<AtomicBool>);

        impl Drop for DropMarker {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let released = Arc::new(AtomicBool::new(false));
        let marker = released.clone();
        let result = claim_suspended_process_with(
            42,
            move |_| Ok(Some(DropMarker(marker))),
            |_| Err("resume failed".to_string()),
        );

        assert_eq!(result.unwrap_err(), "resume failed");
        assert!(released.load(Ordering::Acquire));
    }

    #[cfg(windows)]
    #[test]
    fn windows_managed_process_stays_suspended_until_claimed() {
        use super::{claim_suspended_process, MANAGED_PROCESS_CREATION_FLAGS};
        use std::os::windows::process::CommandExt;

        let unique = format!(
            "devmanager-suspended-process-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let marker = std::env::temp_dir().join(unique);
        let mut child = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[IO.File]::WriteAllText($env:DEVMANAGER_SUSPENDED_MARKER, 'resumed')",
            ])
            .env("DEVMANAGER_SUSPENDED_MARKER", &marker)
            .creation_flags(MANAGED_PROCESS_CREATION_FLAGS)
            .spawn()
            .unwrap();

        std::thread::sleep(Duration::from_millis(150));
        assert!(!marker.exists(), "suspended child must not execute early");

        let job = claim_suspended_process(child.id()).unwrap();
        let status = child.wait().unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "resumed");

        drop(job);
        let _ = std::fs::remove_file(marker);
    }

    #[cfg(windows)]
    #[test]
    fn windows_managed_job_reports_worker_after_intermediate_launcher_exits() {
        use super::attach_process_to_managed_job;

        let unique = format!(
            "devmanager-job-members-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let harness_dir = std::env::temp_dir().join(unique);
        let launcher_script = harness_dir.join("launcher.ps1");
        let worker_script = harness_dir.join("worker.ps1");
        let worker_pid_file = harness_dir.join("worker.pid");
        std::fs::create_dir_all(&harness_dir).expect("create job harness directory");
        std::fs::write(&worker_script, "Start-Sleep -Seconds 30").expect("write worker script");
        std::fs::write(
            &launcher_script,
            "$worker = Start-Process -NoNewWindow -FilePath powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-File',$env:DEVMANAGER_JOB_WORKER_SCRIPT -PassThru\n[IO.File]::WriteAllText($env:DEVMANAGER_JOB_WORKER_PID, [string]$worker.Id)\n",
        )
        .expect("write launcher script");

        let mut child = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Milliseconds 750; $launcher = Start-Process -NoNewWindow -FilePath powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-File',$env:DEVMANAGER_JOB_LAUNCHER_SCRIPT -PassThru; $launcher.WaitForExit(); Start-Sleep -Seconds 30",
            ])
            .env("DEVMANAGER_JOB_LAUNCHER_SCRIPT", &launcher_script)
            .env("DEVMANAGER_JOB_WORKER_SCRIPT", &worker_script)
            .env("DEVMANAGER_JOB_WORKER_PID", &worker_pid_file)
            .spawn()
            .expect("spawn managed process");
        let child_pid = child.id();
        let job = attach_process_to_managed_job(child_pid)
            .expect("attach job")
            .expect("windows managed job");

        let started = std::time::Instant::now();
        while !worker_pid_file.exists() && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let worker_pid: u32 = std::fs::read_to_string(&worker_pid_file)
            .expect("launcher should record worker PID")
            .trim()
            .parse()
            .expect("worker PID should be numeric");
        // The launcher exits immediately after recording the worker.
        std::thread::sleep(Duration::from_millis(250));

        let process_ids = job.active_process_ids().expect("query managed job");
        assert!(
            process_ids.contains(&child_pid),
            "job members {process_ids:?} did not contain assigned pid {child_pid}"
        );
        assert!(
            process_ids.contains(&worker_pid),
            "job members {process_ids:?} lost worker {worker_pid} after its launcher exited"
        );

        let _ = child.kill();
        let _ = child.wait();
        drop(job);
        let _ = std::fs::remove_dir_all(harness_dir);
    }

    #[test]
    fn owned_process_group_cleanup_escalates_when_only_descendants_remain() {
        let mut signals = Vec::new();
        terminate_owned_process_group_with(
            "-42",
            Duration::ZERO,
            |target, signal| {
                signals.push((target.to_string(), signal.to_string()));
                Ok(())
            },
            |_| true,
            |_| {},
        )
        .unwrap();

        assert_eq!(
            signals,
            [
                ("-42".to_string(), "TERM".to_string()),
                ("-42".to_string(), "KILL".to_string())
            ]
        );
    }
}

#[cfg(all(test, not(windows)))]
mod non_windows_tests {
    use super::parse_lsof_listener_port;

    #[test]
    fn parse_lsof_listener_port_handles_localhost_and_ipv6() {
        assert_eq!(parse_lsof_listener_port("127.0.0.1:3000"), Some(3000));
        assert_eq!(parse_lsof_listener_port("[::1]:5174"), Some(5174));
        assert_eq!(parse_lsof_listener_port("*:8080 (LISTEN)"), Some(8080));
    }
}

pub fn open_terminal(folder_path: &str, shell_path: Option<&str>) -> Result<(), String> {
    let path = Path::new(folder_path);
    if !path.exists() {
        return Err(format!("Directory does not exist: {}", path.display()));
    }

    #[cfg(windows)]
    {
        let quoted_path = format!("\"{}\"", folder_path);
        let wt_result = Command::new("cmd")
            .args(["/C", "start", "wt", "-d", &quoted_path])
            .output();
        match wt_result {
            Ok(output) if output.status.success() => Ok(()),
            _ => {
                let command = match shell_path.filter(|value| !value.trim().is_empty()) {
                    Some(shell) => format!("cd /d {quoted_path} && \"{shell}\""),
                    None => format!("cd /d {quoted_path}"),
                };
                Command::new("cmd")
                    .args(["/C", "start", "cmd", "/K", &command])
                    .output()
                    .map_err(|error| format!("Failed to open terminal: {error}"))?;
                Ok(())
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        let shell = shell_path
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("/bin/zsh");
        let terminal_command = format!(
            "cd {} && exec {} -l",
            shell_quote(folder_path),
            shell_quote(shell)
        );
        let output = Command::new("osascript")
            .args(["-e", "tell application \"Terminal\""])
            .args(["-e", "activate"])
            .args([
                "-e",
                &format!("do script {}", applescript_quote(&terminal_command)),
            ])
            .args(["-e", "end tell"])
            .output()
            .map_err(|error| format!("Failed to open Terminal.app: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let output = Command::new("xdg-open")
            .arg(folder_path)
            .output()
            .map_err(|error| format!("Failed to open directory: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }
}

pub fn open_url(url: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        Command::new("cmd")
            .args(["/C", "start", "", url])
            .output()
            .map_err(|error| format!("Failed to open URL: {error}"))?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        let output = Command::new("open")
            .arg(url)
            .output()
            .map_err(|error| format!("Failed to open URL: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let output = Command::new("xdg-open")
            .arg(url)
            .output()
            .map_err(|error| format!("Failed to open URL: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }
}

pub fn open_path(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("Path does not exist: {}", path.display()));
    }

    #[cfg(windows)]
    let mut command = Command::new("explorer.exe");
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = Command::new("xdg-open");

    command
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Failed to open {}: {error}", path.display()))
}

#[cfg(target_os = "macos")]
fn applescript_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(windows)]
fn windows_terminate_pid(pid: u32) -> Result<(), String> {
    if !is_pid_running(pid) {
        return Ok(());
    }
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            if !is_pid_running(pid) {
                return Ok(());
            }
            return Err(format!(
                "OpenProcess({pid}) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let ok = TerminateProcess(handle, 1);
        if ok == 0 && is_pid_running(pid) {
            let err = std::io::Error::last_os_error();
            CloseHandle(handle);
            return Err(format!("TerminateProcess({pid}) failed: {err}"));
        }
        let _ = WaitForSingleObject(handle, 2000);
        CloseHandle(handle);
    }
    if is_pid_running(pid) {
        Err(format!("Process {pid} did not exit after TerminateProcess"))
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn kill_unix_target(pid: u32, as_process_group: bool) -> Result<(), String> {
    let target = pid.to_string();
    let group_target = format!("-{pid}");
    let mut used_group = as_process_group;

    if let Err(error) = send_unix_signal(
        if as_process_group {
            group_target.as_str()
        } else {
            target.as_str()
        },
        "TERM",
    ) {
        if as_process_group {
            used_group = false;
            if is_pid_running(pid) {
                send_unix_signal(target.as_str(), "TERM").map_err(|direct_error| {
                    format!("Failed to terminate process {pid}: {direct_error}")
                })?;
            }
        } else if is_pid_running(pid) {
            return Err(format!("Failed to terminate process {pid}: {error}"));
        } else {
            return Ok(());
        }
    }

    if wait_for_pid_exit(pid, Duration::from_secs(2)) {
        return Ok(());
    }

    let kill_target = if used_group {
        group_target.as_str()
    } else {
        target.as_str()
    };
    if let Err(error) = send_unix_signal(kill_target, "KILL") {
        if is_pid_running(pid) {
            return Err(format!("Failed to kill process {pid}: {error}"));
        }
        return Ok(());
    }

    if wait_for_pid_exit(pid, Duration::from_secs(1)) {
        Ok(())
    } else {
        Err(format!("Process {pid} did not exit after SIGKILL"))
    }
}

#[cfg(not(windows))]
pub(crate) fn terminate_owned_process_group(pid: u32, term_grace: Duration) -> Result<(), String> {
    let group_target = format!("-{pid}");
    terminate_owned_process_group_with(
        &group_target,
        term_grace,
        send_unix_signal,
        unix_process_group_exists,
        thread::sleep,
    )
}

#[cfg(any(not(windows), test))]
fn terminate_owned_process_group_with<Signal, Exists, Sleep>(
    group_target: &str,
    term_grace: Duration,
    mut signal: Signal,
    mut exists: Exists,
    mut sleep: Sleep,
) -> Result<(), String>
where
    Signal: FnMut(&str, &str) -> Result<(), String>,
    Exists: FnMut(&str) -> bool,
    Sleep: FnMut(Duration),
{
    let term_error = signal(group_target, "TERM").err();
    if !exists(group_target) {
        return Ok(());
    }
    sleep(term_grace);
    if !exists(group_target) {
        return Ok(());
    }
    signal(group_target, "KILL").map_err(|kill_error| {
        term_error.map_or_else(
            || format!("Failed to SIGKILL owned process group {group_target}: {kill_error}"),
            |term_error| {
                format!(
                    "Failed to terminate owned process group {group_target}: SIGTERM failed ({term_error}); SIGKILL failed ({kill_error})"
                )
            },
        )
    })
}

#[cfg(not(windows))]
fn send_unix_signal(target: &str, signal: &str) -> Result<(), String> {
    let output = Command::new("kill")
        .args([&format!("-{signal}"), "--", target])
        .output()
        .map_err(|error| format!("Failed to run kill: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(not(windows))]
fn unix_process_group_exists(target: &str) -> bool {
    Command::new("kill")
        .args(["-0", "--", target])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(windows))]
fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    let started_at = Instant::now();
    while started_at.elapsed() < timeout {
        if !is_pid_running(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    !is_pid_running(pid)
}

#[cfg(target_os = "macos")]
fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        "''".to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}
