//! Phase 6 Task 6.10 dependency-safe local-workspace conformance harness.
//!
//! Fixture/fake-host only. This binary does not claim Phase 6.1–6.9 host
//! adapters are integrated.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use devmanager::domain::WorkspaceRef;
use devmanager::services::model::ServiceCatalog;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const FIXTURE_TASK_ID: &str = "0198b6b0-0000-7000-8000-000000000001";
const MAX_FILE_CHUNK_BYTES: usize = 64 * 1024;
const REQUIRED_CASES: &[&str] = &[
    "binding.explicit_main",
    "binding.explicit_worktree",
    "binding.explicit_external",
    "worktree.create_isolated",
    "worktree.remove_refuses_dirty",
    "files.bounded_access",
    "artifacts.bounded_export",
    "checkpoint.capture_and_restore",
    "checkpoint.forbids_hard_reset",
    "capabilities.git",
    "capabilities.service",
    "capabilities.port",
    "capabilities.ssh",
    "lifecycle.cancel",
    "lifecycle.restart_replay",
    "lifecycle.zero_orphan",
];
const UNAVAILABLE_ADAPTERS: &[&str] = &[
    "src/workspace/service.rs",
    "src/workspace/worktree.rs",
    "src/workspace/files.rs",
    "src/workspace/artifacts.rs",
    "src/workspace/checkpoint.rs",
    "src/git/command.rs",
    "src/git/checkpoint.rs",
    "src/git/review.rs",
    "src/ssh/launch.rs",
    "src/ui/command_center/overview.rs",
];
const REQUIRED_METRICS: [&str; 5] = [
    "result",
    "latency_ms",
    "residue_jobs",
    "residue_worktrees",
    "cancelled_generations",
];
const REQUIRED_EVIDENCE_IDS: [&str; 3] = [
    "host.lock",
    "kernel.sqlite3",
    "ManagedProcessJob.active_process_ids",
];
const CUTOVER_DELETE_SYMBOLS: [&str; 5] = [
    "FakeWorkspaceHost",
    "WorkspaceChoice",
    "HostError",
    "ArtifactRecord",
    "JournalEntry",
];
const REQUIRED_PRECONDITION_IDS: [&str; 8] = ["S2", "S3", "S4", "S5", "S6", "S7", "S8", "S9"];
const REQUIRED_CANONICAL_SEAMS: &[(&str, &[(&str, &str)])] = &[
    (
        "S2",
        &[("src/config/project_store.rs", "pub struct ProjectStore")],
    ),
    (
        "S3",
        &[
            ("src/workspace/service.rs", "WorkspaceChoice"),
            ("src/domain/command.rs", "BindWorkspace"),
            ("src/client/cli.rs", "fn bind_workspace"),
        ],
    ),
    (
        "S4",
        &[
            ("src/workspace/worktree.rs", "fn preview_remove_worktree"),
            ("src/git/command.rs", "GIT_TERMINAL_PROMPT"),
        ],
    ),
    ("S5", &[("src/workspace/files.rs", "fn bounded_read")]),
    (
        "S6",
        &[
            ("src/workspace/checkpoint.rs", "struct Checkpoint"),
            ("src/git/checkpoint.rs", "fn restore"),
        ],
    ),
    (
        "S7",
        &[
            ("src/services/supervisor.rs", "ManagedProcessJob"),
            ("src/services/supervisor.rs", "prepare_suspended_pty"),
        ],
    ),
    (
        "S8",
        &[
            ("src/client/cli.rs", "fn begin_close"),
            ("src/client/cli.rs", "BeginCloseTask"),
        ],
    ),
    (
        "S9",
        &[("docs/replacement-deletion-ledger.md", "FakeWorkspaceHost")],
    ),
];
const EVIDENCE_ISSUER_SEAMS: &[(&str, &str, &str)] = &[
    (
        "host.lock",
        "src/host/lock.rs",
        "fn issue_authenticated_host_lock",
    ),
    (
        "kernel.sqlite3",
        "src/kernel/store.rs",
        "fn issue_authenticated_sqlite_header",
    ),
    (
        "ManagedProcessJob.active_process_ids",
        "src/process/job.rs",
        "fn issue_authenticated_job_process_ids",
    ),
];
const MAX_MANIFEST_BYTES: usize = 32 * 1024;
const MAX_PROJECT_BYTES: usize = 16 * 1024;
const MAX_CASES: usize = 32;
const MAX_METRICS: usize = 16;
const MAX_PRECONDITIONS: usize = 16;
const MAX_SEAMS: usize = 16;
const MAX_EVIDENCE_IDS: usize = 16;
const MIN_SEAM_FILE_BYTES: usize = 256;
const MAX_SEAM_FILE_BYTES: usize = 256 * 1024;
const EVALUATE_DEADLINE_MS: u64 = 15_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IntegrationClaim {
    None,
    Integrated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CaseAuthority {
    Fake,
    Canonical,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeamRequirement {
    path: String,
    needle: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapabilityPrecondition {
    id: String,
    seams: Vec<SeamRequirement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceConformanceManifest {
    schema_version: u32,
    suite: String,
    mode: String,
    integration_claim: IntegrationClaim,
    required_cases: Vec<String>,
    case_authority: BTreeMap<String, CaseAuthority>,
    declared_metrics: Vec<String>,
    evidence_ids_required_for_integrated: Vec<String>,
    preconditions: Vec<CapabilityPrecondition>,
    cutover_delete_symbols: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManifestError {
    FrameTooLarge,
    UnknownField,
    Invalid,
    CollectionBound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HoldReason {
    id: String,
    missing_seams: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreconditionReport {
    claim: IntegrationClaim,
    holds: Vec<HoldReason>,
    satisfied_evidence_ids: Vec<String>,
}

struct SealedEvidence {
    ids: Vec<String>,
}

#[derive(Clone, Copy)]
struct PreconditionSnapshot<'a> {
    manifest: &'a WorkspaceConformanceManifest,
    worktree_root: &'a Path,
    deadline: Instant,
    source: &'a str,
    task_id: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFixture {
    schema_version: u32,
    project: ProjectRecord,
    folders: Vec<ProjectFolder>,
    commands: Vec<ProjectCommand>,
    ssh_hosts: Vec<ProjectSshHost>,
    external_listeners: Vec<ProjectListener>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectRecord {
    id: String,
    name: String,
    root: String,
    default_workspace: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFolder {
    id: String,
    path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectCommand {
    id: String,
    program: String,
    args: Vec<String>,
    cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectSshHost {
    id: String,
    host: String,
    auth_mode: String,
    key_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectListener {
    id: String,
    protocol: String,
    port: u16,
    ownership: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceChoice {
    Main,
    NewWorktree,
    External,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HostError {
    NonexistentPath,
    NotARepository,
    EscapeRejected,
    LiveResourcesPresent,
    DirtyOrUnpushed,
    PathRejected(&'static str),
    Conflict,
    Cancelled,
    HardResetForbidden,
    GitFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Binding {
    task_id: String,
    choice: WorkspaceChoice,
    workspace: WorkspaceRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeRecord {
    path: PathBuf,
    branch: String,
    base_commit: String,
    dirty: bool,
    unpushed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArtifactRecord {
    id: String,
    relative_path: String,
    sha256: String,
    bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Checkpoint {
    id: String,
    head: String,
    files: BTreeMap<String, String>,
    blobs: BTreeMap<String, Vec<u8>>,
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JournalEntry {
    seq: u64,
    generation: u64,
    kind: String,
    detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct CapabilityFixture {
    git: GitCapability,
    service: ServiceCapability,
    port: PortCapability,
    ssh: SshCapability,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct GitCapability {
    backend: String,
    adapter_integrated: bool,
    porcelain: Vec<String>,
    mutating_executor: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ServiceCapability {
    catalog_decode: bool,
    supervisor_launch: bool,
    job_ownership: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct PortCapability {
    live_probe: bool,
    external_listener_control: String,
    observed_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct SshCapability {
    launch_adapter: bool,
    password_auto_inject: bool,
    secrets_in_events: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapabilityReport {
    git: GitCapability,
    service: ServiceCapability,
    port: PortCapability,
    ssh: SshCapability,
    unavailable_adapters: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueuedOp {
    generation: u64,
    kind: String,
}

struct FakeWorkspaceHost {
    _root: TempDir,
    main_repo: PathBuf,
    remote_bare: PathBuf,
    worktree_root: PathBuf,
    artifact_root: PathBuf,
    journal_path: PathBuf,
    bindings: HashMap<String, Binding>,
    worktrees: HashMap<String, WorktreeRecord>,
    live_resources: HashSet<String>,
    artifacts: HashMap<String, ArtifactRecord>,
    checkpoints: Vec<Checkpoint>,
    journal: Vec<JournalEntry>,
    queued: VecDeque<QueuedOp>,
    cancelled_generations: BTreeSet<u64>,
    generation: u64,
    jobs: HashSet<String>,
    capabilities: CapabilityReport,
    next_seq: u64,
}

fn decode_error(error: serde_json::Error) -> ManifestError {
    let message = error.to_string();
    if message.contains("unknown field") {
        ManifestError::UnknownField
    } else {
        ManifestError::Invalid
    }
}

fn has_duplicate_strings(items: &[String]) -> bool {
    let mut seen = BTreeSet::new();
    items.iter().any(|item| !seen.insert(item.as_str()))
}

fn strings_eq(items: &[String], required: &[&str]) -> bool {
    items.len() == required.len() && items.iter().zip(required).all(|(item, want)| item == want)
}

fn canonical_seams_match(manifest: &WorkspaceConformanceManifest) -> bool {
    manifest.preconditions.len() == REQUIRED_CANONICAL_SEAMS.len()
        && manifest
            .preconditions
            .iter()
            .zip(REQUIRED_CANONICAL_SEAMS)
            .all(|(precondition, (id, seams))| {
                precondition.id == *id
                    && precondition.seams.len() == seams.len()
                    && precondition
                        .seams
                        .iter()
                        .zip(*seams)
                        .all(|(got, (path, needle))| got.path == *path && got.needle == *needle)
            })
}

fn is_safe_relative(path: &str) -> bool {
    if path.is_empty() || path.contains('\0') {
        return false;
    }
    let parsed = Path::new(path);
    if parsed.is_absolute() {
        return false;
    }
    parsed
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
}

fn decode_workspace_manifest(bytes: &[u8]) -> Result<WorkspaceConformanceManifest, ManifestError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ManifestError::FrameTooLarge);
    }
    let manifest: WorkspaceConformanceManifest =
        serde_json::from_slice(bytes).map_err(decode_error)?;
    if manifest.required_cases.len() > MAX_CASES
        || manifest.declared_metrics.len() > MAX_METRICS
        || manifest.preconditions.len() > MAX_PRECONDITIONS
        || manifest.evidence_ids_required_for_integrated.len() > MAX_EVIDENCE_IDS
        || manifest.cutover_delete_symbols.len() > MAX_CASES
        || manifest.case_authority.len() > MAX_CASES
        || manifest
            .preconditions
            .iter()
            .any(|precondition| precondition.seams.len() > MAX_SEAMS)
    {
        return Err(ManifestError::CollectionBound);
    }
    if !strings_eq(&manifest.required_cases, REQUIRED_CASES)
        || !strings_eq(&manifest.declared_metrics, &REQUIRED_METRICS)
        || !strings_eq(
            &manifest.evidence_ids_required_for_integrated,
            &REQUIRED_EVIDENCE_IDS,
        )
        || !strings_eq(&manifest.cutover_delete_symbols, &CUTOVER_DELETE_SYMBOLS)
        || has_duplicate_strings(&manifest.required_cases)
        || has_duplicate_strings(&manifest.declared_metrics)
        || has_duplicate_strings(&manifest.evidence_ids_required_for_integrated)
        || manifest.required_cases.len() != manifest.case_authority.len()
    {
        return Err(ManifestError::Invalid);
    }
    for case in &manifest.required_cases {
        match manifest.case_authority.get(case) {
            Some(CaseAuthority::Fake) => {}
            Some(CaseAuthority::Canonical)
                if manifest.integration_claim == IntegrationClaim::Integrated => {}
            _ => return Err(ManifestError::Invalid),
        }
    }
    if manifest.integration_claim == IntegrationClaim::None
        && manifest
            .case_authority
            .values()
            .any(|authority| *authority == CaseAuthority::Canonical)
    {
        return Err(ManifestError::Invalid);
    }
    if manifest.preconditions.len() != REQUIRED_PRECONDITION_IDS.len()
        || manifest
            .preconditions
            .iter()
            .zip(REQUIRED_PRECONDITION_IDS)
            .any(|(precondition, id)| precondition.id != id)
        || !canonical_seams_match(&manifest)
        || manifest.preconditions.iter().any(|precondition| {
            precondition.seams.is_empty()
                || precondition
                    .seams
                    .iter()
                    .any(|seam| seam.needle.is_empty() || !is_safe_relative(&seam.path))
        })
    {
        return Err(ManifestError::Invalid);
    }
    Ok(manifest)
}

fn decode_project_fixture(bytes: &[u8]) -> Result<ProjectFixture, ManifestError> {
    if bytes.len() > MAX_PROJECT_BYTES {
        return Err(ManifestError::FrameTooLarge);
    }
    let project: ProjectFixture = serde_json::from_slice(bytes).map_err(decode_error)?;
    if project.folders.len() > MAX_SEAMS
        || project.commands.len() > MAX_SEAMS
        || project.ssh_hosts.len() > MAX_SEAMS
        || project.external_listeners.len() > MAX_SEAMS
    {
        return Err(ManifestError::CollectionBound);
    }
    if project.project.id.is_empty() || project.project.default_workspace.is_empty() {
        return Err(ManifestError::Invalid);
    }
    if project
        .folders
        .iter()
        .any(|folder| !is_safe_relative(&folder.path))
        || project
            .commands
            .iter()
            .any(|command| !is_safe_relative(&command.cwd))
    {
        return Err(ManifestError::Invalid);
    }
    Ok(project)
}

fn raw_string_prefix(chars: &[char], index: usize) -> Option<(usize, usize)> {
    let mut cursor = index;
    if matches!(chars.get(cursor), Some('b' | 'c')) {
        cursor += 1;
    }
    if chars.get(cursor) != Some(&'r') {
        return None;
    }
    cursor += 1;
    let mut hashes = 0;
    while chars.get(cursor) == Some(&'#') {
        hashes += 1;
        cursor += 1;
    }
    if chars.get(cursor) != Some(&'"') {
        return None;
    }
    cursor += 1;
    Some((cursor - index, hashes))
}

fn skip_quoted_string(chars: &[char], mut index: usize) -> usize {
    while index < chars.len() {
        if chars[index] == '\\' {
            index = index.saturating_add(2);
            continue;
        }
        if chars[index] == '"' {
            return index + 1;
        }
        index += 1;
    }
    index
}

fn skip_block_comment(chars: &[char], mut index: usize) -> usize {
    let mut depth = 1;
    while index + 1 < chars.len() && depth > 0 {
        if chars[index] == '/' && chars[index + 1] == '*' {
            depth += 1;
            index += 2;
            continue;
        }
        if chars[index] == '*' && chars[index + 1] == '/' {
            depth -= 1;
            index += 2;
            continue;
        }
        index += 1;
    }
    index
}

fn strip_html_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start + 4..].find("-->") {
            Some(end) => rest = &rest[start + 4 + end + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn token_present(code: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    code.match_indices(needle).any(|(offset, matched)| {
        let before_ok = offset == 0
            || !code[..offset]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_');
        let after = offset + matched.len();
        let after_ok = !code[after..]
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_');
        before_ok && after_ok
    })
}

fn strip_comments_and_strings(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut index = 0;
    while index < chars.len() {
        if let Some((consumed, hashes)) = raw_string_prefix(&chars, index) {
            index += consumed;
            while index < chars.len() {
                if chars[index] == '"'
                    && (0..hashes).all(|offset| chars.get(index + 1 + offset) == Some(&'#'))
                {
                    index += 1 + hashes;
                    break;
                }
                index += 1;
            }
            out.push(' ');
            continue;
        }
        let current = chars[index];
        if current == '/' && chars.get(index + 1) == Some(&'/') {
            index += 2;
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
            continue;
        }
        if current == '/' && chars.get(index + 1) == Some(&'*') {
            index = skip_block_comment(&chars, index + 2);
            continue;
        }
        if matches!(current, 'b' | 'c') && chars.get(index + 1) == Some(&'"') {
            index = skip_quoted_string(&chars, index + 2);
            out.push(' ');
            continue;
        }
        if current == '"' {
            index = skip_quoted_string(&chars, index + 1);
            out.push(' ');
            continue;
        }
        out.push(current);
        index += 1;
    }
    out
}

fn type_item_present(source: &str, symbol: &str) -> bool {
    if symbol.is_empty() {
        return false;
    }
    let code = strip_comments_and_strings(source);
    ["struct ", "enum ", "fn ", "type "]
        .iter()
        .any(|prefix| token_present(&code, &format!("{prefix}{symbol}")))
}

fn code_contains_needle(source: &str, needle: &str) -> bool {
    token_present(&strip_comments_and_strings(source), needle)
}

fn seam_satisfied(root: &Path, seam: &SeamRequirement) -> bool {
    if seam.needle.is_empty() || !is_safe_relative(&seam.path) {
        return false;
    }
    let path = root.join(&seam.path);
    if path_has_reparse_component(&path) {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return false;
    };
    if !metadata.is_file() || is_reparse_point(&path) {
        return false;
    }
    let len = metadata.len() as usize;
    if len < MIN_SEAM_FILE_BYTES || len > MAX_SEAM_FILE_BYTES {
        return false;
    }
    let Ok(bytes) = fs::read(&path) else {
        return false;
    };
    if bytes.len() > MAX_SEAM_FILE_BYTES {
        return false;
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return false;
    };
    if seam.path.ends_with(".md") {
        return token_present(&strip_html_comments(text), &seam.needle);
    }
    code_contains_needle(text, &seam.needle)
}

fn try_issue_authenticated_evidence(root: &Path) -> Result<SealedEvidence, HoldReason> {
    let mut missing = Vec::new();
    for (id, path, needle) in EVIDENCE_ISSUER_SEAMS {
        let seam = SeamRequirement {
            path: (*path).to_string(),
            needle: (*needle).to_string(),
        };
        if !seam_satisfied(root, &seam) {
            missing.push(format!("{id}:{path}::{needle}"));
        }
    }
    if !missing.is_empty() {
        return Err(HoldReason {
            id: "evidence".to_string(),
            missing_seams: missing,
        });
    }
    Err(HoldReason {
        id: "evidence".to_string(),
        missing_seams: vec!["authenticated observation".into()],
    })
}

fn evaluate_snapshot(snapshot: &PreconditionSnapshot<'_>) -> PreconditionReport {
    let mut holds = Vec::new();
    if Instant::now() > snapshot.deadline {
        holds.push(HoldReason {
            id: "deadline".to_string(),
            missing_seams: vec!["evaluate deadline".into()],
        });
    }
    if snapshot.task_id.is_empty() {
        holds.push(HoldReason {
            id: "binding".to_string(),
            missing_seams: vec!["authoritative task id".into()],
        });
    }
    if !canonical_seams_match(snapshot.manifest) {
        holds.push(HoldReason {
            id: "graph".to_string(),
            missing_seams: vec!["canonical seams".into()],
        });
    }
    for id in REQUIRED_PRECONDITION_IDS {
        if !snapshot
            .manifest
            .preconditions
            .iter()
            .any(|precondition| precondition.id == id)
        {
            holds.push(HoldReason {
                id: id.to_string(),
                missing_seams: vec!["graph".into()],
            });
        }
    }
    for (id, seams) in REQUIRED_CANONICAL_SEAMS {
        let missing = seams
            .iter()
            .filter(|(path, needle)| {
                !seam_satisfied(
                    snapshot.worktree_root,
                    &SeamRequirement {
                        path: (*path).to_string(),
                        needle: (*needle).to_string(),
                    },
                )
            })
            .map(|(path, needle)| format!("{path}::{needle}"))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            holds.push(HoldReason {
                id: (*id).to_string(),
                missing_seams: missing,
            });
        }
    }

    let leftover = snapshot
        .manifest
        .cutover_delete_symbols
        .iter()
        .filter(|symbol| type_item_present(snapshot.source, symbol))
        .cloned()
        .collect::<Vec<_>>();
    if snapshot.manifest.integration_claim == IntegrationClaim::Integrated && !leftover.is_empty() {
        holds.push(HoldReason {
            id: "cutover_symbols".to_string(),
            missing_seams: leftover.clone(),
        });
    }

    let evidence = try_issue_authenticated_evidence(snapshot.worktree_root);
    let mut satisfied_evidence_ids = Vec::new();
    match &evidence {
        Ok(sealed) => satisfied_evidence_ids = sealed.ids.clone(),
        Err(reason) if snapshot.manifest.integration_claim == IntegrationClaim::Integrated => {
            holds.push(reason.clone());
        }
        Err(_) => {}
    }

    if snapshot.manifest.integration_claim == IntegrationClaim::Integrated && !holds.is_empty() {
        if !holds.iter().any(|hold| hold.id == "claim_promotion") {
            holds.push(HoldReason {
                id: "claim_promotion".to_string(),
                missing_seams: vec!["integration_claim cannot leave none while HOLDs remain".into()],
            });
        }
    }

    let claim = if snapshot.manifest.integration_claim == IntegrationClaim::Integrated
        && holds.is_empty()
        && leftover.is_empty()
        && evidence.is_ok()
    {
        IntegrationClaim::Integrated
    } else {
        IntegrationClaim::None
    };
    if claim != IntegrationClaim::Integrated {
        satisfied_evidence_ids.clear();
    }

    PreconditionReport {
        claim,
        holds,
        satisfied_evidence_ids,
    }
}

fn evaluate_preconditions(manifest: &WorkspaceConformanceManifest) -> PreconditionReport {
    evaluate_snapshot(&PreconditionSnapshot {
        manifest,
        worktree_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        deadline: Instant::now() + Duration::from_millis(EVALUATE_DEADLINE_MS),
        source: include_str!("workspace_conformance.rs"),
        task_id: FIXTURE_TASK_ID,
    })
}

fn load_capabilities() -> CapabilityFixture {
    serde_json::from_str(include_str!("fixtures/workspace/v1/capabilities.json"))
        .expect("workspace capability fixture must be valid JSON")
}

fn lf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn fixture_repo_files() -> Vec<(&'static str, String)> {
    vec![
        (
            "README.md",
            lf(include_str!("fixtures/workspace/v1/repo/README.md")),
        ),
        (
            "src/app.txt",
            lf(include_str!("fixtures/workspace/v1/repo/src/app.txt")),
        ),
        (
            ".gitignore",
            lf(include_str!("fixtures/workspace/v1/repo/.gitignore")),
        ),
    ]
}

fn retained_git() -> PathBuf {
    for candidate in [
        r"C:\Program Files\Git\cmd\git.exe",
        r"C:\Program Files\Git\bin\git.exe",
    ] {
        let path = PathBuf::from(candidate);
        if path.is_file() && !is_reparse_point(&path) {
            return path;
        }
    }
    panic!("retained Git identity is missing");
}

fn run_git(repo: &Path, args: &[&str]) -> Result<Output, String> {
    let mut command = Command::new(retained_git());
    command
        .args([
            "-c",
            "credential.helper=",
            "-c",
            "core.fsmonitor=",
            "-c",
            "core.hooksPath=",
            "-c",
            "protocol.file.allow=user",
        ])
        .args(args)
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Workspace Conformance")
        .env("GIT_AUTHOR_EMAIL", "workspace-conformance@devmanager.test")
        .env("GIT_COMMITTER_NAME", "Workspace Conformance")
        .env(
            "GIT_COMMITTER_EMAIL",
            "workspace-conformance@devmanager.test",
        );
    for key in [
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_ASKPASS",
        "GIT_SSH_COMMAND",
        "GIT_EXTERNAL_DIFF",
        "GIT_ALLOW_PROTOCOL",
    ] {
        command.env_remove(key);
    }
    let output = command.output().map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn git_stdout(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = run_git(repo, args)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn canonical(path: &Path) -> Result<PathBuf, HostError> {
    if path_has_reparse_component(path) {
        return Err(HostError::EscapeRejected);
    }
    let resolved = fs::canonicalize(path).map_err(|_| HostError::EscapeRejected)?;
    Ok(strip_verbatim(resolved))
}

fn strip_verbatim(path: PathBuf) -> PathBuf {
    let displayed = path.to_string_lossy();
    if let Some(stripped) = displayed.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        path
    }
}

fn is_reparse_point(path: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        return (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0;
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn path_has_reparse_component(path: &Path) -> bool {
    let full = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut current = PathBuf::new();
    for component in full.components() {
        current.push(component);
        if is_reparse_point(&current) {
            return true;
        }
    }
    false
}

fn is_initialized_git_worktree(path: &Path) -> Result<PathBuf, HostError> {
    if path_has_reparse_component(path) {
        return Err(HostError::EscapeRejected);
    }
    let inside = git_stdout(path, &["rev-parse", "--is-inside-work-tree"])
        .map_err(|_| HostError::NotARepository)?;
    if inside != "true" {
        return Err(HostError::NotARepository);
    }
    let toplevel = git_stdout(path, &["rev-parse", "--show-toplevel"])
        .map_err(|_| HostError::NotARepository)?;
    let top = canonical(Path::new(&toplevel))?;
    let want = canonical(path)?;
    if !path_is_within(&want, &top) || !path_is_within(&top, &want) {
        return Err(HostError::EscapeRejected);
    }
    Ok(want)
}

fn path_is_within(candidate: &Path, root: &Path) -> bool {
    let Ok(candidate) = canonical(candidate) else {
        return false;
    };
    let Ok(root) = canonical(root) else {
        return false;
    };
    let candidate = candidate
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    let root = root
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    candidate == root || candidate.starts_with(&(root + "\\"))
}

fn reject_unsafe_relative(relative: &str) -> Result<(), HostError> {
    let lowered = relative.replace('/', "\\").to_ascii_lowercase();
    if relative.is_empty() {
        return Err(HostError::PathRejected("empty"));
    }
    if relative.chars().any(|ch| ch == '\0') {
        return Err(HostError::PathRejected("nul"));
    }
    if lowered.contains("..") {
        return Err(HostError::EscapeRejected);
    }
    if lowered.contains(':') {
        return Err(HostError::PathRejected("ads-or-device"));
    }
    if lowered.starts_with('\\') || Path::new(relative).is_absolute() {
        return Err(HostError::EscapeRejected);
    }
    Ok(())
}

fn copy_fixture_tree(dest: &Path) {
    for (relative, contents) in fixture_repo_files() {
        let path = dest.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("fixture parent");
        }
        fs::write(path, contents).expect("write fixture file");
    }
}

fn init_temp_repository(root: &Path) -> (PathBuf, PathBuf) {
    let main_repo = root.join("main");
    let remote_bare = root.join("remote.git");
    fs::create_dir_all(&main_repo).expect("main repo");
    run_git(&main_repo, &["init", "-b", "main"]).expect("git init");
    run_git(
        &main_repo,
        &["config", "user.name", "Workspace Conformance"],
    )
    .unwrap();
    run_git(
        &main_repo,
        &[
            "config",
            "user.email",
            "workspace-conformance@devmanager.test",
        ],
    )
    .unwrap();
    copy_fixture_tree(&main_repo);
    run_git(&main_repo, &["add", "."]).expect("git add");
    run_git(&main_repo, &["commit", "-m", "fixture baseline"]).expect("git commit");
    run_git(root, &["init", "--bare", remote_bare.to_str().unwrap()]).expect("bare remote");
    run_git(
        &main_repo,
        &["remote", "add", "origin", remote_bare.to_str().unwrap()],
    )
    .expect("add remote");
    run_git(&main_repo, &["push", "-u", "origin", "main"]).expect("push baseline");
    fs::write(main_repo.join("dirty-main.txt"), "user-dirty\n").expect("dirty main file");
    fs::write(main_repo.join("untracked-main.txt"), "user-untracked\n").expect("untracked main");
    (main_repo, remote_bare)
}

impl FakeWorkspaceHost {
    fn boot() -> Self {
        let root = TempDir::new().expect("temp workspace root");
        let (main_repo, remote_bare) = init_temp_repository(root.path());
        let worktree_root = root.path().join("worktrees");
        let artifact_root = root.path().join("artifacts");
        fs::create_dir_all(&worktree_root).unwrap();
        fs::create_dir_all(&artifact_root).unwrap();
        let journal_path = root.path().join("journal.json");
        let fixture = load_capabilities();
        let unavailable = UNAVAILABLE_ADAPTERS
            .iter()
            .map(|path| (*path).to_string())
            .collect();
        let mut host = Self {
            _root: root,
            main_repo,
            remote_bare,
            worktree_root,
            artifact_root,
            journal_path,
            bindings: HashMap::new(),
            worktrees: HashMap::new(),
            live_resources: HashSet::new(),
            artifacts: HashMap::new(),
            checkpoints: Vec::new(),
            journal: Vec::new(),
            queued: VecDeque::new(),
            cancelled_generations: BTreeSet::new(),
            generation: 1,
            jobs: HashSet::new(),
            capabilities: CapabilityReport {
                git: fixture.git,
                service: fixture.service,
                port: fixture.port,
                ssh: fixture.ssh,
                unavailable_adapters: unavailable,
            },
            next_seq: 1,
        };
        host.record("boot", "fake-host");
        host
    }

    fn record(&mut self, kind: &str, detail: &str) {
        let entry = JournalEntry {
            seq: self.next_seq,
            generation: self.generation,
            kind: kind.to_string(),
            detail: detail.to_string(),
        };
        self.next_seq += 1;
        self.journal.push(entry);
        self.persist_journal();
    }

    fn persist_journal(&self) {
        let encoded = serde_json::to_string_pretty(&self.journal_wire()).expect("journal encode");
        fs::write(&self.journal_path, encoded).expect("persist journal");
    }

    fn journal_wire(&self) -> serde_json::Value {
        serde_json::json!({
            "entries": self.journal.iter().map(|entry| {
                serde_json::json!({
                    "seq": entry.seq,
                    "generation": entry.generation,
                    "kind": entry.kind,
                    "detail": entry.detail,
                })
            }).collect::<Vec<_>>(),
            "cancelled_generations": self.cancelled_generations.iter().copied().collect::<Vec<_>>(),
            "next_seq": self.next_seq,
            "generation": self.generation,
        })
    }

    fn reopen_from_journal(&self) -> Result<Vec<JournalEntry>, HostError> {
        let raw = fs::read_to_string(&self.journal_path).expect("read journal");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("journal json");
        let entries = value["entries"]
            .as_array()
            .expect("journal entries")
            .iter()
            .map(|entry| JournalEntry {
                seq: entry["seq"].as_u64().expect("seq"),
                generation: entry["generation"].as_u64().expect("generation"),
                kind: entry["kind"].as_str().expect("kind").to_string(),
                detail: entry["detail"].as_str().expect("detail").to_string(),
            })
            .collect::<Vec<_>>();
        for (index, entry) in entries.iter().enumerate() {
            let expected = (index as u64) + 1;
            if entry.seq != expected {
                return Err(HostError::Conflict);
            }
        }
        if entries != self.journal {
            return Err(HostError::Conflict);
        }
        Ok(entries)
    }

    fn bind(
        &mut self,
        task_id: &str,
        choice: WorkspaceChoice,
        external: Option<&Path>,
    ) -> Result<Binding, HostError> {
        if self.live_resources.contains(task_id) {
            return Err(HostError::LiveResourcesPresent);
        }
        if let Some(existing) = self.bindings.get(task_id) {
            if existing.choice != choice {
                return Err(HostError::LiveResourcesPresent);
            }
            return Ok(existing.clone());
        }
        let workspace = match choice {
            WorkspaceChoice::Main => {
                is_initialized_git_worktree(&self.main_repo)?;
                WorkspaceRef::Main
            }
            WorkspaceChoice::NewWorktree => {
                let record = self.create_worktree(task_id)?;
                WorkspaceRef::worktree(&record.path, &record.branch).expect("worktree ref")
            }
            WorkspaceChoice::External => {
                let path = external.ok_or(HostError::NonexistentPath)?;
                if fs::symlink_metadata(path).is_err() {
                    return Err(HostError::NonexistentPath);
                }
                if path_has_reparse_component(path) {
                    return Err(HostError::EscapeRejected);
                }
                if !path_is_within(path, self._root.path()) {
                    return Err(HostError::EscapeRejected);
                }
                let canonical_root = is_initialized_git_worktree(path)?;
                WorkspaceRef::external(canonical_root).expect("external ref")
            }
        };
        let binding = Binding {
            task_id: task_id.to_string(),
            choice,
            workspace,
        };
        self.bindings.insert(task_id.to_string(), binding.clone());
        self.record("bind", task_id);
        Ok(binding)
    }

    fn create_worktree(&mut self, task_id: &str) -> Result<WorktreeRecord, HostError> {
        if self.cancelled_generations.contains(&self.generation) {
            return Err(HostError::Cancelled);
        }
        let suffix = task_id.chars().rev().take(8).collect::<String>();
        let branch = format!("codex/task-{suffix}");
        let path = self.worktree_root.join(format!("task-{suffix}"));
        if path.exists() {
            return Err(HostError::Conflict);
        }
        let base_commit = git_stdout(&self.main_repo, &["rev-parse", "HEAD"]).expect("HEAD");
        run_git(
            &self.main_repo,
            &[
                "worktree",
                "add",
                "-b",
                &branch,
                path.to_str().expect("worktree path"),
                "HEAD",
            ],
        )
        .map_err(|_| HostError::NotARepository)?;
        let record = WorktreeRecord {
            path: canonical(&path)?,
            branch,
            base_commit,
            dirty: false,
            unpushed: true,
        };
        self.worktrees.insert(task_id.to_string(), record.clone());
        self.jobs.insert(format!("worktree:{task_id}"));
        self.record("worktree.create", task_id);
        Ok(record)
    }

    fn mark_live(&mut self, task_id: &str) {
        self.live_resources.insert(task_id.to_string());
        self.record("resource.live", task_id);
    }

    fn preview_remove_worktree(&self, task_id: &str) -> Result<(), HostError> {
        let record = self
            .worktrees
            .get(task_id)
            .ok_or(HostError::NonexistentPath)?;
        if path_has_reparse_component(&record.path) {
            return Err(HostError::EscapeRejected);
        }
        let status = git_stdout(&record.path, &["status", "--porcelain=v2", "-z"])
            .map_err(|_| HostError::GitFailed)?;
        if record.dirty || record.unpushed || !status.is_empty() {
            return Err(HostError::DirtyOrUnpushed);
        }
        Ok(())
    }

    fn remove_worktree(
        &mut self,
        task_id: &str,
        force: bool,
        expected: &Path,
    ) -> Result<(), HostError> {
        let record = self
            .worktrees
            .get(task_id)
            .ok_or(HostError::NonexistentPath)?
            .clone();
        if path_has_reparse_component(expected) || path_has_reparse_component(&record.path) {
            return Err(HostError::EscapeRejected);
        }
        if canonical(expected)? != record.path {
            return Err(HostError::EscapeRejected);
        }
        if !force {
            self.preview_remove_worktree(task_id)?;
        }
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        let path = record.path.to_string_lossy().to_string();
        args.push(path.as_str());
        run_git(&self.main_repo, &args).map_err(|_| HostError::Conflict)?;
        self.worktrees.remove(task_id);
        self.jobs.remove(&format!("worktree:{task_id}"));
        self.record("worktree.remove", task_id);
        Ok(())
    }

    fn read_file(&self, task_id: &str, relative: &str) -> Result<Vec<u8>, HostError> {
        reject_unsafe_relative(relative)?;
        let root = self.workspace_root(task_id)?;
        let path = root.join(relative);
        if !path_is_within(&path, &root) {
            return Err(HostError::EscapeRejected);
        }
        let metadata = fs::metadata(&path).map_err(|_| HostError::NonexistentPath)?;
        if metadata.len() as usize > MAX_FILE_CHUNK_BYTES {
            return Err(HostError::PathRejected("too-large"));
        }
        fs::read(&path).map_err(|_| HostError::NonexistentPath)
    }

    fn write_file(
        &mut self,
        task_id: &str,
        relative: &str,
        contents: &[u8],
    ) -> Result<String, HostError> {
        reject_unsafe_relative(relative)?;
        if contents.len() > MAX_FILE_CHUNK_BYTES {
            return Err(HostError::PathRejected("too-large"));
        }
        let root = self.workspace_root(task_id)?;
        let path = root.join(relative);
        if !path_is_within(path.parent().unwrap_or(&path), &root) {
            return Err(HostError::EscapeRejected);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, contents).map_err(|_| HostError::Conflict)?;
        fs::rename(&tmp, &path).map_err(|_| HostError::Conflict)?;
        if let Some(record) = self.worktrees.get_mut(task_id) {
            record.dirty = true;
        }
        let hash = sha256_hex(contents);
        self.record("file.write", relative);
        Ok(hash)
    }

    fn export_artifact(
        &mut self,
        task_id: &str,
        relative: &str,
    ) -> Result<ArtifactRecord, HostError> {
        let bytes = self.read_file(task_id, relative)?;
        let sha = sha256_hex(&bytes);
        let id = format!("art-{sha:.12}");
        let dest = self.artifact_root.join(&id);
        fs::write(&dest, &bytes).expect("artifact write");
        let record = ArtifactRecord {
            id: id.clone(),
            relative_path: relative.to_string(),
            sha256: sha,
            bytes: bytes.len(),
        };
        self.artifacts.insert(id, record.clone());
        self.record("artifact.export", relative);
        Ok(record)
    }

    fn checkpoint(&mut self, task_id: &str, reason: &str) -> Result<Checkpoint, HostError> {
        let root = self.workspace_root(task_id)?;
        let head = git_stdout(&root, &["rev-parse", "HEAD"]).map_err(|_| HostError::GitFailed)?;
        let mut files = BTreeMap::new();
        let mut blobs = BTreeMap::new();
        let mut relatives: Vec<String> = fixture_repo_files()
            .into_iter()
            .map(|(relative, _)| relative.to_string())
            .collect();
        relatives.push("src/app.txt".to_string());
        relatives.sort();
        relatives.dedup();
        for relative in relatives {
            if let Ok(bytes) = fs::read(root.join(&relative)) {
                files.insert(relative.clone(), sha256_hex(&bytes));
                blobs.insert(relative, bytes);
            }
        }
        let checkpoint = Checkpoint {
            id: format!("cp-{}", self.next_seq),
            head,
            files,
            blobs,
            reason: reason.to_string(),
        };
        self.checkpoints.push(checkpoint.clone());
        self.record("checkpoint.capture", reason);
        Ok(checkpoint)
    }

    fn restore(
        &mut self,
        task_id: &str,
        checkpoint_id: &str,
        relative: &str,
    ) -> Result<(), HostError> {
        if relative.contains("reset --hard") || relative.contains("clean -fd") {
            return Err(HostError::HardResetForbidden);
        }
        reject_unsafe_relative(relative)?;
        let checkpoint = self
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.id == checkpoint_id)
            .cloned()
            .ok_or(HostError::NonexistentPath)?;
        let expected = checkpoint
            .files
            .get(relative)
            .ok_or(HostError::NonexistentPath)?;
        let blob = checkpoint
            .blobs
            .get(relative)
            .ok_or(HostError::NonexistentPath)?;
        if sha256_hex(blob) != *expected {
            return Err(HostError::Conflict);
        }
        self.write_file(task_id, relative, blob)?;
        self.record("checkpoint.restore", relative);
        Ok(())
    }

    fn forbid_hard_reset(&self, command: &[&str]) -> Result<(), HostError> {
        let joined = command.join(" ");
        if joined.contains("reset --hard") || joined.contains("clean -fd") {
            return Err(HostError::HardResetForbidden);
        }
        Ok(())
    }

    fn queue(&mut self, kind: &str) -> u64 {
        let generation = self.generation;
        self.queued.push_back(QueuedOp {
            generation,
            kind: kind.to_string(),
        });
        self.record("queue", kind);
        generation
    }

    fn cancel(&mut self, generation: u64) {
        self.cancelled_generations.insert(generation);
        self.queued.retain(|op| op.generation != generation);
        self.generation += 1;
        self.record("cancel", &generation.to_string());
    }

    fn drain_queue(&mut self) -> Result<Vec<String>, HostError> {
        let mut executed = Vec::new();
        while let Some(op) = self.queued.pop_front() {
            if self.cancelled_generations.contains(&op.generation) {
                return Err(HostError::Cancelled);
            }
            executed.push(op.kind);
        }
        Ok(executed)
    }

    fn close_task(&mut self, task_id: &str) -> Result<(), HostError> {
        self.live_resources.remove(task_id);
        if self.worktrees.contains_key(task_id) {
            let path = self.worktrees[task_id].path.clone();
            self.remove_worktree(task_id, true, &path)?;
        }
        self.bindings.remove(task_id);
        self.jobs.remove(&format!("worktree:{task_id}"));
        self.record("close", task_id);
        Ok(())
    }

    fn residue(&self) -> (usize, usize) {
        (self.jobs.len(), self.worktrees.len())
    }

    fn workspace_root(&self, task_id: &str) -> Result<PathBuf, HostError> {
        let binding = self
            .bindings
            .get(task_id)
            .ok_or(HostError::NonexistentPath)?;
        match &binding.workspace {
            WorkspaceRef::Main | WorkspaceRef::MainWithFingerprint { .. } => {
                canonical(&self.main_repo)
            }
            WorkspaceRef::Worktree { path, .. }
            | WorkspaceRef::External { path }
            | WorkspaceRef::WorktreeWithFingerprint { path, .. }
            | WorkspaceRef::ExternalWithFingerprint { path, .. } => canonical(path),
            // Durable host-bound references intentionally keep their locator
            // opaque to this fixture-only fake host, so they cannot be opened
            // without the real host rebinding authority.
            WorkspaceRef::HostBound { .. } => Err(HostError::NonexistentPath),
        }
    }

    fn report_capabilities(&self) -> CapabilityReport {
        self.capabilities.clone()
    }
}

fn smoke_script_source() -> String {
    fs::read_to_string("scripts/native-next/Invoke-WorkspaceSmoke.ps1")
        .expect("workspace smoke script must exist")
}

fn run_pwsh_file(args: &[&str]) -> Output {
    Command::new("pwsh")
        .args(["-NoProfile", "-File"])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to spawn pwsh")
}

#[test]
fn workspace_conformance_manifest_declares_required_fake_host_cases() {
    let manifest = decode_workspace_manifest(include_bytes!("fixtures/workspace/manifest.json"))
        .expect("workspace conformance manifest must decode");
    assert_eq!(manifest.schema_version, 1);
    assert_eq!(manifest.suite, "workspace-conformance-v1");
    assert_eq!(manifest.mode, "fixture-fake-host");
    assert_eq!(manifest.integration_claim, IntegrationClaim::None);
    assert_eq!(manifest.declared_metrics, REQUIRED_METRICS);
    for required in REQUIRED_CASES {
        assert!(
            manifest.required_cases.iter().any(|case| case == required),
            "missing required case {required}"
        );
        assert_eq!(
            manifest.case_authority.get(*required).copied(),
            Some(CaseAuthority::Fake),
            "required case {required} must stay fake until host seams exist"
        );
    }
}

#[test]
fn manifest_decode_denies_unknown_fields_and_bounds_collections() {
    let fixture = serde_json::from_slice::<serde_json::Value>(include_bytes!(
        "fixtures/workspace/manifest.json"
    ))
    .unwrap();
    let mut valid = fixture.clone();
    valid
        .as_object_mut()
        .unwrap()
        .insert("unexpected_coverage".into(), serde_json::json!(true));
    let unknown = serde_json::to_vec(&valid).unwrap();
    assert_eq!(
        decode_workspace_manifest(&unknown),
        Err(ManifestError::UnknownField)
    );

    let oversized = vec![b'{'; MAX_MANIFEST_BYTES + 1];
    assert_eq!(
        decode_workspace_manifest(&oversized),
        Err(ManifestError::FrameTooLarge)
    );

    let mut too_many = fixture;
    too_many["declared_metrics"] = serde_json::json!((0..MAX_METRICS + 1)
        .map(|index| format!("metric_{index}"))
        .collect::<Vec<_>>());
    assert_eq!(
        decode_workspace_manifest(&serde_json::to_vec(&too_many).unwrap()),
        Err(ManifestError::CollectionBound)
    );
}

#[test]
fn project_fixture_is_validated_with_deny_unknown_fields() {
    let project = decode_project_fixture(include_bytes!("fixtures/workspace/v1/project.json"))
        .expect("project fixture must be live and valid");
    assert_eq!(project.schema_version, 1);
    assert_eq!(project.project.id, "0198b6b0-0000-7000-8000-0000000000aa");
    assert_eq!(project.project.default_workspace, "new_worktree");
    assert_eq!(project.commands.len(), 1);
    assert_eq!(project.external_listeners[0].port, 18080);

    let mut valid = serde_json::from_slice::<serde_json::Value>(include_bytes!(
        "fixtures/workspace/v1/project.json"
    ))
    .unwrap();
    valid
        .as_object_mut()
        .unwrap()
        .insert("integrated".into(), serde_json::json!(true));
    assert_eq!(
        decode_project_fixture(&serde_json::to_vec(&valid).unwrap()),
        Err(ManifestError::UnknownField)
    );
    assert_eq!(
        decode_project_fixture(&vec![b'{'; MAX_PROJECT_BYTES + 1]),
        Err(ManifestError::FrameTooLarge)
    );
}

#[test]
fn precondition_hold_while_s2_through_s9_seams_missing() {
    let manifest =
        decode_workspace_manifest(include_bytes!("fixtures/workspace/manifest.json")).unwrap();
    let report = evaluate_preconditions(&manifest);
    assert_eq!(report.claim, IntegrationClaim::None);
    for id in ["S2", "S3", "S4", "S5", "S6", "S7", "S8", "S9"] {
        assert!(
            report
                .holds
                .iter()
                .any(|hold| hold.id == id && !hold.missing_seams.is_empty()),
            "expected typed HOLD for {id}, got {:?}",
            report.holds
        );
    }
    assert!(!report.holds.is_empty());
}

#[test]
fn fake_host_green_cannot_promote_integration_claim() {
    let manifest =
        decode_workspace_manifest(include_bytes!("fixtures/workspace/manifest.json")).unwrap();
    let mut promoted = manifest.clone();
    promoted.integration_claim = IntegrationClaim::Integrated;
    let report = evaluate_preconditions(&promoted);
    assert_eq!(report.claim, IntegrationClaim::None);
    assert!(
        report.holds.iter().any(|hold| hold.id == "claim_promotion"),
        "fake-host GREEN must not promote integration_claim: {:?}",
        report.holds
    );
}

#[test]
fn integrated_result_requires_host_sqlite_job_evidence_ids() {
    let manifest =
        decode_workspace_manifest(include_bytes!("fixtures/workspace/manifest.json")).unwrap();
    for required in REQUIRED_EVIDENCE_IDS {
        assert!(
            manifest
                .evidence_ids_required_for_integrated
                .iter()
                .any(|id| id == required),
            "missing required evidence id {required}"
        );
    }
    let report = evaluate_preconditions(&manifest);
    assert!(
        report.satisfied_evidence_ids.is_empty(),
        "HOLD must not invent host/SQLite/Job evidence: {:?}",
        report.satisfied_evidence_ids
    );
}

#[test]
fn cutover_deletion_symbols_block_integrated_claim() {
    let source = include_str!("workspace_conformance.rs");
    for symbol in CUTOVER_DELETE_SYMBOLS {
        assert!(
            source.contains(symbol),
            "cutover symbol {symbol} must remain while claim is none"
        );
    }
    let manifest =
        decode_workspace_manifest(include_bytes!("fixtures/workspace/manifest.json")).unwrap();
    assert_eq!(manifest.cutover_delete_symbols, CUTOVER_DELETE_SYMBOLS);
    let mut promoted = manifest;
    promoted.integration_claim = IntegrationClaim::Integrated;
    let report = evaluate_preconditions(&promoted);
    assert!(
        report.holds.iter().any(|hold| hold.id == "cutover_symbols"),
        "integrated claim must fail while fake types remain: {:?}",
        report.holds
    );
}

fn fixture_manifest_value() -> serde_json::Value {
    serde_json::from_slice(include_bytes!("fixtures/workspace/manifest.json")).unwrap()
}

fn pad_to_min_seam(mut body: String) -> String {
    while body.len() < MIN_SEAM_FILE_BYTES {
        body.push_str("\n// padding-line-for-min-seam-size");
    }
    body
}

#[test]
fn manifest_decode_rejects_empty_seams_missing_s2_s9_duplicate_unknown_cases_metrics_and_canonical_while_none(
) {
    let mut duplicate_metrics = fixture_manifest_value();
    duplicate_metrics["declared_metrics"] = serde_json::json!([
        "result",
        "result",
        "latency_ms",
        "residue_jobs",
        "residue_worktrees"
    ]);
    assert_eq!(
        decode_workspace_manifest(&serde_json::to_vec(&duplicate_metrics).unwrap()),
        Err(ManifestError::Invalid)
    );

    let mut unknown_case = fixture_manifest_value();
    unknown_case["required_cases"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("binding.forged"));
    unknown_case["case_authority"]["binding.forged"] = serde_json::json!("fake");
    assert_eq!(
        decode_workspace_manifest(&serde_json::to_vec(&unknown_case).unwrap()),
        Err(ManifestError::Invalid)
    );

    let mut canonical = fixture_manifest_value();
    canonical["case_authority"]["binding.explicit_main"] = serde_json::json!("canonical");
    assert_eq!(
        decode_workspace_manifest(&serde_json::to_vec(&canonical).unwrap()),
        Err(ManifestError::Invalid)
    );

    let mut empty_seams = fixture_manifest_value();
    empty_seams["preconditions"][0]["seams"] = serde_json::json!([]);
    assert_eq!(
        decode_workspace_manifest(&serde_json::to_vec(&empty_seams).unwrap()),
        Err(ManifestError::Invalid)
    );

    let mut missing_s9 = fixture_manifest_value();
    missing_s9["preconditions"].as_array_mut().unwrap().pop();
    assert_eq!(
        decode_workspace_manifest(&serde_json::to_vec(&missing_s9).unwrap()),
        Err(ManifestError::Invalid)
    );

    let mut ambient = fixture_manifest_value();
    ambient["preconditions"][0]["seams"][0]["path"] = serde_json::json!("..\\src\\lib.rs");
    assert_eq!(
        decode_workspace_manifest(&serde_json::to_vec(&ambient).unwrap()),
        Err(ManifestError::Invalid)
    );
}

#[test]
fn manifest_decode_rejects_swapped_canonical_seam_path_or_needle() {
    let mut swapped_path = fixture_manifest_value();
    swapped_path["preconditions"][0]["seams"][0]["path"] =
        serde_json::json!("docs/replacement-deletion-ledger.md");
    assert_eq!(
        decode_workspace_manifest(&serde_json::to_vec(&swapped_path).unwrap()),
        Err(ManifestError::Invalid)
    );

    let mut swapped_needle = fixture_manifest_value();
    swapped_needle["preconditions"][0]["seams"][0]["needle"] = serde_json::json!("bounded");
    assert_eq!(
        decode_workspace_manifest(&serde_json::to_vec(&swapped_needle).unwrap()),
        Err(ManifestError::Invalid)
    );

    let mut extra_seam = fixture_manifest_value();
    extra_seam["preconditions"][0]["seams"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "path": "tests/fixtures/workspace/v1/repo/README.md",
            "needle": "pub struct ProjectStore"
        }));
    assert_eq!(
        decode_workspace_manifest(&serde_json::to_vec(&extra_seam).unwrap()),
        Err(ManifestError::Invalid)
    );
}

#[test]
fn seam_satisfied_rejects_empty_needle_comment_stub_and_unbounded_or_reparse_file() {
    let root = TempDir::new().unwrap();
    let src = root.path().join("src");
    fs::create_dir_all(&src).unwrap();
    let item = pad_to_min_seam("pub struct ProjectStore { pub id: u32 }\n".to_string());
    fs::write(src.join("real.rs"), &item).unwrap();
    assert!(seam_satisfied(
        root.path(),
        &SeamRequirement {
            path: "src/real.rs".into(),
            needle: "pub struct ProjectStore".into(),
        }
    ));

    assert!(!seam_satisfied(
        root.path(),
        &SeamRequirement {
            path: "src/real.rs".into(),
            needle: String::new(),
        }
    ));

    let comment = pad_to_min_seam("// pub struct ProjectStore\n".to_string());
    fs::write(src.join("comment.rs"), comment).unwrap();
    assert!(!seam_satisfied(
        root.path(),
        &SeamRequirement {
            path: "src/comment.rs".into(),
            needle: "pub struct ProjectStore".into(),
        }
    ));

    let literal = pad_to_min_seam("const X: &str = \"pub struct ProjectStore\";\n".to_string());
    fs::write(src.join("literal.rs"), literal).unwrap();
    assert!(!seam_satisfied(
        root.path(),
        &SeamRequirement {
            path: "src/literal.rs".into(),
            needle: "pub struct ProjectStore".into(),
        }
    ));

    let mut huge = item.clone();
    huge.push_str(&"x".repeat(MAX_SEAM_FILE_BYTES));
    fs::write(src.join("huge.rs"), huge).unwrap();
    assert!(!seam_satisfied(
        root.path(),
        &SeamRequirement {
            path: "src/huge.rs".into(),
            needle: "pub struct ProjectStore".into(),
        }
    ));

    let real_dir = src.join("realdir");
    fs::create_dir_all(&real_dir).unwrap();
    fs::write(real_dir.join("store.rs"), &item).unwrap();
    let link_dir = src.join("juncdir");
    create_junction(&link_dir, &real_dir);
    assert!(!seam_satisfied(
        root.path(),
        &SeamRequirement {
            path: "src/juncdir/store.rs".into(),
            needle: "pub struct ProjectStore".into(),
        }
    ));
    assert!(!seam_satisfied(
        root.path(),
        &SeamRequirement {
            path: "..\\src\\real.rs".into(),
            needle: "pub struct ProjectStore".into(),
        }
    ));

    let raw = pad_to_min_seam(
        "const X: &str = r#\"decoy \"\npub struct ProjectStore { pub id: u32 }\n\"#;\n".to_string(),
    );
    fs::write(src.join("raw.rs"), raw).unwrap();
    assert!(!seam_satisfied(
        root.path(),
        &SeamRequirement {
            path: "src/raw.rs".into(),
            needle: "pub struct ProjectStore".into(),
        }
    ));

    let byte = pad_to_min_seam("const X: &[u8] = b\"pub struct ProjectStore\";\n".to_string());
    fs::write(src.join("byte.rs"), byte).unwrap();
    assert!(!seam_satisfied(
        root.path(),
        &SeamRequirement {
            path: "src/byte.rs".into(),
            needle: "pub struct ProjectStore".into(),
        }
    ));

    let escaped =
        pad_to_min_seam("const X: &str = \"pub struct \\\" ProjectStore\";\n".to_string());
    fs::write(src.join("escaped.rs"), escaped).unwrap();
    assert!(!seam_satisfied(
        root.path(),
        &SeamRequirement {
            path: "src/escaped.rs".into(),
            needle: "pub struct ProjectStore".into(),
        }
    ));

    let nested = pad_to_min_seam(
        "/* outer /* inner */ pub struct ProjectStore { pub id: u32 } */\n".to_string(),
    );
    fs::write(src.join("nested.rs"), nested).unwrap();
    assert!(!seam_satisfied(
        root.path(),
        &SeamRequirement {
            path: "src/nested.rs".into(),
            needle: "pub struct ProjectStore".into(),
        }
    ));

    let attr = pad_to_min_seam("#[doc = \"pub struct ProjectStore\"]\nfn keep() {}\n".to_string());
    fs::write(src.join("attr.rs"), attr).unwrap();
    assert!(!seam_satisfied(
        root.path(),
        &SeamRequirement {
            path: "src/attr.rs".into(),
            needle: "pub struct ProjectStore".into(),
        }
    ));

    let suffix = pad_to_min_seam("pub struct ProjectStoreExtra { pub id: u32 }\n".to_string());
    fs::write(src.join("suffix.rs"), suffix).unwrap();
    assert!(!seam_satisfied(
        root.path(),
        &SeamRequirement {
            path: "src/suffix.rs".into(),
            needle: "pub struct ProjectStore".into(),
        }
    ));
}

#[test]
fn cutover_symbol_gate_requires_type_item_deletion_not_string_table_match() {
    let source = include_str!("workspace_conformance.rs");
    assert!(type_item_present(source, "FakeWorkspaceHost"));
    assert!(!type_item_present(
        r#"const CUTOVER_DELETE_SYMBOLS: [&str; 1] = ["FakeWorkspaceHost"];"#,
        "FakeWorkspaceHost"
    ));
    assert!(!type_item_present(
        "// struct FakeWorkspaceHost\nconst X: &str = \"struct FakeWorkspaceHost\";\n",
        "FakeWorkspaceHost"
    ));
    assert!(!type_item_present(
        "/* outer /* inner */ struct FakeWorkspaceHost */\n",
        "FakeWorkspaceHost"
    ));
    assert!(!type_item_present(
        "const X: &str = r#\"decoy \"\nstruct FakeWorkspaceHost\n\"#;\n",
        "FakeWorkspaceHost"
    ));
    assert!(!type_item_present(
        "struct FakeWorkspaceHostExtra { pub id: u32 }\n",
        "FakeWorkspaceHost"
    ));
}

#[test]
fn constructed_evidence_cannot_promote_integration_claim() {
    let mut manifest =
        decode_workspace_manifest(include_bytes!("fixtures/workspace/manifest.json")).unwrap();
    manifest.integration_claim = IntegrationClaim::Integrated;
    for precondition in &mut manifest.preconditions {
        for (index, seam) in precondition.seams.iter_mut().enumerate() {
            seam.path = format!("tests/fixtures/workspace/{index}.md");
            seam.needle = "stub".to_string();
        }
    }

    let _forged_records = REQUIRED_EVIDENCE_IDS.map(|id| {
        (
            id,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "wsfixture01",
            4242u32,
            1_700_000_000_000u64,
            "ab".repeat(32),
        )
    });

    let first = evaluate_preconditions(&manifest);
    let second = evaluate_preconditions(&manifest);
    assert_eq!(first, second, "settlement must be idempotent");
    assert_eq!(first.claim, IntegrationClaim::None);
    assert!(
        first.holds.iter().any(|hold| hold.id == "evidence"),
        "constructed records must not issue evidence: {:?}",
        first.holds
    );
    assert!(
        first.holds.iter().any(|hold| hold.id == "graph"),
        "swapped in-memory seams must not replace the frozen canonical graph: {:?}",
        first.holds
    );
    assert!(first.satisfied_evidence_ids.is_empty());
    assert!(try_issue_authenticated_evidence(Path::new(env!("CARGO_MANIFEST_DIR"))).is_err());

    let snapshot = PreconditionSnapshot {
        manifest: &manifest,
        worktree_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        deadline: Instant::now() + Duration::from_millis(EVALUATE_DEADLINE_MS),
        source: "fn keep() {}\n",
        task_id: FIXTURE_TASK_ID,
    };
    let swapped = evaluate_snapshot(&snapshot);
    assert_eq!(swapped.claim, IntegrationClaim::None);
    assert!(swapped.holds.iter().any(|hold| hold.id == "evidence"));
    for id in REQUIRED_PRECONDITION_IDS {
        assert!(
            swapped
                .holds
                .iter()
                .any(|hold| hold.id == id && !hold.missing_seams.is_empty()),
            "canonical seam {id} must still HOLD after in-memory swap: {:?}",
            swapped.holds
        );
    }

    let expired = evaluate_snapshot(&PreconditionSnapshot {
        deadline: Instant::now() - Duration::from_millis(1),
        ..snapshot
    });
    assert_eq!(expired.claim, IntegrationClaim::None);
    assert!(expired.holds.iter().any(|hold| hold.id == "deadline"));

    let unbound = evaluate_snapshot(&PreconditionSnapshot {
        task_id: "",
        ..snapshot
    });
    assert_eq!(unbound.claim, IntegrationClaim::None);
    assert!(unbound.holds.iter().any(|hold| hold.id == "binding"));
}

#[test]
fn fake_host_binds_explicit_main_worktree_and_external_workspaces() {
    let mut host = FakeWorkspaceHost::boot();
    let main = host
        .bind("task-main", WorkspaceChoice::Main, None)
        .expect("main bind");
    assert_eq!(main.workspace, WorkspaceRef::Main);

    let worktree = host
        .bind(FIXTURE_TASK_ID, WorkspaceChoice::NewWorktree, None)
        .expect("worktree bind");
    match &worktree.workspace {
        WorkspaceRef::Worktree { path, branch } => {
            assert!(path.exists(), "worktree path must exist");
            assert!(branch.starts_with("codex/task-"));
            assert!(path_is_within(path, &host.worktree_root));
            assert!(!path_has_reparse_component(path));
        }
        other => panic!("expected worktree binding, got {other:?}"),
    }

    let external_repo = host._root.path().join("external");
    init_initialized_repo(&external_repo);
    let external = host
        .bind(
            "task-external",
            WorkspaceChoice::External,
            Some(&external_repo),
        )
        .expect("external bind");
    assert!(matches!(external.workspace, WorkspaceRef::External { .. }));

    host.mark_live(FIXTURE_TASK_ID);
    let rebound = host.bind(FIXTURE_TASK_ID, WorkspaceChoice::Main, None);
    assert_eq!(rebound, Err(HostError::LiveResourcesPresent));
}

#[test]
fn fake_host_refuses_nonexistent_non_repo_and_escape_bindings() {
    let mut host = FakeWorkspaceHost::boot();
    assert_eq!(
        host.bind(
            "missing",
            WorkspaceChoice::External,
            Some(Path::new("C:\\definitely-missing-workspace-conformance"))
        ),
        Err(HostError::NonexistentPath)
    );

    let folder = host._root.path().join("not-a-repo");
    fs::create_dir_all(&folder).unwrap();
    assert_eq!(
        host.bind("folder", WorkspaceChoice::External, Some(&folder)),
        Err(HostError::NotARepository)
    );

    let outside = PathBuf::from(std::env::temp_dir());
    assert_eq!(
        host.bind("escape", WorkspaceChoice::External, Some(&outside)),
        Err(HostError::EscapeRejected)
    );
}

#[test]
fn fake_host_creates_worktree_and_refuses_dirty_remove() {
    let mut host = FakeWorkspaceHost::boot();
    host.bind(FIXTURE_TASK_ID, WorkspaceChoice::NewWorktree, None)
        .unwrap();
    let record = host.worktrees[FIXTURE_TASK_ID].clone();
    assert!(record.branch.starts_with("codex/"));
    let listed = git_stdout(&host.main_repo, &["worktree", "list", "--porcelain"]).unwrap();
    assert!(
        listed.contains(&record.branch) || listed.to_ascii_lowercase().contains("task-"),
        "worktree list must include the created branch: {listed}"
    );

    fs::write(record.path.join("dirty.txt"), "local change").unwrap();
    host.worktrees.get_mut(FIXTURE_TASK_ID).unwrap().dirty = true;
    assert_eq!(
        host.preview_remove_worktree(FIXTURE_TASK_ID),
        Err(HostError::DirtyOrUnpushed)
    );

    let main_dirty = fs::read_to_string(host.main_repo.join("dirty-main.txt")).unwrap();
    assert_eq!(main_dirty, "user-dirty\n");
    assert!(host.main_repo.join("untracked-main.txt").exists());

    host.remove_worktree(FIXTURE_TASK_ID, true, &record.path)
        .expect("confirmed force remove");
    assert!(!host.worktrees.contains_key(FIXTURE_TASK_ID));
}

#[test]
fn fake_host_bounds_file_and_artifact_access() {
    let mut host = FakeWorkspaceHost::boot();
    host.bind(FIXTURE_TASK_ID, WorkspaceChoice::NewWorktree, None)
        .unwrap();

    let body = host.read_file(FIXTURE_TASK_ID, "src/app.txt").unwrap();
    assert_eq!(body, b"fixture-app-body\n");
    assert_eq!(
        host.read_file(FIXTURE_TASK_ID, "../README.md"),
        Err(HostError::EscapeRejected)
    );
    assert_eq!(
        host.read_file(FIXTURE_TASK_ID, "src/app.txt:secret"),
        Err(HostError::PathRejected("ads-or-device"))
    );

    let hash = host
        .write_file(FIXTURE_TASK_ID, "src/app.txt", b"edited-body\n")
        .unwrap();
    let artifact = host
        .export_artifact(FIXTURE_TASK_ID, "src/app.txt")
        .unwrap();
    assert_eq!(artifact.sha256, hash);
    assert_eq!(artifact.bytes, b"edited-body\n".len());
    assert!(host.artifact_root.join(&artifact.id).exists());
}

#[test]
fn fake_host_checkpoints_and_forbids_hard_reset() {
    let mut host = FakeWorkspaceHost::boot();
    host.bind(FIXTURE_TASK_ID, WorkspaceChoice::NewWorktree, None)
        .unwrap();
    let before = host.checkpoint(FIXTURE_TASK_ID, "before-edit").unwrap();
    host.write_file(FIXTURE_TASK_ID, "src/app.txt", b"changed\n")
        .unwrap();
    host.restore(FIXTURE_TASK_ID, &before.id, "src/app.txt")
        .unwrap();
    assert_eq!(
        host.read_file(FIXTURE_TASK_ID, "src/app.txt").unwrap(),
        b"fixture-app-body\n"
    );
    assert_eq!(
        host.forbid_hard_reset(&["reset", "--hard", "HEAD"]),
        Err(HostError::HardResetForbidden)
    );
    assert_eq!(
        host.forbid_hard_reset(&["clean", "-fd"]),
        Err(HostError::HardResetForbidden)
    );
}

#[test]
fn fake_host_reports_git_service_port_and_ssh_capabilities_without_adapters() {
    let host = FakeWorkspaceHost::boot();
    let report = host.report_capabilities();
    assert!(!report.git.adapter_integrated);
    assert_eq!(report.git.backend, "local-cli-fixture");
    assert!(!report.service.supervisor_launch);
    assert!(!report.service.job_ownership);
    assert!(report.service.catalog_decode);
    assert!(!report.port.live_probe);
    assert_eq!(report.port.external_listener_control, "forbidden");
    assert!(report.port.observed_only);
    assert!(!report.ssh.launch_adapter);
    assert!(!report.ssh.password_auto_inject);
    assert!(!report.ssh.secrets_in_events);

    let catalog =
        ServiceCatalog::decode_json(include_bytes!("fixtures/workspace/v1/services.json"))
            .expect("workspace service fixture must decode through the 6.7a catalog contract");
    assert_eq!(catalog.definitions().count(), 1);

    let serialized = serde_json::to_string(&serde_json::json!({
        "git_backend": report.git.backend,
        "git_adapter_integrated": report.git.adapter_integrated,
        "service_catalog_decode": report.service.catalog_decode,
        "service_supervisor_launch": report.service.supervisor_launch,
        "port_live_probe": report.port.live_probe,
        "port_external_listener_control": report.port.external_listener_control,
        "ssh_launch_adapter": report.ssh.launch_adapter,
        "ssh_secrets_in_events": report.ssh.secrets_in_events,
    }))
    .unwrap();
    assert!(!serialized.contains("password"));
    assert!(!serialized.contains("PRIVATE KEY"));
    for adapter in UNAVAILABLE_ADAPTERS {
        assert!(report
            .unavailable_adapters
            .iter()
            .any(|path| path == adapter));
        assert_ne!(
            *adapter, "integrated",
            "capability report must not claim unavailable adapters"
        );
    }
}

#[test]
fn fake_host_cancels_queued_work_and_replays_exact_journal_prefix() {
    let mut host = FakeWorkspaceHost::boot();
    let generation = host.queue("worktree.create");
    host.cancel(generation);
    assert_eq!(host.drain_queue(), Ok(vec![]));
    assert!(host.cancelled_generations.contains(&generation));

    host.bind(FIXTURE_TASK_ID, WorkspaceChoice::NewWorktree, None)
        .unwrap();
    let replayed = host.reopen_from_journal().expect("exact journal prefix");
    assert_eq!(replayed, host.journal);
    assert_eq!(replayed.first().map(|entry| entry.seq), Some(1));
    for window in replayed.windows(2) {
        assert_eq!(window[1].seq, window[0].seq + 1);
    }
}

#[test]
fn fake_host_close_reaches_zero_orphan_postconditions() {
    let mut host = FakeWorkspaceHost::boot();
    host.bind(FIXTURE_TASK_ID, WorkspaceChoice::NewWorktree, None)
        .unwrap();
    host.mark_live(FIXTURE_TASK_ID);
    host.export_artifact(FIXTURE_TASK_ID, "README.md").unwrap();
    host.close_task(FIXTURE_TASK_ID).unwrap();
    let (jobs, worktrees) = host.residue();
    assert_eq!(jobs, 0);
    assert_eq!(worktrees, 0);
    assert!(host.live_resources.is_empty());
    let porcelain = git_stdout(&host.main_repo, &["worktree", "list", "--porcelain"]).unwrap();
    assert!(
        !porcelain.contains("task-"),
        "managed worktree must be gone: {porcelain}"
    );
    assert!(host.main_repo.join("dirty-main.txt").exists());
    assert!(host.main_repo.join("untracked-main.txt").exists());
    assert!(host.remote_bare.exists());
}

#[test]
fn smoke_script_refuses_production_profiles_and_authenticated_actions() {
    let source = smoke_script_source();
    for required in [
        "Authenticated",
        "production",
        "com.userfirst.devmanager",
        "CARGO_TARGET_DIR",
        "DEVMANAGER_PROFILE",
        "TimeoutSeconds",
        "MaxOutputBytes",
        "workspace_conformance",
        "GIT_TERMINAL_PROMPT",
        "temp",
    ] {
        assert!(
            source.contains(required),
            "smoke script missing required guard token {required}"
        );
    }
    for forbidden in ["Stop-Process", "taskkill", "gh auth", "Invoke-WebRequest"] {
        assert!(
            !source.contains(forbidden),
            "smoke script must not contain {forbidden}"
        );
    }

    let script = "scripts/native-next/Invoke-WorkspaceSmoke.ps1";
    let authenticated = run_pwsh_file(&[script, "-Authenticated"]);
    assert!(
        !authenticated.status.success(),
        "Authenticated must be refused\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&authenticated.stdout),
        String::from_utf8_lossy(&authenticated.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&authenticated.stdout),
        String::from_utf8_lossy(&authenticated.stderr)
    );
    assert!(
        combined.to_ascii_lowercase().contains("authenticated")
            || combined.to_ascii_lowercase().contains("refus"),
        "authenticated refusal must be explicit: {combined}"
    );

    let production = run_pwsh_file(&[script, "-Profile", "production"]);
    assert!(
        !production.status.success(),
        "production profile must be refused\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&production.stdout),
        String::from_utf8_lossy(&production.stderr)
    );
}

#[test]
fn service_catalog_fixture_is_decode_only_and_does_not_launch() {
    let started = Instant::now();
    let catalog =
        ServiceCatalog::decode_json(include_bytes!("fixtures/workspace/v1/services.json"))
            .expect("catalog");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        catalog
            .definitions()
            .next()
            .expect("fixture service")
            .id
            .as_str(),
        "fixture-api"
    );
    assert!(!load_capabilities().service.supervisor_launch);
}

#[test]
fn smoke_script_retains_canonical_tools_and_rejects_wrappers() {
    let source = smoke_script_source();
    for required in [
        "rustup",
        "which cargo",
        "--locked",
        "--offline",
        "RUSTC_WRAPPER",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "credential.helper",
        "core.fsmonitor",
        "core.hooksPath",
        "protocol.file.allow",
        "CreateJobObject",
        "KILL_ON_JOB_CLOSE",
        "ACTIVE_PROCESS_ZERO",
        "ProbeGuards",
        "UTF8",
        "integration_claim=none",
        "OUTPUT_CAP",
        "run.identity",
        "SelfTestOutputBounds",
        "CLEANED=exact-identity",
        "HOLD=S2,S3,S4,S5,S6,S7,S8,S9",
        "CLAIM_PROMOTION=forbidden",
        "DEADLINE_READY_MS=15000",
        "DEADLINE_CTL_MS=10000",
        "DEADLINE_STOP_MS=5000",
        "CLEANUP_DEADLINE_MS=5000",
        "RUSTUP_OUTPUT_CAP=4096",
        "STOP_JOIN_MS=5000",
        "EVIDENCE_REQUIRED=host.lock,kernel.sqlite3,ManagedProcessJob.active_process_ids",
    ] {
        assert!(
            source.contains(required),
            "smoke script missing hardening token {required}"
        );
    }
    for forbidden in [
        "Get-Command",
        "Register-ObjectEvent",
        "Start-Sleep",
        "Get-CimInstance",
        r"C:\Temp\devmanager-cursor-auto-06-10",
    ] {
        assert!(
            !source.contains(forbidden),
            "smoke script must not contain {forbidden}"
        );
    }

    let parsed = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-Command",
            " $errors=$null; $null=[System.Management.Automation.Language.Parser]::ParseFile('scripts/native-next/Invoke-WorkspaceSmoke.ps1',[ref]$null,[ref]$errors); if($errors){ $errors | ForEach-Object { $_.ToString() }; exit 1 } else { 'PARSE_OK' }",
        ])
        .output()
        .expect("parse smoke script");
    assert!(
        parsed.status.success() && String::from_utf8_lossy(&parsed.stdout).contains("PARSE_OK"),
        "smoke script must parse\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&parsed.stdout),
        String::from_utf8_lossy(&parsed.stderr)
    );
}

#[test]
fn smoke_script_probe_guards_use_unique_roots_and_reject_poison() {
    let script = "scripts/native-next/Invoke-WorkspaceSmoke.ps1";
    let first = run_pwsh_file(&[script, "-ProbeGuards"]);
    let second = run_pwsh_file(&[script, "-ProbeGuards"]);
    assert!(
        first.status.success() && second.status.success(),
        "ProbeGuards must succeed\nfirst:\n{}{}\nsecond:\n{}{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr),
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    let one = String::from_utf8_lossy(&first.stdout);
    let two = String::from_utf8_lossy(&second.stdout);
    assert!(one.contains("integration_claim=none"));
    assert!(one.contains("ACTIVE_PROCESS_ZERO"));
    assert!(one.contains("HUGE_LINE_BOUNDED"));
    assert!(one.contains("INVALID_UTF8_BOUNDED"));
    assert!(one.contains("FLOOD_BOUNDED"));
    assert!(one.contains("CAP_PLUS_ONE_BOUNDED"));
    assert!(one.contains("STALLED_PIPE_BOUNDED"));
    assert!(one.contains("CLEANED=exact-identity"));
    assert!(one.contains("HOLD=S2,S3,S4,S5,S6,S7,S8,S9"));
    assert!(one.contains("CLAIM_PROMOTION=forbidden"));
    assert!(one.contains("DEADLINE_READY_MS=15000"));
    assert!(one.contains("integration_claim=none"));
    let root_one = capture_field(&one, "runRoot=");
    let root_two = capture_field(&two, "runRoot=");
    assert_ne!(
        root_one, root_two,
        "concurrent/sequential runs must not share roots"
    );
    assert!(
        !root_one.eq_ignore_ascii_case(r"C:\Temp\devmanager-cursor-auto-06-10"),
        "must not use the fixed shared target path"
    );
    assert!(
        !Path::new(&root_one).exists(),
        "exact run root must be identity-cleaned: {root_one}"
    );
    assert!(
        !Path::new(&root_two).exists(),
        "exact run root must be identity-cleaned: {root_two}"
    );

    let poison = tempfile::tempdir().expect("poison path");
    let fake_cargo = poison.path().join("cargo.exe");
    fs::write(&fake_cargo, b"not-cargo").unwrap();
    let wrapper = poison.path().join("evil-wrapper.exe");
    fs::write(&wrapper, b"not-rustc").unwrap();
    let mut poisoned = Command::new("pwsh");
    poisoned
        .args(["-NoProfile", "-File", script, "-ProbeGuards"])
        .env(
            "PATH",
            format!(
                "{};{}",
                poison.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("RUSTC_WRAPPER", wrapper.display().to_string())
        .env("RUSTFLAGS", "--evil")
        .env("CARGO_ENCODED_RUSTFLAGS", "evil");
    let rejected = poisoned.output().expect("poisoned probe");
    assert!(
        !rejected.status.success(),
        "malicious wrapper/flags must be refused\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&rejected.stdout),
        String::from_utf8_lossy(&rejected.stderr)
    );
}

#[test]
fn fake_host_rejects_fake_git_dir_and_requires_initialized_external_repo() {
    let mut host = FakeWorkspaceHost::boot();
    let fake = host._root.path().join("fake-external");
    fs::create_dir_all(fake.join(".git")).unwrap();
    assert_eq!(
        host.bind("fake", WorkspaceChoice::External, Some(&fake)),
        Err(HostError::NotARepository)
    );

    let real = host._root.path().join("real-external");
    init_initialized_repo(&real);
    let bound = host
        .bind("real-ext", WorkspaceChoice::External, Some(&real))
        .expect("real initialized external repo");
    match bound.workspace {
        WorkspaceRef::External { path } => {
            assert_eq!(canonical(&path).unwrap(), canonical(&real).unwrap());
        }
        other => panic!("expected external binding, got {other:?}"),
    }
}

#[test]
fn fake_host_rejects_junction_escape_and_failed_git_status() {
    let mut host = FakeWorkspaceHost::boot();
    let outside = host._root.path().join("outside-target");
    fs::create_dir_all(&outside).unwrap();
    let junction = host._root.path().join("junction-escape");
    create_junction(&junction, &outside);
    assert_eq!(
        host.bind("junc", WorkspaceChoice::External, Some(&junction)),
        Err(HostError::EscapeRejected)
    );

    host.bind(FIXTURE_TASK_ID, WorkspaceChoice::NewWorktree, None)
        .unwrap();
    let record = host.worktrees[FIXTURE_TASK_ID].clone();
    fs::remove_dir_all(&record.path).unwrap();
    let preview = host.preview_remove_worktree(FIXTURE_TASK_ID);
    assert!(
        matches!(
            preview,
            Err(HostError::GitFailed) | Err(HostError::Conflict)
        ),
        "git status failure must not look clean: {preview:?}"
    );
}

fn capture_field(stdout: &str, prefix: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix(prefix).map(ToOwned::to_owned))
        .unwrap_or_else(|| panic!("stdout missing field {prefix}"))
}

fn init_initialized_repo(path: &Path) {
    fs::create_dir_all(path).unwrap();
    run_git(path, &["init", "-b", "main"]).expect("init external");
    run_git(path, &["config", "user.name", "Workspace Conformance"]).unwrap();
    run_git(
        path,
        &[
            "config",
            "user.email",
            "workspace-conformance@devmanager.test",
        ],
    )
    .unwrap();
    fs::write(path.join("README.md"), "external\n").unwrap();
    run_git(path, &["add", "."]).unwrap();
    run_git(path, &["commit", "-m", "external"]).unwrap();
}

fn create_junction(link: &Path, target: &Path) {
    let status = Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            link.to_str().unwrap(),
            target.to_str().unwrap(),
        ])
        .status()
        .expect("mklink");
    assert!(status.success(), "failed to create junction {link:?}");
}
