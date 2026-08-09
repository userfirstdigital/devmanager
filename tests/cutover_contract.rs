#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};
use tempfile::TempDir;

const AUDIT_SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/scripts/native-next/Invoke-CutoverAudit.ps1"
);
const FIXTURE_ROOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/cutover-contract"
);

struct FixtureRepo {
    _temp: TempDir,
    root: PathBuf,
}

struct AuditRun {
    fixture: FixtureRepo,
    output: Output,
    report: Value,
    human: String,
}

fn base_node(id: &str, kind: &str, status: &str) -> Value {
    json!({
        "id": id,
        "kind": kind,
        "status": status,
        "dependsOn": [],
        "evidence": [format!("evidence/{id}.json")]
    })
}

fn base_row(
    id: &str,
    legacy_path: &str,
    symbols: &[&str],
    replacement_path: &str,
    prerequisites: &[&str],
    status: &str,
) -> Value {
    json!({
        "id": id,
        "area": "fixture-contract",
        "legacy": {
            "path": legacy_path,
            "symbols": symbols,
            "tokens": []
        },
        "replacementOwner": {
            "path": replacement_path,
            "symbol": "ReplacementFixture"
        },
        "prerequisites": prerequisites,
        "evidence": {
            "commands": [format!("pwsh -NoProfile -File evidence/{id}.ps1")],
            "artifacts": [format!("evidence/{id}.json")]
        },
        "status": status,
        "approvalRequired": true,
        "approvalRequirement": "Explicit Phase 11 cutover approval"
    })
}

fn contract(rows: Vec<Value>, nodes: Vec<Value>) -> Value {
    json!({
        "schemaVersion": 1,
        "contractId": "phase-11.1-cutover",
        "ledgerPath": "docs/replacement-deletion-ledger.md",
        "statusModel": ["HOLD", "READY", "DELETED"],
        "referencePolicy": {
            "trackedUniverse": "git-ls-files",
            "referenceScanner": "rg --fixed-strings --line-number",
            "allowedLedgerSelfReferences": ["docs/replacement-deletion-ledger.md"],
            "protectedFileBasenames": ["session.json"],
            "maxMatchesPerRow": 20
        },
        "prerequisiteNodes": nodes,
        "forbiddenEntrypoints": [
            {
                "id": "legacy-devmanager-next",
                "path": "src/bin/devmanager-next.rs",
                "tokens": ["devmanager-next", "devmanager-next.exe"]
            }
        ],
        "rows": rows
    })
}

fn write_ledger(root: &Path, document: &Value) {
    let body = serde_json::to_string_pretty(document).expect("serialize fixture ledger");
    let ledger = format!(
        "# Replacement Deletion Ledger\n\nThe JSON contract below is canonical for the Phase 11.1 audit.\n\n```json cutover-contract\n{body}\n```\n"
    );
    let path = root.join("docs/replacement-deletion-ledger.md");
    fs::create_dir_all(path.parent().expect("ledger parent")).expect("ledger directory");
    fs::write(path, ledger).expect("fixture ledger");
}

fn fixture_repo(document: Value, extra_files: &[(&str, &[u8])]) -> FixtureRepo {
    let temp = tempfile::tempdir().expect("fixture tempdir");
    let root = temp.path().to_path_buf();
    fs::create_dir_all(root.join("docs")).expect("fixture docs");
    fs::create_dir_all(root.join("src")).expect("fixture src");
    write_ledger(&root, &document);

    for name in ["legacy.rs", "replacement.rs", "reference.rs", "README.md"] {
        fs::copy(
            Path::new(FIXTURE_ROOT).join(name),
            root.join(if name == "README.md" {
                "README.md".into()
            } else {
                format!("src/{name}")
            }),
        )
        .expect("copy fixture file");
    }
    for (name, contents) in extra_files {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("extra fixture parent");
        }
        fs::write(path, contents).expect("extra fixture file");
    }

    git(&root, &["init", "--quiet"]);
    git(&root, &["add", "--all"]);
    FixtureRepo { _temp: temp, root }
}

fn git(root: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .args(["-C", root.to_str().expect("fixture path utf8")])
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout={}\nstderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn run_audit(document: Value, extra_files: &[(&str, &[u8])]) -> AuditRun {
    let fixture = fixture_repo(document, extra_files);
    let output_path = fixture
        .root
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let output = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-File",
            AUDIT_SCRIPT,
            "-Mode",
            "Parity",
            "-Root",
            fixture.root.to_str().expect("fixture root utf8"),
            "-OutputPath",
            output_path.to_str().expect("output path utf8"),
        ])
        .env("APPDATA", fixture.root.join("protected-appdata"))
        .output()
        .expect("spawn cutover audit");
    assert!(
        output_path.is_file(),
        "audit must publish JSON even when it fails\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&fs::read(&output_path).expect("audit JSON"))
        .expect("valid audit JSON");
    let human_path = output_path.with_extension("txt");
    let human = fs::read_to_string(human_path).expect("human audit report");
    AuditRun {
        fixture,
        output,
        report,
        human,
    }
}

fn row<'a>(report: &'a Value, id: &str) -> &'a Value {
    report["rows"]
        .as_array()
        .expect("report rows")
        .iter()
        .find(|candidate| candidate["id"] == id)
        .unwrap_or_else(|| panic!("missing report row {id}: {report}"))
}

fn strings_at<'a>(value: &'a Value, path: &[&str]) -> Vec<&'a str> {
    let mut current = value;
    for segment in path {
        current = &current[*segment];
    }
    current
        .as_array()
        .expect("string array")
        .iter()
        .map(|value| value.as_str().expect("string entry"))
        .collect()
}

#[test]
fn fixture_audit_detects_legacy_path_symbol_and_external_references() {
    let document = contract(
        vec![base_row(
            "legacy-fixture",
            "src/legacy.rs",
            &["LegacyFixture"],
            "src/replacement.rs",
            &["gate-parity"],
            "HOLD",
        )],
        vec![base_node("gate-parity", "gate", "HOLD")],
    );
    let run = run_audit(document, &[]);
    let legacy = row(&run.report, "legacy-fixture");

    assert!(
        !run.output.status.success(),
        "HOLD evidence must not be green"
    );
    assert_eq!(legacy["legacy"]["pathPresent"], true);
    assert!(strings_at(legacy, &["references", "path"]).contains(&"src/reference.rs"));
    assert!(strings_at(legacy, &["references", "symbol"]).contains(&"src/reference.rs"));
    assert!(!strings_at(legacy, &["references", "path"])
        .contains(&"docs/replacement-deletion-ledger.md"));
    assert!(run.human.contains("legacy-fixture"));
    assert!(run.human.contains("HOLD"));
}

#[test]
fn ledger_parser_rejects_missing_and_duplicate_legacy_paths() {
    let missing = run_audit(
        contract(
            vec![base_row(
                "missing-path",
                "src/missing.rs",
                &["Missing"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    assert!(!missing.output.status.success());
    assert!(strings_at(&missing.report, &["contractErrors"])
        .iter()
        .any(|error| error.contains("legacy path")));

    let duplicate = run_audit(
        contract(
            vec![
                base_row(
                    "first",
                    "src/legacy.rs",
                    &["LegacyFixture"],
                    "src/replacement.rs",
                    &["gate-parity"],
                    "HOLD",
                ),
                base_row(
                    "second",
                    "src/legacy.rs",
                    &["Another"],
                    "src/replacement.rs",
                    &["gate-parity"],
                    "HOLD",
                ),
            ],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    assert!(!duplicate.output.status.success());
    assert!(strings_at(&duplicate.report, &["contractErrors"])
        .iter()
        .any(|error| error.contains("duplicate legacy path")));
}

#[test]
fn prerequisite_graph_rejects_unknown_and_circular_nodes() {
    let unknown = run_audit(
        contract(
            vec![base_row(
                "unknown-prerequisite",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-does-not-exist"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    assert!(!unknown.output.status.success());
    assert!(strings_at(&unknown.report, &["contractErrors"])
        .iter()
        .any(|error| error.contains("unknown prerequisite")));

    let mut first = base_node("gate-a", "gate", "READY");
    first["dependsOn"] = json!(["gate-b"]);
    let mut second = base_node("gate-b", "gate", "READY");
    second["dependsOn"] = json!(["gate-a"]);
    let circular = run_audit(
        contract(
            vec![base_row(
                "circular",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-a"],
                "HOLD",
            )],
            vec![first, second],
        ),
        &[],
    );
    assert!(!circular.output.status.success());
    assert!(strings_at(&circular.report, &["contractErrors"])
        .iter()
        .any(|error| error.contains("circular prerequisite")));
}

#[test]
fn ready_row_requires_all_prerequisites_and_evidence() {
    let run = run_audit(
        contract(
            vec![base_row(
                "not-ready",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "READY",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[],
    );
    let report_row = row(&run.report, "not-ready");
    assert!(!run.output.status.success());
    assert_eq!(report_row["status"], "READY");
    assert!(strings_at(report_row, &["blockers"])
        .iter()
        .any(|blocker| blocker.contains("prerequisite")));
    assert!(strings_at(report_row, &["blockers"])
        .iter()
        .any(|blocker| blocker.contains("evidence")));
}

#[test]
fn ready_prerequisite_requires_its_evidence_artifact() {
    let run = run_audit(
        contract(
            vec![base_row(
                "ready-row",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-ready"],
                "READY",
            )],
            vec![base_node("gate-ready", "gate", "READY")],
        ),
        &[("evidence/ready-row.json", br#"{"ok":true}"#)],
    );
    assert!(!run.output.status.success());
    assert!(strings_at(&run.report, &["blockers"])
        .iter()
        .any(|blocker| blocker.contains("gate-ready") && blocker.contains("evidence artifact")));
    assert_eq!(
        row(&run.report, "ready-row")["status"],
        "READY",
        "row status remains the authored state while the audit blocks the cutover"
    );
}

#[test]
fn deleted_row_fails_when_legacy_path_is_still_present() {
    let run = run_audit(
        contract(
            vec![base_row(
                "still-present",
                "src/legacy.rs",
                &["LegacyFixture"],
                "src/replacement.rs",
                &["gate-parity"],
                "DELETED",
            )],
            vec![base_node("gate-parity", "gate", "READY")],
        ),
        &[
            ("evidence/gate-parity.json", br#"{"ok":true}"#),
            ("evidence/still-present.json", br#"{"ok":true}"#),
        ],
    );
    let report_row = row(&run.report, "still-present");
    assert!(!run.output.status.success());
    assert!(strings_at(report_row, &["blockers"])
        .iter()
        .any(|blocker| blocker.contains("still present")));
}

#[test]
fn stale_devmanager_next_entrypoint_is_reported_from_tracked_fixture() {
    let run = run_audit(
        contract(
            vec![base_row(
                "legacy-entrypoint",
                "src/bin/devmanager-next.rs",
                &["main"],
                "src/main.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[
            ("src/bin/devmanager-next.rs", b"fn main() {}\n"),
            ("Cargo.toml", b"name = \"devmanager-next\"\n"),
        ],
    );
    let report_row = row(&run.report, "legacy-entrypoint");
    assert_eq!(report_row["legacy"]["pathPresent"], true);
    assert!(strings_at(&run.report, &["entrypointFindings"])
        .iter()
        .any(|finding| finding.contains("src/bin/devmanager-next.rs")));
    assert!(strings_at(report_row, &["references", "token"])
        .iter()
        .any(|path| *path == "Cargo.toml"));
}

#[test]
fn exact_session_json_is_path_only_and_external_appdata_is_untouched() {
    let session_bytes = br#"{"secret":"must-not-be-read"}"#;
    let run = run_audit(
        contract(
            vec![base_row(
                "session-path",
                "session.json",
                &["session.json"],
                "src/replacement.rs",
                &["gate-parity"],
                "HOLD",
            )],
            vec![base_node("gate-parity", "gate", "HOLD")],
        ),
        &[
            ("session.json", session_bytes),
            (
                "docs/session-reference.md",
                b"session.json is ignored by the product\n",
            ),
        ],
    );
    let protected = run.fixture.root.join("protected-appdata/session.json");
    fs::create_dir_all(protected.parent().expect("protected parent")).expect("protected dir");
    fs::write(&protected, session_bytes).expect("protected session");

    assert_eq!(
        fs::read(&protected).expect("protected bytes"),
        session_bytes
    );
    assert_eq!(
        fs::read(run.fixture.root.join("session.json")).expect("tracked bytes"),
        session_bytes
    );
    assert!(strings_at(&run.report, &["protectedFilesSkipped"])
        .iter()
        .any(|path| *path == "session.json"));
    assert!(row(&run.report, "session-path")["references"]["path"]
        .as_array()
        .expect("path references")
        .iter()
        .any(|path| path == "docs/session-reference.md"));
    for kind in ["path", "symbol", "token"] {
        assert!(!row(&run.report, "session-path")["references"][kind]
            .as_array()
            .expect("protected reference list")
            .iter()
            .any(|path| path == "session.json"));
    }
    assert!(!run.human.contains("must-not-be-read"));
    assert!(!run.report.to_string().contains("must-not-be-read"));
}

#[test]
fn audit_is_read_only_for_tracked_fixture_files() {
    let document = contract(
        vec![base_row(
            "read-only",
            "src/legacy.rs",
            &["LegacyFixture"],
            "src/replacement.rs",
            &["gate-parity"],
            "HOLD",
        )],
        vec![base_node("gate-parity", "gate", "HOLD")],
    );
    let run = run_audit(document, &[("do-not-delete.txt", b"sentinel\n")]);
    let before = git(&run.fixture.root, &["ls-files"]);
    let before_files = String::from_utf8(before.stdout).expect("tracked paths utf8");
    assert_eq!(
        fs::read(run.fixture.root.join("do-not-delete.txt")).unwrap(),
        b"sentinel\n"
    );
    assert_eq!(
        fs::read(run.fixture.root.join("src/legacy.rs")).unwrap(),
        b"pub struct LegacyFixture;\n"
    );
    let after = git(&run.fixture.root, &["ls-files"]);
    assert_eq!(
        String::from_utf8(after.stdout).expect("tracked paths utf8"),
        before_files
    );
    assert!(run.fixture.root.join("do-not-delete.txt").is_file());
}

#[test]
fn current_repository_produces_deterministic_hold_report() {
    let output_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".devmanager-next/evidence/current/cutover-audit.json");
    let first = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-File",
            AUDIT_SCRIPT,
            "-Mode",
            "Parity",
            "-Root",
            env!("CARGO_MANIFEST_DIR"),
            "-OutputPath",
            output_path.to_str().expect("output path utf8"),
        ])
        .output()
        .expect("run current audit");
    let first_bytes = fs::read(&output_path).expect("current report");
    let first_report: Value = serde_json::from_slice(&first_bytes).expect("current JSON");
    let second = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-File",
            AUDIT_SCRIPT,
            "-Mode",
            "Parity",
            "-Root",
            env!("CARGO_MANIFEST_DIR"),
            "-OutputPath",
            output_path.to_str().expect("output path utf8"),
        ])
        .output()
        .expect("rerun current audit");
    let second_bytes = fs::read(&output_path).expect("current report rerun");
    let second_report: Value = serde_json::from_slice(&second_bytes).expect("current JSON rerun");

    assert!(!first.status.success());
    assert!(!second.status.success());
    assert_eq!(first_report, second_report);
    assert_eq!(first_report["contractStatus"], "HOLD");
    assert!(first_report["rows"]
        .as_array()
        .expect("current rows")
        .iter()
        .all(|row| row["status"] == "HOLD"));
    for row in first_report["rows"].as_array().expect("current rows") {
        for kind in ["path", "symbol", "token"] {
            let count = row["references"][kind]
                .as_array()
                .expect("bounded reference list")
                .len();
            assert!(
                count <= 20,
                "reference list for {kind} exceeded bound: {count}"
            );
        }
    }
    assert!(first_report["entrypointFindings"]
        .as_array()
        .is_some_and(|items| items.len() <= 60));
    assert!(first_report["blockers"]
        .as_array()
        .is_some_and(|items| !items.is_empty()));
}
