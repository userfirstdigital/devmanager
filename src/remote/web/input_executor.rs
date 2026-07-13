use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::time::Duration;

use crate::remote::presentation::StableSessionKey;

const DEFAULT_MAX_WORKERS: usize = 64;
const DEFAULT_QUEUE_CAPACITY: usize = 32;
const DEFAULT_MAX_ITEMS: usize = 512;
const DEFAULT_MAX_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

type WebInputJob = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebInputDispatchError {
    BudgetExceeded,
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
    budget: Arc<WebInputBudget>,
}

#[derive(Clone)]
struct WebInputWorker {
    id: u64,
    sender: mpsc::SyncSender<AccountedWebInputJob>,
}

struct AccountedWebInputJob {
    job: WebInputJob,
    reservation: WebInputBudgetReservation,
}

#[derive(Default)]
struct WebInputBudgetUsage {
    items: usize,
    bytes: usize,
}

struct WebInputBudget {
    usage: Mutex<WebInputBudgetUsage>,
    max_items: usize,
    max_bytes: usize,
}

struct WebInputBudgetReservation {
    budget: Arc<WebInputBudget>,
    bytes: usize,
}

impl Drop for WebInputBudgetReservation {
    fn drop(&mut self) {
        let mut usage = self
            .budget
            .usage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        usage.items = usage.items.saturating_sub(1);
        usage.bytes = usage.bytes.saturating_sub(self.bytes);
    }
}

impl WebInputBudget {
    fn reserve(self: &Arc<Self>, bytes: usize) -> Option<WebInputBudgetReservation> {
        let mut usage = self
            .usage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if usage.items >= self.max_items || bytes > self.max_bytes.saturating_sub(usage.bytes) {
            return None;
        }
        usage.items += 1;
        usage.bytes += bytes;
        Some(WebInputBudgetReservation {
            budget: self.clone(),
            bytes,
        })
    }
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
        Self::with_budget(
            max_workers,
            queue_capacity,
            DEFAULT_MAX_ITEMS,
            DEFAULT_MAX_BYTES,
            idle_timeout,
        )
    }

    pub(crate) fn with_budget(
        max_workers: usize,
        queue_capacity: usize,
        max_items: usize,
        max_bytes: usize,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(WebInputExecutorInner {
                workers: Mutex::new(HashMap::new()),
                max_workers: max_workers.max(1),
                queue_capacity: queue_capacity.max(1),
                idle_timeout,
                next_worker_id: AtomicU64::new(1),
                budget: Arc::new(WebInputBudget {
                    usage: Mutex::new(WebInputBudgetUsage::default()),
                    max_items: max_items.max(1),
                    max_bytes: max_bytes.max(1),
                }),
            }),
        }
    }

    pub(crate) fn dispatch(
        &self,
        key: StableSessionKey,
        retained_bytes: usize,
        job: impl FnOnce() + Send + 'static,
    ) -> Result<(), WebInputDispatchError> {
        let Some(reservation) = self.inner.budget.reserve(retained_bytes) else {
            return Err(WebInputDispatchError::BudgetExceeded);
        };
        let mut pending = Some(AccountedWebInputJob {
            job: Box::new(job),
            reservation,
        });
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

    #[cfg(test)]
    pub(crate) fn budget_usage(&self) -> (usize, usize) {
        self.inner
            .budget
            .usage
            .lock()
            .map(|usage| (usage.items, usage.bytes))
            .unwrap_or_default()
    }
}

fn run_worker(
    executor: Weak<WebInputExecutorInner>,
    key: StableSessionKey,
    id: u64,
    receiver: mpsc::Receiver<AccountedWebInputJob>,
    idle_timeout: Duration,
) {
    loop {
        match receiver.recv_timeout(idle_timeout) {
            Ok(job) => run_accounted_job(job),
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let Some(executor) = executor.upgrade() else {
                    return;
                };
                let Some(job) = claim_pending_job_or_retire(&executor, &key, id, &receiver) else {
                    return;
                };
                run_accounted_job(job);
            }
        }
    }
}

fn claim_pending_job_or_retire(
    executor: &WebInputExecutorInner,
    key: &StableSessionKey,
    id: u64,
    receiver: &mpsc::Receiver<AccountedWebInputJob>,
) -> Option<AccountedWebInputJob> {
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

fn run_accounted_job(accounted: AccountedWebInputJob) {
    let AccountedWebInputJob { job, reservation } = accounted;
    job();
    drop(reservation);
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
            .dispatch(StableSessionKey::from_tab("a"), 0, move || {
                let _ = release_rx.recv();
            })
            .unwrap();
        executor
            .dispatch(StableSessionKey::from_tab("b"), 0, || {})
            .unwrap();
        assert_eq!(executor.active_workers(), 2);
        assert_eq!(
            executor.dispatch(StableSessionKey::from_tab("c"), 0, || {}),
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
            .dispatch(key.clone(), 0, move || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                first_order.send(1).unwrap();
            })
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        executor
            .dispatch(key.clone(), 0, move || order_tx.send(2).unwrap())
            .unwrap();
        assert_eq!(
            executor.dispatch(key, 0, || {}),
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
            .try_send(AccountedWebInputJob {
                job: Box::new(move || ran_tx.send(()).unwrap()),
                reservation: executor.inner.budget.reserve(0).unwrap(),
            })
            .unwrap();

        let job = claim_pending_job_or_retire(&executor.inner, &key, worker_id, &receiver)
            .expect("queued job wins the retirement race");
        assert_eq!(executor.active_workers(), 1);
        run_accounted_job(job);
        assert_eq!(ran_rx.recv_timeout(Duration::from_secs(1)), Ok(()));

        assert!(claim_pending_job_or_retire(&executor.inner, &key, worker_id, &receiver).is_none());
        assert_eq!(executor.active_workers(), 0);
    }

    #[test]
    fn global_budget_counts_running_payloads_across_session_keys() {
        let executor = WebInputExecutor::with_budget(2, 1, 2, 10, Duration::from_secs(1));
        let (entered_tx, entered_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        for (key, bytes) in [("a", 6), ("b", 4)] {
            let entered_tx = entered_tx.clone();
            let release_rx = release_rx.clone();
            executor
                .dispatch(StableSessionKey::from_tab(key), bytes, move || {
                    entered_tx.send(()).unwrap();
                    release_rx.lock().unwrap().recv().unwrap();
                })
                .unwrap();
        }
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        assert_eq!(
            executor.dispatch(StableSessionKey::from_tab("c"), 1, || {}),
            Err(WebInputDispatchError::BudgetExceeded)
        );
        assert_eq!(executor.budget_usage(), (2, 10));

        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while executor.budget_usage() != (0, 0) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(executor.budget_usage(), (0, 0));
        executor
            .dispatch(StableSessionKey::from_tab("a"), 10, || {})
            .unwrap();
    }
}
