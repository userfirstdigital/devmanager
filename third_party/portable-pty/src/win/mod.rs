use crate::{Child, ChildKiller, ExitStatus};
use anyhow::Context as _;
use std::io::{Error as IoError, Result as IoResult};
use std::os::windows::io::{AsRawHandle, BorrowedHandle, RawHandle};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};
use winapi::shared::minwindef::DWORD;
use winapi::shared::winerror::WAIT_TIMEOUT;
use winapi::um::minwinbase::STILL_ACTIVE;
use winapi::um::processthreadsapi::*;
use winapi::um::synchapi::WaitForSingleObject;
use winapi::um::winbase::{INFINITE, WAIT_FAILED, WAIT_OBJECT_0};

pub mod conpty;
mod procthreadattr;
mod psuedocon;

use filedescriptor::OwnedHandle;

#[derive(Debug)]
pub struct WinChild {
    proc: Mutex<OwnedHandle>,
}

const PENDING_ABORT_TIMEOUT_MS: DWORD = 5_000;

/// A ConPTY child whose primary thread has not executed yet.
///
/// This type intentionally does not implement `Child` or `Clone`; consuming
/// `resume` is the sole transition into the ordinary child state.
#[must_use = "a pending PTY child must be resumed after ownership is recorded or explicitly aborted"]
#[derive(Debug)]
pub struct PendingChild {
    child: Option<WinChild>,
    primary_thread: Option<OwnedHandle>,
}

impl PendingChild {
    pub(crate) fn new(child: WinChild, primary_thread: OwnedHandle) -> Self {
        Self {
            child: Some(child),
            primary_thread: Some(primary_thread),
        }
    }

    pub fn process_id(&self) -> u32 {
        self.child
            .as_ref()
            .and_then(Child::process_id)
            .expect("CreateProcessW returned a child without a process ID")
    }

    pub fn process_handle(&self) -> BorrowedHandle<'_> {
        let raw = self
            .child
            .as_ref()
            .expect("pending child already consumed")
            .raw_process_handle();
        // The OwnedHandle remains inside `self` for the returned borrow.
        unsafe { BorrowedHandle::borrow_raw(raw) }
    }

    pub fn resume(mut self) -> anyhow::Result<WinChild> {
        let thread = self
            .primary_thread
            .take()
            .expect("pending primary thread already consumed");
        let previous = unsafe { ResumeThread(thread.as_raw_handle() as _) };
        if previous != 1 {
            let resume_error = if previous == u32::MAX {
                format!("ResumeThread failed: {}", IoError::last_os_error())
            } else {
                format!("ResumeThread returned invalid previous suspend count {previous}")
            };
            let cleanup = self.abort_and_wait_inner();
            return match cleanup {
                Ok(_) => Err(anyhow::anyhow!(resume_error)),
                Err(cleanup_error) => Err(anyhow::anyhow!(
                    "{resume_error}; suspended child cleanup failed: {cleanup_error}"
                )),
            };
        }
        drop(thread);
        Ok(self.child.take().expect("pending child already consumed"))
    }

    pub fn abort_and_wait(mut self) -> anyhow::Result<ExitStatus> {
        self.abort_and_wait_inner()
    }

    fn abort_and_wait_inner(&mut self) -> anyhow::Result<ExitStatus> {
        let child = self
            .child
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("pending child already consumed"))?;
        let process = child.raw_process_handle();
        let terminated = unsafe { TerminateProcess(process as _, 127) };
        if terminated == 0 {
            return Err(IoError::last_os_error()).context("TerminateProcess pending child");
        }

        match unsafe { WaitForSingleObject(process as _, PENDING_ABORT_TIMEOUT_MS) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => {
                anyhow::bail!("pending child did not terminate within {PENDING_ABORT_TIMEOUT_MS}ms")
            }
            WAIT_FAILED => {
                return Err(IoError::last_os_error()).context("WaitForSingleObject pending child")
            }
            value => anyhow::bail!("WaitForSingleObject returned unexpected value {value}"),
        }

        let mut status: DWORD = 0;
        if unsafe { GetExitCodeProcess(process as _, &mut status) } == 0 {
            return Err(IoError::last_os_error()).context("GetExitCodeProcess pending child");
        }
        self.primary_thread.take();
        self.child.take();
        Ok(ExitStatus::with_exit_code(status))
    }
}

impl Drop for PendingChild {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.abort_and_wait_inner();
        }
    }
}

impl WinChild {
    fn raw_process_handle(&self) -> RawHandle {
        self.proc.lock().unwrap().as_raw_handle()
    }

    fn is_complete(&mut self) -> IoResult<Option<ExitStatus>> {
        let mut status: DWORD = 0;
        let proc = self.proc.lock().unwrap().try_clone().unwrap();
        let res = unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) };
        if res != 0 {
            if status == STILL_ACTIVE {
                Ok(None)
            } else {
                Ok(Some(ExitStatus::with_exit_code(status)))
            }
        } else {
            Ok(None)
        }
    }

    fn do_kill(&mut self) -> IoResult<()> {
        let proc = self.proc.lock().unwrap().try_clone().unwrap();
        let res = unsafe { TerminateProcess(proc.as_raw_handle() as _, 1) };
        let err = IoError::last_os_error();
        if res != 0 {
            Err(err)
        } else {
            Ok(())
        }
    }
}

impl ChildKiller for WinChild {
    fn kill(&mut self) -> IoResult<()> {
        self.do_kill().ok();
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        let proc = self.proc.lock().unwrap().try_clone().unwrap();
        Box::new(WinChildKiller { proc })
    }
}

#[derive(Debug)]
pub struct WinChildKiller {
    proc: OwnedHandle,
}

impl ChildKiller for WinChildKiller {
    fn kill(&mut self) -> IoResult<()> {
        let res = unsafe { TerminateProcess(self.proc.as_raw_handle() as _, 1) };
        let err = IoError::last_os_error();
        if res != 0 {
            Err(err)
        } else {
            Ok(())
        }
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        let proc = self.proc.try_clone().unwrap();
        Box::new(WinChildKiller { proc })
    }
}

impl Child for WinChild {
    fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
        self.is_complete()
    }

    fn wait(&mut self) -> IoResult<ExitStatus> {
        if let Ok(Some(status)) = self.try_wait() {
            return Ok(status);
        }
        let proc = self.proc.lock().unwrap().try_clone().unwrap();
        unsafe {
            WaitForSingleObject(proc.as_raw_handle() as _, INFINITE);
        }
        let mut status: DWORD = 0;
        let res = unsafe { GetExitCodeProcess(proc.as_raw_handle() as _, &mut status) };
        if res != 0 {
            Ok(ExitStatus::with_exit_code(status))
        } else {
            Err(IoError::last_os_error())
        }
    }

    fn process_id(&self) -> Option<u32> {
        let res = unsafe { GetProcessId(self.proc.lock().unwrap().as_raw_handle() as _) };
        if res == 0 {
            None
        } else {
            Some(res)
        }
    }

    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        let proc = self.proc.lock().unwrap();
        Some(proc.as_raw_handle())
    }
}

impl std::future::Future for WinChild {
    type Output = anyhow::Result<ExitStatus>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<anyhow::Result<ExitStatus>> {
        match self.is_complete() {
            Ok(Some(status)) => Poll::Ready(Ok(status)),
            Err(err) => Poll::Ready(Err(err).context("Failed to retrieve process exit status")),
            Ok(None) => {
                struct PassRawHandleToWaiterThread(pub RawHandle);
                unsafe impl Send for PassRawHandleToWaiterThread {}

                let proc = self.proc.lock().unwrap().try_clone()?;
                let handle = PassRawHandleToWaiterThread(proc.as_raw_handle());

                let waker = cx.waker().clone();
                std::thread::spawn(move || {
                    unsafe {
                        WaitForSingleObject(handle.0 as _, INFINITE);
                    }
                    waker.wake();
                });
                Poll::Pending
            }
        }
    }
}
