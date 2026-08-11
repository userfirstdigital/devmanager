use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use devmanager::domain::PromptVersionId;
use devmanager::prompts::{
    ExactPromptDiffRequest, ExactPromptVersionLoader, PromptDiffService, PromptDiffServiceError,
    PromptVersionSnapshot, MAX_PROMPT_DIFF_LINE_COUNT, MAX_PROMPT_DIFF_PAYLOAD_BYTES,
};

#[derive(Default)]
struct FakeLoader {
    versions: HashMap<PromptVersionId, PromptVersionSnapshot>,
}

impl ExactPromptVersionLoader for FakeLoader {
    fn load_exact(
        &self,
        id: PromptVersionId,
    ) -> Result<PromptVersionSnapshot, PromptDiffServiceError> {
        self.versions
            .get(&id)
            .cloned()
            .ok_or(PromptDiffServiceError::MissingVersion { id })
    }
}

fn request(
    before: &PromptVersionSnapshot,
    after: &PromptVersionSnapshot,
    generation: u64,
) -> ExactPromptDiffRequest {
    ExactPromptDiffRequest::new(
        before.id(),
        after.id(),
        *before.body_sha256(),
        *after.body_sha256(),
        generation,
    )
}

#[test]
fn service_loads_only_exact_ids_and_returns_body_free_projection() {
    let before_id = PromptVersionId::new();
    let after_id = PromptVersionId::new();
    let before = PromptVersionSnapshot::from_body(before_id, "private service sentinel".into());
    let after = PromptVersionSnapshot::from_body(after_id, "replacement".into());
    let mut loader = FakeLoader::default();
    loader.versions.insert(before.id(), before.clone());
    loader.versions.insert(after.id(), after.clone());
    let mut service = PromptDiffService::new(loader, 2, 16 * 1024);

    let response = service
        .diff_exact(request(&before, &after, 0), &AtomicBool::new(false))
        .expect("exact versions should diff");

    assert_eq!(response.before_id(), before.id());
    assert_eq!(response.after_id(), after.id());
    assert_eq!(response.status(), devmanager::prompts::DiffStatus::Complete);
    assert!(response.has_local_projection());
    let snapshot_debug = format!("{before:?}");
    assert!(snapshot_debug.contains("body_bytes"));
    assert!(!snapshot_debug.contains("private service sentinel"));
    let projection = String::from_utf8_lossy(response.public_projection());
    assert!(!projection.contains("private service sentinel"));
    assert!(!projection.contains("\"text\""));
    assert!(format!("{response:?}").contains("public_projection_bytes"));
    assert!(!format!("{response:?}").contains("private service sentinel"));
}

#[test]
fn service_fails_visibly_for_missing_corrupt_and_stale_versions() {
    let before_id = PromptVersionId::new();
    let after_id = PromptVersionId::new();
    let before = PromptVersionSnapshot::from_body(before_id, "before".into());
    let after = PromptVersionSnapshot::from_body(after_id, "after".into());

    let mut missing_loader = FakeLoader::default();
    missing_loader.versions.insert(before.id(), before.clone());
    let mut missing_service = PromptDiffService::new(missing_loader, 2, 16 * 1024);
    assert!(matches!(
        missing_service.diff_exact(request(&before, &after, 0), &AtomicBool::new(false)),
        Err(PromptDiffServiceError::MissingVersion { id }) if id == after.id()
    ));

    let mut corrupt_loader = FakeLoader::default();
    corrupt_loader.versions.insert(
        before.id(),
        PromptVersionSnapshot::with_body_sha256(
            before.id(),
            "tampered".into(),
            *before.body_sha256(),
        ),
    );
    corrupt_loader.versions.insert(after.id(), after.clone());
    let mut corrupt_service = PromptDiffService::new(corrupt_loader, 2, 16 * 1024);
    assert!(matches!(
        corrupt_service.diff_exact(request(&before, &after, 0), &AtomicBool::new(false)),
        Err(PromptDiffServiceError::CorruptVersion { id }) if id == before.id()
    ));

    let mut stale_service = PromptDiffService::new(
        FakeLoader {
            versions: HashMap::from([(before.id(), before.clone()), (after.id(), after.clone())]),
        },
        2,
        16 * 1024,
    );
    let stale_request =
        ExactPromptDiffRequest::new(before.id(), after.id(), [0xA5; 32], *after.body_sha256(), 0);
    assert!(matches!(
        stale_service.diff_exact(stale_request, &AtomicBool::new(false)),
        Err(PromptDiffServiceError::StaleVersion { id }) if id == before.id()
    ));
}

#[test]
fn service_lru_is_bounded_and_generation_fence_blocks_stale_delivery() {
    let before = PromptVersionSnapshot::from_body(PromptVersionId::new(), "before".into());
    let after = PromptVersionSnapshot::from_body(PromptVersionId::new(), "after".into());
    let third = PromptVersionSnapshot::from_body(PromptVersionId::new(), "third".into());
    let mut service = PromptDiffService::new(
        FakeLoader {
            versions: HashMap::from([
                (before.id(), before.clone()),
                (after.id(), after.clone()),
                (third.id(), third.clone()),
            ]),
        },
        1,
        16 * 1024,
    );
    let cancellation = AtomicBool::new(false);

    let first = service
        .diff_exact(request(&before, &after, 0), &cancellation)
        .expect("first exact diff");
    assert!(!first.cache_hit());
    assert_eq!(service.cache_len(), 1);
    assert!(service.cache_bytes() <= 16 * 1024);

    let second = service
        .diff_exact(request(&before, &third, 0), &cancellation)
        .expect("second exact diff");
    assert!(!second.cache_hit());
    assert_eq!(service.cache_len(), 1);
    assert!(service.cache_bytes() <= 16 * 1024);

    let second_again = service
        .diff_exact(request(&before, &third, 0), &cancellation)
        .expect("exact cache key should hit");
    assert!(second_again.cache_hit());
    assert!(!second_again.has_local_projection());
    assert_eq!(second_again.public_projection(), second.public_projection());

    service.begin_generation(1);
    service.begin_generation(0);
    assert!(matches!(
        service.diff_exact(request(&before, &third, 0), &cancellation),
        Err(PromptDiffServiceError::StaleGeneration {
            requested: 0,
            current: 1
        })
    ));
}

#[test]
fn service_deadline_and_adversarial_line_caps_are_deterministic() {
    let before = PromptVersionSnapshot::from_body(PromptVersionId::new(), "before".into());
    let after = PromptVersionSnapshot::from_body(
        PromptVersionId::new(),
        "x\n".repeat(MAX_PROMPT_DIFF_LINE_COUNT + 1),
    );
    let mut service = PromptDiffService::new(
        FakeLoader {
            versions: HashMap::from([(before.id(), before.clone()), (after.id(), after.clone())]),
        },
        2,
        MAX_PROMPT_DIFF_PAYLOAD_BYTES,
    );
    let cancellation = AtomicBool::new(false);
    let request = request(&before, &after, 0);

    assert!(matches!(
        service.diff_exact_with_deadline(
            request,
            &cancellation,
            Some(Instant::now() - Duration::from_secs(1)),
        ),
        Err(PromptDiffServiceError::DeadlineExceeded)
    ));

    let response = service
        .diff_exact(request, &cancellation)
        .expect("the bounded line cap should produce a visible approximation");
    assert_eq!(
        response.status(),
        devmanager::prompts::DiffStatus::Approximate
    );
    assert!(response.public_projection().len() <= MAX_PROMPT_DIFF_PAYLOAD_BYTES);
    assert_eq!(service.cache_len(), 0);
}
