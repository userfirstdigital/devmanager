use std::sync::{mpsc, Arc, Mutex};

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
    sender: mpsc::SyncSender<WebRequestJob>,
    workers: usize,
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
        let mut workers = 0;
        for index in 0..worker_count.max(1) {
            let receiver = receiver.clone();
            if std::thread::Builder::new()
                .name(format!("web-request-{index}"))
                .spawn(move || run_worker(receiver))
                .is_ok()
            {
                workers += 1;
            }
        }
        Self { sender, workers }
    }

    pub(crate) fn dispatch(
        &self,
        job: impl FnOnce() + Send + 'static,
    ) -> Result<(), WebRequestDispatchError> {
        if self.workers == 0 {
            return Err(WebRequestDispatchError::Unavailable);
        }
        match self.sender.try_send(Box::new(job)) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => Err(WebRequestDispatchError::QueueFull),
            Err(mpsc::TrySendError::Disconnected(_)) => Err(WebRequestDispatchError::Unavailable),
        }
    }
}

fn run_worker(receiver: Arc<Mutex<mpsc::Receiver<WebRequestJob>>>) {
    loop {
        let job = receiver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .recv();
        let Ok(job) = job else {
            return;
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
}
