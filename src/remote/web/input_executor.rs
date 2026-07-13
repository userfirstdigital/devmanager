use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::time::Duration;

use crate::remote::presentation::StableSessionKey;

const DEFAULT_MAX_WORKERS: usize = 64;
const DEFAULT_QUEUE_CAPACITY: usize = 32;
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

type WebInputJob = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebInputDispatchError {
    QueueFull,
    WorkerLimit,
    WorkerUnavailable,
}

#[derive(Clone)]
pub(crate) struct WebInputExecutor {
    inner: Arc<WebInputExecutorInner>,
}

struct WebInputExecutorInner {
    workers: Mutex<HashMap<StableSessionKey, WebInputWorker>>,
    max_workers: usize,
    queue_capacity: usize,
    idle_timeout: Duration,
    next_worker_id: AtomicU64,
}

#[derive(Clone)]
struct WebInputWorker {
    id: u64,
    sender: mpsc::SyncSender<WebInputJob>,
}

impl Default for WebInputExecutor {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_WORKERS,
            DEFAULT_QUEUE_CAPACITY,
            DEFAULT_IDLE_TIMEOUT,
        )
    }
}

impl WebInputExecutor {
    pub(crate) fn new(max_workers: usize, queue_capacity: usize, idle_timeout: Duration) -> Self {
        Self {
            inner: Arc::new(WebInputExecutorInner {
                workers: Mutex::new(HashMap::new()),
                max_workers: max_workers.max(1),
                queue_capacity: queue_capacity.max(1),
                idle_timeout,
                next_worker_id: AtomicU64::new(1),
            }),
        }
    }

    pub(crate) fn dispatch(
        &self,
        key: StableSessionKey,
        job: impl FnOnce() + Send + 'static,
    ) -> Result<(), WebInputDispatchError> {
        let mut pending = Some(Box::new(job) as WebInputJob);
        // One retry handles a worker that retired between lookup and send;
        // the third pass sends through the replacement created by pass two.
        for _ in 0..3 {
            let mut workers = self
                .inner
                .workers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(worker) = workers.get(&key) {
                let job = pending.take().expect("web input job dispatched once");
                match worker.sender.try_send(job) {
                    Ok(()) => return Ok(()),
                    Err(mpsc::TrySendError::Full(_job)) => {
                        return Err(WebInputDispatchError::QueueFull);
                    }
                    Err(mpsc::TrySendError::Disconnected(job)) => {
                        pending = Some(job);
                        workers.remove(&key);
                        continue;
                    }
                }
            }
            if workers.len() >= self.inner.max_workers {
                return Err(WebInputDispatchError::WorkerLimit);
            }
            let id = self.inner.next_worker_id.fetch_add(1, Ordering::Relaxed);
            let (sender, receiver) = mpsc::sync_channel(self.inner.queue_capacity);
            let weak = Arc::downgrade(&self.inner);
            let worker_key = key.clone();
            let idle_timeout = self.inner.idle_timeout;
            let spawn = std::thread::Builder::new()
                .name(format!("web-input-{id}"))
                .spawn(move || run_worker(weak, worker_key, id, receiver, idle_timeout));
            if spawn.is_err() {
                return Err(WebInputDispatchError::WorkerUnavailable);
            }
            workers.insert(key.clone(), WebInputWorker { id, sender });
        }
        Err(WebInputDispatchError::WorkerUnavailable)
    }

    #[cfg(test)]
    pub(crate) fn active_workers(&self) -> usize {
        self.inner
            .workers
            .lock()
            .map(|workers| workers.len())
            .unwrap_or(0)
    }
}

fn run_worker(
    executor: Weak<WebInputExecutorInner>,
    key: StableSessionKey,
    id: u64,
    receiver: mpsc::Receiver<WebInputJob>,
    idle_timeout: Duration,
) {
    loop {
        match receiver.recv_timeout(idle_timeout) {
            Ok(job) => job(),
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let Some(executor) = executor.upgrade() else {
                    return;
                };
                let Some(job) = claim_pending_job_or_retire(&executor, &key, id, &receiver) else {
                    return;
                };
                job();
            }
        }
    }
}

fn claim_pending_job_or_retire(
    executor: &WebInputExecutorInner,
    key: &StableSessionKey,
    id: u64,
    receiver: &mpsc::Receiver<WebInputJob>,
) -> Option<WebInputJob> {
    let mut workers = executor
        .workers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !workers.get(key).is_some_and(|worker| worker.id == id) {
        return None;
    }
    match receiver.try_recv() {
        Ok(job) => Some(job),
        Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {
            workers.remove(key);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc as std_mpsc;
    use std::time::Instant;

    #[test]
    fn workers_are_keyed_bounded_and_retire_after_idle() {
        let executor = WebInputExecutor::new(2, 1, Duration::from_millis(20));
        let (release_tx, release_rx) = std_mpsc::channel();
        executor
            .dispatch(StableSessionKey::from_tab("a"), move || {
                let _ = release_rx.recv();
            })
            .unwrap();
        executor
            .dispatch(StableSessionKey::from_tab("b"), || {})
            .unwrap();
        assert_eq!(executor.active_workers(), 2);
        assert_eq!(
            executor.dispatch(StableSessionKey::from_tab("c"), || {}),
            Err(WebInputDispatchError::WorkerLimit)
        );
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while executor.active_workers() != 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(executor.active_workers(), 0);
    }

    #[test]
    fn same_session_is_serial_and_its_queue_is_bounded() {
        let executor = WebInputExecutor::new(1, 1, Duration::from_secs(1));
        let key = StableSessionKey::from_tab("a");
        let (entered_tx, entered_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let (order_tx, order_rx) = std_mpsc::channel();
        let first_order = order_tx.clone();
        executor
            .dispatch(key.clone(), move || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                first_order.send(1).unwrap();
            })
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        executor
            .dispatch(key.clone(), move || order_tx.send(2).unwrap())
            .unwrap();
        assert_eq!(
            executor.dispatch(key, || {}),
            Err(WebInputDispatchError::QueueFull)
        );
        release_tx.send(()).unwrap();
        assert_eq!(order_rx.recv_timeout(Duration::from_secs(1)), Ok(1));
        assert_eq!(order_rx.recv_timeout(Duration::from_secs(1)), Ok(2));
    }

    #[test]
    fn retirement_claims_a_job_queued_at_the_idle_boundary() {
        let executor = WebInputExecutor::new(1, 1, Duration::from_secs(1));
        let key = StableSessionKey::from_tab("a");
        let worker_id = 7;
        let (sender, receiver) = mpsc::sync_channel(1);
        executor.inner.workers.lock().unwrap().insert(
            key.clone(),
            WebInputWorker {
                id: worker_id,
                sender: sender.clone(),
            },
        );
        let (ran_tx, ran_rx) = std_mpsc::channel();
        sender
            .try_send(Box::new(move || ran_tx.send(()).unwrap()))
            .unwrap();

        let job = claim_pending_job_or_retire(&executor.inner, &key, worker_id, &receiver)
            .expect("queued job wins the retirement race");
        assert_eq!(executor.active_workers(), 1);
        job();
        assert_eq!(ran_rx.recv_timeout(Duration::from_secs(1)), Ok(()));

        assert!(claim_pending_job_or_retire(&executor.inner, &key, worker_id, &receiver).is_none());
        assert_eq!(executor.active_workers(), 0);
    }
}
