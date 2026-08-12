use devmanager::providers::capabilities::{
    CapabilitySupport, ProviderCapabilities, ProviderExecutable, ProviderKind,
};
use devmanager::providers::conformance::{
    authenticate_fixture, classify_compatibility, classify_fixture_event, confined_artifact_path,
    decide_strict_resume, decode_fixture_bytes, dependency_holds, discover_provider_smoke_holds,
    evaluate_provider_smoke, promote_seeded_trace, provider_smoke_environment,
    reject_smoke_sensitive_payload, CompatibilityMode, ConformanceArm, ConformanceError,
    ConformanceHold, ConformanceIndex, DeclaredMetricId, EventDisposition,
    PinnedGenerationContract, ProviderConformanceCaseId, ProviderConformanceLab,
    ProviderEventClass, ProviderSmokeArm, ProviderSmokeDisposition, ProviderSmokeEvidence,
    ProviderSmokeHold, ProviderSmokeInvariants, ProviderSmokeRejection, ProviderSmokeRequest,
    ResumeOutcome, SanitizerRejection, StrictResumeFailure, MAX_CONFORMANCE_ARRAY_ITEMS,
    MAX_CONFORMANCE_DECODE_BYTES, MAX_CONFORMANCE_DEPTH, MAX_CONFORMANCE_MAP_KEYS,
    MAX_CONFORMANCE_NODES, MAX_PROVIDER_SMOKE_DEADLINE_MS, PROVIDER_CONFORMANCE_SCHEMA_VERSION,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

const BASELINE: &str = include_str!("fixtures/conformance/providers/v1/baseline.json");
const NEWER: &str = include_str!("fixtures/conformance/providers/v1/newer_version.json");
const MISSING_HOOKS: &str = include_str!("fixtures/conformance/providers/v1/missing_hooks.json");
const UNKNOWN_EVENT: &str = include_str!("fixtures/conformance/providers/v1/unknown_event.json");
const MALFORMED_EVENT: &str =
    include_str!("fixtures/conformance/providers/v1/malformed_event.json");
const PROBE_FAILURE: &str = include_str!("fixtures/conformance/providers/v1/probe_failure.json");
const RESUME_SUCCESS: &str = include_str!("fixtures/conformance/providers/v1/resume_success.json");
const RESUME_FAILURE: &str = include_str!("fixtures/conformance/providers/v1/resume_failure.json");
const INTERRUPTED: &str = include_str!("fixtures/conformance/providers/v1/interrupted.json");
const SEEDED_CLEAN: &str =
    include_str!("fixtures/conformance/providers/v1/seeded_failure_clean.json");

fn fixture(raw: &str) -> Value {
    serde_json::from_str(raw).expect("committed provider conformance fixture")
}

fn capabilities_from(raw: &str) -> ProviderCapabilities {
    serde_json::from_value(fixture(raw)["capabilities"].clone()).expect("fixture capabilities")
}

fn executable(label: &str) -> ProviderExecutable {
    ProviderExecutable::new(PathBuf::from(format!("C:/fixture/{label}")), {
        let mut digest = [0_u8; 32];
        digest[0] = label.as_bytes()[0];
        digest
    })
    .unwrap()
}

#[test]
fn baseline_and_newer_versions_compare_only_declared_metrics() {
    let root = tempfile::tempdir().unwrap();
    let lab = ProviderConformanceLab::open(root.path()).unwrap();
    let baseline = lab
        .execute_fixture(BASELINE, ConformanceArm::Baseline)
        .unwrap();
    let newer = lab.execute_fixture(NEWER, ConformanceArm::Variant).unwrap();

    assert_eq!(
        baseline.case_id(),
        ProviderConformanceCaseId::BaselineVersion
    );
    assert_eq!(newer.case_id(), ProviderConformanceCaseId::NewerVersion);
    assert_eq!(
        classify_compatibility(&capabilities_from(BASELINE), true, true).unwrap(),
        CompatibilityMode::Semantic
    );
    assert_eq!(
        classify_compatibility(&capabilities_from(NEWER), true, true).unwrap(),
        CompatibilityMode::Semantic
    );

    let index = ConformanceIndex::rebuild([baseline.clone(), newer.clone()]).unwrap();
    let comparison = index
        .compare_arms(
            ProviderConformanceCaseId::BaselineVersion,
            ConformanceArm::Baseline,
            ProviderConformanceCaseId::NewerVersion,
            ConformanceArm::Variant,
        )
        .unwrap();
    assert!(comparison
        .delta(DeclaredMetricId::UnknownEventFallback)
        .is_some());
    assert_eq!(
        newer.metric(DeclaredMetricId::UnknownEventFallback),
        Some(1)
    );
    assert_eq!(
        baseline.metric(DeclaredMetricId::NormalizedEventCount),
        Some(3)
    );
    assert_eq!(
        newer.metric(DeclaredMetricId::NormalizedEventCount),
        Some(3)
    );
    assert!(comparison.undeclared_metric_ids().next().is_none());
}

#[test]
fn missing_hooks_and_probe_failure_keep_terminal_only_launch() {
    let missing = classify_compatibility(&capabilities_from(MISSING_HOOKS), true, true).unwrap();
    let probe_failed =
        classify_compatibility(&capabilities_from(PROBE_FAILURE), false, true).unwrap();
    assert_eq!(missing, CompatibilityMode::TerminalOnly);
    assert_eq!(probe_failed, CompatibilityMode::TerminalOnly);

    let root = tempfile::tempdir().unwrap();
    let lab = ProviderConformanceLab::open(root.path()).unwrap();
    let missing_run = lab
        .execute_fixture(MISSING_HOOKS, ConformanceArm::Baseline)
        .unwrap();
    let probe_run = lab
        .execute_fixture(PROBE_FAILURE, ConformanceArm::Variant)
        .unwrap();
    assert_eq!(
        missing_run.metric(DeclaredMetricId::TerminalFallback),
        Some(1)
    );
    assert_eq!(
        probe_run.metric(DeclaredMetricId::TerminalFallback),
        Some(1)
    );
    assert_eq!(
        missing_run.metric(DeclaredMetricId::ExactResumeResult),
        Some(0)
    );
}

#[test]
fn unknown_and_malformed_events_are_quarantined_without_stopping_pty() {
    let unknown = fixture(UNKNOWN_EVENT);
    let malformed = fixture(MALFORMED_EVENT);
    let events = unknown["events"].as_array().unwrap();
    let (class, disposition) = classify_fixture_event(&events[1]);
    assert!(matches!(
        class,
        ProviderEventClass::Unknown { ref source_type, .. } if source_type == "future_widget_event"
    ));
    assert_eq!(disposition, EventDisposition::QuarantineKeepPtyAlive);

    let malformed_events = malformed["events"].as_array().unwrap();
    let (class, disposition) = classify_fixture_event(&malformed_events[1]);
    assert!(matches!(class, ProviderEventClass::Malformed { .. }));
    assert_eq!(disposition, EventDisposition::QuarantineKeepPtyAlive);
    let (class, disposition) = classify_fixture_event(&malformed_events[2]);
    assert!(matches!(class, ProviderEventClass::Malformed { .. }));
    assert_eq!(disposition, EventDisposition::QuarantineKeepPtyAlive);

    let root = tempfile::tempdir().unwrap();
    let lab = ProviderConformanceLab::open(root.path()).unwrap();
    let unknown_run = lab
        .execute_fixture(UNKNOWN_EVENT, ConformanceArm::Baseline)
        .unwrap();
    assert_eq!(
        unknown_run.metric(DeclaredMetricId::UnknownEventFallback),
        Some(1)
    );
    assert_eq!(
        unknown_run.metric(DeclaredMetricId::NormalizedEventCount),
        Some(2)
    );
    assert_eq!(
        unknown_run.metric(DeclaredMetricId::TerminalFallback),
        Some(0)
    );
    assert!(!unknown_run.pty_terminated());
}

#[test]
fn strict_resume_succeeds_or_fails_visibly_without_fresh_fallback() {
    let success = decide_strict_resume(&fixture(RESUME_SUCCESS)).unwrap();
    assert!(matches!(
        success,
        ResumeOutcome::Succeeded { ref provider_session_id }
            if provider_session_id.as_str() == "sess_fixture_resume_ok"
    ));

    let failure = decide_strict_resume(&fixture(RESUME_FAILURE)).unwrap();
    assert!(matches!(
        failure,
        ResumeOutcome::FailedVisible {
            reason: StrictResumeFailure::NotFound,
        }
    ));

    let missing_id = json!({
        "capabilities": fixture(RESUME_SUCCESS)["capabilities"],
        "resume_command_proven": true
    });
    assert!(matches!(
        decide_strict_resume(&missing_id).unwrap(),
        ResumeOutcome::FailedVisible {
            reason: StrictResumeFailure::MissingProviderSessionId,
        }
    ));

    let unproven = json!({
        "capabilities": fixture(MISSING_HOOKS)["capabilities"],
        "resume_command_proven": false,
        "provider_session_id": "sess_unproven"
    });
    assert!(matches!(
        decide_strict_resume(&unproven).unwrap(),
        ResumeOutcome::FailedVisible {
            reason: StrictResumeFailure::ResumeCommandUnproven,
        }
    ));

    let root = tempfile::tempdir().unwrap();
    let lab = ProviderConformanceLab::open(root.path()).unwrap();
    let succeeded = lab
        .execute_fixture(RESUME_SUCCESS, ConformanceArm::Baseline)
        .unwrap();
    assert_eq!(
        succeeded.metric(DeclaredMetricId::ExactResumeResult),
        Some(1)
    );
    assert_eq!(
        succeeded.metric(DeclaredMetricId::IdentityCorrelationResult),
        Some(1)
    );
    let failed = lab
        .execute_fixture(RESUME_FAILURE, ConformanceArm::Variant)
        .unwrap();
    assert_eq!(failed.metric(DeclaredMetricId::ExactResumeResult), Some(0));
    assert_eq!(
        failed.metric(DeclaredMetricId::IdentityCorrelationResult),
        Some(1)
    );
}

#[test]
fn executable_replacement_does_not_mutate_pinned_generation() {
    let original = executable("claude");
    let replacement = executable("replacement");
    let pinned =
        PinnedGenerationContract::pin(7, original.clone(), capabilities_from(BASELINE)).unwrap();
    let after_disk_change = pinned
        .retain_after_executable_replacement(replacement.clone(), capabilities_from(NEWER))
        .unwrap();

    assert_eq!(after_disk_change.generation(), 7);
    assert_eq!(after_disk_change.executable(), &original);
    assert_eq!(after_disk_change.capabilities().version.as_str(), "1.0.0");
    assert_eq!(
        after_disk_change.capabilities().semantic_events,
        CapabilitySupport::Supported
    );

    let next = after_disk_change
        .next_generation_after_probe(replacement.clone(), capabilities_from(NEWER))
        .unwrap();
    assert_eq!(next.generation(), 8);
    assert_eq!(next.executable(), &replacement);
    assert_eq!(next.capabilities().version.as_str(), "2.0.0");
}

#[test]
fn stable_operational_metrics_reject_model_quality_fields() {
    let root = tempfile::tempdir().unwrap();
    let lab = ProviderConformanceLab::open(root.path()).unwrap();
    let mut undeclared = BTreeMap::new();
    undeclared.insert("model_answer_quality".to_string(), 1_i64);
    let error = lab
        .record_metrics(
            ProviderConformanceCaseId::BaselineVersion,
            ConformanceArm::Baseline,
            &capabilities_from(BASELINE),
            undeclared,
        )
        .unwrap_err();
    assert!(error.to_string().contains("model_answer_quality"));

    let mut declared = BTreeMap::new();
    declared.insert(DeclaredMetricId::ProcessResidue.as_str().to_string(), 0);
    declared.insert(DeclaredMetricId::ForcedResync.as_str().to_string(), 0);
    let recorded = lab
        .record_metrics(
            ProviderConformanceCaseId::BaselineVersion,
            ConformanceArm::Baseline,
            &capabilities_from(BASELINE),
            declared,
        )
        .unwrap();
    assert_eq!(recorded.metric(DeclaredMetricId::ProcessResidue), Some(0));
}

#[test]
fn interrupted_case_resumes_from_durable_cursor_without_duplicate_settlement() {
    let root = tempfile::tempdir().unwrap();
    {
        let lab = ProviderConformanceLab::open(root.path()).unwrap();
        let mut run = lab
            .start_fixture(INTERRUPTED, ConformanceArm::Baseline)
            .unwrap();
        run.settle_next().unwrap();
        run.settle_next().unwrap();
        assert_eq!(run.settled_step_count(), 2);
        run.interrupt().unwrap();
    }

    let lab = ProviderConformanceLab::open(root.path()).unwrap();
    let mut resumed = lab.resume_interrupted().unwrap();
    assert_eq!(
        resumed.case_id(),
        ProviderConformanceCaseId::InterruptedCaseResume
    );
    assert_eq!(resumed.settled_step_count(), 2);
    assert_eq!(resumed.settled_step_ids(), &["launch", "first_output"]);
    while !resumed.is_complete() {
        resumed.settle_next().unwrap();
    }
    assert_eq!(
        resumed.settled_step_ids(),
        &[
            "launch",
            "first_output",
            "first_update",
            "outcome",
            "stop",
            "close"
        ]
    );
    assert_eq!(resumed.duplicate_settlements(), 0);
    assert_eq!(resumed.metric(DeclaredMetricId::ProcessResidue), Some(0));
}

#[test]
fn sanitizer_promotes_clean_seed_and_rejects_sensitive_bodies() {
    let promoted = promote_seeded_trace(SEEDED_CLEAN).unwrap();
    assert_eq!(
        promoted.case_id(),
        ProviderConformanceCaseId::TerminalOnlyFallback
    );
    assert_eq!(promoted.provider(), ProviderKind::Cursor);
    assert!(promoted.raw().get("prompt").is_none());

    let prompt = json!({
        "case_id": "terminal_only_fallback",
        "prompt": "write the next feature"
    });
    assert!(matches!(
        promote_seeded_trace(&prompt.to_string()).unwrap_err(),
        SanitizerRejection::PromptBody
    ));

    let response = json!({
        "case_id": "terminal_only_fallback",
        "response": "here is the implementation"
    });
    assert!(matches!(
        promote_seeded_trace(&response.to_string()).unwrap_err(),
        SanitizerRejection::ResponseBody
    ));

    let credential = json!({
        "case_id": "terminal_only_fallback",
        "api_key": "sk-test-not-a-real-secret"
    });
    assert!(matches!(
        promote_seeded_trace(&credential.to_string()).unwrap_err(),
        SanitizerRejection::Credential
    ));

    let absolute = json!({
        "case_id": "terminal_only_fallback",
        "cwd": "C:/Users/alice/src/app"
    });
    assert!(matches!(
        promote_seeded_trace(&absolute.to_string()).unwrap_err(),
        SanitizerRejection::AbsoluteUserPath
    ));

    let source = json!({
        "case_id": "terminal_only_fallback",
        "source_body": "fn main() { println!(\"secret sauce\"); }"
    });
    assert!(matches!(
        promote_seeded_trace(&source.to_string()).unwrap_err(),
        SanitizerRejection::ProprietarySourceBody
    ));
}

#[test]
fn fixture_authenticity_pins_schema_hash_and_declared_metrics() {
    let authenticated = authenticate_fixture(BASELINE).unwrap();
    assert_eq!(
        authenticated.schema_version(),
        PROVIDER_CONFORMANCE_SCHEMA_VERSION
    );
    assert_eq!(
        authenticated.case_id(),
        ProviderConformanceCaseId::BaselineVersion
    );
    assert_eq!(authenticated.fixture_sha256().len(), 64);
    assert_ne!(
        authenticated.fixture_sha256(),
        authenticate_fixture(&BASELINE.replace("1.0.0", "9.9.9"))
            .unwrap()
            .fixture_sha256()
    );
    assert!(authenticated
        .declared_metrics()
        .contains(&DeclaredMetricId::ExactResumeResult));
    assert_eq!(authenticated.provider(), ProviderKind::ClaudeCode);
    assert_eq!(authenticated.version().as_str(), "1.0.0");

    let minified = serde_json::to_string(&fixture(BASELINE)).unwrap();
    assert_ne!(minified, BASELINE);
    assert_eq!(
        authenticated.fixture_sha256(),
        authenticate_fixture(&minified).unwrap().fixture_sha256()
    );
    let spaced = BASELINE.replace(':', " : ").replace(',', ",  ");
    assert_eq!(
        authenticated.fixture_sha256(),
        authenticate_fixture(&spaced).unwrap().fixture_sha256()
    );

    let root = tempfile::tempdir().unwrap();
    let lab = ProviderConformanceLab::open(root.path()).unwrap();
    lab.start_fixture(INTERRUPTED, ConformanceArm::Baseline)
        .unwrap();
    let interrupted = authenticate_fixture(INTERRUPTED).unwrap();
    let cursor: Value =
        serde_json::from_slice(&std::fs::read(root.path().join("cursor.json")).unwrap()).unwrap();
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(
            root.path()
                .join("manifest_interrupted_case_resume_baseline"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        interrupted.fixture_sha256(),
        cursor["fixture_sha256"].as_str().unwrap()
    );
    assert_eq!(
        interrupted.fixture_sha256(),
        manifest["payload"]["fixture_sha256"].as_str().unwrap()
    );
    assert_eq!(manifest["payload"]["provider"], "claude_code");
    assert_eq!(manifest["payload"]["version"], "1.0.0");
    assert_eq!(
        manifest["payload"]["correlation"]["nonce"],
        "nonce-interrupted"
    );
    assert!(!manifest["sha256"].as_str().unwrap().is_empty());

    let original = std::fs::read(root.path().join("fixture.json")).unwrap();
    let tampered = String::from_utf8(original)
        .unwrap()
        .replace("sess_fixture_interrupted", "sess_fixture_tampered____");
    std::fs::write(root.path().join("fixture.json"), tampered).unwrap();
    let error = lab.resume_interrupted().unwrap_err();
    assert!(error.to_string().contains("digest"), "{error}");
}

#[test]
fn physical_decode_rejects_oversize_and_non_utf8_payloads() {
    let oversize = vec![b'x'; MAX_CONFORMANCE_DECODE_BYTES + 1];
    assert!(matches!(
        decode_fixture_bytes(&oversize),
        Err(devmanager::providers::conformance::ConformanceError::DecodeBoundExceeded { bytes })
            if bytes == MAX_CONFORMANCE_DECODE_BYTES + 1
    ));
    assert!(decode_fixture_bytes(&[0xff, 0xfe]).is_err());
    assert!(decode_fixture_bytes(BASELINE.as_bytes()).is_ok());
}

#[test]
fn lab_paths_are_confined_and_refuse_installed_profile_roots() {
    let root = tempfile::tempdir().unwrap();
    assert!(confined_artifact_path(root.path(), "cursor.json").is_ok());
    assert!(confined_artifact_path(root.path(), "../cursor.json").is_err());
    assert!(confined_artifact_path(root.path(), "..\\cursor.json").is_err());
    assert!(confined_artifact_path(root.path(), "nested/cursor.json").is_err());

    let forbidden = root.path().join("com.userfirst.devmanager");
    assert!(matches!(
        ProviderConformanceLab::open(&forbidden),
        Err(devmanager::providers::conformance::ConformanceError::ForbiddenProfileRoot)
    ));
    assert!(!forbidden.exists());
}

#[test]
fn fixture_replay_is_deterministic_across_identical_arms() {
    let root = tempfile::tempdir().unwrap();
    let lab = ProviderConformanceLab::open(root.path()).unwrap();
    let first = lab.execute_fixture(NEWER, ConformanceArm::Variant).unwrap();
    let second = lab.execute_fixture(NEWER, ConformanceArm::Variant).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        first.metric(DeclaredMetricId::UnknownEventFallback),
        Some(1)
    );
}

#[test]
fn identity_and_exact_resume_results_stay_independent() {
    let mut incompatible = fixture(RESUME_FAILURE);
    incompatible["nonce"] = json!("nonce-incompatible");
    incompatible["capabilities"]["evidence"] = json!([{
        "source": "capability_probe",
        "observed_at": 1,
        "status": "failed",
        "diagnostic": { "code": "version_malformed" }
    }]);
    let root = tempfile::tempdir().unwrap();
    let lab = ProviderConformanceLab::open(root.path()).unwrap();
    let record = lab
        .execute_fixture(&incompatible.to_string(), ConformanceArm::Baseline)
        .unwrap();
    assert_eq!(record.metric(DeclaredMetricId::ExactResumeResult), Some(0));
    assert_eq!(
        record.metric(DeclaredMetricId::IdentityCorrelationResult),
        Some(1)
    );
}

#[test]
fn dependency_holds_are_honest_and_do_not_claim_absent_subsystems() {
    let holds = dependency_holds();
    assert_eq!(holds, ConformanceHold::ALL.as_slice());
    assert!(holds
        .iter()
        .any(|hold| *hold == ConformanceHold::ProviderRuntimeSession));
    assert!(holds
        .iter()
        .any(|hold| *hold == ConformanceHold::ProviderJournal));
    assert!(holds
        .iter()
        .any(|hold| *hold == ConformanceHold::ProviderSessionsCompatibilityGate));
    assert!(holds
        .iter()
        .any(|hold| *hold == ConformanceHold::Phase2ConformanceArtifactRunner));
    assert!(!std::path::Path::new("src/providers/session.rs").exists());
    assert!(!std::path::Path::new("src/providers/journal.rs").exists());
    assert!(!std::path::Path::new("tests/provider_sessions.rs").exists());
    assert!(!std::path::Path::new("src/conformance").exists());
    assert!(!std::path::Path::new("src/providers/claude.rs").exists());
    assert!(!std::path::Path::new("src/providers/codex.rs").exists());
    assert!(!std::path::Path::new("src/providers/cursor.rs").exists());
    assert!(holds
        .iter()
        .any(|hold| *hold == ConformanceHold::ProviderClaudeAdapter));
    assert!(holds
        .iter()
        .any(|hold| *hold == ConformanceHold::ProviderCodexAdapter));
    assert!(holds
        .iter()
        .any(|hold| *hold == ConformanceHold::ProviderCursorAdapter));
}

#[test]
fn physical_decode_rejects_nesting_map_array_and_node_bombs() {
    let nested = format!(
        "{}{}",
        "[".repeat(MAX_CONFORMANCE_DEPTH + 1),
        "]".repeat(MAX_CONFORMANCE_DEPTH + 1)
    );
    assert!(matches!(
        decode_fixture_bytes(nested.as_bytes()),
        Err(ConformanceError::DecodeDepthExceeded { depth }) if depth > MAX_CONFORMANCE_DEPTH
    ));

    let mut huge_map = String::from("{");
    for index in 0..=MAX_CONFORMANCE_MAP_KEYS {
        if index > 0 {
            huge_map.push(',');
        }
        huge_map.push_str(&format!("\"k{index}\":{index}"));
    }
    huge_map.push('}');
    assert!(matches!(
        decode_fixture_bytes(huge_map.as_bytes()),
        Err(ConformanceError::DecodeMapKeyLimit { keys }) if keys > MAX_CONFORMANCE_MAP_KEYS
    ));

    let mut huge_array = String::from("[");
    for index in 0..=MAX_CONFORMANCE_ARRAY_ITEMS {
        if index > 0 {
            huge_array.push(',');
        }
        huge_array.push_str("0");
    }
    huge_array.push(']');
    assert!(matches!(
        decode_fixture_bytes(huge_array.as_bytes()),
        Err(ConformanceError::DecodeArrayItemLimit { items }) if items > MAX_CONFORMANCE_ARRAY_ITEMS
    ));

    let mut node_bomb = String::from("{");
    for index in 0..MAX_CONFORMANCE_MAP_KEYS {
        if index > 0 {
            node_bomb.push(',');
        }
        node_bomb.push_str(&format!(
            "\"g{index}\":[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15]"
        ));
    }
    node_bomb.push('}');
    assert!(matches!(
        decode_fixture_bytes(node_bomb.as_bytes()),
        Err(ConformanceError::DecodeNodeLimit { nodes }) if nodes > MAX_CONFORMANCE_NODES
    ));
}

#[test]
fn resume_rejects_huge_physical_fixture_without_unbounded_read() {
    let root = tempfile::tempdir().unwrap();
    let lab = ProviderConformanceLab::open(root.path()).unwrap();
    lab.start_fixture(INTERRUPTED, ConformanceArm::Baseline)
        .unwrap();
    let huge = vec![b'x'; MAX_CONFORMANCE_DECODE_BYTES + 1];
    std::fs::write(root.path().join("fixture.json"), &huge).unwrap();
    assert!(matches!(
        lab.resume_interrupted(),
        Err(ConformanceError::DecodeBoundExceeded { bytes })
            if bytes == MAX_CONFORMANCE_DECODE_BYTES + 1
    ));
}

#[test]
fn authenticate_rejects_provider_kind_and_version_inconsistency() {
    let kind_mismatch = BASELINE.replacen(
        "\"provider\": \"claude_code\"",
        "\"provider\": \"cursor\"",
        1,
    );
    assert!(matches!(
        authenticate_fixture(&kind_mismatch),
        Err(ConformanceError::InconsistentProviderIdentity)
    ));

    let version_mismatch = BASELINE.replacen("\"version\": \"1.0.0\"", "\"version\": \"9.9.9\"", 1);
    assert!(matches!(
        authenticate_fixture(&version_mismatch),
        Err(ConformanceError::InconsistentProviderIdentity)
    ));
}

#[test]
fn lab_paths_reject_unc_device_absolute_trailing_and_reparse_forms() {
    let root = tempfile::tempdir().unwrap();
    assert!(matches!(
        confined_artifact_path(root.path(), "cursor.json."),
        Err(ConformanceError::ForbiddenPathForm)
    ));
    assert!(matches!(
        confined_artifact_path(root.path(), "cursor.json "),
        Err(ConformanceError::ForbiddenPathForm)
    ));
    assert!(matches!(
        confined_artifact_path(root.path(), r"C:\Windows\cursor.json"),
        Err(ConformanceError::PathEscapesLab)
    ));
    assert!(matches!(
        confined_artifact_path(root.path(), r"\\server\share\cursor.json"),
        Err(ConformanceError::ForbiddenPathForm)
    ));
    assert!(matches!(
        confined_artifact_path(root.path(), r"\\.\NUL"),
        Err(ConformanceError::ForbiddenPathForm)
    ));
    assert!(matches!(
        confined_artifact_path(root.path(), "NUL"),
        Err(ConformanceError::ForbiddenPathForm)
    ));
    assert!(matches!(
        ProviderConformanceLab::open(std::path::Path::new(r"\\server\share\conformance-lab")),
        Err(ConformanceError::ForbiddenPathForm)
    ));
    assert!(matches!(
        ProviderConformanceLab::open(std::path::Path::new(r"\\.\C:\conformance-lab")),
        Err(ConformanceError::ForbiddenPathForm)
    ));
    let trailing = root.path().join("lab.");
    assert!(matches!(
        ProviderConformanceLab::open(&trailing),
        Err(ConformanceError::ForbiddenPathForm)
    ));
    assert!(!root.path().join("lab").exists());

    #[cfg(windows)]
    {
        let real = root.path().join("real_lab");
        let link = root.path().join("reparse_lab");
        std::fs::create_dir(&real).unwrap();
        let linked = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &link.to_string_lossy(),
                &real.to_string_lossy(),
            ])
            .status()
            .expect("mklink /J should run");
        assert!(linked.success(), "mklink /J failed");
        assert!(matches!(
            ProviderConformanceLab::open(&link),
            Err(ConformanceError::ForbiddenPathForm)
        ));

        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let child_link = root.path().join("cursor.json");
        let child_linked = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &child_link.to_string_lossy(),
                &elsewhere.to_string_lossy(),
            ])
            .status()
            .expect("child mklink /J should run");
        assert!(child_linked.success(), "child mklink /J failed");
        let lab = ProviderConformanceLab::open(root.path()).unwrap();
        assert!(lab
            .start_fixture(INTERRUPTED, ConformanceArm::Baseline)
            .is_err());
    }
}

#[test]
fn event_permutation_changes_order_metric_without_changing_count() {
    let mut permuted = fixture(BASELINE);
    permuted["events"].as_array_mut().unwrap().swap(0, 2);
    let root = tempfile::tempdir().unwrap();
    let lab = ProviderConformanceLab::open(root.path()).unwrap();
    let ordered = lab
        .execute_fixture(BASELINE, ConformanceArm::Baseline)
        .unwrap();
    let shuffled = lab
        .execute_fixture(&permuted.to_string(), ConformanceArm::Variant)
        .unwrap();
    assert_eq!(
        ordered.metric(DeclaredMetricId::NormalizedEventOrder),
        Some(1)
    );
    assert_eq!(
        shuffled.metric(DeclaredMetricId::NormalizedEventOrder),
        Some(0)
    );
    assert_eq!(
        ordered.metric(DeclaredMetricId::NormalizedEventCount),
        shuffled.metric(DeclaredMetricId::NormalizedEventCount)
    );
}

#[test]
fn auth_failure_keeps_identity_independent_of_exact_resume() {
    let mut auth_failure = fixture(RESUME_SUCCESS);
    auth_failure["case_id"] = json!("strict_resume_failure");
    auth_failure["nonce"] = json!("nonce-auth-failure");
    auth_failure["provider_session_id"] = json!("sess_auth_failure");
    auth_failure["capabilities"]["auth_state"] = json!("auth_required");
    auth_failure["capabilities"]["evidence"] = json!([
        {
            "source": "capability_probe",
            "observed_at": 1,
            "status": "supported"
        },
        {
            "source": "auth_status_probe",
            "observed_at": 1,
            "status": "auth_required"
        }
    ]);
    let root = tempfile::tempdir().unwrap();
    let lab = ProviderConformanceLab::open(root.path()).unwrap();
    let record = lab
        .execute_fixture(&auth_failure.to_string(), ConformanceArm::Variant)
        .unwrap();
    assert_eq!(record.metric(DeclaredMetricId::ExactResumeResult), Some(0));
    assert_eq!(
        record.metric(DeclaredMetricId::IdentityCorrelationResult),
        Some(1)
    );
}

#[test]
fn replay_index_keeps_case_and_arm_distinct() {
    let root = tempfile::tempdir().unwrap();
    let lab = ProviderConformanceLab::open(root.path()).unwrap();
    let baseline = lab
        .execute_fixture(BASELINE, ConformanceArm::Baseline)
        .unwrap();
    let variant = lab
        .execute_fixture(BASELINE, ConformanceArm::Variant)
        .unwrap();
    let index = ConformanceIndex::rebuild([baseline.clone(), variant.clone()]).unwrap();
    assert_eq!(index.record_count(), 2);
    let comparison = index
        .compare_arms(
            ProviderConformanceCaseId::BaselineVersion,
            ConformanceArm::Baseline,
            ProviderConformanceCaseId::BaselineVersion,
            ConformanceArm::Variant,
        )
        .unwrap();
    assert_eq!(
        comparison.delta(DeclaredMetricId::NormalizedEventCount),
        Some(0)
    );
    assert_eq!(
        baseline.metric(DeclaredMetricId::NormalizedEventOrder),
        variant.metric(DeclaredMetricId::NormalizedEventOrder)
    );
    assert_ne!(baseline, variant);
}

#[test]
fn decode_rejects_duplicate_object_keys() {
    assert!(matches!(
        decode_fixture_bytes(br#"{"a":1,"a":2}"#),
        Err(ConformanceError::DuplicateKey)
    ));
}

#[test]
fn fixtures_cannot_self_assert_resume_outcome() {
    let mut forged = fixture(RESUME_SUCCESS);
    forged["resume_outcome"] = json!("succeeded");
    assert!(matches!(
        decide_strict_resume(&forged),
        Err(ConformanceError::InvalidFixture(reason))
            if reason.contains("resume_outcome")
    ));
    assert!(authenticate_fixture(&forged.to_string()).is_err());
}

#[test]
fn unauthenticated_cursor_fixture_arm_is_rejected() {
    let mut cursor_arm = fixture(MISSING_HOOKS);
    cursor_arm["provider"] = json!("cursor");
    cursor_arm["capabilities"]["kind"] = json!("cursor");
    let root = tempfile::tempdir().unwrap();
    let lab = ProviderConformanceLab::open(root.path()).unwrap();
    assert!(matches!(
        lab.execute_fixture(&cursor_arm.to_string(), ConformanceArm::Variant),
        Err(ConformanceError::UnauthenticatedCursorArm)
    ));
}

fn smoke_profile(root: &std::path::Path) -> PathBuf {
    root.join("isolated-profile")
}

#[test]
fn fixture_smoke_contract_holds_and_never_passes() {
    let root = tempfile::tempdir().unwrap();
    let worktree = root.path().join("worktree");
    std::fs::create_dir(&worktree).unwrap();
    let request = ProviderSmokeRequest::fixture(
        smoke_profile(root.path()),
        MAX_PROVIDER_SMOKE_DEADLINE_MS,
        provider_smoke_environment(),
    )
    .unwrap();
    assert_eq!(request.arm(), ProviderSmokeArm::Fixture);
    let disposition = evaluate_provider_smoke(&request, &worktree).unwrap();
    assert!(!disposition.is_pass());
    let ProviderSmokeDisposition::Hold(report) = disposition else {
        panic!("fixture smoke must HOLD, not reject");
    };
    assert!(!report.launched_providers());
    assert_eq!(
        report.required_evidence(),
        ProviderSmokeEvidence::required()
    );
    assert_eq!(report.invariants(), ProviderSmokeInvariants::required());
    assert!(report
        .holds()
        .contains(&ProviderSmokeHold::FixtureRuntimeUnimplemented));
    assert!(report.holds().contains(&ProviderSmokeHold::Dependency(
        ConformanceHold::ProviderRuntimeSession
    )));
    assert!(report.holds().contains(&ProviderSmokeHold::Dependency(
        ConformanceHold::ProviderJournal
    )));
    assert!(report.holds().contains(&ProviderSmokeHold::Dependency(
        ConformanceHold::ProviderSessionsCompatibilityGate
    )));
    assert!(report.holds().contains(&ProviderSmokeHold::Dependency(
        ConformanceHold::ProviderClaudeAdapter
    )));
}

#[test]
fn authenticated_smoke_requires_opt_in_allowlist_and_isolated_profile() {
    let root = tempfile::tempdir().unwrap();
    let env = provider_smoke_environment();
    assert!(matches!(
        ProviderSmokeRequest::authenticated(
            smoke_profile(root.path()),
            vec![ProviderKind::ClaudeCode],
            false,
            true,
            false,
            false,
            false,
            1_000,
            env.clone(),
        ),
        Err(ProviderSmokeRejection::AuthenticatedWithoutOptIn)
    ));
    assert!(matches!(
        ProviderSmokeRequest::authenticated(
            smoke_profile(root.path()),
            Vec::new(),
            true,
            true,
            false,
            false,
            false,
            1_000,
            env.clone(),
        ),
        Err(ProviderSmokeRejection::AuthenticatedWithoutAllowlist)
    ));
    assert!(matches!(
        ProviderSmokeRequest::authenticated(
            smoke_profile(root.path()),
            vec![ProviderKind::ClaudeCode, ProviderKind::ClaudeCode],
            true,
            true,
            false,
            false,
            false,
            1_000,
            env.clone(),
        ),
        Err(ProviderSmokeRejection::AuthenticatedDuplicateAllowlist)
    ));
    assert!(matches!(
        ProviderSmokeRequest::authenticated(
            smoke_profile(root.path()),
            vec![ProviderKind::Codex],
            true,
            false,
            false,
            false,
            false,
            1_000,
            env.clone(),
        ),
        Err(ProviderSmokeRejection::AuthenticatedInCiOrNoninteractive)
    ));
    assert!(matches!(
        ProviderSmokeRequest::authenticated(
            smoke_profile(root.path()),
            vec![ProviderKind::Cursor],
            true,
            true,
            true,
            false,
            false,
            1_000,
            env.clone(),
        ),
        Err(ProviderSmokeRejection::AuthenticatedInCiOrNoninteractive)
    ));
    assert!(matches!(
        ProviderSmokeRequest::authenticated(
            smoke_profile(root.path()),
            vec![ProviderKind::ClaudeCode],
            true,
            true,
            false,
            false,
            true,
            1_000,
            env.clone(),
        ),
        Err(ProviderSmokeRejection::AuthenticatedWithoutHostRegistration)
    ));
    assert!(matches!(
        ProviderSmokeRequest::authenticated(
            smoke_profile(root.path()),
            vec![ProviderKind::ClaudeCode],
            true,
            true,
            false,
            true,
            false,
            1_000,
            env.clone(),
        ),
        Err(ProviderSmokeRejection::AuthenticatedCapabilityUnsupported)
    ));
    assert!(matches!(
        ProviderSmokeRequest::authenticated(
            root.path().join("com.userfirst.devmanager"),
            vec![ProviderKind::ClaudeCode],
            true,
            true,
            false,
            true,
            true,
            1_000,
            env.clone(),
        ),
        Err(ProviderSmokeRejection::ProductionProfile)
    ));
    assert!(matches!(
        ProviderSmokeRequest::authenticated(
            root.path().join(".codex"),
            vec![ProviderKind::Codex],
            true,
            true,
            false,
            true,
            true,
            1_000,
            env.clone(),
        ),
        Err(ProviderSmokeRejection::ProductionBrowserProfile)
    ));

    let claimed = ProviderSmokeRequest::authenticated(
        smoke_profile(root.path()),
        vec![ProviderKind::ClaudeCode],
        true,
        true,
        false,
        true,
        true,
        1_000,
        env,
    )
    .unwrap();
    let worktree = root.path().join("worktree");
    std::fs::create_dir(&worktree).unwrap();
    assert!(matches!(
        evaluate_provider_smoke(&claimed, &worktree).unwrap(),
        ProviderSmokeDisposition::Rejected(
            ProviderSmokeRejection::AuthenticatedWithoutHostRegistration
        )
    ));
}

#[test]
fn smoke_contract_rejects_unbounded_deadline_inherited_env_and_bodies() {
    let root = tempfile::tempdir().unwrap();
    let mut inherited = provider_smoke_environment();
    inherited.insert("ANTHROPIC_API_KEY".to_string(), "sk-test".to_string());
    assert!(matches!(
        ProviderSmokeRequest::fixture(smoke_profile(root.path()), 0, provider_smoke_environment()),
        Err(ProviderSmokeRejection::DeadlineOutOfBounds)
    ));
    assert!(matches!(
        ProviderSmokeRequest::fixture(
            smoke_profile(root.path()),
            MAX_PROVIDER_SMOKE_DEADLINE_MS + 1,
            provider_smoke_environment(),
        ),
        Err(ProviderSmokeRejection::DeadlineOutOfBounds)
    ));
    assert!(matches!(
        ProviderSmokeRequest::fixture(smoke_profile(root.path()), 1_000, inherited),
        Err(ProviderSmokeRejection::InheritedOrSecretEnvironment)
    ));
    assert!(matches!(
        reject_smoke_sensitive_payload(&json!({"prompt": "write the next feature"})),
        Err(ProviderSmokeRejection::PromptResponseOrCredential)
    ));
    assert!(matches!(
        reject_smoke_sensitive_payload(&json!({"api_key": "sk-test"})),
        Err(ProviderSmokeRejection::PromptResponseOrCredential)
    ));
    let holds = discover_provider_smoke_holds(root.path()).unwrap();
    assert!(holds.contains(&ProviderSmokeHold::FixtureRuntimeUnimplemented));
    assert!(!holds.is_empty());
}
