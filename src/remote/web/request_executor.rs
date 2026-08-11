use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

const DEFAULT_WORKERS: usize = 8;
const DEFAULT_QUEUE_CAPACITY: usize = 256;

type WebRequestJob = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebRequestDispatchError {
    QueueFull,
    Unavailable,
}

#[derive(Clone)]
pub(crate) struct WebRequestExecutor {
    inner: Arc<WebRequestExecutorInner>,
}

struct WebRequestExecutorInner {
    sender: Mutex<Option<mpsc::SyncSender<WebRequestJob>>>,
    workers: usize,
    stopping: Arc<AtomicBool>,
    handles: Mutex<HashMap<usize, JoinHandle<()>>>,
    completion_rx: Mutex<mpsc::Receiver<usize>>,
}

impl Default for WebRequestExecutor {
    fn default() -> Self {
        Self::new(DEFAULT_WORKERS, DEFAULT_QUEUE_CAPACITY)
    }
}

impl WebRequestExecutor {
    pub(crate) fn new(worker_count: usize, queue_capacity: usize) -> Self {
        let (sender, receiver) = mpsc::sync_channel(queue_capacity.max(1));
        let receiver = Arc::new(Mutex::new(receiver));
        let (completion_tx, completion_rx) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let mut workers = 0;
        let mut handles = HashMap::new();
        for index in 0..worker_count.max(1) {
            let receiver = receiver.clone();
            let stopping = stopping.clone();
            let completion_tx = completion_tx.clone();
            if let Ok(handle) = std::thread::Builder::new()
                .name(format!("web-request-{index}"))
                .spawn(move || run_worker(receiver, stopping, completion_tx, index))
            {
                workers += 1;
                handles.insert(index, handle);
            }
        }
        Self {
            inner: Arc::new(WebRequestExecutorInner {
                sender: Mutex::new(Some(sender)),
                workers,
                stopping,
                handles: Mutex::new(handles),
                completion_rx: Mutex::new(completion_rx),
            }),
        }
    }

    pub(crate) fn dispatch(
        &self,
        job: impl FnOnce() + Send + 'static,
    ) -> Result<(), WebRequestDispatchError> {
        if self.inner.workers == 0 || self.inner.stopping.load(Ordering::Acquire) {
            return Err(WebRequestDispatchError::Unavailable);
        }
        let sender = self
            .inner
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or(WebRequestDispatchError::Unavailable)?;
        match sender.try_send(Box::new(job)) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => Err(WebRequestDispatchError::QueueFull),
            Err(mpsc::TrySendError::Disconnected(_)) => Err(WebRequestDispatchError::Unavailable),
        }
    }

    /// Close the bounded queue and join workers that honor the lifecycle
    /// deadline. Handles that are still running remain in this executor's
    /// owned map so a dropped service never detaches callback workers.
    pub(crate) fn shutdown_until(&self, deadline: Instant) -> usize {
        self.inner.stopping.store(true, Ordering::Release);
        self.inner
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        self.join_finished_workers();

        loop {
            let remaining = self
                .inner
                .handles
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len();
            if remaining == 0 {
                return 0;
            }
            let wait = deadline.saturating_duration_since(Instant::now());
            if wait.is_zero() {
                return remaining;
            }
            let event = self
                .inner
                .completion_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv_timeout(wait);
            match event {
                Ok(index) => self.join_worker(index),
                Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
                    return self
                        .inner
                        .handles
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .len();
                }
            }
        }
    }

    pub(crate) fn take_unfinished_worker_handles(&self) -> Vec<JoinHandle<()>> {
        self.inner
            .handles
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .map(|(_, handle)| handle)
            .collect()
    }

    fn join_worker(&self, index: usize) {
        let handle = self
            .inner
            .handles
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&index);
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    fn join_finished_workers(&self) {
        let mut indexes = {
            let receiver = self
                .inner
                .completion_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>()
        };
        indexes.extend(
            self.inner
                .handles
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .filter_map(|(index, handle)| handle.is_finished().then_some(*index)),
        );
        indexes.sort_unstable();
        indexes.dedup();
        for index in indexes {
            self.join_worker(index);
        }
    }
}

fn run_worker(
    receiver: Arc<Mutex<mpsc::Receiver<WebRequestJob>>>,
    stopping: Arc<AtomicBool>,
    completion_tx: mpsc::Sender<usize>,
    index: usize,
) {
    struct Completion {
        index: usize,
        tx: mpsc::Sender<usize>,
    }
    impl Drop for Completion {
        fn drop(&mut self) {
            let _ = self.tx.send(self.index);
        }
    }
    let _completion = Completion {
        index,
        tx: completion_tx,
    };
    loop {
        let job = receiver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .recv();
        let Ok(job) = job else {
            return;
        };
        if stopping.load(Ordering::Acquire) {
            continue;
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn worker_and_waiter_queue_are_bounded() {
        let executor = WebRequestExecutor::new(1, 1);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        executor
            .dispatch(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        executor.dispatch(|| {}).unwrap();
        assert_eq!(
            executor.dispatch(|| {}),
            Err(WebRequestDispatchError::QueueFull)
        );
        release_tx.send(()).unwrap();
    }

    #[test]
    fn shutdown_joins_paused_request_workers_without_detaching_them() {
        let executor = WebRequestExecutor::new(2, 1);
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        executor
            .dispatch(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .unwrap();
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("request callback should enter before shutdown");
        assert_eq!(
            executor.shutdown_until(Instant::now() + Duration::from_millis(50)),
            1,
            "paused request worker should remain visible after its owner deadline"
        );
        let handles = executor.take_unfinished_worker_handles();
        assert_eq!(
            handles.len(),
            1,
            "paused request worker handle must stay owned"
        );
        release_tx.send(()).unwrap();
        for handle in handles {
            handle.join().unwrap();
        }
    }
}
