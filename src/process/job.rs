//! Windows Job Object ownership for managed process trees.

#[cfg(windows)]
use std::ffi::c_void;
#[cfg(windows)]
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
#[cfg(windows)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(windows)]
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use std::thread::JoinHandle;

#[cfg(windows)]
use crate::process::identity::{ManagedProcessId, ManagedProcessIdentity};
use crate::process::registry::{
    JobCompletionEvent, JobCompletionMessage, JobMemberInfo, ManagedProcessFence,
};

/// Capability held only by the managed Job completion receiver.
///
/// The registry requires this token before it will mark a completion message
/// as receiver-owned. No production caller outside this module can construct
/// one, so a fence/event pair supplied by another adapter remains untrusted.
pub(crate) struct CompletionReceiverToken(());

impl CompletionReceiverToken {
    fn issue() -> Self {
        Self(())
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
    fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> *mut c_void;
    fn SetInformationJobObject(
        job: *mut c_void,
        job_object_info_class: u32,
        job_object_info: *mut c_void,
        job_object_info_length: u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
    fn TerminateJobObject(job: *mut c_void, exit_code: u32) -> i32;
    fn QueryInformationJobObject(
        job: *mut c_void,
        job_object_info_class: u32,
        job_object_info: *mut c_void,
        job_object_info_length: u32,
        return_length: *mut u32,
    ) -> i32;
    fn CreateIoCompletionPort(
        file_handle: *mut c_void,
        existing_completion_port: *mut c_void,
        completion_key: usize,
        number_of_concurrent_threads: u32,
    ) -> *mut c_void;
    fn GetQueuedCompletionStatus(
        completion_port: *mut c_void,
        number_of_bytes_transferred: *mut u32,
        completion_key: *mut usize,
        overlapped: *mut *mut c_void,
        milliseconds: u32,
    ) -> i32;
    fn PostQueuedCompletionStatus(
        completion_port: *mut c_void,
        number_of_bytes_transferred: u32,
        completion_key: usize,
        overlapped: *mut c_void,
    ) -> i32;
    fn GetProcessTimes(
        process: *mut c_void,
        creation_time: *mut FileTime,
        exit_time: *mut FileTime,
        kernel_time: *mut FileTime,
        user_time: *mut FileTime,
    ) -> i32;
    fn QueryFullProcessImageNameW(
        process: *mut c_void,
        flags: u32,
        executable_name: *mut u16,
        size: *mut u32,
    ) -> i32;
    fn IsProcessInJob(process: *mut c_void, job: *mut c_void, result: *mut i32) -> i32;
}

#[cfg(windows)]
const PROCESS_TERMINATE: u32 = 0x0001;
#[cfg(windows)]
const PROCESS_SET_QUOTA: u32 = 0x0100;
#[cfg(windows)]
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
#[cfg(windows)]
const JOB_OBJECT_BASIC_PROCESS_ID_LIST_CLASS: u32 = 3;
#[cfg(windows)]
const JOB_OBJECT_ASSOCIATE_COMPLETION_PORT_INFORMATION_CLASS: u32 = 7;
#[cfg(windows)]
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: u32 = 9;
#[cfg(windows)]
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x00002000;
#[cfg(windows)]
const ERROR_MORE_DATA: i32 = 234;
#[cfg(windows)]
const MAX_JOB_PROCESS_ID_CAPACITY: usize = 16_384;
#[cfg(windows)]
const MAX_COMPLETION_MESSAGES: usize = 4_096;
#[cfg(windows)]
const COMPLETION_OVERFLOW_DETAIL: &str =
    "managed Job completion mailbox overflow; authoritative reconciliation required";
#[cfg(windows)]
const JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO: u32 = 4;
#[cfg(windows)]
const JOB_OBJECT_MSG_NEW_PROCESS: u32 = 6;
#[cfg(windows)]
const JOB_OBJECT_MSG_EXIT_PROCESS: u32 = 7;
#[cfg(windows)]
const JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS: u32 = 8;
#[cfg(windows)]
const SHUTDOWN_MESSAGE: u32 = u32::MAX;
#[cfg(windows)]
const SHUTDOWN_COMPLETION_KEY: usize = 0;
#[cfg(windows)]
static NEXT_COMPLETION_KEY: AtomicUsize = AtomicUsize::new(1);

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct FileTime {
    low_date_time: u32,
    high_date_time: u32,
}

#[cfg(windows)]
#[repr(C)]
struct JobObjectAssociateCompletionPort {
    completion_key: *mut c_void,
    completion_port: *mut c_void,
}

#[cfg(windows)]
#[derive(Debug, Default)]
struct CompletionMailbox {
    messages: std::collections::VecDeque<JobCompletionMessage>,
    overflowed: bool,
}

#[cfg(windows)]
impl CompletionMailbox {
    fn push(&mut self, receiver: &CompletionReceiverToken, message: JobCompletionMessage) {
        let is_active_zero = matches!(message.event(), JobCompletionEvent::ActiveProcessZero);
        let has_active_zero = self
            .messages
            .iter()
            .any(|queued| matches!(queued.event(), JobCompletionEvent::ActiveProcessZero));
        if is_active_zero && has_active_zero {
            return;
        }

        if self.overflowed {
            if is_active_zero {
                self.messages.push_back(message);
            }
            return;
        }
        if self.messages.len() < MAX_COMPLETION_MESSAGES {
            self.messages.push_back(message);
            return;
        }

        let preserved_zero = if is_active_zero {
            Some(message.clone())
        } else {
            self.messages
                .iter()
                .find(|queued| matches!(queued.event(), JobCompletionEvent::ActiveProcessZero))
                .cloned()
        };
        self.messages.clear();
        self.messages
            .push_back(JobCompletionMessage::from_completion_receiver(
                receiver,
                message.fence().clone(),
                JobCompletionEvent::MonitorFailed {
                    detail: COMPLETION_OVERFLOW_DETAIL.to_string(),
                },
            ));
        if let Some(active_zero) = preserved_zero {
            self.messages.push_back(active_zero);
        }
        self.overflowed = true;
    }

    fn drain(&mut self) -> Vec<JobCompletionMessage> {
        self.overflowed = false;
        self.messages.drain(..).collect()
    }
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct JobObjectBasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct IoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct JobObjectExtendedLimitInformation {
    basic_limit_information: JobObjectBasicLimitInformation,
    io_info: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

/// An exclusively owned kill-on-close Job Object handle.
///
/// The handle is created non-inheritable and is intentionally not cloneable.
/// Dropping the last owner closes the Job Object and terminates its members.
#[cfg(windows)]
#[derive(Debug)]
pub struct ManagedProcessJob {
    handle: Option<OwnedHandle>,
    completion_port: Option<OwnedHandle>,
    completion_key: usize,
    completion_fence: Option<ManagedProcessFence>,
    completion_mailbox: Arc<Mutex<CompletionMailbox>>,
    completion_listener: Option<JoinHandle<()>>,
}

/// Non-Windows marker type returned only behind `Option::None`.
#[cfg(not(windows))]
#[derive(Debug)]
pub struct ManagedProcessJob {
    _unsupported: (),
}

impl ManagedProcessJob {
    /// Creates an empty, non-inheritable Job Object whose final handle closes
    /// every process in the tree.
    pub fn create() -> Result<Option<Self>, String> {
        #[cfg(windows)]
        {
            create_windows_job().map(Some)
        }

        #[cfg(not(windows))]
        {
            Ok(None)
        }
    }

    /// Returns the active process IDs currently assigned to this Job Object.
    ///
    /// On non-Windows platforms this always returns an empty list.
    pub fn active_process_ids(&self) -> Result<Vec<u32>, String> {
        #[cfg(windows)]
        {
            query_job_active_process_ids(self.raw_job_handle())
        }

        #[cfg(not(windows))]
        {
            Ok(Vec::new())
        }
    }

    /// Explicitly terminates every current member of this owned Job Object.
    ///
    /// This is intentionally Job-scoped rather than PID-scoped. The Job,
    /// completion port, and listener remain owned by this value until the
    /// caller has observed the exact completion fence and authoritative empty
    /// membership before releasing it.
    pub fn terminate_tree(&self) -> Result<(), String> {
        #[cfg(windows)]
        {
            if unsafe { TerminateJobObject(self.raw_job_handle(), 1) } == 0 {
                return Err(format!(
                    "TerminateJobObject failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        }

        #[cfg(not(windows))]
        {
            Err("Job Object termination is unavailable off Windows".to_string())
        }
    }

    pub fn inspect_process(&self, pid: u32) -> Result<JobMemberInfo, String> {
        #[cfg(windows)]
        {
            inspect_windows_process(self.raw_job_handle(), pid)
        }

        #[cfg(not(windows))]
        {
            let _ = pid;
            Err("process inspection is unavailable off Windows".to_string())
        }
    }

    pub fn bind_completion_fence(&mut self, fence: ManagedProcessFence) -> Result<(), String> {
        #[cfg(windows)]
        {
            self.bind_windows_completion_fence(fence)
        }

        #[cfg(not(windows))]
        {
            let _ = fence;
            Ok(())
        }
    }

    pub fn drain_completion_messages(&self) -> Vec<JobCompletionMessage> {
        #[cfg(windows)]
        {
            self.completion_mailbox
                .lock()
                .expect("managed Job completion mailbox poisoned")
                .drain()
        }

        #[cfg(not(windows))]
        {
            Vec::new()
        }
    }

    #[cfg(windows)]
    pub(crate) fn borrowed_handle(&self) -> BorrowedHandle<'_> {
        self.handle
            .as_ref()
            .expect("managed Job handle exists until drop")
            .as_handle()
    }

    #[cfg(windows)]
    fn raw_job_handle(&self) -> *mut c_void {
        self.handle
            .as_ref()
            .expect("managed Job handle exists until drop")
            .as_raw_handle()
    }

    #[cfg(windows)]
    fn bind_windows_completion_fence(&mut self, fence: ManagedProcessFence) -> Result<(), String> {
        if let Some(bound) = self.completion_fence.as_ref() {
            return if *bound == fence {
                Ok(())
            } else {
                Err(format!(
                    "Job completion port is already bound to {:?}, not {:?}",
                    bound, fence
                ))
            };
        }
        let port = self
            .completion_port
            .as_ref()
            .ok_or_else(|| "managed Job completion port is closed".to_string())?
            .as_raw_handle() as usize;
        let completion_key = self.completion_key;
        let mailbox = Arc::clone(&self.completion_mailbox);
        let listener_fence = fence.clone();
        let receiver = CompletionReceiverToken::issue();
        let listener = std::thread::Builder::new()
            .name(format!(
                "devmanager-job-{}-{}",
                fence.resource().resource_id,
                fence.resource().runtime_generation
            ))
            .spawn(move || {
                completion_listener(port, completion_key, listener_fence, mailbox, receiver)
            })
            .map_err(|error| format!("could not spawn Job completion listener: {error}"))?;
        self.completion_fence = Some(fence);
        self.completion_listener = Some(listener);
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for ManagedProcessJob {
    fn drop(&mut self) {
        if let Some(listener) = self.completion_listener.take() {
            let posted = self.completion_port.as_ref().is_some_and(|port| unsafe {
                PostQueuedCompletionStatus(
                    port.as_raw_handle(),
                    SHUTDOWN_MESSAGE,
                    SHUTDOWN_COMPLETION_KEY,
                    std::ptr::null_mut(),
                ) != 0
            });
            if !posted {
                drop(self.completion_port.take());
            }
            let _ = listener.join();
        }
        drop(self.completion_port.take());
        drop(self.handle.take());
    }
}

/// Assigns an existing process to a new kill-on-close Job Object.
///
/// New managed launchers must still create the process suspended before calling
/// this compatibility boundary. The Phase 3 launcher slice will replace PID
/// reopening with the primary process handle returned by process creation.
pub fn attach_process_to_managed_job(pid: u32) -> Result<Option<ManagedProcessJob>, String> {
    #[cfg(windows)]
    {
        attach_process_to_windows_job(pid).map(Some)
    }

    #[cfg(not(windows))]
    {
        let _ = pid;
        Ok(None)
    }
}

#[cfg(windows)]
fn query_job_active_process_ids(job: *mut c_void) -> Result<Vec<u32>, String> {
    if job.is_null() {
        return Err("managed job handle is null".to_string());
    }

    // JOBOBJECT_BASIC_PROCESS_ID_LIST:
    //   DWORD NumberOfAssignedProcesses;
    //   DWORD NumberOfProcessIdsInList;
    //   ULONG_PTR ProcessIdList[ANYSIZE_ARRAY];
    let header_bytes = std::mem::size_of::<u32>()
        .checked_mul(2)
        .ok_or_else(|| "job process list header size overflow".to_string())?;
    let mut capacity = 16usize;

    loop {
        if capacity > MAX_JOB_PROCESS_ID_CAPACITY {
            return Err(format!(
                "QueryInformationJobObject process list exceeds {MAX_JOB_PROCESS_ID_CAPACITY} members"
            ));
        }
        let list_bytes = capacity
            .checked_mul(std::mem::size_of::<usize>())
            .ok_or_else(|| "job process list size overflow".to_string())?;
        let total_bytes = header_bytes
            .checked_add(list_bytes)
            .ok_or_else(|| "job process list buffer size overflow".to_string())?;
        if total_bytes > u32::MAX as usize {
            return Err(
                "job process list buffer exceeds QueryInformationJobObject limit".to_string(),
            );
        }
        let align = std::mem::align_of::<usize>().max(std::mem::align_of::<u32>());
        let storage_len = total_bytes
            .checked_add(align)
            .ok_or_else(|| "job process list storage size overflow".to_string())?;
        let mut storage = vec![0u8; storage_len];
        let offset = storage.as_ptr().align_offset(align);
        let end = offset
            .checked_add(total_bytes)
            .ok_or_else(|| "job process list aligned range overflow".to_string())?;
        let buffer = storage
            .get_mut(offset..end)
            .ok_or_else(|| "job process list aligned buffer out of range".to_string())?;

        let mut return_length = 0u32;
        let ok = unsafe {
            QueryInformationJobObject(
                job,
                JOB_OBJECT_BASIC_PROCESS_ID_LIST_CLASS,
                buffer.as_mut_ptr() as *mut c_void,
                total_bytes as u32,
                &mut return_length,
            )
        };
        if ok == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_MORE_DATA) {
                let assigned = u32::from_ne_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
                let needed = (assigned as usize).max(1);
                if needed > MAX_JOB_PROCESS_ID_CAPACITY || capacity >= MAX_JOB_PROCESS_ID_CAPACITY {
                    return Err(format!(
                        "QueryInformationJobObject process list exceeds {MAX_JOB_PROCESS_ID_CAPACITY} members"
                    ));
                }
                let next_capacity = needed
                    .max(capacity.saturating_mul(2))
                    .min(MAX_JOB_PROCESS_ID_CAPACITY);
                if next_capacity <= capacity {
                    return Err(format!(
                        "QueryInformationJobObject returned ERROR_MORE_DATA but capacity {capacity} cannot grow"
                    ));
                }
                capacity = next_capacity;
                continue;
            }
            return Err(format!("QueryInformationJobObject failed: {error}"));
        }

        let count = u32::from_ne_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;
        if count > capacity {
            if count > MAX_JOB_PROCESS_ID_CAPACITY {
                return Err(format!(
                    "QueryInformationJobObject returned {count} members (max {MAX_JOB_PROCESS_ID_CAPACITY})"
                ));
            }
            capacity = count;
            continue;
        }

        let list_ptr = unsafe { buffer.as_ptr().add(header_bytes) as *const usize };
        let mut process_ids = Vec::with_capacity(count);
        for index in 0..count {
            let pid = unsafe { *list_ptr.add(index) } as u32;
            if pid != 0 {
                process_ids.push(pid);
            }
        }
        process_ids.sort_unstable();
        process_ids.dedup();
        return Ok(process_ids);
    }
}

#[cfg(windows)]
fn create_windows_job() -> Result<ManagedProcessJob, String> {
    unsafe {
        let raw_job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
        if raw_job.is_null() {
            return Err(format!(
                "CreateJobObjectW failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // Null SECURITY_ATTRIBUTES make this handle non-inheritable. OwnedHandle
        // closes exactly this sole owner on every subsequent error path.
        let job = OwnedHandle::from_raw_handle(raw_job);

        let raw_completion_port =
            CreateIoCompletionPort((-1isize) as *mut c_void, std::ptr::null_mut(), 0, 1);
        if raw_completion_port.is_null() {
            return Err(format!(
                "CreateIoCompletionPort failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let completion_port = OwnedHandle::from_raw_handle(raw_completion_port);
        let completion_key = next_completion_key();
        let mut association = JobObjectAssociateCompletionPort {
            completion_key: completion_key as *mut c_void,
            completion_port: completion_port.as_raw_handle(),
        };
        if SetInformationJobObject(
            job.as_raw_handle(),
            JOB_OBJECT_ASSOCIATE_COMPLETION_PORT_INFORMATION_CLASS,
            &mut association as *mut _ as *mut c_void,
            std::mem::size_of::<JobObjectAssociateCompletionPort>() as u32,
        ) == 0
        {
            return Err(format!(
                "SetInformationJobObject completion port association failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        let mut limits = JobObjectExtendedLimitInformation::default();
        limits.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set_ok = SetInformationJobObject(
            job.as_raw_handle(),
            JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
            &mut limits as *mut _ as *mut c_void,
            std::mem::size_of::<JobObjectExtendedLimitInformation>() as u32,
        );
        if set_ok == 0 {
            return Err(format!(
                "SetInformationJobObject failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        Ok(ManagedProcessJob {
            handle: Some(job),
            completion_port: Some(completion_port),
            completion_key,
            completion_fence: None,
            completion_mailbox: Arc::new(Mutex::new(CompletionMailbox::default())),
            completion_listener: None,
        })
    }
}

#[cfg(windows)]
fn attach_process_to_windows_job(pid: u32) -> Result<ManagedProcessJob, String> {
    unsafe {
        let job = create_windows_job()?;

        let raw_process = OpenProcess(PROCESS_TERMINATE | PROCESS_SET_QUOTA, 0, pid);
        if raw_process.is_null() {
            return Err(format!(
                "OpenProcess({pid}) for job assignment failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // inherit_handle=0 above makes the temporary process handle
        // non-inheritable as well.
        let process = OwnedHandle::from_raw_handle(raw_process);

        if AssignProcessToJobObject(job.raw_job_handle(), process.as_raw_handle()) == 0 {
            return Err(format!(
                "AssignProcessToJobObject({pid}) failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        Ok(job)
    }
}

#[cfg(windows)]
fn next_completion_key() -> usize {
    loop {
        let key = NEXT_COMPLETION_KEY.fetch_add(1, Ordering::Relaxed);
        if key != SHUTDOWN_COMPLETION_KEY {
            return key;
        }
    }
}

#[cfg(windows)]
fn completion_listener(
    completion_port_value: usize,
    expected_completion_key: usize,
    fence: ManagedProcessFence,
    mailbox: Arc<Mutex<CompletionMailbox>>,
    receiver: CompletionReceiverToken,
) {
    let completion_port = completion_port_value as *mut c_void;
    loop {
        let mut message_id = 0u32;
        let mut completion_key = 0usize;
        let mut process_value = std::ptr::null_mut();
        let ok = unsafe {
            GetQueuedCompletionStatus(
                completion_port,
                &mut message_id,
                &mut completion_key,
                &mut process_value,
                u32::MAX,
            )
        };
        if completion_key == SHUTDOWN_COMPLETION_KEY
            && message_id == SHUTDOWN_MESSAGE
            && process_value.is_null()
        {
            break;
        }
        if ok == 0 {
            let detail = std::io::Error::last_os_error().to_string();
            mailbox
                .lock()
                .expect("managed Job completion mailbox poisoned")
                .push(
                    &receiver,
                    JobCompletionMessage::from_completion_receiver(
                        &receiver,
                        fence.clone(),
                        JobCompletionEvent::MonitorFailed { detail },
                    ),
                );
            break;
        }
        if completion_key != expected_completion_key {
            continue;
        }

        // For Job completion packets, lpOverlapped is a PID-shaped value.
        // It is never an OVERLAPPED pointer and must never be dereferenced.
        let pid = usize::from_ne_bytes((process_value as usize).to_ne_bytes()) as u32;
        let event = match message_id {
            JOB_OBJECT_MSG_NEW_PROCESS => JobCompletionEvent::NewProcess { pid },
            JOB_OBJECT_MSG_EXIT_PROCESS => JobCompletionEvent::ExitProcess { pid },
            JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS => JobCompletionEvent::AbnormalExitProcess { pid },
            JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO => JobCompletionEvent::ActiveProcessZero,
            _ => JobCompletionEvent::Limit {
                message_id,
                pid: (pid != 0).then_some(pid),
            },
        };
        mailbox
            .lock()
            .expect("managed Job completion mailbox poisoned")
            .push(
                &receiver,
                JobCompletionMessage::from_completion_receiver(&receiver, fence.clone(), event),
            );
    }
}

#[cfg(windows)]
fn inspect_windows_process(job: *mut c_void, pid: u32) -> Result<JobMemberInfo, String> {
    unsafe {
        let raw_process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if raw_process.is_null() {
            return Err(format!(
                "OpenProcess({pid}) for identity inspection failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let process = OwnedHandle::from_raw_handle(raw_process);
        let mut is_member = 0;
        if IsProcessInJob(process.as_raw_handle(), job, &mut is_member) == 0 {
            return Err(format!(
                "IsProcessInJob({pid}) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        if is_member == 0 {
            return Err(format!("PID {pid} no longer belongs to the managed Job"));
        }

        let mut creation = FileTime::default();
        let mut exit = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        if GetProcessTimes(
            process.as_raw_handle(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        ) == 0
        {
            return Err(format!(
                "GetProcessTimes({pid}) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let creation_time_100ns =
            ((creation.high_date_time as u64) << 32) | creation.low_date_time as u64;
        let process_id = ManagedProcessId::new(pid, creation_time_100ns)
            .map_err(|error| format!("invalid process identity for PID {pid}: {error}"))?;

        let mut executable_buffer = vec![0u16; 32_768];
        let mut executable_length = executable_buffer.len() as u32;
        if QueryFullProcessImageNameW(
            process.as_raw_handle(),
            0,
            executable_buffer.as_mut_ptr(),
            &mut executable_length,
        ) == 0
        {
            return Err(format!(
                "QueryFullProcessImageNameW({pid}) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        executable_buffer.truncate(executable_length as usize);
        let executable = std::path::PathBuf::from(String::from_utf16_lossy(&executable_buffer));
        let identity = ManagedProcessIdentity::new(process_id, executable)
            .map_err(|error| format!("could not canonicalize executable for PID {pid}: {error}"))?;

        let sys_pid = sysinfo::Pid::from_u32(pid);
        let mut system = sysinfo::System::new();
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[sys_pid]),
            true,
            sysinfo::ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always),
        );
        let candidate_command_line = system.process(sys_pid).and_then(|process| {
            let parts: Vec<String> = process
                .cmd()
                .iter()
                .map(|part| part.to_string_lossy().into_owned())
                .collect();
            (!parts.is_empty()).then(|| parts.join(" "))
        });
        let command_line = (current_creation_time_100ns(job, pid).as_ref()
            == Ok(&creation_time_100ns))
        .then_some(candidate_command_line)
        .flatten();
        Ok(JobMemberInfo::new(identity, command_line))
    }
}

#[cfg(windows)]
fn current_creation_time_100ns(job: *mut c_void, pid: u32) -> Result<u64, String> {
    unsafe {
        let raw_process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if raw_process.is_null() {
            return Err(format!(
                "OpenProcess({pid}) for identity recheck failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let process = OwnedHandle::from_raw_handle(raw_process);
        let mut is_member = 0;
        if IsProcessInJob(process.as_raw_handle(), job, &mut is_member) == 0 {
            return Err(format!(
                "IsProcessInJob({pid}) identity recheck failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        if is_member == 0 {
            return Err(format!(
                "PID {pid} left the managed Job before command-line publication"
            ));
        }
        let mut creation = FileTime::default();
        let mut exit = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        if GetProcessTimes(
            process.as_raw_handle(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        ) == 0
        {
            return Err(format!(
                "GetProcessTimes({pid}) identity recheck failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(((creation.high_date_time as u64) << 32) | creation.low_date_time as u64)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::{CompletionMailbox, MAX_COMPLETION_MESSAGES};
    use crate::domain::id::ResourceId;
    use crate::domain::operation::ResourceFence;
    use crate::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
    use crate::process::registry::{JobCompletionEvent, JobCompletionMessage, ManagedProcessFence};

    fn authority() -> ManagedProcessFence {
        let resource_id = ResourceId::from_bytes([
            0x01, 0x9a, 0x11, 0x22, 0x33, 0x44, 0x70, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x71,
        ])
        .expect("resource id");
        let identity = ManagedProcessIdentity::new(
            ManagedProcessId::new(71, 7_100).expect("process id"),
            std::env::current_exe().expect("test executable"),
        )
        .expect("process identity");
        ManagedProcessFence::new(
            ResourceFence::new(resource_id, 1),
            ProcessOwner::Host,
            identity,
        )
    }

    fn message(authority: &ManagedProcessFence, event: JobCompletionEvent) -> JobCompletionMessage {
        let receiver = super::CompletionReceiverToken::issue();
        JobCompletionMessage::from_completion_receiver(&receiver, authority.clone(), event)
    }

    #[test]
    fn membership_mailbox_overflow_is_visible_and_preserves_arriving_zero() {
        let authority = authority();
        let receiver = super::CompletionReceiverToken::issue();
        let mut mailbox = CompletionMailbox::default();
        for pid in 1..=MAX_COMPLETION_MESSAGES as u32 {
            mailbox.push(
                &receiver,
                message(&authority, JobCompletionEvent::ExitProcess { pid }),
            );
        }
        mailbox.push(
            &receiver,
            message(&authority, JobCompletionEvent::ActiveProcessZero),
        );

        let messages = mailbox.drain();
        assert!(messages.len() <= MAX_COMPLETION_MESSAGES);
        assert!(messages.iter().any(|message| {
            matches!(
                message.event(),
                JobCompletionEvent::MonitorFailed { detail } if detail.contains("overflow")
            )
        }));
        assert!(messages
            .iter()
            .any(|message| matches!(message.event(), JobCompletionEvent::ActiveProcessZero)));
    }

    #[test]
    fn membership_mailbox_overflow_is_visible_and_preserves_queued_zero() {
        let authority = authority();
        let receiver = super::CompletionReceiverToken::issue();
        let mut mailbox = CompletionMailbox::default();
        mailbox.push(
            &receiver,
            message(&authority, JobCompletionEvent::ActiveProcessZero),
        );
        for pid in 1..=MAX_COMPLETION_MESSAGES as u32 {
            mailbox.push(
                &receiver,
                message(&authority, JobCompletionEvent::NewProcess { pid }),
            );
        }

        let messages = mailbox.drain();
        assert!(messages.len() <= MAX_COMPLETION_MESSAGES);
        assert!(messages.iter().any(|message| {
            matches!(
                message.event(),
                JobCompletionEvent::MonitorFailed { detail } if detail.contains("overflow")
            )
        }));
        assert!(messages
            .iter()
            .any(|message| matches!(message.event(), JobCompletionEvent::ActiveProcessZero)));
    }
}
