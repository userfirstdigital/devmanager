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
use crate::process::identity::ManagedProcessId;
use crate::process::identity::ManagedProcessIdentity;
use crate::process::registry::{
    JobCompletionEvent, JobCompletionMessage, JobMemberInfo, ManagedProcessFence,
};
use crate::process::sampler::SamplingBudget;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobMemberObservation {
    Accessible {
        identity: ManagedProcessIdentity,
    },
    Inaccessible {
        pid: u32,
        creation_time_100ns: Option<u64>,
        reason: String,
    },
}

/// Turn a Job's PID list into exact observations without granting a PID-only
/// ownership claim. The inspector must perform the current `IsProcessInJob`
/// and creation-time checks; failures remain visible as partial members.
pub fn collect_exact_job_observations<F>(
    active_process_ids: Result<Vec<u32>, String>,
    inspect_process: F,
) -> Result<Vec<JobMemberObservation>, String>
where
    F: FnMut(u32) -> Result<JobMemberInfo, String>,
{
    let mut budget = SamplingBudget::new(
        Instant::now() + Duration::from_secs(30),
        MAX_JOB_PROCESS_ID_CAPACITY,
    );
    collect_exact_job_observations_with_budget(active_process_ids, inspect_process, &mut budget)
}

/// Collect exact Job observations under the same bounded tick budget used by
/// process accounting. The input is consumed incrementally into a set capped
/// at `budget.max_members()`; a large Windows Job list is rejected before any
/// process inspection or unbounded output allocation occurs.
pub fn collect_exact_job_observations_with_budget<F>(
    active_process_ids: Result<Vec<u32>, String>,
    mut inspect_process: F,
    budget: &mut SamplingBudget,
) -> Result<Vec<JobMemberObservation>, String>
where
    F: FnMut(u32) -> Result<JobMemberInfo, String>,
{
    budget.note_job_query();
    let mut process_ids = BTreeSet::new();
    let mut new_local_members = 0usize;
    for pid in active_process_ids? {
        budget.checkpoint().map_err(|error| error.to_string())?;
        if pid == 0 {
            continue;
        }
        budget.note_job_candidate();
        let is_new_tick_member = !budget.contains_pid(pid);
        let is_new_local_member = !process_ids.contains(&pid);
        if is_new_local_member
            && (process_ids.len() >= budget.max_members()
                || (is_new_tick_member && new_local_members >= budget.remaining_capacity()))
        {
            return Err(format!(
                "managed Job process list exceeds {} members",
                budget.remaining_capacity()
            ));
        }
        if process_ids.insert(pid) && is_new_tick_member {
            new_local_members = new_local_members.saturating_add(1);
        }
    }

    let mut observations = Vec::with_capacity(process_ids.len());
    for pid in process_ids {
        // The identity/metrics inspector can perform several OS calls. Check
        // before every member so a slow first query cannot turn the bounded
        // tick into an unbounded Job walk.
        if let Err(error) = budget.checkpoint() {
            return Err(error.to_string());
        }
        budget.note_identity_inspection();
        let observation = match inspect_process(pid) {
            Ok(member) if member.identity().id().pid() == pid => JobMemberObservation::Accessible {
                identity: member.identity().clone(),
            },
            Ok(member) => JobMemberObservation::Inaccessible {
                pid,
                creation_time_100ns: Some(member.identity().id().creation_time_100ns()),
                reason: format!(
                    "Job inspection returned PID {} while observing PID {pid}",
                    member.identity().id().pid()
                ),
            },
            Err(reason) => JobMemberObservation::Inaccessible {
                pid,
                creation_time_100ns: None,
                reason,
            },
        };
        budget.checkpoint().map_err(|error| error.to_string())?;
        match &observation {
            JobMemberObservation::Accessible { identity } => {
                budget
                    .admit_identity(identity)
                    .map_err(|error| error.to_string())?;
            }
            JobMemberObservation::Inaccessible {
                pid,
                creation_time_100ns,
                ..
            } => {
                budget
                    .admit_inaccessible(*pid, *creation_time_100ns)
                    .map_err(|error| error.to_string())?;
            }
        }
        observations.push(observation);
    }
    Ok(observations)
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
const MAX_JOB_PROCESS_ID_CAPACITY: usize = 16_384;

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum JobProcessQueryError {
    InvalidHandle,
    ZeroBudget,
    BoundedOverflow {
        max_members: usize,
        reported_members: Option<usize>,
    },
    BufferSizeOverflow,
    BufferTooLarge,
    MisalignedBuffer,
    NativeFailure(String),
}

#[cfg(windows)]
impl std::fmt::Display for JobProcessQueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidHandle => formatter.write_str("managed job handle is null"),
            Self::ZeroBudget => formatter.write_str("managed Job process list budget is zero"),
            Self::BoundedOverflow {
                max_members,
                reported_members,
            } => match reported_members {
                Some(reported_members) => write!(
                    formatter,
                    "QueryInformationJobObject bounded Job overflow: process list exceeds {max_members} members ({reported_members} reported)"
                ),
                None => write!(
                    formatter,
                    "QueryInformationJobObject bounded Job overflow: process list exceeds {max_members} members"
                ),
            },
            Self::BufferSizeOverflow => formatter.write_str("job process list size overflow"),
            Self::BufferTooLarge => formatter.write_str(
                "job process list buffer exceeds QueryInformationJobObject limit",
            ),
            Self::MisalignedBuffer => {
                formatter.write_str("job process list aligned buffer out of range")
            }
            Self::NativeFailure(detail) => {
                write!(formatter, "QueryInformationJobObject failed: {detail}")
            }
        }
    }
}

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
    fn push(&mut self, message: JobCompletionMessage) {
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
        self.messages.push_back(JobCompletionMessage::new(
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
            query_job_active_process_ids(self.raw_job_handle(), MAX_JOB_PROCESS_ID_CAPACITY)
                .map_err(|error| error.to_string())
        }

        #[cfg(not(windows))]
        {
            Ok(Vec::new())
        }
    }

    pub fn active_process_observations(&self) -> Result<Vec<JobMemberObservation>, String> {
        let active_process_ids = self.active_process_ids();
        collect_exact_job_observations(active_process_ids, |pid| self.inspect_process(pid))
    }

    /// Query and inspect the current Job members without exceeding the
    /// caller's process-accounting tick budget. This is the only production
    /// path used by resource sampling; the legacy unbounded entry point above
    /// remains for control/reconciliation callers.
    pub fn active_process_observations_with_budget(
        &self,
        budget: &mut SamplingBudget,
    ) -> Result<Vec<JobMemberObservation>, String> {
        budget.checkpoint().map_err(|error| error.to_string())?;
        let active_process_ids = {
            #[cfg(windows)]
            {
                // Each OS query is itself capped at the global maximum. The
                // collector then deduplicates exact identities against the
                // already-admitted tick members before consuming remaining
                // capacity, so a repeated authoritative query can still be
                // recognized when the tick is otherwise full.
                query_job_active_process_ids(self.raw_job_handle(), budget.max_members())
                    .map_err(|error| error.to_string())
            }

            #[cfg(not(windows))]
            {
                Ok(Vec::new())
            }
        };
        budget.checkpoint().map_err(|error| error.to_string())?;
        collect_exact_job_observations_with_budget(
            active_process_ids,
            |pid| self.inspect_process(pid),
            budget,
        )
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
        let listener = std::thread::Builder::new()
            .name(format!(
                "devmanager-job-{}-{}",
                fence.resource().resource_id,
                fence.resource().runtime_generation
            ))
            .spawn(move || completion_listener(port, completion_key, listener_fence, mailbox))
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
fn query_job_active_process_ids(
    job: *mut c_void,
    max_members: usize,
) -> Result<Vec<u32>, JobProcessQueryError> {
    if job.is_null() {
        return Err(JobProcessQueryError::InvalidHandle);
    }
    if max_members == 0 {
        return Err(JobProcessQueryError::ZeroBudget);
    }
    if max_members > MAX_JOB_PROCESS_ID_CAPACITY {
        return Err(JobProcessQueryError::BoundedOverflow {
            max_members: MAX_JOB_PROCESS_ID_CAPACITY,
            reported_members: Some(max_members),
        });
    }

    // JOBOBJECT_BASIC_PROCESS_ID_LIST:
    //   DWORD NumberOfAssignedProcesses;
    //   DWORD NumberOfProcessIdsInList;
    //   ULONG_PTR ProcessIdList[ANYSIZE_ARRAY];
    let header_bytes = std::mem::size_of::<u32>()
        .checked_mul(2)
        .ok_or(JobProcessQueryError::BufferSizeOverflow)?;
    let mut capacity = 16usize.min(max_members);

    loop {
        if capacity > max_members {
            return Err(JobProcessQueryError::BoundedOverflow {
                max_members,
                reported_members: Some(capacity),
            });
        }
        let list_bytes = capacity
            .checked_mul(std::mem::size_of::<usize>())
            .ok_or(JobProcessQueryError::BufferSizeOverflow)?;
        let total_bytes = header_bytes
            .checked_add(list_bytes)
            .ok_or(JobProcessQueryError::BufferSizeOverflow)?;
        if total_bytes > u32::MAX as usize {
            return Err(JobProcessQueryError::BufferTooLarge);
        }
        let align = std::mem::align_of::<usize>().max(std::mem::align_of::<u32>());
        let storage_len = total_bytes
            .checked_add(align)
            .ok_or(JobProcessQueryError::BufferSizeOverflow)?;
        let mut storage = vec![0u8; storage_len];
        let offset = storage.as_ptr().align_offset(align);
        let end = offset
            .checked_add(total_bytes)
            .ok_or(JobProcessQueryError::BufferSizeOverflow)?;
        let buffer = storage
            .get_mut(offset..end)
            .ok_or(JobProcessQueryError::MisalignedBuffer)?;

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
                if needed > max_members || capacity >= max_members {
                    return Err(JobProcessQueryError::BoundedOverflow {
                        max_members,
                        reported_members: Some(assigned as usize),
                    });
                }
                let next_capacity = needed.max(capacity.saturating_mul(2)).min(max_members);
                if next_capacity <= capacity {
                    return Err(JobProcessQueryError::BoundedOverflow {
                        max_members,
                        reported_members: Some(assigned as usize),
                    });
                }
                capacity = next_capacity;
                continue;
            }
            return Err(JobProcessQueryError::NativeFailure(error.to_string()));
        }

        let count = u32::from_ne_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;
        if count > capacity {
            if count > max_members {
                return Err(JobProcessQueryError::BoundedOverflow {
                    max_members,
                    reported_members: Some(count),
                });
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
                .push(JobCompletionMessage::new(
                    fence.clone(),
                    JobCompletionEvent::MonitorFailed { detail },
                ));
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
            .push(JobCompletionMessage::new(fence.clone(), event));
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
    use super::{
        query_job_active_process_ids, CompletionMailbox, JobProcessQueryError, ManagedProcessJob,
        MAX_COMPLETION_MESSAGES, MAX_JOB_PROCESS_ID_CAPACITY,
    };
    use crate::domain::id::ResourceId;
    use crate::domain::operation::ResourceFence;
    use crate::process::identity::{ManagedProcessId, ManagedProcessIdentity, ProcessOwner};
    use crate::process::registry::{JobCompletionEvent, JobCompletionMessage, ManagedProcessFence};
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    mod sealed_fence_issuer {
        use super::*;

        trait Sealed {}

        struct Issuer;

        impl Sealed for Issuer {}

        trait IssueExactFence: Sealed {
            fn issue(&self) -> ManagedProcessFence;
        }

        impl IssueExactFence for Issuer {
            fn issue(&self) -> ManagedProcessFence {
                let resource_id = ResourceId::from_bytes([
                    0x01, 0x9a, 0x11, 0x22, 0x33, 0x44, 0x70, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x71,
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
        }

        pub(super) fn issue() -> ManagedProcessFence {
            Issuer.issue()
        }
    }

    fn authority() -> ManagedProcessFence {
        sealed_fence_issuer::issue()
    }

    fn message(authority: &ManagedProcessFence, event: JobCompletionEvent) -> JobCompletionMessage {
        JobCompletionMessage::new(authority.clone(), event)
    }

    #[test]
    fn query_information_job_object_enforces_the_16384_member_cap() {
        let Some(job) = ManagedProcessJob::create().expect("create test Job") else {
            return;
        };
        let members =
            query_job_active_process_ids(job.raw_job_handle(), MAX_JOB_PROCESS_ID_CAPACITY)
                .expect("empty test Job query at the maximum capacity");
        assert!(members.len() <= MAX_JOB_PROCESS_ID_CAPACITY);

        let overflow =
            query_job_active_process_ids(job.raw_job_handle(), MAX_JOB_PROCESS_ID_CAPACITY + 1)
                .expect_err("capacity above the QueryInformationJobObject cap must fail closed");
        match overflow {
            JobProcessQueryError::BoundedOverflow {
                max_members,
                reported_members: Some(reported_members),
            } => {
                assert_eq!(max_members, MAX_JOB_PROCESS_ID_CAPACITY);
                assert_eq!(reported_members, MAX_JOB_PROCESS_ID_CAPACITY + 1);
            }
            other => panic!("expected hard-cap overflow, got {other:?}"),
        }
    }

    #[test]
    fn query_information_job_object_reports_native_bounded_overflow_without_growing() {
        let Some(job) = ManagedProcessJob::create().expect("create test Job") else {
            return;
        };
        let helpers = SuspendedJobMembers::spawn_two(&job).expect("spawn suspended Job helpers");
        assert_eq!(helpers.members.len(), 2);

        let overflow = query_job_active_process_ids(job.raw_job_handle(), 1)
            .expect_err("a one-member buffer must reject the two-member Job");
        assert!(matches!(
            overflow,
            JobProcessQueryError::BoundedOverflow {
                max_members: 1,
                reported_members: Some(reported_members),
            } if reported_members >= 2
        ));
        drop(helpers);
        assert!(
            job.active_process_ids()
                .expect("authoritative zero-members query")
                .is_empty(),
            "test helpers must be cleaned from the Job before teardown"
        );
    }

    #[derive(Debug)]
    struct SuspendedJobMembers {
        members: Vec<SuspendedJobMember>,
    }

    impl SuspendedJobMembers {
        fn spawn_two(job: &ManagedProcessJob) -> Result<Self, String> {
            let first = SuspendedJobMember::spawn(job)?;
            let second = match SuspendedJobMember::spawn(job) {
                Ok(member) => member,
                Err(error) => {
                    drop(first);
                    return Err(error);
                }
            };
            Ok(Self {
                members: vec![first, second],
            })
        }
    }

    #[derive(Debug)]
    struct SuspendedJobMember {
        process: *mut c_void,
        thread: *mut c_void,
    }

    impl SuspendedJobMember {
        fn spawn(job: &ManagedProcessJob) -> Result<Self, String> {
            let executable = std::env::current_exe().map_err(|error| error.to_string())?;
            let mut executable = executable
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>();
            let mut startup = StartupInfoW::default();
            startup.cb = std::mem::size_of::<StartupInfoW>() as u32;
            let mut information = ProcessInformation::default();
            let created = unsafe {
                CreateProcessW(
                    executable.as_mut_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                    CREATE_NO_WINDOW | CREATE_SUSPENDED,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut startup,
                    &mut information,
                )
            };
            if created == 0 {
                return Err(format!(
                    "CreateProcessW suspended Job helper failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let assigned =
                unsafe { AssignProcessToJobObject(job.raw_job_handle(), information.process) };
            if assigned == 0 {
                let error = std::io::Error::last_os_error();
                unsafe {
                    TerminateProcess(information.process, 1);
                    WaitForSingleObject(information.process, 5_000);
                    CloseHandle(information.thread);
                    CloseHandle(information.process);
                }
                return Err(format!(
                    "AssignProcessToJobObject suspended Job helper failed: {error}"
                ));
            }
            unsafe {
                CloseHandle(information.thread);
            }
            Ok(Self {
                process: information.process,
                thread: std::ptr::null_mut(),
            })
        }
    }

    impl Drop for SuspendedJobMember {
        fn drop(&mut self) {
            if self.process.is_null() {
                return;
            }
            unsafe {
                TerminateProcess(self.process, 0);
                WaitForSingleObject(self.process, 5_000);
                CloseHandle(self.process);
                if !self.thread.is_null() {
                    CloseHandle(self.thread);
                }
            }
            self.process = std::ptr::null_mut();
            self.thread = std::ptr::null_mut();
        }
    }

    #[repr(C)]
    #[derive(Default)]
    struct StartupInfoW {
        cb: u32,
        reserved: *mut u16,
        desktop: *mut u16,
        title: *mut u16,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_count_chars: u32,
        y_count_chars: u32,
        fill_attribute: u32,
        flags: u32,
        show_window: u16,
        reserved2: u16,
        reserved2_ptr: *mut u8,
        stdin: *mut c_void,
        stdout: *mut c_void,
        stderr: *mut c_void,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ProcessInformation {
        process: *mut c_void,
        thread: *mut c_void,
        process_id: u32,
        thread_id: u32,
    }

    #[cfg(windows)]
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    #[cfg(windows)]
    const CREATE_SUSPENDED: u32 = 0x0000_0004;

    #[link(name = "kernel32")]
    extern "system" {
        fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        fn CreateProcessW(
            application_name: *mut u16,
            command_line: *mut u16,
            process_attributes: *mut c_void,
            thread_attributes: *mut c_void,
            inherit_handles: i32,
            creation_flags: u32,
            environment: *mut c_void,
            current_directory: *mut u16,
            startup_info: *mut StartupInfoW,
            process_information: *mut ProcessInformation,
        ) -> i32;
        fn TerminateProcess(process: *mut c_void, exit_code: u32) -> i32;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    #[test]
    fn membership_mailbox_overflow_is_visible_and_preserves_arriving_zero() {
        let authority = authority();
        let mut mailbox = CompletionMailbox::default();
        for pid in 1..=MAX_COMPLETION_MESSAGES as u32 {
            mailbox.push(message(&authority, JobCompletionEvent::ExitProcess { pid }));
        }
        mailbox.push(message(&authority, JobCompletionEvent::ActiveProcessZero));

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
        let mut mailbox = CompletionMailbox::default();
        mailbox.push(message(&authority, JobCompletionEvent::ActiveProcessZero));
        for pid in 1..=MAX_COMPLETION_MESSAGES as u32 {
            mailbox.push(message(&authority, JobCompletionEvent::NewProcess { pid }));
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
