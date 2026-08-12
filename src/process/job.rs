//! Windows Job Object ownership for managed process trees.

use std::time::{Duration, Instant};

#[cfg(windows)]
use std::ffi::c_void;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(windows)]
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use std::thread::JoinHandle;

#[cfg(windows)]
use crate::domain::operation::ResourceFence;
use crate::process::identity::ManagedProcessIdentity;
#[cfg(windows)]
use crate::process::identity::{ManagedProcessId, ProcessOwner};
use crate::process::registry::{
    JobCompletionEvent, JobCompletionMessage, JobMemberInfo, ManagedProcessFence,
};
use crate::process::sampler::SamplingBudget;
use std::collections::BTreeSet;

/// A read-only, exact-identity observation of one current Job member.
///
/// This type carries no handle and grants no termination authority. An
/// inaccessible member remains visible by PID instead of being silently
/// omitted or being promoted to a PID-only ownership claim.
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

/// Converts one already-bounded Job PID query into exact member observations.
///
/// PID values are sorted and deduplicated before the caller's cap is applied.
/// Identity inspection stays inside the same absolute deadline and preserves
/// inaccessible members explicitly. This helper is intentionally read-only;
/// it cannot mint or release Job authority.
fn collect_exact_job_observations_until<F>(
    active_process_ids: Result<Vec<u32>, String>,
    mut inspect_process: F,
    absolute_deadline: std::time::Instant,
    max_members: usize,
) -> Result<Vec<JobMemberObservation>, String>
where
    F: FnMut(u32) -> Result<JobMemberInfo, String>,
{
    if max_members > MAX_JOB_PROCESS_ID_CAPACITY {
        return Err(format!(
            "managed Job observation capacity {max_members} exceeds {MAX_JOB_PROCESS_ID_CAPACITY} members"
        ));
    }
    if std::time::Instant::now() >= absolute_deadline {
        return Err("managed Job observation exceeded absolute deadline".to_string());
    }
    let mut process_ids = active_process_ids?;
    if std::time::Instant::now() >= absolute_deadline {
        return Err("managed Job observation exceeded absolute deadline".to_string());
    }

    process_ids.retain(|pid| *pid != 0);
    process_ids.sort_unstable();
    process_ids.dedup();
    if std::time::Instant::now() >= absolute_deadline {
        return Err("managed Job observation exceeded absolute deadline".to_string());
    }
    if process_ids.len() > max_members {
        return Err(format!(
            "managed Job observation contains {} members and exceeds {max_members} members",
            process_ids.len()
        ));
    }

    let mut observations = Vec::new();
    observations
        .try_reserve_exact(process_ids.len())
        .map_err(|error| format!("managed Job observation allocation failed: {error}"))?;
    if std::time::Instant::now() >= absolute_deadline {
        return Err("managed Job observation exceeded absolute deadline".to_string());
    }
    for pid in process_ids {
        if std::time::Instant::now() >= absolute_deadline {
            return Err("managed Job observation exceeded absolute deadline".to_string());
        }
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
        if std::time::Instant::now() >= absolute_deadline {
            return Err("managed Job observation exceeded absolute deadline".to_string());
        }
        observations.push(observation);
        if std::time::Instant::now() >= absolute_deadline {
            return Err("managed Job observation exceeded absolute deadline".to_string());
        }
    }
    Ok(observations)
}

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
    fn SetLastError(error_code: u32);
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
const ERROR_ALREADY_EXISTS: i32 = 183;
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
const COMPLETION_LISTENER_POLL_MILLIS: u32 = 25;
#[cfg(windows)]
const COMPLETION_LISTENER_RELEASE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(COMPLETION_LISTENER_POLL_MILLIS as u64 * 4);
#[cfg(windows)]
const COMPLETION_LISTENER_DROP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(windows)]
const ERROR_TIMEOUT: i32 = 258;
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
pub(crate) struct ManagedProcessJob {
    internal_name: String,
    handle: Option<OwnedHandle>,
    completion_port: Option<OwnedHandle>,
    completion_key: usize,
    completion_fence: Option<ManagedProcessFence>,
    completion_mailbox: Arc<Mutex<CompletionMailbox>>,
    completion_listener: Option<JoinHandle<()>>,
    completion_stop: Arc<AtomicBool>,
}

/// Non-Windows marker type returned only behind `Option::None`.
#[cfg(not(windows))]
#[derive(Debug)]
pub(crate) struct ManagedProcessJob {
    _unsupported: (),
}

impl ManagedProcessJob {
    /// Creates an empty, non-inheritable Job Object whose final handle closes
    /// every process in the tree.
    #[cfg(test)]
    pub(crate) fn create() -> Result<Option<Self>, String> {
        #[cfg(windows)]
        {
            let name = format!("Local\\DevManager-Test-{}", uuid::Uuid::now_v7());
            create_windows_job(&name).map(Some)
        }

        #[cfg(not(windows))]
        {
            Ok(None)
        }
    }

    /// Creates the one Job authority for an exact managed resource
    /// generation. The stable internal name is intentionally derived only
    /// from non-secret ownership/fence values and is never a user-facing
    /// process label.
    #[cfg(windows)]
    pub(crate) fn create_for_resource(
        owner: ProcessOwner,
        fence: ResourceFence,
    ) -> Result<Self, String> {
        let owner = match owner {
            ProcessOwner::Task(task_id) => format!("Task-{task_id}"),
            ProcessOwner::Host => "Host".to_string(),
        };
        let internal_name = format!(
            "Local\\DevManager-{owner}-{}-{}",
            fence.resource_id, fence.runtime_generation
        );
        create_windows_job(&internal_name)
    }

    #[cfg(windows)]
    pub(crate) fn internal_name(&self) -> &str {
        &self.internal_name
    }

    /// Returns the active process IDs currently assigned to this Job Object.
    ///
    /// On non-Windows platforms this always returns an empty list.
    pub(crate) fn active_process_ids(&self) -> Result<Vec<u32>, String> {
        #[cfg(windows)]
        {
            query_job_active_process_ids(self.raw_job_handle(), MAX_JOB_PROCESS_ID_CAPACITY)
        }

        #[cfg(not(windows))]
        {
            Ok(Vec::new())
        }
    }

    pub(crate) fn active_process_ids_until(
        &self,
        absolute_deadline: std::time::Instant,
    ) -> Result<Vec<u32>, String> {
        self.active_process_ids_with_capacity_until(absolute_deadline, MAX_JOB_PROCESS_ID_CAPACITY)
    }

    fn active_process_ids_with_capacity_until(
        &self,
        absolute_deadline: std::time::Instant,
        max_members: usize,
    ) -> Result<Vec<u32>, String> {
        if max_members == 0 || max_members > MAX_JOB_PROCESS_ID_CAPACITY {
            return Err(format!(
                "managed Job membership capacity must be between 1 and {MAX_JOB_PROCESS_ID_CAPACITY}"
            ));
        }
        if std::time::Instant::now() >= absolute_deadline {
            return Err("managed Job membership query exceeded teardown deadline".to_string());
        }
        #[cfg(windows)]
        let process_ids = query_job_active_process_ids(self.raw_job_handle(), max_members)?;
        #[cfg(not(windows))]
        let process_ids = Vec::new();
        if std::time::Instant::now() >= absolute_deadline {
            return Err("managed Job membership query exceeded teardown deadline".to_string());
        }
        Ok(process_ids)
    }

    /// Observes the current Job members as exact identities without exposing
    /// the Job handle or any close capability. The OS query allocation,
    /// identity walk, and returned vector all share one caller deadline and
    /// one bounded per-Job member cap.
    pub(crate) fn active_process_observations_until(
        &self,
        absolute_deadline: std::time::Instant,
        max_members: usize,
    ) -> Result<Vec<JobMemberObservation>, String> {
        if std::time::Instant::now() >= absolute_deadline {
            return Err("managed Job observation exceeded absolute deadline".to_string());
        }
        let active_process_ids =
            self.active_process_ids_with_capacity_until(absolute_deadline, max_members);
        if std::time::Instant::now() >= absolute_deadline {
            return Err("managed Job observation exceeded absolute deadline".to_string());
        }
        collect_exact_job_observations_until(
            active_process_ids,
            |pid| self.inspect_process_until(pid, absolute_deadline),
            absolute_deadline,
            max_members,
        )
    }

    /// Explicitly terminates every current member of this owned Job Object.
    ///
    /// This is intentionally Job-scoped rather than PID-scoped. The Job,
    /// completion port, and listener remain owned by this value until the
    /// caller has observed the exact completion fence and authoritative empty
    /// membership before releasing it.
    pub(crate) fn terminate_tree(&self) -> Result<(), String> {
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

    /// Terminates every process currently owned by this Job Object without
    /// dropping ownership. Callers can then wait for the authoritative
    /// ACTIVE_PROCESS_ZERO state before releasing the Job handle.
    pub fn terminate_members(&self) -> Result<(), String> {
        #[cfg(windows)]
        unsafe {
            if TerminateJobObject(self.raw_job_handle(), 1) == 0 {
                return Err(format!(
                    "TerminateJobObject failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        }

        #[cfg(not(windows))]
        {
            Ok(())
        }
    }

    pub(crate) fn terminate_tree_until(
        &self,
        absolute_deadline: std::time::Instant,
    ) -> Result<(), String> {
        if std::time::Instant::now() >= absolute_deadline {
            return Err("managed Job termination exceeded teardown deadline".to_string());
        }
        self.terminate_tree()?;
        if std::time::Instant::now() >= absolute_deadline {
            return Err("managed Job termination exceeded teardown deadline".to_string());
        }
        Ok(())
    }

    /// Waits for the Job's authoritative empty state until the deadline.
    pub fn wait_for_active_process_zero(&self, deadline: Instant) -> Result<bool, String> {
        loop {
            if self.active_process_ids()?.is_empty() {
                return Ok(true);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(5)));
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

    pub(crate) fn inspect_process_until(
        &self,
        pid: u32,
        absolute_deadline: std::time::Instant,
    ) -> Result<JobMemberInfo, String> {
        if std::time::Instant::now() >= absolute_deadline {
            return Err("managed Job process inspection exceeded teardown deadline".to_string());
        }
        let member = self.inspect_process(pid)?;
        if std::time::Instant::now() >= absolute_deadline {
            return Err("managed Job process inspection exceeded teardown deadline".to_string());
        }
        Ok(member)
    }

    pub(crate) fn bind_completion_fence(
        &mut self,
        fence: ManagedProcessFence,
    ) -> Result<(), String> {
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

    /// Stops the completion listener and joins it before returning.
    ///
    /// The listener owns the only receiver capability and may mutate the
    /// completion mailbox. Joining it is therefore part of the release
    /// boundary: once this method returns, no receiver thread can outlive the
    /// Job or mutate its mailbox.
    pub(crate) fn shutdown_for_release(&mut self) -> Result<(), String> {
        #[cfg(windows)]
        {
            let deadline = std::time::Instant::now()
                .checked_add(COMPLETION_LISTENER_RELEASE_TIMEOUT)
                .ok_or_else(|| "managed Job release deadline overflow".to_string())?;
            self.shutdown_listener_until(deadline)
        }

        #[cfg(not(windows))]
        {
            Ok(())
        }
    }

    pub(crate) fn shutdown_for_release_until(
        &mut self,
        absolute_deadline: std::time::Instant,
    ) -> Result<(), String> {
        #[cfg(windows)]
        {
            self.shutdown_listener_until(absolute_deadline)
        }

        #[cfg(not(windows))]
        {
            let _ = absolute_deadline;
            Ok(())
        }
    }

    #[cfg(test)]
    pub(crate) fn drain_completion_messages(&self) -> Vec<JobCompletionMessage> {
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

    pub(crate) fn drain_completion_messages_until(
        &self,
        absolute_deadline: std::time::Instant,
    ) -> Result<Vec<JobCompletionMessage>, String> {
        #[cfg(windows)]
        {
            loop {
                if std::time::Instant::now() >= absolute_deadline {
                    return Err(
                        "managed Job completion drain exceeded teardown deadline".to_string()
                    );
                }
                match self.completion_mailbox.try_lock() {
                    Ok(mut mailbox) => {
                        let messages = mailbox.drain();
                        if std::time::Instant::now() >= absolute_deadline {
                            return Err("managed Job completion drain exceeded teardown deadline"
                                .to_string());
                        }
                        return Ok(messages);
                    }
                    Err(std::sync::TryLockError::WouldBlock) => std::thread::yield_now(),
                    Err(std::sync::TryLockError::Poisoned(_)) => {
                        return Err("managed Job completion mailbox poisoned".to_string())
                    }
                }
            }
        }

        #[cfg(not(windows))]
        {
            if std::time::Instant::now() >= absolute_deadline {
                return Err("managed Job completion drain exceeded teardown deadline".to_string());
            }
            Ok(Vec::new())
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
        let completion_stop = Arc::clone(&self.completion_stop);
        let listener_fence = fence.clone();
        let receiver = CompletionReceiverToken::issue();
        let listener = std::thread::Builder::new()
            .name(format!(
                "devmanager-job-{}-{}",
                fence.resource().resource_id,
                fence.resource().runtime_generation
            ))
            .spawn(move || {
                completion_listener(
                    port,
                    completion_key,
                    listener_fence,
                    mailbox,
                    completion_stop,
                    receiver,
                )
            })
            .map_err(|error| format!("could not spawn Job completion listener: {error}"))?;
        self.completion_fence = Some(fence);
        self.completion_listener = Some(listener);
        Ok(())
    }

    #[cfg(windows)]
    fn shutdown_listener_until(
        &mut self,
        absolute_deadline: std::time::Instant,
    ) -> Result<(), String> {
        let Some(listener) = self.completion_listener.take() else {
            return Ok(());
        };

        self.completion_stop.store(true, Ordering::SeqCst);
        let post_error = match self.completion_port.as_ref() {
            Some(port) => {
                let posted = unsafe {
                    PostQueuedCompletionStatus(
                        port.as_raw_handle(),
                        SHUTDOWN_MESSAGE,
                        SHUTDOWN_COMPLETION_KEY,
                        std::ptr::null_mut(),
                    ) != 0
                };
                (!posted).then(|| {
                    format!(
                        "managed Job completion listener shutdown post failed: {}",
                        std::io::Error::last_os_error()
                    )
                })
            }
            None => {
                Some("managed Job completion port is unavailable while stopping listener".into())
            }
        };
        while !listener.is_finished() && std::time::Instant::now() < absolute_deadline {
            std::thread::yield_now();
        }
        if !listener.is_finished() {
            // Keep ownership of the receiver capability.  The exact release
            // authority remains retryable and no JoinHandle is detached.
            self.completion_listener = Some(listener);
            return Err("managed Job completion listener did not acknowledge cancellation before the release deadline".to_string());
        }
        let join_error = listener
            .join()
            .err()
            .map(|_| "managed Job completion listener panicked during shutdown".to_string());
        if let Some(detail) = post_error {
            return Err(detail);
        }
        if let Some(detail) = join_error {
            return Err(detail);
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for ManagedProcessJob {
    fn drop(&mut self) {
        // The listener must be joined before either handle is closed. The
        // completion port is the listener's wakeup boundary, while the
        // polling stop flag is the fail-closed fallback if posting fails.
        let release_deadline = std::time::Instant::now()
            .checked_add(COMPLETION_LISTENER_RELEASE_TIMEOUT)
            .unwrap_or_else(|| std::process::abort());
        if self.shutdown_listener_until(release_deadline).is_err()
            && self.completion_listener.is_some()
        {
            // Closing the completion port is the final documented cancellation
            // boundary for GetQueuedCompletionStatus.  Wait only for the
            // bounded private receiver to acknowledge it, then join an
            // already-finished thread.
            drop(self.completion_port.take());
            if let Some(listener) = self.completion_listener.take() {
                let wait_started = std::time::Instant::now();
                while !listener.is_finished()
                    && wait_started.elapsed() < COMPLETION_LISTENER_DROP_TIMEOUT
                {
                    std::thread::yield_now();
                }
                if !listener.is_finished() {
                    // Returning would detach the sole receiver capability and
                    // violate the Job's no-orphan contract.
                    std::process::abort();
                }
                let _ = listener.join();
            }
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
pub(crate) fn attach_process_to_managed_job(pid: u32) -> Result<Option<ManagedProcessJob>, String> {
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
fn query_job_active_process_ids(job: *mut c_void, max_capacity: usize) -> Result<Vec<u32>, String> {
    if job.is_null() {
        return Err("managed job handle is null".to_string());
    }
    if max_capacity == 0 || max_capacity > MAX_JOB_PROCESS_ID_CAPACITY {
        return Err(format!(
            "managed Job membership capacity must be between 1 and {MAX_JOB_PROCESS_ID_CAPACITY}"
        ));
    }

    // JOBOBJECT_BASIC_PROCESS_ID_LIST:
    //   DWORD NumberOfAssignedProcesses;
    //   DWORD NumberOfProcessIdsInList;
    //   ULONG_PTR ProcessIdList[ANYSIZE_ARRAY];
    let header_bytes = std::mem::size_of::<u32>()
        .checked_mul(2)
        .ok_or_else(|| "job process list header size overflow".to_string())?;
    let mut capacity = 16usize.min(max_capacity);

    loop {
        if capacity > max_capacity {
            return Err(format!(
                "QueryInformationJobObject process list exceeds {max_capacity} members"
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
                if needed > max_capacity || capacity >= max_capacity {
                    return Err(format!(
                        "QueryInformationJobObject process list exceeds {max_capacity} members"
                    ));
                }
                let next_capacity = needed.max(capacity.saturating_mul(2)).min(max_capacity);
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

        let returned_bytes = usize::try_from(return_length)
            .map_err(|_| "job process list result length does not fit usize".to_string())?;
        if returned_bytes < header_bytes || returned_bytes > total_bytes {
            return Err(format!(
                "QueryInformationJobObject returned invalid process-list length {returned_bytes} for a {total_bytes}-byte buffer"
            ));
        }

        let count = u32::from_ne_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;
        if count > capacity {
            if count > max_capacity {
                return Err(format!(
                    "QueryInformationJobObject returned {count} members (max {max_capacity})"
                ));
            }
            capacity = count;
            continue;
        }
        let required_bytes = header_bytes
            .checked_add(
                count
                    .checked_mul(std::mem::size_of::<usize>())
                    .ok_or_else(|| "job process list result size overflow".to_string())?,
            )
            .ok_or_else(|| "job process list result size overflow".to_string())?;
        if required_bytes > returned_bytes {
            return Err(format!(
                "QueryInformationJobObject reported {count} members requiring {required_bytes} bytes but returned only {returned_bytes} bytes"
            ));
        }

        let list_ptr = unsafe { buffer.as_ptr().add(header_bytes) as *const usize };
        let mut process_ids = Vec::new();
        process_ids
            .try_reserve_exact(count)
            .map_err(|error| format!("managed Job PID snapshot allocation failed: {error}"))?;
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
fn create_windows_job(internal_name: &str) -> Result<ManagedProcessJob, String> {
    if internal_name.is_empty() || internal_name.len() > 240 || internal_name.contains('\0') {
        return Err("managed Job internal identity is invalid".to_string());
    }
    let wide_name: Vec<u16> = std::ffi::OsStr::new(internal_name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        SetLastError(0);
        let raw_job = CreateJobObjectW(std::ptr::null_mut(), wide_name.as_ptr());
        if raw_job.is_null() {
            return Err(format!(
                "CreateJobObjectW failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // Null SECURITY_ATTRIBUTES make this handle non-inheritable. OwnedHandle
        // closes exactly this sole owner on every subsequent error path.
        let job = OwnedHandle::from_raw_handle(raw_job);
        if std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ALREADY_EXISTS) {
            return Err(format!(
                "managed Job identity `{internal_name}` is already owned"
            ));
        }

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
            internal_name: internal_name.to_string(),
            handle: Some(job),
            completion_port: Some(completion_port),
            completion_key,
            completion_fence: None,
            completion_mailbox: Arc::new(Mutex::new(CompletionMailbox::default())),
            completion_listener: None,
            completion_stop: Arc::new(AtomicBool::new(false)),
        })
    }
}

#[cfg(windows)]
fn attach_process_to_windows_job(pid: u32) -> Result<ManagedProcessJob, String> {
    unsafe {
        let job = create_windows_job(&format!(
            "Local\\DevManager-Legacy-{}-{}",
            pid,
            uuid::Uuid::now_v7()
        ))?;

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
    stop: Arc<AtomicBool>,
    receiver: CompletionReceiverToken,
) {
    let completion_port = completion_port_value as *mut c_void;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let mut message_id = 0u32;
        let mut completion_key = 0usize;
        let mut process_value = std::ptr::null_mut();
        let ok = unsafe {
            GetQueuedCompletionStatus(
                completion_port,
                &mut message_id,
                &mut completion_key,
                &mut process_value,
                COMPLETION_LISTENER_POLL_MILLIS,
            )
        };
        if completion_key == SHUTDOWN_COMPLETION_KEY
            && message_id == SHUTDOWN_MESSAGE
            && process_value.is_null()
        {
            break;
        }
        if ok == 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(ERROR_TIMEOUT) {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }
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
        let executable_length = checked_process_image_length(
            executable_length,
            executable_buffer.len(),
            "managed Job process image",
        )?;
        executable_buffer.truncate(executable_length);
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
fn checked_process_image_length(
    reported_length: u32,
    buffer_capacity: usize,
    context: &str,
) -> Result<usize, String> {
    let reported_length = usize::try_from(reported_length)
        .map_err(|_| format!("{context} length does not fit usize"))?;
    if reported_length > buffer_capacity {
        return Err(format!(
            "{context} length {reported_length} exceeds buffer capacity {buffer_capacity}"
        ));
    }
    Ok(reported_length)
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
    use crate::process::registry::{
        JobCompletionEvent, JobCompletionMessage, JobMemberInfo, ManagedProcessFence,
    };
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

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
    fn process_image_length_is_checked_before_job_identity_buffer_truncation() {
        assert_eq!(
            super::checked_process_image_length(4, 4, "test process image"),
            Ok(4)
        );
        let error = super::checked_process_image_length(5, 4, "test process image")
            .expect_err("an OS-reported length outside the allocation must fail closed");
        assert!(error.contains("buffer capacity"), "{error}");
    }

    #[test]
    fn exact_job_observation_collection_rejects_capacity_before_identity_inspection() {
        let inspections = AtomicUsize::new(0);
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(1))
            .expect("observation deadline");

        let error = super::collect_exact_job_observations_until(
            Ok(vec![71, 72]),
            |_| {
                inspections.fetch_add(1, Ordering::SeqCst);
                unreachable!("over-capacity input must fail before identity inspection")
            },
            deadline,
            1,
        )
        .expect_err("over-capacity Job observations must fail closed");

        assert!(error.contains("exceeds 1 members"), "{error}");
        assert_eq!(inspections.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn exact_job_observation_collection_preserves_accessible_and_inaccessible_members() {
        let root = authority().root().clone();
        let root_pid = root.id().pid();
        let inaccessible_pid = root_pid.saturating_add(1);
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(1))
            .expect("observation deadline");

        let observations = super::collect_exact_job_observations_until(
            Ok(vec![root_pid, root_pid, inaccessible_pid, inaccessible_pid]),
            |pid| {
                if pid == root_pid {
                    Ok(JobMemberInfo::new(root.clone(), None))
                } else {
                    Err("identity access denied".to_string())
                }
            },
            deadline,
            2,
        )
        .expect("bounded exact observations");

        assert!(observations.iter().any(|observation| matches!(
            observation,
            super::JobMemberObservation::Accessible { identity } if identity == &root
        )));
        assert!(observations.iter().any(|observation| matches!(
            observation,
            super::JobMemberObservation::Inaccessible { pid, creation_time_100ns: None, reason }
                if *pid == inaccessible_pid && reason == "identity access denied"
        )));
        assert_eq!(
            observations.len(),
            2,
            "duplicate PID observations must be deduplicated before the cap"
        );
    }

    #[test]
    fn completion_listener_ignores_mismatched_completion_keys() {
        let raw_port = unsafe {
            super::CreateIoCompletionPort(
                (-1isize) as *mut super::c_void,
                std::ptr::null_mut(),
                0,
                1,
            )
        };
        assert!(!raw_port.is_null(), "create completion port");
        let port = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw_port) };
        let port_value = port.as_raw_handle() as usize;
        let expected_key = 0x71usize;
        let mailbox = Arc::new(Mutex::new(super::CompletionMailbox::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let listener = std::thread::spawn({
            let mailbox = Arc::clone(&mailbox);
            let stop = Arc::clone(&stop);
            let fence = authority();
            move || {
                super::completion_listener(
                    port_value,
                    expected_key,
                    fence,
                    mailbox,
                    stop,
                    super::CompletionReceiverToken::issue(),
                )
            }
        });

        let wrong_key_posted = unsafe {
            super::PostQueuedCompletionStatus(
                port_value as *mut super::c_void,
                super::JOB_OBJECT_MSG_NEW_PROCESS,
                expected_key + 1,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(wrong_key_posted, 0, "post mismatched completion packet");
        std::thread::sleep(Duration::from_millis(75));
        assert!(
            !listener.is_finished(),
            "a mismatched packet must not terminate the receiver"
        );

        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let shutdown_posted = unsafe {
            super::PostQueuedCompletionStatus(
                port_value as *mut super::c_void,
                super::SHUTDOWN_MESSAGE,
                super::SHUTDOWN_COMPLETION_KEY,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(shutdown_posted, 0, "post listener shutdown packet");
        listener.join().expect("completion listener join");
        assert!(mailbox
            .lock()
            .expect("completion mailbox")
            .drain()
            .is_empty());
        drop(port);
    }

    #[test]
    fn managed_job_drop_joins_listener_before_return() {
        let Some(mut job) = super::ManagedProcessJob::create()
            .expect("create managed Job for listener shutdown test")
        else {
            return;
        };
        let listener_finished = Arc::new(AtomicBool::new(false));
        let listener_finished_for_thread = Arc::clone(&listener_finished);
        job.completion_listener = Some(std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(750));
            listener_finished_for_thread.store(true, std::sync::atomic::Ordering::SeqCst);
        }));

        let drop_started = Instant::now();
        drop(job);
        let returned_before_listener = !listener_finished.load(Ordering::SeqCst);
        let elapsed = drop_started.elapsed();
        assert!(
            !returned_before_listener,
            "managed Job drop returned while its completion listener was still live (elapsed {elapsed:?})"
        );
    }

    #[test]
    fn listener_release_timeout_retains_join_authority_for_exact_retry() {
        let Some(mut job) = super::ManagedProcessJob::create()
            .expect("create managed Job for retryable listener release")
        else {
            return;
        };
        let listener_finished = Arc::new(AtomicBool::new(false));
        let listener_finished_for_thread = Arc::clone(&listener_finished);
        job.completion_listener = Some(std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            listener_finished_for_thread.store(true, Ordering::SeqCst);
        }));

        let first = job
            .shutdown_for_release_until(Instant::now())
            .expect_err("expired release must retain a live listener handle");
        assert!(first.contains("did not acknowledge cancellation"));
        assert!(job.completion_listener.is_some());

        let retry_deadline = Instant::now()
            .checked_add(Duration::from_secs(1))
            .expect("retry deadline");
        job.shutdown_for_release_until(retry_deadline)
            .expect("same Job listener authority remains retryable");
        assert!(listener_finished.load(Ordering::SeqCst));
        assert!(job.completion_listener.is_none());
    }

    #[test]
    fn repeated_managed_job_drops_join_every_listener() {
        for _ in 0..4 {
            let Some(mut job) = super::ManagedProcessJob::create()
                .expect("create managed Job for repeated listener shutdown")
            else {
                return;
            };
            let listener_finished = Arc::new(AtomicBool::new(false));
            let listener_finished_for_thread = Arc::clone(&listener_finished);
            job.completion_listener = Some(std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(25));
                listener_finished_for_thread.store(true, Ordering::SeqCst);
            }));

            drop(job);
            assert!(
                listener_finished.load(Ordering::SeqCst),
                "repeated Job drop left a completion listener running"
            );
        }
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
