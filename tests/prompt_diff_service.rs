use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use devmanager::domain::PromptVersionId;
use devmanager::prompts::{
    ExactPromptDiffRequest, ExactPromptVersionLoader, ExactPromptVersionMetadata,
    PromptDiffServiceError, PromptDiffWorker, PromptVersionBodyWriter, MAX_PROMPT_DIFF_LINE_COUNT,
    MAX_PROMPT_DIFF_PAYLOAD_BYTES,
};

#[derive(Clone)]
struct Version {
    metadata: ExactPromptVersionMetadata,
    body: Vec<u8>,
}

impl Version {
    fn new(id: PromptVersionId, body: &str) -> Self {
        let body = body.as_bytes().to_vec();
        Self {
            metadata: ExactPromptVersionMetadata::new(id, body.len(), sha256(&body)),
            body,
        }
    }
}

#[derive(Clone, Default)]
struct FakeLoader {
    versions: HashMap<PromptVersionId, Version>,
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
        cancellation: &AtomicBool,
        deadline: Option<Instant>,
    ) -> Result<(), PromptDiffServiceError> {
        if cancellation.load(std::sync::atomic::Ordering::Acquire) {
            return Err(PromptDiffServiceError::Cancelled);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(PromptDiffServiceError::DeadlineExceeded);
        }
        let version = self
            .versions
            .get(&id)
            .ok_or(PromptDiffServiceError::MissingVersion { id })?;
        writer.write_chunk(&version.body)
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

fn wait_for_result<L>(worker: &PromptDiffWorker<L>) -> devmanager::prompts::PromptDiffWorkerResult {
    for _ in 0..5_000 {
        match worker.try_recv() {
            Ok(Some(result)) => return result,
            Ok(None) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => panic!("worker result channel closed: {error:?}"),
        }
    }
    panic!("worker did not deliver a result");
}

fn run(
    loader: FakeLoader,
    request: ExactPromptDiffRequest,
    deadline: Option<Instant>,
) -> devmanager::prompts::PromptDiffWorkerResult {
    let worker = PromptDiffWorker::spawn(loader, 2, 16 * 1024);
    worker
        .submit(request, deadline)
        .expect("exact diff should enqueue");
    wait_for_result(&worker)
}

#[test]
fn service_loads_only_exact_ids_and_returns_body_free_projection() {
    let before = Version::new(PromptVersionId::new(), "private service sentinel");
    let after = Version::new(PromptVersionId::new(), "replacement");
    let mut loader = FakeLoader::default();
    loader.versions.insert(before.metadata.id(), before.clone());
    loader.versions.insert(after.metadata.id(), after.clone());

    let result = run(loader, request(&before, &after), None);
    assert_eq!(result.before_id(), before.metadata.id());
    assert_eq!(result.after_id(), after.metadata.id());
    let response = result.outcome().expect("exact versions should diff");
    assert_eq!(response.status(), devmanager::prompts::DiffStatus::Complete);
    assert!(response.has_local_projection());
    let metadata_debug = format!("{:?}", before.metadata);
    assert!(metadata_debug.contains("body_bytes"));
    assert!(!metadata_debug.contains("private service sentinel"));
    let projection = String::from_utf8_lossy(response.public_projection());
    assert!(!projection.contains("private service sentinel"));
    assert!(!projection.contains("\"text\""));
    assert!(format!("{response:?}").contains("public_projection_bytes"));
    assert!(!format!("{response:?}").contains("private service sentinel"));
}

#[test]
fn service_fails_visibly_for_missing_corrupt_and_stale_versions() {
    let before = Version::new(PromptVersionId::new(), "before");
    let after = Version::new(PromptVersionId::new(), "after");

    let mut missing_loader = FakeLoader::default();
    missing_loader
        .versions
        .insert(before.metadata.id(), before.clone());
    let missing = run(missing_loader, request(&before, &after), None);
    assert!(matches!(
        missing.outcome(),
        Err(PromptDiffServiceError::MissingVersion { id }) if id == after.metadata.id()
    ));

    let mut corrupt_loader = FakeLoader::default();
    corrupt_loader.versions.insert(
        before.metadata.id(),
        Version {
            metadata: before.metadata,
            body: b"tampered".to_vec(),
        },
    );
    corrupt_loader
        .versions
        .insert(after.metadata.id(), after.clone());
    let corrupt = run(corrupt_loader, request(&before, &after), None);
    assert!(matches!(
        corrupt.outcome(),
        Err(PromptDiffServiceError::CorruptVersion { id }) if id == before.metadata.id()
    ));

    let mut stale_loader = FakeLoader::default();
    stale_loader
        .versions
        .insert(before.metadata.id(), before.clone());
    stale_loader
        .versions
        .insert(after.metadata.id(), after.clone());
    let stale_request = ExactPromptDiffRequest::new(
        before.metadata.id(),
        after.metadata.id(),
        [0xA5; 32],
        after.metadata.body_sha256(),
        0,
    );
    let stale = run(stale_loader, stale_request, None);
    assert!(matches!(
        stale.outcome(),
        Err(PromptDiffServiceError::StaleVersion { id }) if id == before.metadata.id()
    ));
}

#[test]
fn service_cache_returns_body_free_hit_for_same_exact_key() {
    let before = Version::new(PromptVersionId::new(), "before");
    let after = Version::new(PromptVersionId::new(), "after");
    let mut loader = FakeLoader::default();
    loader.versions.insert(before.metadata.id(), before.clone());
    loader.versions.insert(after.metadata.id(), after.clone());
    let worker = PromptDiffWorker::spawn(loader, 1, 16 * 1024);

    worker
        .submit(request(&before, &after), None)
        .expect("first");
    let first = wait_for_result(&worker).outcome().expect("first diff");
    assert!(!first.cache_hit());
    assert!(first.has_local_projection());

    worker
        .submit(request(&before, &after), None)
        .expect("second");
    let second = wait_for_result(&worker).outcome().expect("cache hit");
    assert!(second.cache_hit());
    assert!(!second.has_local_projection());
    assert_eq!(second.public_projection(), first.public_projection());
}

#[test]
fn service_deadline_and_adversarial_line_caps_are_deterministic() {
    let before = Version::new(PromptVersionId::new(), "before");
    let after = Version::new(
        PromptVersionId::new(),
        &"x\n".repeat(MAX_PROMPT_DIFF_LINE_COUNT + 1),
    );
    let mut loader = FakeLoader::default();
    loader.versions.insert(before.metadata.id(), before.clone());
    loader.versions.insert(after.metadata.id(), after.clone());

    let deadline = run(
        loader.clone(),
        request(&before, &after),
        Some(Instant::now() - Duration::from_secs(1)),
    );
    assert!(matches!(
        deadline.outcome(),
        Err(PromptDiffServiceError::DeadlineExceeded)
    ));

    let result = run(loader, request(&before, &after), None);
    let response = result
        .outcome()
        .expect("the bounded line cap should produce a visible approximation");
    assert_eq!(
        response.status(),
        devmanager::prompts::DiffStatus::Approximate
    );
    assert!(response.public_projection().len() <= MAX_PROMPT_DIFF_PAYLOAD_BYTES);
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    Sha256::digest(bytes).into()
}
