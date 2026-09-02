use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{c_void, OsStr};
#[cfg(not(windows))]
use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(not(windows))]
use std::process::Stdio;
#[cfg(not(windows))]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
#[cfg(not(windows))]
use std::thread;
#[cfg(any(not(windows), test))]
use std::time::Duration;
#[cfg(not(windows))]
use std::time::Instant;

pub(crate) use crate::process::job::{attach_process_to_managed_job, ManagedProcessJob};
use crate::process::ports::TcpEndpointRecord;

const MAX_LISTENER_PORT_BATCH: usize = 4_096;
#[cfg(windows)]
const MAX_WINDOWS_TCP_TABLE_BYTES: u32 = 64 * 1024 * 1024;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;
#[cfg(windows)]
const CREATE_SUSPENDED: u32 = 0x00000004;
#[cfg(windows)]
pub const MANAGED_PROCESS_CREATION_FLAGS: u32 = CREATE_NO_WINDOW | CREATE_SUSPENDED;

pub fn snapshot_listener_pids(ports: &[u16]) -> Result<BTreeMap<u16, Vec<u32>>, String> {
    if ports.len() > MAX_LISTENER_PORT_BATCH {
        return Err(format!(
            "listener snapshot exceeds {MAX_LISTENER_PORT_BATCH} ports"
        ));
    }
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
    if ports.len() > MAX_LISTENER_PORT_BATCH {
        return Err(format!(
            "listener snapshot exceeds {MAX_LISTENER_PORT_BATCH} ports"
        ));
    }
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
    let absolute_deadline = std::time::Instant::now()
        .checked_add(std::time::Duration::from_secs(1))
        .ok_or_else(|| "listener snapshot deadline overflow".to_string())?;
    snapshot_listener_endpoints_windows_until(ports, absolute_deadline)
}

#[cfg(windows)]
fn snapshot_listener_endpoints_windows_until(
    ports: &[u16],
    absolute_deadline: std::time::Instant,
) -> Result<BTreeMap<u16, Vec<TcpEndpointRecord>>, String> {
    check_listener_snapshot_deadline(absolute_deadline, "listener filter allocation")?;
    let filter: HashSet<u16> = ports.iter().copied().collect();
    check_listener_snapshot_deadline(absolute_deadline, "listener filter allocation")?;
    let mut listeners = BTreeMap::new();
    collect_windows_listener_endpoints(AF_INET, &filter, &mut listeners, absolute_deadline)?;
    collect_windows_listener_endpoints(AF_INET6, &filter, &mut listeners, absolute_deadline)?;
    check_listener_snapshot_deadline(absolute_deadline, "listener snapshot completion")?;
    for rows in listeners.values_mut() {
        rows.sort_unstable();
        rows.dedup();
    }
    Ok(listeners)
}

#[cfg(windows)]
fn snapshot_listener_pids_windows(ports: &[u16]) -> Result<HashMap<u16, u32>, String> {
    let absolute_deadline = std::time::Instant::now()
        .checked_add(std::time::Duration::from_secs(1))
        .ok_or_else(|| "listener snapshot deadline overflow".to_string())?;
    snapshot_listener_pids_windows_until(ports, absolute_deadline)
}

#[cfg(windows)]
fn snapshot_listener_pids_windows_until(
    ports: &[u16],
    absolute_deadline: std::time::Instant,
) -> Result<HashMap<u16, u32>, String> {
    let endpoints = snapshot_listener_endpoints_windows_until(ports, absolute_deadline)?;
    let mut listeners = HashMap::new();
    for (port, rows) in endpoints {
        if let Some(row) = rows.first() {
            listeners.entry(port).or_insert(row.pid());
        }
    }
    Ok(listeners)
}

#[cfg(windows)]
fn collect_windows_listener_endpoints(
    address_family: u32,
    filter: &HashSet<u16>,
    listeners: &mut BTreeMap<u16, Vec<TcpEndpointRecord>>,
    absolute_deadline: std::time::Instant,
) -> Result<(), String> {
    check_listener_snapshot_deadline(absolute_deadline, "TCP table size lookup")?;
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
    check_listener_snapshot_deadline(absolute_deadline, "TCP table size lookup")?;
    if first != ERROR_INSUFFICIENT_BUFFER && first != NO_ERROR {
        return Err(format!(
            "GetExtendedTcpTable size probe failed for AF {address_family}: {first}"
        ));
    }
    if size == 0 {
        return Ok(());
    }
    if size > MAX_WINDOWS_TCP_TABLE_BYTES {
        return Err(format!(
            "Windows TCP listener table exceeds {MAX_WINDOWS_TCP_TABLE_BYTES} bytes"
        ));
    }

    let buffer_len = usize::try_from(size)
        .map_err(|_| "Windows TCP listener table size does not fit usize".to_string())?;
    check_listener_snapshot_deadline(absolute_deadline, "TCP table allocation")?;
    let mut buffer = vec![0u8; buffer_len];
    check_listener_snapshot_deadline(absolute_deadline, "TCP table allocation")?;
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
    check_listener_snapshot_deadline(absolute_deadline, "TCP table lookup")?;
    if result != NO_ERROR {
        return Err(format!(
            "GetExtendedTcpTable failed for AF {address_family}: {result}"
        ));
    }
    let returned_len = usize::try_from(size)
        .map_err(|_| "Windows TCP listener table result size does not fit usize".to_string())?;
    if returned_len > buffer.len() {
        return Err(format!(
            "Windows TCP listener table returned {returned_len} bytes into a {} byte allocation",
            buffer.len()
        ));
    }
    let table_bytes = &buffer[..returned_len];

    match address_family {
        AF_INET => {
            for_each_windows_tcp_row::<MibTcpRowOwnerPid, _>(table_bytes, |row| {
                check_listener_snapshot_deadline(absolute_deadline, "IPv4 listener projection")?;
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
                Ok::<(), String>(())
            })?;
        }
        AF_INET6 => {
            for_each_windows_tcp_row::<MibTcp6RowOwnerPid, _>(table_bytes, |row| {
                check_listener_snapshot_deadline(absolute_deadline, "IPv6 listener projection")?;
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
                Ok::<(), String>(())
            })?;
        }
        _ => {}
    }

    check_listener_snapshot_deadline(absolute_deadline, "TCP listener projection")?;
    Ok(())
}

#[cfg(windows)]
fn for_each_windows_tcp_row<Row: Copy, Visit>(bytes: &[u8], mut visit: Visit) -> Result<(), String>
where
    Visit: FnMut(Row) -> Result<(), String>,
{
    const HEADER_BYTES: usize = std::mem::size_of::<u32>();
    if bytes.len() < HEADER_BYTES {
        return Err("Windows TCP listener table is missing its entry-count header".to_string());
    }
    let header: [u8; HEADER_BYTES] = bytes[..HEADER_BYTES]
        .try_into()
        .map_err(|_| "Windows TCP listener table header length is invalid".to_string())?;
    let entry_count = u32::from_ne_bytes(header) as usize;
    let row_size = std::mem::size_of::<Row>();
    if row_size == 0 {
        return Err("Windows TCP listener table row type has zero size".to_string());
    }
    let row_bytes = entry_count
        .checked_mul(row_size)
        .ok_or_else(|| "Windows TCP listener table row count overflow".to_string())?;
    let required = HEADER_BYTES
        .checked_add(row_bytes)
        .ok_or_else(|| "Windows TCP listener table byte count overflow".to_string())?;
    if required > bytes.len() {
        return Err(format!(
            "Windows TCP listener table claims {entry_count} rows requiring {required} bytes, but only {} bytes were returned",
            bytes.len()
        ));
    }

    for index in 0..entry_count {
        let offset = HEADER_BYTES + index * row_size;
        // The IP Helper API writes into byte storage whose alignment is not a
        // Rust type guarantee. Bounds are proven above; read each C row with
        // `read_unaligned` instead of constructing a potentially misaligned
        // typed slice.
        let row = unsafe { std::ptr::read_unaligned(bytes.as_ptr().add(offset).cast::<Row>()) };
        visit(row)?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn snapshot_listener_endpoints_with_lsof(
    ports: &[u16],
) -> Result<BTreeMap<u16, Vec<TcpEndpointRecord>>, String> {
    let filter: HashSet<u16> = ports.iter().copied().collect();
    let output = run_bounded_command(
        trusted_lsof_program(),
        &["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pn"],
    )?;
    if !output.status.success() {
        return Err("listener_probe.command_failed".to_string());
    }

    let mut listeners = BTreeMap::new();
    let mut current_pid = None;
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|_| "listener_probe.invalid_utf8".to_string())?;
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        if line.len() < 2 {
            return Err("listener_probe.malformed_record".to_string());
        }
        let (prefix, value) = line.split_at(1);
        match prefix {
            "p" => {
                current_pid = Some(
                    value
                        .trim()
                        .parse::<u32>()
                        .map_err(|_| "listener_probe.invalid_pid".to_string())?,
                );
            }
            "n" => {
                let Some(pid) = current_pid else {
                    return Err("listener_probe.endpoint_without_pid".to_string());
                };
                let endpoint = parse_lsof_listener_endpoint(value, pid)
                    .ok_or_else(|| "listener_probe.malformed_endpoint".to_string())?;
                if filter.contains(&endpoint.port()) {
                    let rows = listeners.entry(endpoint.port()).or_default();
                    rows.push(endpoint);
                    if rows.len() > crate::process::ports::MAX_ENDPOINTS_PER_SCAN {
                        return Err(format!("listener_probe.endpoint_limit_exceeded"));
                    }
                }
            }
            _ => {
                return Err("listener_probe.unsupported_record_field".to_string());
            }
        }
    }

    for rows in listeners.values_mut() {
        rows.sort_unstable();
        rows.dedup();
    }
    Ok(listeners)
}

#[cfg(target_os = "linux")]
const TRUSTED_LSOF_PATH: &str = "/usr/bin/lsof";

#[cfg(target_os = "macos")]
const TRUSTED_LSOF_PATH: &str = "/usr/sbin/lsof";

#[cfg(not(windows))]
fn trusted_lsof_program() -> &'static str {
    TRUSTED_LSOF_PATH
}

#[cfg(all(unix, not(target_os = "linux"), not(target_os = "macos")))]
const TRUSTED_LSOF_PATH: &str = "/usr/sbin/lsof";

#[cfg(not(windows))]
const MAX_LSOF_OUTPUT_BYTES: usize = 256 * 1024;

#[cfg(not(windows))]
const MAX_LSOF_RUNTIME: Duration = Duration::from_millis(750);

#[cfg(not(windows))]
struct BoundedChildOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[cfg(not(windows))]
fn run_bounded_command(program: &str, args: &[&str]) -> Result<BoundedChildOutput, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| "listener_probe.spawn_failed".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "listener_probe.stdout_unavailable".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "listener_probe.stderr_unavailable".to_string())?;
    let output_exceeded = Arc::new(AtomicBool::new(false));
    let stdout_exceeded = output_exceeded.clone();
    let stderr_exceeded = output_exceeded.clone();
    let stdout_reader =
        thread::spawn(move || read_bounded(stdout, MAX_LSOF_OUTPUT_BYTES, stdout_exceeded));
    let stderr_reader =
        thread::spawn(move || read_bounded(stderr, MAX_LSOF_OUTPUT_BYTES, stderr_exceeded));

    let deadline = Instant::now()
        .checked_add(MAX_LSOF_RUNTIME)
        .unwrap_or_else(Instant::now);
    let mut status = None;
    let mut timed_out = false;
    loop {
        if output_exceeded.load(Ordering::Acquire) {
            let _ = child.kill();
            break;
        }
        match child.try_wait() {
            Ok(Some(next)) => {
                status = Some(next);
                break;
            }
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                let _ = child.kill();
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(_error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err("listener_probe.poll_failed".to_string());
            }
        }
    }
    let status = match status {
        Some(status) => status,
        None => child
            .wait()
            .map_err(|_| "listener_probe.wait_failed".to_string())?,
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "listener_probe.stdout_reader_panicked".to_string())?
        .map_err(|_| "listener_probe.stdout_read_failed".to_string())?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "listener_probe.stderr_reader_panicked".to_string())?
        .map_err(|_| "listener_probe.stderr_read_failed".to_string())?;

    if timed_out {
        return Err("listener_probe.deadline_exceeded".to_string());
    }
    if output_exceeded.load(Ordering::Acquire) {
        return Err("listener_probe.output_limit_exceeded".to_string());
    }
    Ok(BoundedChildOutput {
        status,
        stdout,
        stderr,
    })
}

#[cfg(not(windows))]
fn read_bounded(
    mut reader: impl Read,
    max_bytes: usize,
    exceeded: Arc<AtomicBool>,
) -> io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(max_bytes.min(8192));
    let mut chunk = [0u8; 8192];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(count) > max_bytes {
            exceeded.store(true, Ordering::Release);
            return Ok(output);
        }
        output.extend_from_slice(&chunk[..count]);
    }
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
#[derive(Clone, Copy)]
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
#[derive(Clone, Copy)]
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
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> *mut c_void;
    fn Thread32First(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
    fn Thread32Next(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
    fn OpenThread(desired_access: u32, inherit_handle: i32, thread_id: u32) -> *mut c_void;
    fn ResumeThread(thread: *mut c_void) -> u32;
    fn GetActiveProcessorCount(group_number: u16) -> u32;
}

/// Deadline-bound listener inventory used by teardown settlement. The
/// non-Windows `lsof` adapter cannot cancel `Command::output`, so it is
/// deliberately unavailable at this authority boundary instead of returning
/// while an unowned helper may still be running.
pub(crate) fn snapshot_listener_pids_until(
    ports: &[u16],
    absolute_deadline: std::time::Instant,
) -> Result<HashMap<u16, u32>, String> {
    check_listener_snapshot_deadline(absolute_deadline, "listener snapshot admission")?;
    if ports.len() > MAX_LISTENER_PORT_BATCH {
        return Err(format!(
            "listener snapshot exceeds {MAX_LISTENER_PORT_BATCH} ports"
        ));
    }
    if ports.is_empty() {
        return Ok(HashMap::new());
    }

    #[cfg(windows)]
    let listeners = snapshot_listener_pids_windows_until(ports, absolute_deadline)?;

    #[cfg(not(windows))]
    let listeners = {
        let _ = ports;
        return Err(
            "deadline-bound listener reconciliation is unavailable on this platform".to_string(),
        );
    };

    check_listener_snapshot_deadline(absolute_deadline, "listener snapshot completion")?;
    Ok(listeners)
}

fn check_listener_snapshot_deadline(
    absolute_deadline: std::time::Instant,
    context: &str,
) -> Result<(), String> {
    if std::time::Instant::now() >= absolute_deadline {
        Err(format!("{context} exceeded its absolute deadline"))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
    fn TerminateProcess(handle: *mut c_void, exit_code: u32) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn ReadProcessMemory(
        process: *mut c_void,
        base_address: *const c_void,
        buffer: *mut c_void,
        size: usize,
        bytes_read: *mut usize,
    ) -> i32;
    fn IsWow64Process(process: *mut c_void, wow64_process: *mut i32) -> i32;
}

#[cfg(all(windows, target_pointer_width = "64"))]
#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationProcess(
        process: *mut c_void,
        information_class: u32,
        information: *mut c_void,
        information_length: u32,
        return_length: *mut u32,
    ) -> i32;
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

/// Root-process working directory, read from the target process's PEB.
///
/// This is the only rung that observes `cmd.exe` and stock PowerShell changing
/// directory: neither emits an OSC 7 cwd report under this manager, and both
/// update `ProcessParameters->CurrentDirectory.DosPath` through
/// `SetCurrentDirectory`. Every cross-process read is length-checked, and any
/// failure -- exited process, denied access, short read, WOW64 target, or a
/// path that is not an existing absolute directory -- returns `None` rather
/// than a guess, because a wrong answer here becomes a durable terminal fact.
#[cfg(all(windows, target_pointer_width = "64"))]
pub fn root_process_cwd(pid: u32) -> Option<PathBuf> {
    // x64 / arm64 layout. PEB->ProcessParameters, then
    // RTL_USER_PROCESS_PARAMETERS->CurrentDirectory.DosPath (a UNICODE_STRING).
    const PROCESS_BASIC_INFORMATION_CLASS: u32 = 0;
    const PROCESS_BASIC_INFORMATION_SIZE: usize = 48;
    const PEB_BASE_ADDRESS_OFFSET: usize = 0x08;
    const PEB_PROCESS_PARAMETERS_OFFSET: usize = 0x20;
    const PARAMETERS_CURRENT_DIRECTORY_OFFSET: usize = 0x38;
    const UNICODE_STRING_SIZE: usize = 16;
    const UNICODE_STRING_BUFFER_OFFSET: usize = 8;
    // UNICODE_STRING.Length is a byte count held in a u16.
    const MAX_UNICODE_STRING_BYTES: usize = u16::MAX as usize;

    if pid == 0 {
        return None;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let process = ProcessHandleGuard(handle);

    // A 32-bit target keeps its live directory in the 32-bit PEB; the 64-bit
    // one read here can be stale, and a stale directory is precisely the wrong
    // answer to publish as a fact.
    let mut is_wow64: i32 = 0;
    if unsafe { IsWow64Process(process.0, &mut is_wow64) } == 0 || is_wow64 != 0 {
        return None;
    }

    let mut basic = [0u8; PROCESS_BASIC_INFORMATION_SIZE];
    let status = unsafe {
        NtQueryInformationProcess(
            process.0,
            PROCESS_BASIC_INFORMATION_CLASS,
            basic.as_mut_ptr().cast(),
            PROCESS_BASIC_INFORMATION_SIZE as u32,
            std::ptr::null_mut(),
        )
    };
    if status != 0 {
        return None;
    }

    let peb = pointer_at(&basic, PEB_BASE_ADDRESS_OFFSET)?;
    let parameters =
        read_remote_pointer(process.0, peb.checked_add(PEB_PROCESS_PARAMETERS_OFFSET)?)?;
    let current_directory = parameters.checked_add(PARAMETERS_CURRENT_DIRECTORY_OFFSET)?;

    let mut unicode_string = [0u8; UNICODE_STRING_SIZE];
    read_remote(process.0, current_directory, &mut unicode_string)?;
    let length = u16::from_ne_bytes([unicode_string[0], unicode_string[1]]) as usize;
    let buffer = pointer_at(&unicode_string, UNICODE_STRING_BUFFER_OFFSET)?;
    if length == 0 || length % 2 != 0 || length > MAX_UNICODE_STRING_BYTES {
        return None;
    }

    let mut bytes = vec![0u8; length];
    read_remote(process.0, buffer, &mut bytes)?;
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
        .collect();
    let path = PathBuf::from(String::from_utf16(&units).ok()?);
    (path.is_absolute() && path.is_dir()).then_some(path)
}

/// Non-Windows and 32-bit hosts have no PEB rung. The cwd ladder falls through
/// to the shell's own OSC 7 report and then to the launch directory.
#[cfg(not(all(windows, target_pointer_width = "64")))]
pub fn root_process_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

#[cfg(all(windows, target_pointer_width = "64"))]
const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
#[cfg(all(windows, target_pointer_width = "64"))]
const PROCESS_VM_READ: u32 = 0x0010;

#[cfg(all(windows, target_pointer_width = "64"))]
struct ProcessHandleGuard(*mut c_void);

#[cfg(all(windows, target_pointer_width = "64"))]
impl Drop for ProcessHandleGuard {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Read one pointer-sized field out of an already-copied local buffer.
#[cfg(all(windows, target_pointer_width = "64"))]
fn pointer_at(buffer: &[u8], offset: usize) -> Option<usize> {
    const POINTER_BYTES: usize = std::mem::size_of::<usize>();
    let slice = buffer.get(offset..offset.checked_add(POINTER_BYTES)?)?;
    let mut raw = [0u8; POINTER_BYTES];
    raw.copy_from_slice(slice);
    Some(usize::from_ne_bytes(raw))
}

/// Fill `buffer` from the target process, or report failure. A partial read is
/// a failure: half a path is not a shorter path.
#[cfg(all(windows, target_pointer_width = "64"))]
fn read_remote(process: *mut c_void, address: usize, buffer: &mut [u8]) -> Option<()> {
    if address == 0 || buffer.is_empty() {
        return None;
    }
    let mut read: usize = 0;
    let ok = unsafe {
        ReadProcessMemory(
            process,
            address as *const c_void,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut read,
        )
    };
    (ok != 0 && read == buffer.len()).then_some(())
}

#[cfg(all(windows, target_pointer_width = "64"))]
fn read_remote_pointer(process: *mut c_void, address: usize) -> Option<usize> {
    const POINTER_BYTES: usize = std::mem::size_of::<usize>();
    let mut raw = [0u8; POINTER_BYTES];
    read_remote(process, address, &mut raw)?;
    Some(usize::from_ne_bytes(raw))
}

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

/// Test-process cleanup only. Production termination must flow through a
/// retained Job/fence or another sealed pre-authority capability.
#[cfg(test)]
pub(crate) fn kill_process_tree(pid: u32) -> Result<(), String> {
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

fn claim_suspended_process_with<Job, Attach>(
    pid: u32,
    attach: Attach,
) -> Result<Option<Job>, String>
where
    Attach: FnOnce(u32) -> Result<Option<Job>, String>,
{
    attach(pid)
}

#[cfg(test)]
fn resume_suspended_process_with<Resume>(pid: u32, resume: Resume) -> Result<(), String>
where
    Resume: FnOnce(u32) -> Result<(), String>,
{
    resume(pid)
}

/// Claims a process created with [`MANAGED_PROCESS_CREATION_FLAGS`] before
/// allowing any of its code to execute. On Windows the returned job must stay
/// alive for as long as the process tree is owned. The caller must explicitly
/// resume the process after all identity checks have completed.
pub fn claim_suspended_process(pid: u32) -> Result<Option<ManagedProcessJob>, String> {
    #[cfg(windows)]
    {
        claim_suspended_process_with(pid, attach_process_to_managed_job)
    }

    #[cfg(not(windows))]
    {
        attach_process_to_managed_job(pid)
    }
}

#[cfg(windows)]
pub fn resume_suspended_process(pid: u32) -> Result<(), String> {
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

/// Return the platform-native executable path without assuming Linux `/proc`.
/// sysinfo uses `proc_pidpath` on macOS and the native process APIs elsewhere.
pub fn capture_process_executable(pid: u32) -> Option<PathBuf> {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    system
        .process(sysinfo::Pid::from_u32(pid))
        .and_then(|process| process.exe().map(Path::to_path_buf))
}

/// Capture a process creation token with more precision than sysinfo's
/// display-oriented Unix seconds. Linux exposes the kernel start tick in
/// `/proc`; other Unix platforms retain the verified sysinfo timestamp while
/// using their native executable lookup above (macOS has no `/proc` tree).
pub fn capture_process_creation_time_100ns(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, fields) = stat.rsplit_once(") ")?;
        let start_ticks = fields.split_whitespace().nth(19)?.parse::<u64>().ok()?;
        // `_SC_CLK_TCK` is 2 on Linux. Keep the FFI local so the application
        // does not need to expose libc as a public dependency.
        unsafe extern "C" {
            fn sysconf(name: i32) -> i64;
        }
        let ticks_per_second = unsafe { sysconf(2) };
        if ticks_per_second <= 0 {
            return None;
        }
        return start_ticks
            .checked_mul(10_000_000)
            .and_then(|ticks| ticks.checked_div(ticks_per_second as u64));
    }

    #[cfg(not(target_os = "linux"))]
    {
        capture_process_identity(pid)?
            .started_at_unix_secs
            .checked_mul(10_000_000)
    }
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
        let pid_text = pid.to_string();
        let output = run_bounded_command("ps", &["-p", pid_text.as_str(), "-o", "comm="])?;
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
    use super::{
        claim_suspended_process_with, resume_suspended_process_with, snapshot_listener_pids,
        snapshot_listener_pids_until, terminate_owned_process_group_with, MAX_LISTENER_PORT_BATCH,
    };
    use std::cell::RefCell;
    use std::time::Duration;

    #[test]
    fn listener_snapshot_rejects_unbounded_port_batches_before_allocation() {
        let ports = vec![80; MAX_LISTENER_PORT_BATCH + 1];
        assert!(snapshot_listener_pids(&ports).is_err());
    }

    #[test]
    fn teardown_listener_snapshot_rejects_expired_deadline_before_lookup() {
        let error = snapshot_listener_pids_until(&[80], std::time::Instant::now())
            .expect_err("an expired teardown lookup must fail before platform I/O");
        assert!(error.contains("absolute deadline"));
    }

    /// The PEB reader must be proven able to SEE a directory it is supposed to
    /// report before any caller trusts a `None` from it. Reading this process's
    /// own PEB is the only subject whose answer is independently known.
    #[cfg(all(windows, target_pointer_width = "64"))]
    #[test]
    fn root_process_cwd_reads_this_process_current_directory() {
        let expected = std::env::current_dir()
            .expect("test cwd")
            .canonicalize()
            .expect("canonical test cwd");
        let observed = super::root_process_cwd(std::process::id())
            .expect("the PEB reader must see this process's own current directory");
        assert!(observed.is_absolute());
        assert_eq!(
            observed.canonicalize().expect("canonical observed cwd"),
            expected
        );
    }

    /// A pid that cannot be opened is `None`, never a stale or borrowed path.
    #[cfg(windows)]
    #[test]
    fn root_process_cwd_reports_nothing_for_an_impossible_pid() {
        assert_eq!(super::root_process_cwd(0), None);
    }

    #[cfg(windows)]
    use super::{for_each_windows_tcp_row, windows_port, MibTcpRowOwnerPid};

    #[cfg(windows)]
    #[test]
    fn windows_port_decodes_network_order_port() {
        assert_eq!(windows_port(0x5000), 80);
        assert_eq!(windows_port(0x3614), 5174);
    }

    #[cfg(windows)]
    #[test]
    fn windows_tcp_rows_reject_truncated_header_and_claimed_rows() {
        let mut visited = 0usize;
        assert!(
            for_each_windows_tcp_row::<MibTcpRowOwnerPid, _>(&[0, 0, 0], |_| {
                visited += 1;
                Ok(())
            })
            .is_err()
        );

        let mut truncated = vec![0u8; std::mem::size_of::<u32>()];
        truncated[..4].copy_from_slice(&2u32.to_ne_bytes());
        assert!(
            for_each_windows_tcp_row::<MibTcpRowOwnerPid, _>(&truncated, |_| {
                visited += 1;
                Ok(())
            })
            .is_err()
        );
        assert_eq!(visited, 0, "malformed tables must not expose any row");
    }

    #[cfg(windows)]
    #[test]
    fn windows_tcp_rows_are_read_safely_from_unaligned_storage() {
        let row = MibTcpRowOwnerPid {
            dw_state: 2,
            dw_local_addr: 0,
            dw_local_port: 0x5000,
            dw_remote_addr: 0,
            dw_remote_port: 0,
            dw_owning_pid: 42,
        };
        let row_bytes = unsafe {
            std::slice::from_raw_parts(
                std::ptr::addr_of!(row).cast::<u8>(),
                std::mem::size_of::<MibTcpRowOwnerPid>(),
            )
        };
        let table_len = std::mem::size_of::<u32>() + row_bytes.len();
        let mut storage = vec![0u8; table_len + 1];
        let table = &mut storage[1..];
        table[..4].copy_from_slice(&1u32.to_ne_bytes());
        table[4..].copy_from_slice(row_bytes);

        let mut observed = Vec::new();
        for_each_windows_tcp_row::<MibTcpRowOwnerPid, _>(table, |row| {
            observed.push((row.dw_local_port, row.dw_owning_pid));
            Ok(())
        })
        .expect("unaligned API storage must be parsed with unaligned reads");
        assert_eq!(observed, [(0x5000, 42)]);
    }

    #[test]
    fn suspended_process_claim_only_assigns_job_before_explicit_resume() {
        let events = RefCell::new(Vec::new());

        let job = claim_suspended_process_with(42, |pid| {
            events.borrow_mut().push(("assign", pid));
            Ok(Some("job"))
        })
        .unwrap();

        assert_eq!(job, Some("job"));
        assert_eq!(events.into_inner(), [("assign", 42)]);
    }

    #[test]
    fn logical_processor_count_is_never_zero() {
        assert!(super::logical_processor_count() > 0);
    }

    #[test]
    fn explicit_resume_failure_is_reported_without_claiming_again() {
        let result = resume_suspended_process_with(42, |_| Err("resume failed".to_string()));

        assert_eq!(result.unwrap_err(), "resume failed");
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
        super::resume_suspended_process(child.id()).unwrap();
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
    use super::{parse_lsof_listener_port, run_bounded_command};

    #[test]
    fn listener_probe_uses_a_pinned_lsof_path() {
        let path = super::trusted_lsof_program();
        assert!(path.starts_with('/'));
        assert_eq!(
            std::path::Path::new(path)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("lsof")
        );
    }

    #[test]
    fn parse_lsof_listener_port_handles_localhost_and_ipv6() {
        assert_eq!(parse_lsof_listener_port("127.0.0.1:3000"), Some(3000));
        assert_eq!(parse_lsof_listener_port("[::1]:5174"), Some(5174));
        assert_eq!(parse_lsof_listener_port("*:8080 (LISTEN)"), Some(8080));
    }

    #[test]
    fn lsof_diagnostics_are_fixed_and_path_free() {
        let error = match run_bounded_command("/definitely/missing/lsof", &[]) {
            Ok(_) => panic!("missing trusted executable must fail"),
            Err(error) => error,
        };
        assert_eq!(error, "listener_probe.spawn_failed");
        assert!(!error.contains("/definitely"));
    }

    #[test]
    fn lsof_nonzero_stderr_is_not_forwarded() {
        let error = match run_bounded_command(
            "/bin/sh",
            &["-c", "printf 'secret-path-and-diagnostics' >&2; exit 7"],
        ) {
            Ok(_) => panic!("nonzero command must fail at the listener boundary"),
            Err(error) => error,
        };
        assert_eq!(error, "listener_probe.command_failed");
        assert!(!error.contains("secret-path"));
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

#[cfg(all(not(windows), test))]
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
    let signal = match signal {
        "TERM" => libc::SIGTERM,
        "KILL" => libc::SIGKILL,
        other => return Err(format!("Unsupported Unix signal {other}")),
    };
    let target = target
        .parse::<libc::pid_t>()
        .map_err(|error| format!("Invalid Unix process target {target}: {error}"))?;
    if target == 0 {
        return Err("Unix process target cannot be zero".to_string());
    }
    if unsafe { libc::kill(target, signal) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

#[cfg(not(windows))]
fn unix_process_group_exists(target: &str) -> bool {
    let Ok(target) = target.parse::<libc::pid_t>() else {
        return false;
    };
    if target == 0 {
        return false;
    }
    let result = unsafe { libc::kill(target, 0) };
    result == 0
        || (result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

#[cfg(not(windows))]
#[cfg(test)]
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
