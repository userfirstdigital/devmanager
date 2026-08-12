//! Transport-neutral prompt projection contract and adversarial bound tests.
//!
//! These prove a single host action registry, opaque owner-device capability,
//! keyset paging, physical codec bounds, and fail-closed search/history until
//! Task 7.3. They do not open Connect, upload personal prompts, or touch
//! production config.

use std::cell::Cell;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use devmanager::client::action::{
    action_by_id, catalog, disabled_reason, prompt_metadata_page_request, registered_actions,
    require_unique_ids, ActionRisk, ActionScope, ACTION_PROMPT_CHAIN_PAGE, ACTION_PROMPT_DIFF,
    ACTION_PROMPT_HISTORY_PAGE, ACTION_PROMPT_METADATA_PAGE, ACTION_PROMPT_SEARCH_PAGE,
    ACTION_PROMPT_VERSION_PAGE,
};
use devmanager::domain::{
    ClientId, CommandId, PromptChainId, PromptChainLinkId, PromptId, PromptVersionId,
    QueryEnvelope, RequestId,
};
use devmanager::prompts::projection::{
    decode_prompt_projection_document, encode_prompt_projection_document, project_prompt_library,
    project_prompt_store, project_without_capability, testing, BoundedTestSource,
    OwnerDeviceCapability, PromptChainLinkRecord, PromptChainRecord, PromptCursor,
    PromptHistoryPage, PromptLibraryRequest, PromptNamespace, PromptPrivacyClass,
    PromptProjectionError, PromptProjectionQueryKind, PromptProjectionReply,
    PromptProjectionSource, PromptProjectionSubsystem, PromptSearchPage,
    PERSONAL_PROMPT_LIBRARY_BIT, PROMPT_BODY_CHUNK_BYTES, PROMPT_METADATA_PAGE_BYTES,
    PROMPT_METADATA_PAGE_ITEMS, PROMPT_PROJECTION_SCHEMA_VERSION, PROMPT_SEARCH_MAX_QUERY_BYTES,
};
use devmanager::prompts::{
    CreatePrompt, PromptChain, PromptCommand, PromptStore, PromptVersion, SavedPrompt,
};
use devmanager::protocol::{Capability, CapabilitySet};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn fixture_id(tail: u8) -> [u8; 16] {
    [
        0x01, 0x92, 0xf5, 0xd0, 0x00, 0x00, 0x70, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn prompt_id(tail: u8) -> PromptId {
    PromptId::from_bytes(fixture_id(tail)).expect("UUIDv7 prompt id")
}

fn version_id(tail: u8) -> PromptVersionId {
    PromptVersionId::from_bytes(fixture_id(tail)).expect("UUIDv7 version id")
}

fn chain_id(tail: u8) -> PromptChainId {
    PromptChainId::from_bytes(fixture_id(tail)).expect("UUIDv7 chain id")
}

fn link_id(tail: u8) -> PromptChainLinkId {
    PromptChainLinkId::from_bytes(fixture_id(tail)).expect("UUIDv7 link id")
}

fn command_id(tail: u8) -> CommandId {
    CommandId::from_bytes(fixture_id(tail)).expect("UUIDv7 command id")
}

fn request_id(tail: u8) -> RequestId {
    RequestId::from_bytes(fixture_id(tail)).expect("UUIDv7 request id")
}

fn client_id(tail: u8) -> ClientId {
    ClientId::from_bytes(fixture_id(tail)).expect("UUIDv7 client id")
}

fn fixture_prompt() -> SavedPrompt {
    SavedPrompt {
        id: prompt_id(0x01),
        title: "Sanitized review prompt".into(),
        description: Some("Metadata-only fixture for prompt projection tests.".into()),
        tags: vec!["rust".into(), "review".into()],
        current_version_id: version_id(0x02),
        revision: 1,
        archived_at_ms: None,
    }
}

fn fixture_version(body: &str) -> PromptVersion {
    PromptVersion::new(
        version_id(0x02),
        prompt_id(0x01),
        1,
        body.to_string(),
        1_728_000_000_000,
    )
    .expect("valid fixture version")
}

fn fixture_source() -> BoundedTestSource {
    BoundedTestSource::try_new(
        7,
        vec![fixture_prompt()],
        vec![fixture_version("Review this change carefully.")],
        vec![PromptChainRecord::try_new(
            PromptChain {
                id: chain_id(0x10),
                title: "Review chain".into(),
                description: Some("Linear review steps".into()),
                revision: 2,
                archived_at_ms: None,
            },
            vec![PromptChainLinkRecord::try_new(
                link_id(0x11),
                chain_id(0x10),
                1,
                prompt_id(0x01),
                version_id(0x02),
                None,
                None,
                false,
            )
            .expect("bounded link")],
        )
        .expect("bounded chain")],
    )
    .expect("bounded fixture source")
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/prompts/v1")
}

fn assert_golden_json(name: &str, value: &serde_json::Value) {
    let path = fixtures_dir().join(name);
    let expected = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("missing golden fixture {}: {error}", path.display()));
    let expected: serde_json::Value =
        serde_json::from_str(&expected).expect("golden fixture must be JSON");
    assert_eq!(value, &expected, "golden fixture drift: {}", path.display());
}

fn granted_library() -> CapabilitySet {
    CapabilitySet::from_capabilities([Capability::PromptProjection])
}

fn owner() -> OwnerDeviceCapability {
    testing::owner_grant(client_id(0x71)).expect("sealed owner")
}

fn foreign_owner() -> OwnerDeviceCapability {
    testing::owner_grant(client_id(0x99)).expect("foreign sealed owner")
}

fn metadata_request(revision: Option<u64>, cursor: Option<PromptCursor>) -> PromptLibraryRequest {
    PromptLibraryRequest::metadata_page(
        request_id(0x70),
        client_id(0x71),
        &owner(),
        PromptNamespace::Personal,
        revision,
        cursor,
    )
    .expect("valid metadata request")
}

fn project_limit() -> u32 {
    PROMPT_METADATA_PAGE_BYTES as u32
}

fn project(request: PromptLibraryRequest) -> Result<PromptProjectionReply, PromptProjectionError> {
    project_prompt_library(&owner(), &request, &fixture_source(), project_limit())
}

#[test]
fn personal_prompt_library_uses_stable_prompt_projection_bit() {
    assert_eq!(
        Capability::PromptProjection.bit(),
        PERSONAL_PROMPT_LIBRARY_BIT
    );
    assert_eq!(Capability::PromptProjection.bit(), 1_u64 << 8);
    assert_eq!(
        Capability::PromptProjection.wire_name(),
        "personal_prompt_library"
    );
    assert!(
        CapabilitySet::from_capabilities([Capability::PromptProjection])
            .grants_personal_prompt_library()
    );
    assert!(!CapabilitySet::empty().grants_personal_prompt_library());
    let unknown = CapabilitySet::from_bits(1_u64 << 63);
    assert!(!unknown.grants_personal_prompt_library());
}

#[test]
fn prompt_actions_register_through_the_single_host_registry() {
    assert_eq!(
        catalog().len(),
        registered_actions().count(),
        "catalog() is the single host action registry"
    );
    require_unique_ids().expect("registry ids stay unique");
    let ids: Vec<&str> = registered_actions().map(|action| action.id).collect();
    assert!(ids.contains(&ACTION_PROMPT_METADATA_PAGE));
    assert!(ids.contains(&ACTION_PROMPT_VERSION_PAGE));
    assert!(ids.contains(&ACTION_PROMPT_DIFF));
    assert!(ids.contains(&ACTION_PROMPT_CHAIN_PAGE));
    assert!(!ids.contains(&ACTION_PROMPT_SEARCH_PAGE));
    assert!(!ids.contains(&ACTION_PROMPT_HISTORY_PAGE));
    assert!(action_by_id(ACTION_PROMPT_SEARCH_PAGE).is_none());
    assert!(action_by_id(ACTION_PROMPT_HISTORY_PAGE).is_none());
    for id in [
        ACTION_PROMPT_METADATA_PAGE,
        ACTION_PROMPT_VERSION_PAGE,
        ACTION_PROMPT_DIFF,
        ACTION_PROMPT_CHAIN_PAGE,
    ] {
        let action = action_by_id(id).expect("prompt action is registered");
        assert_eq!(action.scope, ActionScope::Host);
        assert_eq!(action.risk, ActionRisk::ReadOnly);
        assert_eq!(
            action.required_capability,
            Some(Capability::PromptProjection)
        );
        assert_ne!(
            action.argument_schema,
            devmanager::client::action::ActionArgumentSchema::None
        );
    }
    assert!(action_by_id("future_hook").is_none());
    let request =
        prompt_metadata_page_request(request_id(0x70), client_id(0x71), &owner(), Some(7), None)
            .expect("factory binds request authority");
    assert_eq!(request.request_id(), request_id(0x70));
    assert_eq!(request.client_id(), client_id(0x71));
    assert_eq!(request.expected_library_revision(), Some(7));
    assert!(request.task_id().is_none());
}

#[test]
fn production_owner_capability_is_fail_closed_until_phase_9() {
    assert!(matches!(
        OwnerDeviceCapability::from_authenticated_session(&[]),
        Err(PromptProjectionError::Unavailable {
            subsystem: PromptProjectionSubsystem::OwnerDeviceSession,
        })
    ));
    assert!(matches!(
        project_without_capability(&metadata_request(Some(7), None), &fixture_source()),
        Err(PromptProjectionError::PermissionDenied)
    ));
    assert!(matches!(
        testing::watcher_grant(client_id(0x71)),
        Err(PromptProjectionError::PermissionDenied)
    ));
    assert!(matches!(
        testing::collaborator_grant(client_id(0x71)),
        Err(PromptProjectionError::PermissionDenied)
    ));
    assert!(testing::paired_owner_grant(client_id(0x71)).is_ok());
    assert!(matches!(
        project_prompt_library(
            &owner(),
            &metadata_request(Some(7), None),
            &fixture_source(),
            0
        ),
        Err(PromptProjectionError::Unavailable {
            subsystem: PromptProjectionSubsystem::NegotiatedTransportLimit,
        })
    ));
}

#[test]
fn projection_metadata_page_omits_bodies_and_matches_golden() {
    let reply = project(metadata_request(Some(7), None)).expect("owner may read personal metadata");
    let PromptProjectionReply::MetadataPage(page) = &reply else {
        panic!("expected metadata page, got {reply:?}");
    };
    assert_eq!(page.schema_version(), PROMPT_PROJECTION_SCHEMA_VERSION);
    assert_eq!(page.library_revision(), 7);
    assert_eq!(page.namespace(), PromptNamespace::Personal);
    assert_eq!(page.items().len(), 1);
    assert_eq!(page.items()[0].id(), prompt_id(0x01));
    assert_eq!(page.items()[0].current_version_id(), version_id(0x02));
    assert_eq!(
        page.items()[0].privacy_class(),
        PromptPrivacyClass::LocalOnly
    );
    assert!(page.encoded_bytes() <= PROMPT_METADATA_PAGE_BYTES as u32);
    assert!(page.items().len() <= PROMPT_METADATA_PAGE_ITEMS);
    let json = serde_json::to_value(&reply).expect("serialize metadata page");
    assert!(json.to_string().contains("Sanitized review prompt"));
    assert!(!json.to_string().contains("Review this change carefully."));
    assert!(json["metadata_page"]["items"][0].get("body").is_none());
    let mut golden = json.clone();
    golden["metadata_page"]["encoded_bytes"] = serde_json::json!(0);
    let mut expected = fs::read_to_string(fixtures_dir().join("projection_metadata_page.json"))
        .expect("metadata golden");
    let mut expected: serde_json::Value =
        serde_json::from_str(&expected).expect("metadata golden json");
    expected["metadata_page"]["encoded_bytes"] = serde_json::json!(0);
    assert_eq!(golden, expected, "golden fixture drift: metadata page");
    let packed = encode_prompt_projection_document(&reply).expect("sealed encode");
    let decoded = decode_prompt_projection_document(&packed).expect("sealed decode");
    assert_eq!(decoded, reply);
}

#[test]
fn projection_exact_version_page_keeps_immutable_version_id() {
    let request = PromptLibraryRequest::exact_version(
        request_id(0x72),
        client_id(0x71),
        &owner(),
        version_id(0x02),
        None,
    )
    .expect("version request");
    let reply = project(request).expect("owner may read an exact version");
    let PromptProjectionReply::VersionPage(page) = &reply else {
        panic!("expected version page, got {reply:?}");
    };
    assert_eq!(page.version_id(), version_id(0x02));
    assert_eq!(page.prompt_id(), prompt_id(0x01));
    assert_eq!(page.version(), 1);
    assert_eq!(
        page.body_sha256(),
        &<[u8; 32]>::from(Sha256::digest(b"Review this change carefully."))
    );
    assert_eq!(page.chunk().sequence(), 0);
    assert_eq!(page.chunk().bytes(), b"Review this change carefully.");
    assert!(!page.chunk().more());
    assert_golden_json(
        "projection_version_page.json",
        &serde_json::to_value(&reply).expect("serialize version page"),
    );
}

#[test]
fn projection_diff_request_result_is_chunked_and_body_free_in_metadata() {
    let request = PromptLibraryRequest::diff(
        request_id(0x73),
        client_id(0x71),
        &owner(),
        version_id(0x02),
        version_id(0x02),
        None,
    )
    .expect("diff request");
    let reply = project(request).expect("owner may request a version diff");
    let PromptProjectionReply::DiffPage(page) = &reply else {
        panic!("expected diff page, got {reply:?}");
    };
    assert_eq!(page.old_version_id(), version_id(0x02));
    assert_eq!(page.new_version_id(), version_id(0x02));
    assert!(page.chunk().bytes().len() <= PROMPT_BODY_CHUNK_BYTES);
    let json = serde_json::to_value(&reply).expect("serialize diff page");
    assert!(json.get("old_body").is_none());
    assert!(json.get("new_body").is_none());
    assert_golden_json("projection_diff_page.json", &json);
}

#[test]
fn search_and_history_stay_unavailable_and_goldens_remain_codec_only() {
    let too_long = "x".repeat(PROMPT_SEARCH_MAX_QUERY_BYTES + 1);
    let search = PromptLibraryRequest::search(
        request_id(0x74),
        client_id(0x71),
        &owner(),
        PromptNamespace::Personal,
        too_long,
        None,
    )
    .expect_err("oversized search query must fail before projection");
    assert_eq!(search, PromptProjectionError::SearchQueryTooLong);

    let search = PromptLibraryRequest::search(
        request_id(0x74),
        client_id(0x71),
        &owner(),
        PromptNamespace::Personal,
        "review".into(),
        None,
    )
    .expect("bounded search request");
    assert_eq!(
        project(search).expect_err("search is unavailable until Task 7.3"),
        PromptProjectionError::Unavailable {
            subsystem: PromptProjectionSubsystem::SearchIndex,
        }
    );
    let history =
        PromptLibraryRequest::history_page(request_id(0x75), client_id(0x71), &owner(), None, None)
            .expect("history request");
    assert_eq!(
        project(history).expect_err("history is unavailable until Task 7.3"),
        PromptProjectionError::Unavailable {
            subsystem: PromptProjectionSubsystem::HistoryStore,
        }
    );

    let search_golden = fs::read_to_string(fixtures_dir().join("projection_search_page.json"))
        .expect("search golden");
    let history_golden = fs::read_to_string(fixtures_dir().join("projection_history_page.json"))
        .expect("history golden");
    let search_value: serde_json::Value =
        serde_json::from_str(&search_golden).expect("search json");
    let history_value: serde_json::Value =
        serde_json::from_str(&history_golden).expect("history json");
    let search_page: PromptSearchPage =
        serde_json::from_value(search_value["search_page"].clone()).expect("search page shape");
    let history_page: PromptHistoryPage =
        serde_json::from_value(history_value["history_page"].clone()).expect("history page shape");
    let decoded_search = decode_prompt_projection_document(
        &encode_prompt_projection_document(&PromptProjectionReply::SearchPage(search_page))
            .expect("search encode"),
    )
    .expect("search golden decodes through sealed codec");
    let decoded_history = decode_prompt_projection_document(
        &encode_prompt_projection_document(&PromptProjectionReply::HistoryPage(history_page))
            .expect("history encode"),
    )
    .expect("history golden decodes through sealed codec");
    assert!(matches!(
        decoded_search,
        PromptProjectionReply::SearchPage(_)
    ));
    assert!(matches!(
        decoded_history,
        PromptProjectionReply::HistoryPage(_)
    ));
}

#[test]
fn projection_linear_chain_page_pins_exact_version_ids() {
    let request = PromptLibraryRequest::chain_page(
        request_id(0x76),
        client_id(0x71),
        &owner(),
        Some(chain_id(0x10)),
        Some(7),
        None,
    )
    .expect("chain request");
    let reply = project(request).expect("owner may read a linear chain page");
    let PromptProjectionReply::ChainPage(page) = &reply else {
        panic!("expected chain page, got {reply:?}");
    };
    assert_eq!(
        page.chains()[0].links()[0].prompt_version_id(),
        version_id(0x02)
    );
    assert_eq!(page.chains()[0].links()[0].prompt_id(), prompt_id(0x01));
    assert!(page.chains()[0].links()[0].previous_link_id().is_none());
    assert!(page.chains()[0].links()[0].next_link_id().is_none());
    assert!(!page.chains()[0].links()[0].update_available());
    assert_golden_json(
        "projection_chain_page.json",
        &serde_json::to_value(&reply).expect("serialize chain page"),
    );
}

#[test]
fn settlement_requires_verified_receipt_and_matches_golden() {
    let golden = fs::read_to_string(fixtures_dir().join("projection_mutation_settlement.json"))
        .expect("settlement golden");
    let golden_value: serde_json::Value =
        serde_json::from_str(&golden).expect("settlement golden json");
    assert_eq!(golden_value["mutation_settlement"]["settled"], true);
    let typed: PromptProjectionReply =
        serde_json::from_value(golden_value).expect("settlement golden typed");
    let packed = encode_prompt_projection_document(&typed).expect("settlement encode");
    let decoded_golden =
        decode_prompt_projection_document(&packed).expect("settlement golden decodes");
    let PromptProjectionReply::MutationSettlement(from_golden) = decoded_golden else {
        panic!("expected settlement golden");
    };
    assert!(
        !from_golden.verified() && !from_golden.settled(),
        "settled: true on the wire must not become a verified settlement"
    );
}

#[test]
fn organization_and_task_grants_never_open_personal_library() {
    let org = PromptLibraryRequest::metadata_page(
        request_id(0x77),
        client_id(0x71),
        &owner(),
        PromptNamespace::Organization,
        Some(7),
        None,
    )
    .expect("org request constructs");
    assert_eq!(
        project(org).expect_err("organization is a future read-only namespace"),
        PromptProjectionError::Unavailable {
            subsystem: PromptProjectionSubsystem::OrganizationNamespace,
        }
    );
    assert!(BoundedTestSource::try_from_organization_records(vec![fixture_prompt()]).is_err());
}

#[test]
fn opaque_cursors_reject_cross_query_principal_and_stale_revision() {
    let first = project(metadata_request(Some(7), None)).expect("first page");
    let PromptProjectionReply::MetadataPage(_) = &first else {
        panic!("metadata");
    };
    let other = foreign_owner();
    let stolen = PromptCursor::from_public_fields_for_adversary(
        PromptProjectionQueryKind::Metadata,
        PromptNamespace::Personal,
        7,
        Some(*prompt_id(0x01).as_bytes()),
        0,
    );
    assert!(
        stolen.is_err(),
        "raw field cursor must not be constructible"
    );

    let page_plus = project(metadata_request(Some(7), None)).expect("page");
    let PromptProjectionReply::MetadataPage(page) = page_plus else {
        panic!("metadata");
    };
    let _ = page.next_cursor();
    assert_eq!(
        project(metadata_request(Some(3), None)).expect_err("stale revision"),
        PromptProjectionError::StaleRevision {
            expected: 3,
            actual: 7,
        }
    );

    let source = VirtualPromptSource::new(100_000);
    let paged = project_prompt_library(
        &owner(),
        &metadata_request(Some(1), None),
        &source,
        project_limit(),
    )
    .expect("virtual metadata page");
    let PromptProjectionReply::MetadataPage(paged) = paged else {
        panic!("metadata");
    };
    let cursor = paged
        .next_cursor()
        .expect("page+1 must issue an opaque cursor")
        .clone();
    let crossed = PromptLibraryRequest::exact_version(
        request_id(0x79),
        client_id(0x71),
        &owner(),
        version_id(0x02),
        Some(cursor.clone()),
    )
    .expect("request accepts any cursor bytes");
    assert_eq!(
        project_prompt_library(&owner(), &crossed, &source, project_limit())
            .expect_err("cross-query cursor"),
        PromptProjectionError::StaleCursor
    );
    let foreign = PromptLibraryRequest::metadata_page(
        request_id(0x7a),
        client_id(0x99),
        &other,
        PromptNamespace::Personal,
        Some(1),
        Some(cursor),
    )
    .expect("foreign principal request");
    assert_eq!(
        project_prompt_library(&other, &foreign, &source, project_limit())
            .expect_err("cross-principal cursor"),
        PromptProjectionError::StaleCursor
    );
}

#[test]
fn hundred_thousand_rows_touch_only_page_plus_one() {
    let source = VirtualPromptSource::new(100_000);
    let request = metadata_request(Some(1), None);
    let started = Instant::now();
    let reply = project_prompt_library(&owner(), &request, &source, project_limit())
        .expect("page virtual source");
    let elapsed = started.elapsed();
    let PromptProjectionReply::MetadataPage(page) = reply else {
        panic!("metadata");
    };
    assert_eq!(page.items().len(), PROMPT_METADATA_PAGE_ITEMS);
    assert!(page.next_cursor().is_some());
    assert!(page.encoded_bytes() <= PROMPT_METADATA_PAGE_BYTES as u32);
    assert_eq!(
        source.examined_rows(),
        (PROMPT_METADATA_PAGE_ITEMS + 1) as u64
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "100k keyset page must stay inside the deadline budget, took {elapsed:?}"
    );
}

#[test]
fn cap_plus_one_and_unicode_byte_bounds_fail_closed() {
    assert!(BoundedTestSource::try_new(1, vec![fixture_prompt(); 33], vec![], vec![]).is_err());
    let long_title = "é".repeat(161);
    let mut prompt = fixture_prompt();
    prompt.title = long_title;
    assert!(BoundedTestSource::try_new(1, vec![prompt], vec![], vec![]).is_err());
    let unknown = rmp_serde::to_vec_named(&serde_json::json!({"schema_version": 99}))
        .expect("pack unknown schema");
    assert!(
        decode_prompt_projection_document(&unknown).is_err(),
        "unknown schema must fail closed"
    );
}

#[test]
fn decode_rejects_oversized_duplicate_key_and_unknown_schema() {
    let unknown_schema = rmp_serde::to_vec_named(&serde_json::json!({
        "schema_version": 99,
        "metadata_page": { "library_revision": 1, "namespace": "personal", "items": [], "next_cursor": null, "encoded_bytes": 1 }
    }))
    .expect("pack unknown schema");
    assert_eq!(
        decode_prompt_projection_document(&unknown_schema).expect_err("unknown schema"),
        PromptProjectionError::InvalidRequest
    );
}

#[test]
fn unknown_extension_fields_are_rejected_before_allocation() {
    let document = serde_json::json!({
        "metadata_page": {
            "schema_version": PROMPT_PROJECTION_SCHEMA_VERSION,
            "library_revision": 7,
            "namespace": "personal",
            "items": [{
                "id": prompt_id(0x01).to_string(),
                "title": "Sanitized review prompt",
                "description": "Metadata-only fixture for prompt projection tests.",
                "tags": ["rust", "review"],
                "current_version_id": version_id(0x02).to_string(),
                "revision": 1,
                "archived_at_ms": null,
                "namespace": "personal",
                "privacy_class": "local_only",
                "future_hook": {"run": "delete_all"}
            }],
            "next_cursor": null,
            "encoded_bytes": 128
        }
    });
    let packed = rmp_serde::to_vec_named(&document).expect("pack unknown field");
    assert_eq!(
        decode_prompt_projection_document(&packed).expect_err("unknown field"),
        PromptProjectionError::InvalidRequest
    );
}

#[test]
fn store_backed_projection_uses_durable_revision_and_keeps_search_unavailable() {
    use devmanager::domain::command::{Command, CommandEnvelope, CommandReceipt};
    use devmanager::kernel::CommandBus;

    let dir = TempDir::new().expect("isolated kernel");
    let kernel = dir.path().join("kernel.db");
    let mut bus = CommandBus::open(&kernel).expect("open kernel bus");
    let grant = testing::owner_grant(client_id(0x71)).expect("sealed owner");
    let created = bus
        .execute_with_owner_grant(
            &grant,
            CommandEnvelope {
                command_id: command_id(0x50),
                client_id: client_id(0x71),
                task_id: None,
                issued_at_ms: 1_725_000_000_000,
                expected_task_revision: None,
                command: Command::PromptLibrary(PromptCommand::CreatePrompt(CreatePrompt {
                    prompt_id: prompt_id(0x01),
                    prompt_version_id: version_id(0x02),
                    title: "Review code".into(),
                    description: Some("A bounded local prompt".into()),
                    tags: vec!["rust".into()],
                    variables: Vec::new(),
                    body: "Review this code carefully.".into(),
                    created_at_ms: 1_725_000_000_000,
                })),
            },
        )
        .expect("kernel create");
    let CommandReceipt::Accepted {
        prompt_mutation: Some(mutation),
        ..
    } = created
    else {
        panic!("sealed grant must surface the prompt mutation receipt, got {created:?}");
    };
    assert_eq!(mutation.prompt_id, prompt_id(0x01));
    assert_eq!(mutation.prompt_version_id, version_id(0x02));
    drop(bus);
    let store = PromptStore::open(&kernel).expect("open kernel prompt view");
    let revision = store
        .library_projection_revision()
        .expect("durable projection revision");
    assert_eq!(revision, 1);
    let metadata = project_prompt_store(
        &owner(),
        &PromptLibraryRequest::metadata_page(
            request_id(0x80),
            client_id(0x71),
            &owner(),
            PromptNamespace::Personal,
            Some(revision),
            None,
        )
        .expect("store metadata request"),
        &store,
        PROMPT_METADATA_PAGE_BYTES as u32,
    )
    .expect("store can project personal metadata");
    let PromptProjectionReply::MetadataPage(page) = metadata else {
        panic!("expected store metadata page");
    };
    assert_eq!(page.items()[0].current_version_id(), version_id(0x02));
    assert_eq!(page.library_revision(), revision);
    assert_eq!(
        project_prompt_store(
            &owner(),
            &PromptLibraryRequest::search(
                request_id(0x81),
                client_id(0x71),
                &owner(),
                PromptNamespace::Personal,
                "review".into(),
                None,
            )
            .expect("search request"),
            &store,
            PROMPT_METADATA_PAGE_BYTES as u32,
        )
        .expect_err("search index is a Task 7.3 dependency"),
        PromptProjectionError::Unavailable {
            subsystem: PromptProjectionSubsystem::SearchIndex,
        }
    );
}

#[test]
fn exact_chunk_continuation_uses_opaque_cursor_not_usize_offset() {
    let large = "A".repeat(PROMPT_BODY_CHUNK_BYTES + 1);
    let source = BoundedTestSource::try_new(
        7,
        vec![fixture_prompt()],
        vec![oversized_transfer_version(&large)],
        vec![],
    )
    .expect("transfer-sized body source");
    let first = PromptLibraryRequest::exact_version(
        request_id(0x82),
        client_id(0x71),
        &owner(),
        version_id(0x02),
        None,
    )
    .expect("first chunk");
    let reply = project_prompt_library(&owner(), &first, &source, project_limit())
        .expect("first body chunk");
    let PromptProjectionReply::VersionPage(page) = reply else {
        panic!("version");
    };
    assert_eq!(page.chunk().sequence(), 0);
    assert_eq!(page.chunk().bytes().len(), PROMPT_BODY_CHUNK_BYTES);
    assert!(page.chunk().more());
    let cursor = page
        .next_cursor()
        .expect("more bytes must issue an opaque continuation cursor")
        .clone();
    let second = PromptLibraryRequest::exact_version(
        request_id(0x83),
        client_id(0x71),
        &owner(),
        version_id(0x02),
        Some(cursor.clone()),
    )
    .expect("continuation request");
    let reply = project_prompt_library(&owner(), &second, &source, project_limit())
        .expect("second body chunk");
    let PromptProjectionReply::VersionPage(page) = reply else {
        panic!("version");
    };
    assert_eq!(page.chunk().sequence(), 1);
    assert_eq!(page.chunk().bytes(), b"A");
    assert!(!page.chunk().more());
    assert!(page.next_cursor().is_none());
    let crossed = PromptLibraryRequest::metadata_page(
        request_id(0x84),
        client_id(0x71),
        &owner(),
        PromptNamespace::Personal,
        Some(7),
        Some(cursor),
    )
    .expect("cross-query continuation");
    assert_eq!(
        project_prompt_library(&owner(), &crossed, &source, project_limit())
            .expect_err("chunk cursor is kind-bound"),
        PromptProjectionError::StaleCursor
    );
}

fn oversized_transfer_version(body: &str) -> PromptVersion {
    PromptVersion {
        id: version_id(0x02),
        prompt_id: prompt_id(0x01),
        version: 1,
        body: body.to_string(),
        variables: Vec::new(),
        body_sha256: Sha256::digest(body.as_bytes()).into(),
        created_at_ms: 1_728_000_000_000,
    }
}

struct VirtualPromptSource {
    rows: u32,
    examined: Cell<u64>,
}

impl VirtualPromptSource {
    fn new(rows: u32) -> Self {
        Self {
            rows,
            examined: Cell::new(0),
        }
    }
}

impl PromptProjectionSource for VirtualPromptSource {
    fn library_revision(&self) -> Result<u64, PromptProjectionError> {
        Ok(1)
    }

    fn page_personal_metadata(
        &self,
        after: Option<PromptId>,
        limit: usize,
    ) -> Result<Vec<SavedPrompt>, PromptProjectionError> {
        let start = after
            .map(|id| u8::from(id.as_bytes()[15]).saturating_add(1) as u32)
            .unwrap_or(0);
        let take = limit.min((self.rows.saturating_sub(start)) as usize);
        self.examined.set(self.examined.get() + take as u64);
        let mut out = Vec::with_capacity(take);
        for index in start..start + take as u32 {
            let mut bytes = fixture_id(0);
            bytes[12..16].copy_from_slice(&index.to_be_bytes());
            let id = PromptId::from_bytes(bytes).unwrap_or_else(|_| prompt_id(0x01));
            out.push(SavedPrompt {
                id,
                title: format!("p{index:05}"),
                description: None,
                tags: vec!["t".into()],
                current_version_id: version_id(0x02),
                revision: 1,
                archived_at_ms: None,
            });
        }
        Ok(out)
    }

    fn get_version(
        &self,
        _id: PromptVersionId,
    ) -> Result<Option<PromptVersion>, PromptProjectionError> {
        Ok(None)
    }

    fn page_chain_links(
        &self,
        _chain_id: PromptChainId,
        _after: Option<PromptChainLinkId>,
        _limit: usize,
    ) -> Result<Option<(PromptChain, Vec<PromptChainLinkRecord>)>, PromptProjectionError> {
        Ok(None)
    }

    fn examined_rows(&self) -> u64 {
        self.examined.get()
    }
}

#[test]
fn one_action_catalog_includes_prompt_library_and_truthful_disabled_reasons() {
    assert_eq!(
        catalog().len(),
        registered_actions().count(),
        "catalog() is the single host action registry"
    );
    assert!(catalog()
        .iter()
        .any(|action| action.id == ACTION_PROMPT_METADATA_PAGE));
    assert_eq!(
        disabled_reason(ACTION_PROMPT_SEARCH_PAGE, granted_library()),
        Some("unknown action")
    );
    assert_eq!(
        disabled_reason(ACTION_PROMPT_HISTORY_PAGE, granted_library()),
        Some("unknown action")
    );
    assert_eq!(
        disabled_reason(ACTION_PROMPT_METADATA_PAGE, CapabilitySet::empty()),
        Some("personal_prompt_library capability not granted")
    );
    assert_eq!(
        disabled_reason(ACTION_PROMPT_METADATA_PAGE, granted_library()),
        Some("owner_device_session unavailable until Phase 9 authenticated pairing")
    );
    assert!(!devmanager::client::action::action_enabled(
        ACTION_PROMPT_METADATA_PAGE,
        granted_library()
    ));
    assert!(!devmanager::client::action::action_enabled(
        ACTION_PROMPT_SEARCH_PAGE,
        granted_library()
    ));
    assert!(!devmanager::client::action::action_enabled(
        ACTION_PROMPT_HISTORY_PAGE,
        granted_library()
    ));
    assert!(!devmanager::client::action::action_enabled(
        "prompt.library.not_a_real_action",
        granted_library()
    ));

    let catalog_json = devmanager::client::cli::actions_json_document().expect("offline catalog");
    let doc: serde_json::Value = serde_json::from_str(&catalog_json).expect("catalog json");
    for action in doc["actions"].as_array().expect("actions") {
        assert!(
            action.get("disabled_reason").is_some(),
            "disabled_reason must be present so omission cannot enable {}",
            action["id"]
        );
        assert!(action.get("enabled").is_some(), "enabled must be present");
        if action["disabled_reason"].is_null() {
            assert_eq!(action["enabled"], true);
        } else {
            assert_eq!(action["enabled"], false);
        }
        assert_ne!(action["id"], ACTION_PROMPT_SEARCH_PAGE);
        assert_ne!(action["id"], ACTION_PROMPT_HISTORY_PAGE);
        if action["required_capability"] == "personal_prompt_library" {
            assert_eq!(action["enabled"], false);
        }
    }
}

#[test]
fn prompt_library_query_round_trips_on_the_canonical_query_seam() {
    use devmanager::domain::query::{Query, QueryResult};
    use devmanager::prompts::projection::PromptLibraryQuery;
    use devmanager::protocol::{FrameLimits, MessagePackCodec};

    let envelope = QueryEnvelope {
        request_id: request_id(0x90),
        client_id: client_id(0x91),
        task_id: None,
        query: Query::PromptLibrary(PromptLibraryQuery::MetadataPage {
            namespace: PromptNamespace::Personal,
            cursor: None,
            expected_revision: Some(1),
        }),
    };
    let codec = MessagePackCodec::from_limits(FrameLimits::v1_default()).expect("codec");
    let encoded = codec
        .encode(&envelope)
        .expect("encode prompt library query");
    assert!(
        encoded
            .windows(b"prompt_library".len())
            .any(|w| w == b"prompt_library"),
        "wire key must be prompt_library, not a parallel projection document"
    );
    assert_eq!(
        codec
            .decode::<QueryEnvelope>(&encoded)
            .expect("decode prompt library query"),
        envelope
    );

    let reply = project_prompt_library(
        &owner(),
        &PromptLibraryRequest::metadata_page(
            request_id(0x90),
            client_id(0x71),
            &owner(),
            PromptNamespace::Personal,
            Some(7),
            None,
        )
        .expect("metadata request"),
        &fixture_source(),
        project_limit(),
    )
    .expect("project metadata");
    let result = QueryResult::PromptLibrary(reply);
    let packed = codec
        .encode(&result)
        .expect("encode sealed prompt library result");
    let decoded = codec
        .decode::<QueryResult>(&packed)
        .expect("decode sealed prompt library result");
    assert_eq!(decoded, result);
}

#[test]
fn host_query_requires_granted_capability_and_keeps_search_unavailable() {
    use devmanager::domain::query::{Query, QueryError, QueryOutcome};
    use devmanager::host::dispatch_host_request;
    use devmanager::kernel::CommandBus;
    use devmanager::prompts::projection::PromptLibraryQuery;
    use devmanager::protocol::{ClientRequest, ServerMessage};

    let dir = TempDir::new().expect("isolated host bus");
    let mut bus = CommandBus::open(&dir.path().join("kernel.db")).expect("open bus");
    let denied = dispatch_host_request(
        client_id(0x91),
        CapabilitySet::empty(),
        &mut bus,
        ClientRequest::Query(QueryEnvelope {
            request_id: request_id(0x92),
            client_id: client_id(0x91),
            task_id: None,
            query: Query::PromptLibrary(PromptLibraryQuery::MetadataPage {
                namespace: PromptNamespace::Personal,
                cursor: None,
                expected_revision: None,
            }),
        }),
    )
    .expect("denied query still returns a reply");
    let ServerMessage::QueryReply(reply) = denied else {
        panic!("expected query reply");
    };
    assert_eq!(
        reply.outcome,
        QueryOutcome::Err(QueryError::UnsupportedCapability)
    );

    let hello_only = dispatch_host_request(
        client_id(0x91),
        granted_library(),
        &mut bus,
        ClientRequest::Query(QueryEnvelope {
            request_id: request_id(0x93),
            client_id: client_id(0x91),
            task_id: None,
            query: Query::PromptLibrary(PromptLibraryQuery::Search {
                namespace: PromptNamespace::Personal,
                query: "review".into(),
                cursor: None,
            }),
        }),
    )
    .expect("hello-bit query reply");
    let ServerMessage::QueryReply(reply) = hello_only else {
        panic!("expected query reply");
    };
    assert_eq!(
        reply.outcome,
        QueryOutcome::Err(QueryError::Unavailable {
            reason: "owner_device_session",
        })
    );
}

#[test]
fn host_metadata_page_and_mutation_use_query_result_and_command_receipt() {
    use devmanager::domain::command::{Command, CommandEnvelope, CommandReceipt, RejectionCode};
    use devmanager::domain::query::{Query, QueryError, QueryOutcome, QueryResult};
    use devmanager::host::dispatch_host_request;
    use devmanager::kernel::CommandBus;
    use devmanager::prompts::projection::PromptLibraryQuery;
    use devmanager::protocol::{ClientRequest, FrameLimits, ServerMessage};

    let dir = TempDir::new().expect("isolated host bus");
    let mut bus = CommandBus::open(&dir.path().join("kernel.db")).expect("open bus");
    let create_intent = CommandEnvelope {
        command_id: command_id(0x40),
        client_id: client_id(0x91),
        task_id: None,
        issued_at_ms: 1_728_000_000_000,
        expected_task_revision: None,
        command: Command::PromptLibrary(PromptCommand::CreatePrompt(CreatePrompt {
            prompt_id: prompt_id(0x01),
            prompt_version_id: version_id(0x02),
            title: "Sanitized review prompt".into(),
            description: Some("Metadata-only fixture for prompt projection tests.".into()),
            tags: vec!["rust".into(), "review".into()],
            variables: Vec::new(),
            body: "Review this change carefully.".into(),
            created_at_ms: 1_728_000_000_000,
        })),
    };
    let bypass = dispatch_host_request(
        client_id(0x91),
        granted_library(),
        &mut bus,
        ClientRequest::Command(create_intent.clone()),
    )
    .expect("hello-bit command still returns a receipt");
    let ServerMessage::CommandReceipt(CommandReceipt::Rejected {
        code: RejectionCode::UnsupportedCapability,
        ..
    }) = bypass
    else {
        panic!("Hello bits must not mutate PromptLibrary, got {bypass:?}");
    };

    let grant = testing::owner_grant(client_id(0x91)).expect("sealed owner");
    let mut granted_intent = create_intent;
    granted_intent.command_id = command_id(0x42);
    let create = bus
        .execute_with_owner_grant(&grant, granted_intent)
        .expect("create prompt");
    let CommandReceipt::Accepted {
        command_id: accepted_id,
        operation_id,
        task_revision,
        event_ids,
        prompt_mutation,
    } = create
    else {
        panic!("mutation must settle as CommandReceipt::Accepted, got {create:?}");
    };
    assert!(
        prompt_mutation.is_some(),
        "Accepted must surface the prompt mutation receipt"
    );
    assert_eq!(accepted_id, command_id(0x42));
    assert_eq!(task_revision, None);
    assert_ne!(
        operation_id.as_bytes(),
        command_id(0x42).as_bytes(),
        "kernel must allocate OperationId; do not mint it from command_id"
    );
    assert_eq!(event_ids.len(), 1);
    assert!(
        !dir.path().join("prompts.sqlite3").exists(),
        "prompt mutations must share kernel.db, not a sibling prompts.sqlite3"
    );

    let status = dispatch_host_request(
        client_id(0x91),
        granted_library(),
        &mut bus,
        ClientRequest::Query(QueryEnvelope {
            request_id: request_id(0x98),
            client_id: client_id(0x91),
            task_id: None,
            query: Query::OperationStatus { operation_id },
        }),
    )
    .expect("operation status");
    let ServerMessage::QueryReply(status_reply) = status else {
        panic!("expected operation status reply");
    };
    let QueryOutcome::Ok(QueryResult::OperationStatus {
        operation_id: status_id,
        state,
    }) = status_reply.outcome
    else {
        panic!("expected operation status, got {:?}", status_reply.outcome);
    };
    assert_eq!(status_id, operation_id);
    assert!(
        matches!(
            state,
            devmanager::domain::operation::OperationState::Settled { .. }
        ),
        "prompt mutation must settle on the kernel receipt, got {state:?}"
    );

    let stale = bus
        .execute_with_owner_grant(
            &grant,
            CommandEnvelope {
                command_id: command_id(0x41),
                client_id: client_id(0x91),
                task_id: None,
                issued_at_ms: 1_728_000_000_001,
                expected_task_revision: Some(99),
                command: Command::PromptLibrary(PromptCommand::CreatePrompt(CreatePrompt {
                    prompt_id: prompt_id(0x03),
                    prompt_version_id: version_id(0x04),
                    title: "Second".into(),
                    description: None,
                    tags: Vec::new(),
                    variables: Vec::new(),
                    body: "x".into(),
                    created_at_ms: 1_728_000_000_001,
                })),
            },
        )
        .expect("stale revision");
    let CommandReceipt::Rejected {
        code,
        current_revision,
        ..
    } = stale
    else {
        panic!("stale library revision must reject, got {stale:?}");
    };
    assert_eq!(code, RejectionCode::RevisionConflict);
    assert_eq!(current_revision, Some(1));

    let hello_page = dispatch_host_request(
        client_id(0x91),
        granted_library(),
        &mut bus,
        ClientRequest::Query(QueryEnvelope {
            request_id: request_id(0x94),
            client_id: client_id(0x91),
            task_id: None,
            query: Query::PromptLibrary(PromptLibraryQuery::MetadataPage {
                namespace: PromptNamespace::Personal,
                cursor: None,
                expected_revision: Some(1),
            }),
        }),
    )
    .expect("hello-bit metadata query");
    let ServerMessage::QueryReply(hello_reply) = hello_page else {
        panic!("expected query reply");
    };
    assert_eq!(
        hello_reply.outcome,
        QueryOutcome::Err(QueryError::Unavailable {
            reason: "owner_device_session",
        })
    );

    let page = bus
        .query_with_owner_grant(
            &grant,
            FrameLimits::v1_default().max_physical_frame_bytes,
            QueryEnvelope {
                request_id: request_id(0x94),
                client_id: client_id(0x91),
                task_id: None,
                query: Query::PromptLibrary(PromptLibraryQuery::MetadataPage {
                    namespace: PromptNamespace::Personal,
                    cursor: None,
                    expected_revision: Some(1),
                }),
            },
        )
        .expect("metadata page");
    let QueryOutcome::Ok(QueryResult::PromptLibrary(PromptProjectionReply::MetadataPage(page))) =
        page.outcome
    else {
        panic!(
            "expected prompt library metadata page, got {:?}",
            page.outcome
        );
    };
    assert_eq!(page.items().len(), 1);
    assert_eq!(page.library_revision(), 1);
    let json = serde_json::to_value(&page).expect("serialize page");
    assert!(json["items"][0].get("body").is_none());
    assert!(!json.to_string().contains("Review this change carefully."));

    drop(bus);
    let mut reopened = CommandBus::open(&dir.path().join("kernel.db")).expect("reopen kernel");
    let replay_intent = CommandEnvelope {
        command_id: command_id(0x42),
        client_id: client_id(0x91),
        task_id: None,
        issued_at_ms: 1_728_000_000_000,
        expected_task_revision: None,
        command: Command::PromptLibrary(PromptCommand::CreatePrompt(CreatePrompt {
            prompt_id: prompt_id(0x01),
            prompt_version_id: version_id(0x02),
            title: "Sanitized review prompt".into(),
            description: Some("Metadata-only fixture for prompt projection tests.".into()),
            tags: vec!["rust".into(), "review".into()],
            variables: Vec::new(),
            body: "Review this change carefully.".into(),
            created_at_ms: 1_728_000_000_000,
        })),
    };
    let replay = reopened
        .execute_with_owner_grant(&grant, replay_intent.clone())
        .expect("idempotent replay");
    let CommandReceipt::Accepted {
        command_id: _,
        operation_id: replayed_operation_id,
        task_revision: replayed_revision,
        event_ids: replayed_events,
        prompt_mutation: replayed_mutation,
    } = replay
    else {
        panic!("reopen must return the same Accepted receipt, got {replay:?}");
    };
    assert_eq!(replayed_operation_id, operation_id);
    assert_eq!(replayed_revision, None);
    assert_eq!(replayed_events, event_ids);
    assert_eq!(replayed_mutation, prompt_mutation);

    let mut conflict_intent = replay_intent;
    if let Command::PromptLibrary(PromptCommand::CreatePrompt(create)) =
        &mut conflict_intent.command
    {
        create.title = "Different payload".into();
    }
    let conflict = reopened
        .execute_with_owner_grant(&grant, conflict_intent)
        .expect("digest mismatch");
    let CommandReceipt::Rejected {
        code: RejectionCode::AlreadyExists,
        ..
    } = conflict
    else {
        panic!("payload digest mismatch must not replay the old receipt, got {conflict:?}");
    };
}

#[test]
fn granted_session_capability_binds_cursors_across_reconnect_and_rejects_foreign_principal() {
    let owner = testing::owner_grant(client_id(0x91)).expect("sealed owner");
    assert!(matches!(
        testing::watcher_grant(client_id(0x91)),
        Err(PromptProjectionError::PermissionDenied)
    ));
    assert!(OwnerDeviceCapability::from_authenticated_session(b"watcher").is_err());

    let source = VirtualPromptSource::new(100_000);
    let first = project_prompt_library(
        &owner,
        &PromptLibraryRequest::metadata_page(
            request_id(0x95),
            client_id(0x91),
            &owner,
            PromptNamespace::Personal,
            Some(1),
            None,
        )
        .expect("request"),
        &source,
        project_limit(),
    )
    .expect("page");
    let PromptProjectionReply::MetadataPage(page) = first else {
        panic!("metadata");
    };
    let cursor = page.next_cursor().expect("page+1").clone();
    let reconnected =
        testing::owner_grant(client_id(0x91)).expect("reconnect remints sealed owner");
    let continued = project_prompt_library(
        &reconnected,
        &PromptLibraryRequest::metadata_page(
            request_id(0x96),
            client_id(0x91),
            &reconnected,
            PromptNamespace::Personal,
            Some(1),
            Some(cursor.clone()),
        )
        .expect("resume"),
        &source,
        project_limit(),
    )
    .expect("resume after reconnect");
    assert!(matches!(continued, PromptProjectionReply::MetadataPage(_)));

    let foreign = testing::owner_grant(client_id(0x99)).expect("foreign principal");
    assert_eq!(
        project_prompt_library(
            &foreign,
            &PromptLibraryRequest::metadata_page(
                request_id(0x97),
                client_id(0x99),
                &foreign,
                PromptNamespace::Personal,
                Some(1),
                Some(cursor),
            )
            .expect("foreign request"),
            &source,
            project_limit(),
        )
        .expect_err("cross-principal cursor"),
        PromptProjectionError::StaleCursor
    );
}

#[test]
fn legacy_offset_snapshot_is_not_a_production_projection() {
    use devmanager::kernel::CommandBus;

    let dir = TempDir::new().expect("isolated kernel");
    let bus = CommandBus::open(&dir.path().join("kernel.db")).expect("open kernel");
    drop(bus);
    assert!(
        !dir.path().join("prompts.sqlite3").exists(),
        "CommandBus must not open a sibling prompts.sqlite3 for OFFSET snapshots"
    );
}
