//! Async rendezvous for the existing bounded remote OS-worker/reaper lane.
//!
//! Dropping a Tokio blocking JoinHandle detaches its work and can make runtime
//! shutdown unbounded. This guard transfers the exact OS worker to the existing
//! reaper instead. Cancellation wins only before explicit mutation admission;
//! an admitted durable operation remains owned until it settles.
//!
//! Completed results stay in worker custody until the receiver explicitly claims
//! them before the deadline. Timeout, drop, abort-before-claim, and sender
//! failure dispose `T` on the owned OS worker — never on an async caller.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use tokio::sync::{oneshot, Notify};

use super::{defer_unowned_remote_worker, RemoteWorker};

const QUEUED: u8 = 0;
const ADMITTED: u8 = 1;
const CANCELLED: u8 = 2;
const FINISHED: u8 = 3;
const FINISHED_UNADMITTED: u8 = 4;

#[derive(Clone)]
pub(crate) struct RemoteWorkAdmission {
    state: Arc<AtomicU8>,
    deadline: Instant,
}

impl RemoteWorkAdmission {
    /// Cooperative stop check for bounded read-only work. This never admits a
    /// write and is not evidence that an already-admitted mutation was undone.
    pub(crate) fn cancellation_requested(&self) -> bool {
        Instant::now() >= self.deadline || self.state.load(Ordering::Acquire) == CANCELLED
    }

    /// Call after acquiring any store lock and immediately before mutation.
    /// A later disconnect cannot roll back an already-admitted durable write.
    pub(crate) fn try_admit(&self) -> bool {
        if Instant::now() >= self.deadline {
            self.cancel_queued();
            return false;
        }
        self.state
            .compare_exchange(QUEUED, ADMITTED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn cancel_queued(&self) {
        let _ = self
            .state
            .compare_exchange(QUEUED, CANCELLED, Ordering::AcqRel, Ordering::Acquire);
    }

    fn finish(&self) {
        // Completion is not mutation admission: read-only or rejected work
        // must not acquire write authority merely by returning a result.
        let _ = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                Some(if matches!(state, ADMITTED | FINISHED) {
                    FINISHED
                } else {
                    FINISHED_UNADMITTED
                })
            });
    }

    fn deadline_error(&self) -> RemoteWorkError {
        self.cancel_queued();
        RemoteWorkError::Deadline {
            admitted: matches!(self.state.load(Ordering::Acquire), ADMITTED | FINISHED),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteWorkError {
    Unavailable,
    /// An admitted write may still settle; this is not proof of cancellation.
    Deadline {
        admitted: bool,
    },
}

struct HandoffInner<T> {
    value: Option<T>,
    ready: bool,
    claimed: bool,
    abandoned: bool,
}

/// Bounded synchronous custody: the OS worker owns `T` until claim or dispose.
struct ResultHandoff<T> {
    inner: Mutex<HandoffInner<T>>,
    cv: Condvar,
}

impl<T> ResultHandoff<T> {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(HandoffInner {
                value: None,
                ready: false,
                claimed: false,
                abandoned: false,
            }),
            cv: Condvar::new(),
        })
    }

    fn is_ready(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).ready
    }

    fn abandon(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.abandoned = true;
        self.cv.notify_all();
    }

    /// Place `value`, wake waiters, then wait for claim/abandon/deadline.
    /// Unclaimed values are dropped on this OS worker before returning.
    fn publish_and_await_custody_release(
        &self,
        value: T,
        ready_tx: oneshot::Sender<()>,
        deadline: Instant,
    ) {
        {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.value = Some(value);
            guard.ready = true;
            self.cv.notify_all();
        }
        let _ = ready_tx.send(());

        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        while !guard.claimed && !guard.abandoned {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let wait = deadline.saturating_duration_since(now);
            let (next, wait_result) = self
                .cv
                .wait_timeout(guard, wait)
                .unwrap_or_else(|e| e.into_inner());
            guard = next;
            if wait_result.timed_out() && Instant::now() >= deadline {
                break;
            }
        }
        let unclaimed = if !guard.claimed {
            guard.value.take()
        } else {
            None
        };
        drop(guard);
        // Dispose only after releasing the handoff mutex: T's Drop may join
        // runtimes/threads and must not block abandon/is_ready on this lock.
        drop(unclaimed);
    }

    fn try_claim(&self, deadline: Instant) -> ClaimOutcome<T> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.claimed || guard.abandoned || !guard.ready {
            return ClaimOutcome::Unavailable;
        }
        if Instant::now() >= deadline {
            return ClaimOutcome::Expired;
        }
        let Some(value) = guard.value.take() else {
            return ClaimOutcome::Unavailable;
        };
        guard.claimed = true;
        self.cv.notify_all();
        ClaimOutcome::Taken(value)
    }
}

enum ClaimOutcome<T> {
    Taken(T),
    /// Ready but past the original deadline; ownership stays with the worker.
    Expired,
    Unavailable,
}

pub(crate) struct RemoteBlockingWork<T> {
    worker: Option<RemoteWorker>,
    handoff: Arc<ResultHandoff<T>>,
    ready: Option<oneshot::Receiver<()>>,
    ready_observed: bool,
    admission: RemoteWorkAdmission,
}

impl<T: Send + 'static> RemoteBlockingWork<T> {
    /// Nonblocking claim for native event loops without an entered Tokio runtime.
    /// A pending result stays in worker custody; no timer, task, or waiter is created.
    pub(crate) fn try_take(&mut self) -> Result<Option<T>, RemoteWorkError> {
        if Instant::now() >= self.admission.deadline {
            return Err(self.admission.deadline_error());
        }
        self.try_take_if_ready()
    }

    /// Explicit synchronous callers use the same owned job without nesting a
    /// Tokio runtime. Park on the result's waker under the original deadline.
    pub(crate) fn wait_blocking(&mut self) -> Result<T, RemoteWorkError> {
        use futures_util::task::{waker_ref, ArcWake};
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct WakeThread(std::thread::Thread);
        impl ArcWake for WakeThread {
            fn wake_by_ref(this: &Arc<Self>) {
                this.0.unpark();
            }
        }
        let wake = Arc::new(WakeThread(std::thread::current()));
        let waker = waker_ref(&wake);
        let mut context = Context::from_waker(&waker);
        loop {
            if let Some(result) = self.try_take_if_ready()? {
                return Ok(result);
            }
            let remaining = self
                .admission
                .deadline
                .saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(self.admission.deadline_error());
            }
            let Some(ready) = self.ready.as_mut() else {
                return Err(RemoteWorkError::Unavailable);
            };
            match Pin::new(ready).poll(&mut context) {
                Poll::Ready(Ok(())) => {
                    self.ready_observed = true;
                    self.ready = None;
                }
                Poll::Ready(Err(_)) => return Err(RemoteWorkError::Unavailable),
                Poll::Pending => std::thread::park_timeout(remaining),
            }
        }
    }

    pub(crate) fn spawn(
        name: &'static str,
        deadline: Instant,
        work: impl FnOnce(RemoteWorkAdmission) -> T + Send + 'static,
    ) -> Result<Self, RemoteWorkError> {
        let admission = RemoteWorkAdmission {
            state: Arc::new(AtomicU8::new(QUEUED)),
            deadline,
        };
        let worker_admission = admission.clone();
        let handoff = ResultHandoff::new();
        let worker_handoff = Arc::clone(&handoff);
        let (ready_tx, ready_rx) = oneshot::channel();
        let worker = RemoteWorker::try_spawn(name, None, move || {
            let result = work(worker_admission.clone());
            worker_admission.finish();
            worker_handoff.publish_and_await_custody_release(result, ready_tx, deadline);
        })
        .map_err(|_| RemoteWorkError::Unavailable)?;
        Ok(Self {
            worker: Some(worker),
            handoff,
            ready: Some(ready_rx),
            ready_observed: false,
            admission,
        })
    }

    pub(crate) async fn wait(&mut self) -> Result<T, RemoteWorkError> {
        if let Some(result) = self.try_take_if_ready()? {
            return Ok(result);
        }
        if !self.ready_observed {
            let Some(ready) = self.ready.as_mut() else {
                return Err(RemoteWorkError::Unavailable);
            };
            match tokio::time::timeout_at(
                tokio::time::Instant::from_std(self.admission.deadline),
                ready,
            )
            .await
            {
                Ok(Ok(())) => {
                    self.ready_observed = true;
                    self.ready = None;
                }
                Ok(Err(_)) => return Err(RemoteWorkError::Unavailable),
                Err(_) => return Err(self.admission.deadline_error()),
            }
        }
        if let Some(result) = self.try_take_if_ready()? {
            return Ok(result);
        }
        Err(RemoteWorkError::Unavailable)
    }

    /// Claim only after readiness is known. Deadline is checked under the
    /// handoff lock immediately before taking `T`; expired ready results stay
    /// in worker custody for dispose.
    fn try_take_if_ready(&mut self) -> Result<Option<T>, RemoteWorkError> {
        if !self.ready_observed {
            if self.handoff.is_ready() {
                self.ready_observed = true;
                self.ready = None;
            } else {
                return Ok(None);
            }
        }
        match self.handoff.try_claim(self.admission.deadline) {
            ClaimOutcome::Taken(result) => Ok(Some(result)),
            ClaimOutcome::Expired => Err(self.admission.deadline_error()),
            ClaimOutcome::Unavailable => Err(RemoteWorkError::Unavailable),
        }
    }
}

impl<T> Drop for RemoteBlockingWork<T> {
    fn drop(&mut self) {
        self.admission.cancel_queued();
        self.handoff.abandon();
        self.ready = None;
        if let Some(worker) = self.worker.take() {
            // Never block an async executor or abandon an unfinished OS worker.
            // The bounded reaper retains its admission permit until real join.
            defer_unowned_remote_worker(worker);
        }
    }
}

/// Out-of-band stop signal for a long-lived owned background OS worker.
///
/// Distinct from [`RemoteBlockingWork`] mutation admission: this never invents a
/// second reaper. Drop of the owning [`RemoteBackgroundWork`] requests stop and
/// transfers an unfinished worker to `defer_unowned_remote_worker`.
#[derive(Clone)]
pub(crate) struct BackgroundWorkStop {
    stop_flag: Arc<AtomicBool>,
    stop_notify: Arc<Notify>,
}

impl BackgroundWorkStop {
    pub(crate) fn is_requested(&self) -> bool {
        self.stop_flag.load(Ordering::Acquire)
    }

    /// Wait until stop is requested without losing a wakeup that races between
    /// the flag check and Notify registration.
    pub(crate) async fn cancelled(&self) {
        loop {
            let notified = self.stop_notify.notified();
            tokio::pin!(notified);
            // Register interest before observing the flag.
            if notified.as_mut().enable() {
                if self.is_requested() {
                    return;
                }
                continue;
            }
            if self.is_requested() {
                return;
            }
            notified.await;
            if self.is_requested() {
                return;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteBackgroundWorkError {
    Unavailable,
}

impl From<RemoteBackgroundWorkError> for RemoteWorkError {
    fn from(value: RemoteBackgroundWorkError) -> Self {
        match value {
            RemoteBackgroundWorkError::Unavailable => RemoteWorkError::Unavailable,
        }
    }
}

/// Long-lived owned OS worker on the existing bounded `RemoteWorker` / reaper lane.
pub(crate) struct RemoteBackgroundWork {
    worker: Option<RemoteWorker>,
    stop_flag: Arc<AtomicBool>,
    stop_notify: Arc<Notify>,
    finished_rx: Option<oneshot::Receiver<()>>,
}

impl RemoteBackgroundWork {
    pub(crate) fn spawn(
        name: impl Into<String>,
        job: impl FnOnce(BackgroundWorkStop) + Send + 'static,
    ) -> Result<Self, RemoteBackgroundWorkError> {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_notify = Arc::new(Notify::new());
        let stop = BackgroundWorkStop {
            stop_flag: Arc::clone(&stop_flag),
            stop_notify: Arc::clone(&stop_notify),
        };
        let (finished_tx, finished_rx) = oneshot::channel();
        let worker = RemoteWorker::try_spawn(name, None, move || {
            struct FinishOnDrop(Option<oneshot::Sender<()>>);
            impl Drop for FinishOnDrop {
                fn drop(&mut self) {
                    if let Some(tx) = self.0.take() {
                        let _ = tx.send(());
                    }
                }
            }
            let _finish = FinishOnDrop(Some(finished_tx));
            job(stop);
        })
        .map_err(|_| RemoteBackgroundWorkError::Unavailable)?;
        Ok(Self {
            worker: Some(worker),
            stop_flag,
            stop_notify,
            finished_rx: Some(finished_rx),
        })
    }

    pub(crate) fn request_stop(&self) {
        self.stop_flag.store(true, Ordering::Release);
        self.stop_notify.notify_waiters();
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(RemoteWorker::is_finished)
    }

    /// Async wait until the OS worker exits. Does not block the executor on a
    /// live join; once finished, `join` is immediate. Dropping this future
    /// still leaves the worker owned (Drop → stop + reaper defer).
    pub(crate) async fn wait(&mut self) {
        if let Some(rx) = self.finished_rx.as_mut() {
            let _ = rx.await;
        }
        self.finished_rx = None;
        // The job's completion signal precedes the enclosing OS thread's final
        // teardown. Never block an async executor in join during that interval.
        while self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for RemoteBackgroundWork {
    fn drop(&mut self) {
        self.request_stop();
        self.finished_rx = None;
        if let Some(worker) = self.worker.take() {
            if worker.is_finished() {
                let _ = worker.join();
            } else {
                defer_unowned_remote_worker(worker);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread::ThreadId;
    use std::time::Duration;

    /// Worker-only drop probe: canceled/unclaimed paths must not run on Tokio.
    struct DropProbe {
        drop_tid: Arc<Mutex<Option<ThreadId>>>,
        signal: mpsc::SyncSender<()>,
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            assert!(
                tokio::runtime::Handle::try_current().is_err(),
                "DropProbe must not drop on a Tokio worker"
            );
            *self.drop_tid.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(std::thread::current().id());
            let _ = self.signal.try_send(());
        }
    }

    /// Plain recorder for successful claims (caller custody is allowed).
    struct RecordingProbe {
        drop_tid: Arc<Mutex<Option<ThreadId>>>,
        signal: mpsc::SyncSender<()>,
    }

    impl Drop for RecordingProbe {
        fn drop(&mut self) {
            *self.drop_tid.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(std::thread::current().id());
            let _ = self.signal.try_send(());
        }
    }

    /// Owns a multi-thread Tokio runtime whose Drop uses `block_on`.
    struct RuntimeOwningResult {
        runtime: Option<tokio::runtime::Runtime>,
        drop_tid: Arc<Mutex<Option<ThreadId>>>,
        signal: mpsc::SyncSender<()>,
    }

    impl Drop for RuntimeOwningResult {
        fn drop(&mut self) {
            assert!(
                tokio::runtime::Handle::try_current().is_err(),
                "runtime-owning result must not drop on a Tokio worker"
            );
            *self.drop_tid.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(std::thread::current().id());
            if let Some(runtime) = self.runtime.take() {
                // Nested-runtime panic if this Drop runs on a Tokio worker.
                let _ = runtime.block_on(async { 1u8 });
            }
            let _ = self.signal.try_send(());
        }
    }

    /// Destructor that blocks until the test releases it.
    struct BlockedDrop {
        entered: Option<mpsc::SyncSender<()>>,
        release: mpsc::Receiver<()>,
    }

    impl Drop for BlockedDrop {
        fn drop(&mut self) {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            let _ = self.release.recv_timeout(Duration::from_secs(5));
        }
    }

    fn wait_drop_signal(rx: &mpsc::Receiver<()>) {
        rx.recv_timeout(Duration::from_secs(5))
            .expect("result disposed on worker");
    }

    fn wait_until_ready<T>(handoff: &ResultHandoff<T>, bound: Instant) {
        while Instant::now() < bound {
            if handoff.is_ready() {
                return;
            }
            std::thread::park_timeout(Duration::from_millis(1));
        }
        panic!("handoff not ready before absolute deadline");
    }

    fn join_blocking_job<T>(job: &mut RemoteBlockingWork<T>) {
        let worker = job
            .worker
            .take()
            .expect("blocking job still owns its RemoteWorker");
        worker
            .completion_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker completed before join");
        let _ = worker.join();
    }

    fn assert_worker_permit_reusable() {
        let mut probe = RemoteBlockingWork::spawn(
            "handoff-permit-reuse",
            Instant::now() + Duration::from_secs(5),
            |_| (),
        )
        .expect("permit reusable after prior job settled");
        assert!(probe.wait_blocking().is_ok());
        join_blocking_job(&mut probe);
    }

    #[test]
    fn native_nonblocking_poll_needs_no_tokio_runtime_and_claims_once() {
        assert!(tokio::runtime::Handle::try_current().is_err());
        let (release, held) = mpsc::sync_channel(1);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut job = RemoteBlockingWork::spawn("native-poll-no-runtime", deadline, move |_| {
            held.recv_timeout(Duration::from_secs(5)).expect("released");
            42u8
        })
        .expect("worker");
        assert_eq!(job.try_take(), Ok(None));
        release.send(()).expect("release");
        wait_until_ready(&job.handoff, deadline);
        assert_eq!(job.try_take(), Ok(Some(42)));
        assert_eq!(job.try_take(), Err(RemoteWorkError::Unavailable));
        join_blocking_job(&mut job);
    }

    #[test]
    fn native_nonblocking_poll_preserves_admitted_deadline_and_worker_custody() {
        let (drop_tx, drop_rx) = mpsc::sync_channel(1);
        let drop_tid = Arc::new(Mutex::new(None));
        let probe = DropProbe {
            drop_tid: Arc::clone(&drop_tid),
            signal: drop_tx,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut job =
            RemoteBlockingWork::spawn("native-poll-expired", deadline, move |admission| {
                assert!(admission.try_admit());
                probe
            })
            .expect("worker");
        wait_until_ready(&job.handoff, deadline);
        // Expire the caller's original-deadline check deterministically without sleeping.
        job.admission.deadline = Instant::now();
        assert!(matches!(
            job.try_take(),
            Err(RemoteWorkError::Deadline { admitted: true })
        ));
        job.handoff.abandon();
        wait_drop_signal(&drop_rx);
        assert_ne!(
            *drop_tid.lock().expect("drop thread"),
            Some(std::thread::current().id())
        );
        join_blocking_job(&mut job);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_background_wait_can_resume_and_join_the_same_worker() {
        let (release, held) = std::sync::mpsc::sync_channel(1);
        let mut job = RemoteBackgroundWork::spawn("background-wait-cancel-test", move |_| {
            let _ = held.recv_timeout(Duration::from_secs(5));
        })
        .expect("worker");
        assert!(tokio::time::timeout(Duration::from_millis(10), job.wait())
            .await
            .is_err());
        assert!(job.finished_rx.is_some());
        assert!(job.worker.is_some());
        release.send(()).expect("release");
        tokio::time::timeout(Duration::from_secs(5), job.wait())
            .await
            .expect("same worker joined");
        assert!(job.worker.is_none());
        assert!(job.finished_rx.is_none());
    }

    #[test]
    fn finishing_preserves_whether_a_mutation_was_ever_admitted() {
        for initial in [QUEUED, ADMITTED, CANCELLED] {
            let admission = RemoteWorkAdmission {
                state: Arc::new(AtomicU8::new(initial)),
                deadline: Instant::now(),
            };
            admission.finish();
            assert_eq!(
                admission.deadline_error(),
                RemoteWorkError::Deadline {
                    admitted: initial == ADMITTED
                }
            );
            assert!(!admission.try_admit());
        }
    }

    #[test]
    fn read_only_cancellation_checks_never_admit_mutation() {
        let admission = RemoteWorkAdmission {
            state: Arc::new(AtomicU8::new(QUEUED)),
            deadline: Instant::now() + Duration::from_secs(5),
        };
        assert!(!admission.cancellation_requested());
        assert_eq!(admission.state.load(Ordering::Acquire), QUEUED);
        admission.cancel_queued();
        assert!(admission.cancellation_requested());
        assert!(!admission.try_admit());

        let expired = RemoteWorkAdmission {
            state: Arc::new(AtomicU8::new(QUEUED)),
            deadline: Instant::now(),
        };
        assert!(expired.cancellation_requested());
        assert_eq!(expired.state.load(Ordering::Acquire), QUEUED);
        assert!(!expired.try_admit());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn explicit_blocking_wait_does_not_nest_an_async_runtime() {
        let mut job = RemoteBlockingWork::spawn(
            "remote-blocking-wait-test",
            Instant::now() + Duration::from_secs(5),
            |_| 42,
        )
        .expect("owned worker");
        assert_eq!(job.wait_blocking().expect("result"), 42);
    }

    #[tokio::test]
    async fn dropped_connect_work_cannot_admit_after_waiting_for_a_store() {
        let (release, held) = std::sync::mpsc::sync_channel(1);
        let (finished, observed) = oneshot::channel();
        let job = RemoteBlockingWork::spawn(
            "connect-admission-cancel-test",
            Instant::now() + Duration::from_secs(5),
            move |admission| {
                let _ = held.recv_timeout(Duration::from_secs(5));
                let _ = finished.send(admission.try_admit());
            },
        )
        .unwrap();
        drop(job);
        release.send(()).unwrap();
        assert!(!tokio::time::timeout(Duration::from_secs(5), observed)
            .await
            .unwrap()
            .unwrap());
    }

    #[test]
    fn admitted_connect_write_is_not_falsely_reported_cancelled() {
        let admission = RemoteWorkAdmission {
            state: Arc::new(AtomicU8::new(QUEUED)),
            deadline: Instant::now() + Duration::from_secs(5),
        };
        assert!(admission.try_admit());
        admission.cancel_queued();
        assert_eq!(admission.state.load(Ordering::Acquire), ADMITTED);
        assert!(!admission.try_admit());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drop_before_work_finishes_disposes_on_worker_thread() {
        let caller = std::thread::current().id();
        let worker_tid = Arc::new(Mutex::new(None));
        let drop_tid = Arc::new(Mutex::new(None));
        let (drop_tx, drop_rx) = mpsc::sync_channel(1);
        let (release, held) = mpsc::sync_channel(0);
        let worker_tid_for_job = Arc::clone(&worker_tid);
        let drop_tid_for_job = Arc::clone(&drop_tid);
        let job = RemoteBlockingWork::spawn(
            "handoff-drop-before-finish",
            Instant::now() + Duration::from_secs(5),
            move |_| {
                *worker_tid_for_job.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(std::thread::current().id());
                let _ = held.recv_timeout(Duration::from_secs(5));
                DropProbe {
                    drop_tid: drop_tid_for_job,
                    signal: drop_tx,
                }
            },
        )
        .expect("job");
        drop(job);
        release.send(()).expect("release worker");
        wait_drop_signal(&drop_rx);
        let observed_worker = worker_tid
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("worker tid");
        let observed_drop = drop_tid
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("drop tid");
        assert_eq!(observed_drop, observed_worker);
        assert_ne!(observed_drop, caller);
        assert_worker_permit_reusable();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drop_after_ready_before_claim_disposes_on_worker_thread() {
        let caller = std::thread::current().id();
        let worker_tid = Arc::new(Mutex::new(None));
        let drop_tid = Arc::new(Mutex::new(None));
        let (drop_tx, drop_rx) = mpsc::sync_channel(1);
        let (ready_gate_tx, ready_gate_rx) = mpsc::sync_channel(0);
        let (publish_tx, publish_rx) = mpsc::sync_channel(1);
        let worker_tid_for_job = Arc::clone(&worker_tid);
        let drop_tid_for_job = Arc::clone(&drop_tid);
        let job = RemoteBlockingWork::spawn(
            "handoff-drop-after-ready",
            Instant::now() + Duration::from_secs(5),
            move |_| {
                *worker_tid_for_job.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(std::thread::current().id());
                let _ = ready_gate_rx.recv_timeout(Duration::from_secs(5));
                let probe = DropProbe {
                    drop_tid: drop_tid_for_job,
                    signal: drop_tx,
                };
                let _ = publish_tx.send(());
                probe
            },
        )
        .expect("job");
        ready_gate_tx.send(()).expect("allow publish");
        publish_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("work returned");
        wait_until_ready(&job.handoff, Instant::now() + Duration::from_secs(5));
        drop(job);
        wait_drop_signal(&drop_rx);
        let observed_worker = worker_tid
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("worker tid");
        let observed_drop = drop_tid
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("drop tid");
        assert_eq!(observed_drop, observed_worker);
        assert_ne!(observed_drop, caller);
        assert_worker_permit_reusable();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn expired_late_result_disposes_on_worker_not_caller() {
        let caller = std::thread::current().id();
        let worker_tid = Arc::new(Mutex::new(None));
        let drop_tid = Arc::new(Mutex::new(None));
        let (drop_tx, drop_rx) = mpsc::sync_channel(1);
        let (release, held) = mpsc::sync_channel(0);
        let (published_tx, published_rx) = mpsc::sync_channel(1);
        let deadline = Instant::now() + Duration::from_millis(200);
        let worker_tid_for_job = Arc::clone(&worker_tid);
        let drop_tid_for_job = Arc::clone(&drop_tid);
        let mut job =
            RemoteBlockingWork::spawn("handoff-expired-late-result", deadline, move |_| {
                *worker_tid_for_job.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(std::thread::current().id());
                let _ = held.recv_timeout(Duration::from_secs(5));
                let probe = DropProbe {
                    drop_tid: drop_tid_for_job,
                    signal: drop_tx,
                };
                let _ = published_tx.send(());
                probe
            })
            .expect("job");
        while Instant::now() < deadline {
            std::thread::park_timeout(Duration::from_millis(1));
        }
        release.send(()).expect("publish after deadline");
        published_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("published");
        wait_until_ready(&job.handoff, Instant::now() + Duration::from_secs(5));
        let err = job.wait().await;
        assert!(matches!(err, Err(RemoteWorkError::Deadline { .. })));
        wait_drop_signal(&drop_rx);
        let observed_worker = worker_tid
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("worker tid");
        let observed_drop = drop_tid
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("drop tid");
        assert_eq!(observed_drop, observed_worker);
        assert_ne!(observed_drop, caller);
        join_blocking_job(&mut job);
        assert_worker_permit_reusable();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ready_but_expired_direct_claim_keeps_worker_custody() {
        let mut job = RemoteBlockingWork::spawn(
            "handoff-ready-expired-claim",
            Instant::now() + Duration::from_secs(5),
            move |admission| {
                assert!(admission.try_admit());
                99u32
            },
        )
        .expect("job");
        wait_until_ready(&job.handoff, Instant::now() + Duration::from_secs(5));
        // Deadline checked under the claim lock; a past deadline must not take T.
        assert!(matches!(
            job.handoff.try_claim(Instant::now()),
            ClaimOutcome::Expired
        ));
        assert!(matches!(
            job.handoff.try_claim(job.admission.deadline),
            ClaimOutcome::Taken(99)
        ));
        assert!(matches!(
            job.wait().await,
            Err(RemoteWorkError::Unavailable)
        ));
        join_blocking_job(&mut job);
        assert_worker_permit_reusable();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ready_but_expired_wait_maps_deadline_with_admission_fact() {
        let deadline = Instant::now() + Duration::from_millis(150);
        let mut job =
            RemoteBlockingWork::spawn("handoff-ready-expired-wait", deadline, move |admission| {
                assert!(admission.try_admit());
                ()
            })
            .expect("job");
        wait_until_ready(&job.handoff, Instant::now() + Duration::from_secs(5));
        while Instant::now() < deadline {
            std::thread::park_timeout(Duration::from_millis(1));
        }
        assert_eq!(
            job.wait().await,
            Err(RemoteWorkError::Deadline { admitted: true })
        );
        join_blocking_job(&mut job);
        assert_worker_permit_reusable();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_destructor_does_not_hold_handoff_mutex() {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let (published_tx, published_rx) = mpsc::sync_channel(1);
        let mut job = RemoteBlockingWork::spawn(
            "handoff-blocked-destructor",
            Instant::now() + Duration::from_secs(5),
            move |_| {
                let probe = BlockedDrop {
                    entered: Some(entered_tx),
                    release: release_rx,
                };
                let _ = published_tx.send(());
                probe
            },
        )
        .expect("job");
        published_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("published");
        let handoff = Arc::clone(&job.handoff);
        wait_until_ready(&handoff, Instant::now() + Duration::from_secs(5));
        // Keep the exact OS worker so we can join it after releasing Drop.
        let worker = job.worker.take().expect("worker");
        drop(job);
        // abandon already returned; readiness must stay observable while Drop blocks.
        assert!(handoff.is_ready());
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("blocked destructor entered on worker");
        assert!(handoff.is_ready());
        handoff.abandon();
        release_tx.send(()).expect("release blocked destructor");
        worker
            .completion_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker completed after destructor release");
        let _ = worker.join();
        assert_worker_permit_reusable();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn aborted_wait_keeps_job_resumable_for_later_claim() {
        let (release, held) = mpsc::sync_channel(0);
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let mut job = RemoteBlockingWork::spawn(
            "handoff-aborted-wait-resume",
            Instant::now() + Duration::from_secs(5),
            move |_| {
                let _ = started_tx.send(());
                let _ = held.recv_timeout(Duration::from_secs(5));
                7u32
            },
        )
        .expect("job");
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker started");
        assert!(tokio::time::timeout(Duration::from_millis(20), job.wait())
            .await
            .is_err());
        assert!(job.worker.is_some());
        assert!(!job.ready_observed);
        assert!(job.ready.is_some());
        release.send(()).expect("release");
        assert_eq!(job.wait().await.expect("claimed after resume"), 7);
        join_blocking_job(&mut job);
        assert_worker_permit_reusable();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn successful_claim_transfers_exactly_once() {
        let drop_tid = Arc::new(Mutex::new(None));
        let (drop_tx, drop_rx) = mpsc::sync_channel(1);
        let drop_tid_for_job = Arc::clone(&drop_tid);
        let mut job = RemoteBlockingWork::spawn(
            "handoff-claim-once",
            Instant::now() + Duration::from_secs(5),
            move |_| RecordingProbe {
                drop_tid: drop_tid_for_job,
                signal: drop_tx,
            },
        )
        .expect("job");
        let claimed = job.wait().await.expect("claim");
        assert!(matches!(
            job.handoff.try_claim(job.admission.deadline),
            ClaimOutcome::Unavailable
        ));
        assert!(matches!(
            job.wait().await,
            Err(RemoteWorkError::Unavailable)
        ));
        let caller = std::thread::current().id();
        drop(claimed);
        wait_drop_signal(&drop_rx);
        let observed_drop = drop_tid
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("drop tid");
        assert_eq!(observed_drop, caller);
        join_blocking_job(&mut job);
        assert_worker_permit_reusable();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_runtime_owning_result_drops_on_worker() {
        let caller = std::thread::current().id();
        let worker_tid = Arc::new(Mutex::new(None));
        let drop_tid = Arc::new(Mutex::new(None));
        let (drop_tx, drop_rx) = mpsc::sync_channel(1);
        let (release, held) = mpsc::sync_channel(0);
        let worker_tid_for_job = Arc::clone(&worker_tid);
        let drop_tid_for_job = Arc::clone(&drop_tid);
        let job = RemoteBlockingWork::spawn(
            "handoff-runtime-owning-cancel",
            Instant::now() + Duration::from_secs(5),
            move |_| {
                *worker_tid_for_job.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(std::thread::current().id());
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .expect("runtime");
                let _ = held.recv_timeout(Duration::from_secs(5));
                RuntimeOwningResult {
                    runtime: Some(runtime),
                    drop_tid: drop_tid_for_job,
                    signal: drop_tx,
                }
            },
        )
        .expect("job");
        drop(job);
        release.send(()).expect("release");
        wait_drop_signal(&drop_rx);
        let observed_worker = worker_tid
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("worker tid");
        let observed_drop = drop_tid
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("drop tid");
        assert_eq!(observed_drop, observed_worker);
        assert_ne!(observed_drop, caller);
        assert_worker_permit_reusable();
    }
}
