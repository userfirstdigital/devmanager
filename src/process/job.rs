//! Windows Job Object ownership for managed process trees.

#[cfg(windows)]
use std::ffi::c_void;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

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
}

#[cfg(windows)]
const PROCESS_TERMINATE: u32 = 0x0001;
#[cfg(windows)]
const PROCESS_SET_QUOTA: u32 = 0x0100;
#[cfg(windows)]
const JOB_OBJECT_BASIC_PROCESS_ID_LIST_CLASS: u32 = 3;
#[cfg(windows)]
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: u32 = 9;
#[cfg(windows)]
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x00002000;
#[cfg(windows)]
const ERROR_MORE_DATA: i32 = 234;
#[cfg(windows)]
const MAX_JOB_PROCESS_ID_CAPACITY: usize = 16_384;

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
    handle: OwnedHandle,
}

/// Non-Windows marker type returned only behind `Option::None`.
#[cfg(not(windows))]
#[derive(Debug)]
pub struct ManagedProcessJob {
    _unsupported: (),
}

impl ManagedProcessJob {
    /// Returns the active process IDs currently assigned to this Job Object.
    ///
    /// On non-Windows platforms this always returns an empty list.
    pub fn active_process_ids(&self) -> Result<Vec<u32>, String> {
        #[cfg(windows)]
        {
            query_job_active_process_ids(self.handle.as_raw_handle())
        }

        #[cfg(not(windows))]
        {
            Ok(Vec::new())
        }
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
fn attach_process_to_windows_job(pid: u32) -> Result<ManagedProcessJob, String> {
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

        if AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) == 0 {
            return Err(format!(
                "AssignProcessToJobObject({pid}) failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        Ok(ManagedProcessJob { handle: job })
    }
}
