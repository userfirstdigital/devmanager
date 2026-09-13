use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use devmanager::domain::PromptVersionId;
use devmanager::prompts::{
    ExactPromptDiffRequest, ExactPromptVersionLoader, ExactPromptVersionMetadata,
    PromptDiffServiceError, PromptDiffWorker, PromptDiffWorkerError, PromptVersionBodyWriter,
    MAX_PROMPT_BODY_BYTES,
};

#[derive(Clone)]
struct Version {
    metadata: ExactPromptVersionMetadata,
    body: Vec<u8>,
}

impl Version {
    fn new(id: PromptVersionId, body: &str) -> Self {
        let body = body.as_bytes().to_vec();
        let body_sha256 = sha256(&body);
        Self {
            metadata: ExactPromptVersionMetadata::new(id, body.len(), body_sha256),
            body,
        }
    }
}

#[derive(Clone, Default)]
struct FakeLoader {
    versions: HashMap<PromptVersionId, Version>,
    read_bytes: Arc<AtomicUsize>,
    hash_calls: Arc<AtomicUsize>,
}

impl FakeLoader {
    fn insert(&mut self, version: Version) {
        self.versions.insert(version.metadata.id(), version);
    }
}

impl ExactPromptVersionLoader for FakeLoader {
    fn load_exact_metadata(
        &mut self,
        id: PromptVersionId,
    ) -> Result<ExactPromptVersionMetadata, PromptDiffServiceError> {
        self.versions
            .get(&id)
            .map(|version| version.metadata)
            .ok_or(PromptDiffServiceError::MissingVersion { id })
    }

    fn read_exact_body(
        &mut self,
        id: PromptVersionId,
        writer: &mut PromptVersionBodyWriter,
        _cancellation: &AtomicBool,
        _deadline: Option<Instant>,
    ) -> Result<(), PromptDiffServiceError> {
        let version = self
            .versions
            .get(&id)
            .ok_or(PromptDiffServiceError::MissingVersion { id })?;
        self.read_bytes
            .fetch_add(version.body.len(), Ordering::Relaxed);
        let _source_digest = sha256(&version.body);
        self.hash_calls.fetch_add(1, Ordering::Relaxed);
        writer.write_chunk(&version.body)
    }
}

struct BlockingLoader {
    inner: FakeLoader,
    started: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

impl ExactPromptVersionLoader for BlockingLoader {
    fn load_exact_metadata(
        &mut self,
        id: PromptVersionId,
    ) -> Result<ExactPromptVersionMetadata, PromptDiffServiceError> {
        self.inner.load_exact_metadata(id)
    }

    fn read_exact_body(
        &mut self,
        id: PromptVersionId,
        writer: &mut PromptVersionBodyWriter,
        cancellation: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<(), PromptDiffServiceError> {
        self.started.store(true, Ordering::Release);
        while !self.release.load(Ordering::Acquire) {
            if cancellation.load(Ordering::Acquire) {
                return Err(PromptDiffServiceError::Cancelled);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(PromptDiffServiceError::DeadlineExceeded);
            }
            thread::sleep(Duration::from_millis(1));
        }
        self.inner
            .read_exact_body(id, writer, cancellation, deadline)
    }
}

impl Drop for BlockingLoader {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

fn request(before: &Version, after: &Version) -> ExactPromptDiffRequest {
    ExactPromptDiffRequest::new(
        before.metadata.id(),
        after.metadata.id(),
        before.metadata.body_sha256(),
        after.metadata.body_sha256(),
        0,
    )
}

fn wait_until(flag: &AtomicBool) {
    for _ in 0..5_000 {
        if flag.load(Ordering::Acquire) {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!("worker did not reach the expected state");
}

fn wait_until_pending_result<L>(worker: &PromptDiffWorker<L>) {
    for _ in 0..5_000 {
        if worker.pending_result_count() == 1 {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!("worker did not publish a result into its bounded mailbox");
}

fn wait_for_result<L>(worker: &PromptDiffWorker<L>) -> devmanager::prompts::PromptDiffWorkerResult {
    for _ in 0..5_000 {
        match worker.try_recv() {
            Ok(Some(result)) => return result,
            Ok(None) => thread::sleep(Duration::from_millis(1)),
            Err(error) => panic!("worker result channel closed: {error:?}"),
        }
    }
    panic!("worker did not deliver a result");
}

#[test]
fn worker_delivers_exact_ids_hashes_and_body_free_projection() {
    let before = Version::new(PromptVersionId::new(), "private worker sentinel");
    let after = Version::new(PromptVersionId::new(), "replacement");
    let mut loader = FakeLoader::default();
    loader.insert(before.clone());
    loader.insert(after.clone());
    let worker = PromptDiffWorker::spawn(loader, 2, 16 * 1024);

    let submission = worker
        .submit(request(&before, &after), None)
        .expect("submit");
    let result = wait_for_result(&worker);
    assert_eq!(result.request(), submission.request());
    assert_eq!(result.before_id(), before.metadata.id());
    assert_eq!(result.after_id(), after.metadata.id());
    assert_eq!(result.before_body_sha256(), &before.metadata.body_sha256());
    assert_eq!(result.after_body_sha256(), &after.metadata.body_sha256());
    let response = result.outcome().expect("exact diff");
    assert!(
        !String::from_utf8_lossy(response.public_projection()).contains("private worker sentinel")
    );
    assert!(!format!("{response:?}").contains("private worker sentinel"));
}

#[test]
fn worker_assigns_monotonic_generation_and_replaces_queued_intent() {
    let before = Version::new(PromptVersionId::new(), "before");
    let after = Version::new(PromptVersionId::new(), "after");
    let third = Version::new(PromptVersionId::new(), "third");
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let mut inner = FakeLoader::default();
    inner.insert(before.clone());
    inner.insert(after.clone());
    inner.insert(third.clone());
    let worker = PromptDiffWorker::spawn(
        BlockingLoader {
            inner,
            started: started.clone(),
            release: release.clone(),
            dropped,
        },
        2,
        16 * 1024,
    );

    let first = worker
        .submit(request(&before, &after), None)
        .expect("first");
    wait_until(&started);
    let second = worker
        .submit(request(&before, &third), None)
        .expect("second queued");
    assert!(second.generation() > first.generation());
    let first_receipt = second
        .superseded()
        .iter()
        .find(|receipt| receipt.generation() == first.generation())
        .expect("active request cancellation is accounted");
    assert_eq!(first_receipt.request(), first.request());
    assert_eq!(first_receipt.by_request(), second.request());
    let third = worker
        .submit(request(&after, &third), None)
        .expect("latest intent replaces the queued request");
    let superseded: HashSet<_> = third
        .superseded()
        .iter()
        .map(|receipt| receipt.generation())
        .collect();
    assert!(superseded.contains(&second.generation()));
    let queued_receipt = third
        .superseded()
        .iter()
        .find(|receipt| receipt.generation() == second.generation())
        .expect("queued request replacement is accounted");
    assert_eq!(queued_receipt.request(), second.request());
    assert_eq!(queued_receipt.by_request(), third.request());

    release.store(true, Ordering::Release);
    let result = wait_for_result(&worker);
    assert_eq!(result.request(), third.request());
    assert!(result.outcome().is_ok());
}

#[test]
fn accepted_newer_request_drains_unread_completed_result_before_poll() {
    let before = Version::new(PromptVersionId::new(), "before");
    let after = Version::new(PromptVersionId::new(), "after");
    let third = Version::new(PromptVersionId::new(), "third");
    let mut loader = FakeLoader::default();
    loader.insert(before.clone());
    loader.insert(after.clone());
    loader.insert(third.clone());
    let worker = PromptDiffWorker::spawn(loader, 2, 16 * 1024);

    let first = worker
        .submit(request(&before, &after), None)
        .expect("first");
    wait_until_pending_result(&worker);
    let second = worker
        .submit(request(&before, &third), None)
        .expect("newer request");
    assert!(second
        .superseded()
        .iter()
        .any(|receipt| receipt.generation() == first.generation()));

    let maybe_result = worker.try_recv().expect("result channel remains open");
    if let Some(result) = maybe_result {
        assert_eq!(result.request(), second.request());
        return;
    }
    let result = wait_for_result(&worker);
    assert_eq!(result.request(), second.request());
}

#[test]
fn latest_intent_flood_keeps_mailboxes_bounded_and_accounts_superseded() {
    let before = Version::new(PromptVersionId::new(), "before");
    let after = Version::new(PromptVersionId::new(), "after");
    let third = Version::new(PromptVersionId::new(), "third");
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let mut inner = FakeLoader::default();
    inner.insert(before.clone());
    inner.insert(after.clone());
    inner.insert(third.clone());
    let worker = PromptDiffWorker::spawn(
        BlockingLoader {
            inner,
            started: started.clone(),
            release: release.clone(),
            dropped,
        },
        2,
        16 * 1024,
    );

    let first = worker
        .submit(request(&before, &after), None)
        .expect("first");
    wait_until(&started);
    let mut submissions = vec![first];
    for _ in 0..100 {
        let submission = worker
            .submit(request(&before, &third), None)
            .expect("latest intent remains admissible");
        assert!(worker.pending_request_count() <= 1);
        assert!(worker.pending_result_count() <= 1);
        submissions.push(submission);
    }
    release.store(true, Ordering::Release);
    wait_until_pending_result(&worker);
    for _ in 0..100 {
        let submission = worker
            .submit(request(&after, &third), None)
            .expect("latest intent remains admissible after a result");
        assert!(worker.pending_request_count() <= 1);
        assert!(worker.pending_result_count() <= 1);
        submissions.push(submission);
    }
    let latest = submissions.last().expect("flood has a latest request");
    let mut accounted = HashSet::new();
    for submission in &submissions {
        for receipt in submission.superseded() {
            accounted.insert(receipt.generation());
        }
    }
    for generation in 1..latest.generation() {
        assert!(
            accounted.contains(&generation),
            "superseded generation {generation} had no typed receipt"
        );
    }

    let result = wait_for_result(&worker);
    assert_eq!(result.request(), latest.request());
    assert!(worker.pending_request_count() <= 1);
    assert!(worker.pending_result_count() <= 1);
    assert!(worker.try_recv().expect("channel open").is_none());
}

#[test]
fn worker_cancellation_returns_cancelled_result() {
    let before = Version::new(PromptVersionId::new(), "before");
    let after = Version::new(PromptVersionId::new(), "after");
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let mut inner = FakeLoader::default();
    inner.insert(before.clone());
    inner.insert(after.clone());
    let worker = PromptDiffWorker::spawn(
        BlockingLoader {
            inner,
            started: started.clone(),
            release,
            dropped,
        },
        2,
        16 * 1024,
    );

    let submission = worker
        .submit(request(&before, &after), None)
        .expect("submit");
    wait_until(&started);
    submission.cancel();
    let result = wait_for_result(&worker);
    assert_eq!(result.request(), submission.request());
    assert!(matches!(
        result.outcome(),
        Err(PromptDiffServiceError::Cancelled)
    ));
}

#[test]
fn worker_deadline_is_per_request_and_visible() {
    let before = Version::new(PromptVersionId::new(), "before");
    let after = Version::new(PromptVersionId::new(), "after");
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let mut inner = FakeLoader::default();
    inner.insert(before.clone());
    inner.insert(after.clone());
    let worker = PromptDiffWorker::spawn(
        BlockingLoader {
            inner,
            started: started.clone(),
            release,
            dropped,
        },
        2,
        16 * 1024,
    );

    let submission = worker
        .submit(
            request(&before, &after),
            Some(Instant::now() + Duration::from_millis(10)),
        )
        .expect("submit");
    wait_until(&started);
    let result = wait_for_result(&worker);
    assert_eq!(result.request(), submission.request());
    assert!(matches!(
        result.outcome(),
        Err(PromptDiffServiceError::DeadlineExceeded)
    ));
}

#[test]
fn newer_submission_discards_stale_result_and_cancels_active_work() {
    let before = Version::new(PromptVersionId::new(), "before");
    let after = Version::new(PromptVersionId::new(), "after");
    let third = Version::new(PromptVersionId::new(), "third");
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let mut inner = FakeLoader::default();
    inner.insert(before.clone());
    inner.insert(after.clone());
    inner.insert(third.clone());
    let worker = PromptDiffWorker::spawn(
        BlockingLoader {
            inner,
            started: started.clone(),
            release: release.clone(),
            dropped,
        },
        2,
        16 * 1024,
    );

    let first = worker
        .submit(request(&before, &after), None)
        .expect("first");
    wait_until(&started);
    let second = worker
        .submit(request(&before, &third), None)
        .expect("second");
    release.store(true, Ordering::Release);
    let result = wait_for_result(&worker);
    assert_eq!(result.request(), second.request());
    assert_ne!(result.request(), first.request());
    assert!(result.outcome().is_ok());
    assert!(worker.try_recv().expect("channel open").is_none());
}

#[test]
fn oversized_metadata_is_rejected_before_reader_or_hash_work() {
    let before = Version::new(PromptVersionId::new(), "before");
    let after_id = PromptVersionId::new();
    let read_bytes = Arc::new(AtomicUsize::new(0));
    let hash_calls = Arc::new(AtomicUsize::new(0));
    let mut loader = FakeLoader::default();
    loader.insert(before.clone());
    loader.versions.insert(
        after_id,
        Version {
            metadata: ExactPromptVersionMetadata::new(
                after_id,
                MAX_PROMPT_BODY_BYTES + 1,
                [0xA5; 32],
            ),
            body: vec![b'x'; 4 * 1024 * 1024],
        },
    );
    loader.read_bytes = read_bytes.clone();
    loader.hash_calls = hash_calls.clone();
    let worker = PromptDiffWorker::spawn(loader, 2, 16 * 1024);
    let request = ExactPromptDiffRequest::new(
        before.metadata.id(),
        after_id,
        before.metadata.body_sha256(),
        [0xA5; 32],
        0,
    );
    let submission = worker.submit(request, None).expect("submit");
    let result = wait_for_result(&worker);
    assert_eq!(result.request(), submission.request());
    assert!(matches!(
        result.outcome(),
        Err(PromptDiffServiceError::OversizedVersion { id, .. }) if id == after_id
    ));
    assert_eq!(read_bytes.load(Ordering::Acquire), 0);
    assert_eq!(hash_calls.load(Ordering::Acquire), 0);
}

#[test]
fn drop_cancels_reader_and_joins_owned_worker() {
    let before = Version::new(PromptVersionId::new(), "before");
    let after = Version::new(PromptVersionId::new(), "after");
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let mut inner = FakeLoader::default();
    inner.insert(before.clone());
    inner.insert(after.clone());
    let worker = PromptDiffWorker::spawn(
        BlockingLoader {
            inner,
            started: started.clone(),
            release,
            dropped: dropped.clone(),
        },
        2,
        16 * 1024,
    );
    worker
        .submit(request(&before, &after), None)
        .expect("submit");
    wait_until(&started);
    drop(worker);
    assert!(dropped.load(Ordering::Acquire));
}

#[test]
fn explicit_shutdown_cancels_and_joins_owned_worker() {
    let before = Version::new(PromptVersionId::new(), "before");
    let after = Version::new(PromptVersionId::new(), "after");
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let mut inner = FakeLoader::default();
    inner.insert(before.clone());
    inner.insert(after.clone());
    let mut worker = PromptDiffWorker::spawn(
        BlockingLoader {
            inner,
            started: started.clone(),
            release,
            dropped: dropped.clone(),
        },
        2,
        16 * 1024,
    );
    worker
        .submit(request(&before, &after), None)
        .expect("submit");
    wait_until(&started);
    worker.shutdown().expect("worker should join cleanly");
    assert!(dropped.load(Ordering::Acquire));
    assert!(matches!(
        worker.submit(request(&before, &after), None),
        Err(PromptDiffWorkerError::Closed)
    ));
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    Sha256::digest(bytes).into()
}
