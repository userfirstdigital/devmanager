//! Registry adapter for an already-attested Linux cgroup session.

use std::collections::BTreeSet;
use std::sync::{Mutex, TryLockError};
use std::time::{Duration, Instant};

use super::{
    collect_exact_job_observations_until, CompletionReceiverToken, JobMemberObservation,
    MAX_JOB_PROCESS_ID_CAPACITY,
};
use crate::process::linux_cgroup::LinuxCgroup;
use crate::process::registry::{
    JobCompletionEvent, JobCompletionMessage, JobMemberInfo, ManagedProcessFence,
};

#[derive(Debug, Default)]
struct CompletionState {
    seen: BTreeSet<u32>,
    zero_reported: bool,
}

#[derive(Debug)]
pub(crate) struct ManagedProcessJob {
    session: LinuxCgroup,
    fence: Option<ManagedProcessFence>,
    completion: Mutex<CompletionState>,
}

fn check(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        Err("Linux managed process deadline expired".into())
    } else {
        Ok(())
    }
}

impl ManagedProcessJob {
    pub(crate) fn from_linux_session(session: LinuxCgroup) -> Self {
        Self {
            session,
            fence: None,
            completion: Mutex::new(CompletionState::default()),
        }
    }
    // Legacy unit fixtures use None when they have no native session. Concrete
    // Linux jobs can only be minted by the gated cgroup launcher above.
    #[cfg(test)]
    pub(crate) fn create() -> Result<Option<Self>, String> {
        Ok(None)
    }
    pub(crate) fn internal_name(&self) -> &str {
        self.session.internal_name()
    }
    pub(crate) fn active_process_ids(&self) -> Result<Vec<u32>, String> {
        self.active_process_ids_until(Instant::now() + Duration::from_secs(5))
    }
    pub(crate) fn active_process_ids_until(&self, deadline: Instant) -> Result<Vec<u32>, String> {
        self.session
            .active_process_ids(deadline)
            .map_err(|e| e.to_string())
    }
    pub(crate) fn active_process_observations_until(
        &self,
        deadline: Instant,
        max_members: usize,
    ) -> Result<Vec<JobMemberObservation>, String> {
        check(deadline)?;
        if max_members == 0 || max_members > MAX_JOB_PROCESS_ID_CAPACITY {
            return Err("invalid Linux managed process member bound".into());
        }
        collect_exact_job_observations_until(
            self.active_process_ids_until(deadline),
            |pid| self.inspect_process_until(pid, deadline),
            deadline,
            max_members,
        )
    }
    pub(crate) fn terminate_tree(&self) -> Result<(), String> {
        self.session.terminate().map_err(|e| e.to_string())
    }
    pub fn terminate_members(&self) -> Result<(), String> {
        self.terminate_tree()
    }
    pub(crate) fn terminate_tree_until(&self, deadline: Instant) -> Result<(), String> {
        check(deadline)?;
        self.terminate_tree()?;
        check(deadline)
    }
    pub fn wait_for_active_process_zero(&self, deadline: Instant) -> Result<bool, String> {
        loop {
            check(deadline)?;
            if self.session.settled() && self.active_process_ids_until(deadline)?.is_empty() {
                return Ok(true);
            }
            if Instant::now() + Duration::from_millis(5) >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    pub fn inspect_process(&self, pid: u32) -> Result<JobMemberInfo, String> {
        self.inspect_process_until(pid, Instant::now() + Duration::from_secs(5))
    }
    pub(crate) fn inspect_process_until(
        &self,
        pid: u32,
        deadline: Instant,
    ) -> Result<JobMemberInfo, String> {
        let identity = self
            .session
            .inspect_member(pid, deadline)
            .map_err(|e| e.to_string())?;
        Ok(JobMemberInfo::new(identity, None))
    }
    pub(crate) fn bind_completion_fence(
        &mut self,
        fence: ManagedProcessFence,
    ) -> Result<(), String> {
        if fence.root() != self.session.root() {
            return Err("Linux completion fence does not name this session root".into());
        }
        if let Some(bound) = &self.fence {
            return if bound == &fence {
                Ok(())
            } else {
                Err("Linux completion fence is already bound to another generation".into())
            };
        }
        self.fence = Some(fence);
        Ok(())
    }
    pub(crate) fn shutdown_for_release(&mut self) -> Result<(), String> {
        self.shutdown_for_release_until(Instant::now() + Duration::from_secs(5))
    }
    pub(crate) fn shutdown_for_release_until(&mut self, deadline: Instant) -> Result<(), String> {
        self.session.join(deadline).map_err(|e| e.to_string())
    }
    #[cfg(test)]
    pub(crate) fn drain_completion_messages(&self) -> Vec<JobCompletionMessage> {
        self.drain_completion_messages_until(Instant::now() + Duration::from_secs(5))
            .unwrap_or_default()
    }
    pub(crate) fn drain_completion_messages_until(
        &self,
        deadline: Instant,
    ) -> Result<Vec<JobCompletionMessage>, String> {
        check(deadline)?;
        let fence = self
            .fence
            .as_ref()
            .ok_or("Linux process completion fence is not bound")?;
        let mut state = loop {
            check(deadline)?;
            match self.completion.try_lock() {
                Ok(state) => break state,
                Err(TryLockError::WouldBlock) => std::thread::yield_now(),
                Err(TryLockError::Poisoned(_)) => {
                    return Err("Linux process completion state poisoned".into())
                }
            }
        };
        let current: BTreeSet<_> = self
            .active_process_ids_until(deadline)?
            .into_iter()
            .collect();
        // Kernel membership at the exec barrier is not resumed execution.
        if !self.session.resumed() && !current.is_empty() {
            return Ok(Vec::new());
        }
        let receiver = CompletionReceiverToken::issue();
        let mut messages = Vec::new();
        for pid in current.difference(&state.seen) {
            messages.push(JobCompletionMessage::from_completion_receiver(
                &receiver,
                fence.clone(),
                JobCompletionEvent::NewProcess { pid: *pid },
            ));
        }
        for pid in state.seen.difference(&current) {
            messages.push(JobCompletionMessage::from_completion_receiver(
                &receiver,
                fence.clone(),
                JobCompletionEvent::ExitProcess { pid: *pid },
            ));
        }
        // Only the concrete backend can issue zero authority, after its
        // guardian/wrapper has settled and authoritative membership is empty.
        // An empty intermediate sample during startup is insufficient.
        let report_zero = current.is_empty() && self.session.settled() && !state.zero_reported;
        if report_zero {
            messages.push(JobCompletionMessage::from_completion_receiver(
                &receiver,
                fence.clone(),
                JobCompletionEvent::ActiveProcessZero,
            ));
        }
        check(deadline)?;
        state.zero_reported |= report_zero;
        state.seen = current;
        Ok(messages)
    }
}
