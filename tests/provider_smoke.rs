use devmanager::domain::ProviderSessionId;
use devmanager::providers::adapter::{
    LaunchProviderRequest, ProviderAdapter, ProviderProbeKind, ProviderProbeRequest,
};
use devmanager::providers::capabilities::{ProviderCapability, ProviderExecutable, ProviderKind};
use devmanager::providers::conformance::{
    decide_strict_resume, reject_smoke_sensitive_payload, ResumeOutcome, StrictResumeFailure,
};
use devmanager::providers::cursor::CursorAdapter;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const SMOKE_MATRIX: &str = include_str!("fixtures/providers/smoke/matrix.json");
const SMOKE_REDACTION: &str = include_str!("fixtures/providers/smoke/redaction.json");
const CURSOR_CONTRACT: &str =
    include_str!("fixtures/providers/cursor/phase4_11_smoke_contract.json");
const CLAUDE_HELP: &str = include_str!("fixtures/providers/claude/help.txt");
const CLAUDE_AUTH_SUBSCRIPTION: &str =
    include_str!("fixtures/providers/claude/auth_status_authenticated.txt");
const CLAUDE_AUTH_API_KEY: &str = include_str!("fixtures/providers/claude/auth_api_key.txt");
const CLAUDE_AUTH_AMBIGUOUS: &str = include_str!("fixtures/providers/claude/auth_ambiguous.txt");
const CLAUDE_RESUME_NOT_FOUND: &str =
    include_str!("fixtures/providers/claude/resume_not_found.txt");
const CODEX_RESUME_HELP: &str = include_str!("fixtures/providers/codex/resume_help.txt");
const CODEX_RESUME_LAST_ONLY: &str =
    include_str!("fixtures/providers/codex/resume_help_last_only.txt");
const CODEX_LOGIN_SUBSCRIPTION: &str =
    include_str!("fixtures/providers/codex/login_status_chatgpt_subscription.txt");
const CODEX_LOGIN_API_KEY: &str = include_str!("fixtures/providers/codex/login_status_api_key.txt");
const CODEX_LOGIN_AMBIGUOUS: &str =
    include_str!("fixtures/providers/codex/login_status_logged_in_only.txt");
const RESUME_FAILURE_FIXTURE: &str =
    include_str!("fixtures/conformance/providers/v1/resume_failure.json");

fn matrix() -> Value {
    serde_json::from_str(SMOKE_MATRIX).expect("committed smoke matrix")
}

fn redaction() -> Value {
    serde_json::from_str(SMOKE_REDACTION).expect("committed smoke redaction fixture")
}

fn provider<'a>(document: &'a Value, id: &str) -> &'a Value {
    document["providers"]
        .as_array()
        .expect("providers array")
        .iter()
        .find(|provider| provider["id"] == id)
        .unwrap_or_else(|| panic!("missing provider {id}"))
}

fn probe_kind(name: &str) -> ProviderProbeKind {
    match name {
        "version" => ProviderProbeKind::Version,
        "help" => ProviderProbeKind::Help,
        "auth_status" => ProviderProbeKind::AuthStatus,
        "login_status" => ProviderProbeKind::LoginStatus,
        "resume_help" => ProviderProbeKind::ResumeHelp,
        other => panic!("unknown probe kind {other}"),
    }
}

fn classify_claude_auth(raw: &str) -> &'static str {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return "unknown";
    };
    match (
        value.get("loggedIn").and_then(Value::as_bool),
        value.get("authMethod").and_then(Value::as_str),
    ) {
        (Some(false), _) => "auth_required",
        (Some(true), Some("claude.ai")) => "authenticated_subscription",
        _ => "unknown",
    }
}

fn classify_codex_auth(raw: &str) -> &'static str {
    let lines: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.iter().any(|line| {
        matches!(
            *line,
            "not authenticated" | "not logged in" | "auth required" | "login required"
        )
    }) {
        return "auth_required";
    }
    let method = lines.iter().any(|line| *line == "Logged in using ChatGPT");
    let plan = lines
        .iter()
        .any(|line| *line == "ChatGPT Plus subscription");
    if method && plan {
        "authenticated_subscription"
    } else {
        "unknown"
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn invoke_provider_smoke(args: &[&str]) -> (i32, String, String) {
    let script = repo_root().join("scripts/native-next/Invoke-ProviderSmoke.ps1");
    let mut command = Command::new("pwsh");
    command
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-File")
        .arg(&script)
        .args(args)
        .current_dir(repo_root())
        .env_remove("DEVMANAGER_PROFILE")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("CURSOR_API_KEY")
        .env_remove("CLAUDE_API_KEY");
    let output = command
        .output()
        .expect("pwsh must invoke Invoke-ProviderSmoke.ps1");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let code = output.status.code().unwrap_or(1);
    (code, stdout, stderr)
}

fn parse_result_json(stdout: &str) -> Value {
    let json_line = stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('{') && line.contains("schemaVersion"))
        .unwrap_or_else(|| panic!("smoke stdout missing JSON result: {stdout}"));
    serde_json::from_str(json_line).unwrap_or_else(|error| {
        panic!("smoke JSON parse failed ({error}): {json_line}");
    })
}

#[test]
fn smoke_matrix_probe_argv_matches_public_probe_kind_contract() {
    let document = matrix();
    assert_eq!(document["schemaVersion"], 1);
    assert_eq!(document["mode"], "fixture");
    assert_eq!(document["launchesProvider"], false);
    assert_eq!(document["residueCount"], 0);

    let executable = ProviderExecutable::new(PathBuf::from("C:/bin/claude"), [0x11; 32]).unwrap();
    let handle = executable.open_for_launch().unwrap();

    for provider in document["providers"].as_array().unwrap() {
        for probe in provider["probes"].as_array().unwrap() {
            let kind = probe_kind(probe["kind"].as_str().unwrap());
            let expected: Vec<&str> = probe["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect();
            assert_eq!(kind.arguments(), expected.as_slice());
            let request = ProviderProbeRequest::new(handle.clone(), kind).unwrap();
            assert_eq!(request.arguments(), expected.as_slice());
            assert!(request.uses_null_stdin());
            assert!(!request.uses_shell());
            assert!(request.strips_api_key_environment());
            assert!(request.timeout() <= Duration::from_secs(30));
        }
    }
}

#[test]
fn smoke_matrix_forbids_prompt_and_session_creating_tokens() {
    let document = matrix();
    let prohibited: Vec<&str> = document["sharedProhibitedTokens"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert!(prohibited.contains(&"exec"));
    assert!(prohibited.contains(&"--print"));
    assert!(prohibited.contains(&"-p"));
    assert!(prohibited.contains(&"create-chat"));
    assert!(prohibited.contains(&"--continue"));
    assert!(prohibited.contains(&"--last"));

    for provider in document["providers"].as_array().unwrap() {
        for probe in provider["probes"].as_array().unwrap() {
            for argument in probe["argv"].as_array().unwrap() {
                let token = argument.as_str().unwrap();
                assert!(
                    !prohibited
                        .iter()
                        .any(|forbidden| token.eq_ignore_ascii_case(forbidden)),
                    "{} probe {:?} contains prohibited token {token}",
                    provider["id"],
                    probe["kind"]
                );
            }
        }
        if let Some(command) = provider["exactResume"]["command"].as_array() {
            for argument in command {
                let token = argument.as_str().unwrap();
                if token == "<id>" {
                    continue;
                }
                assert!(
                    !prohibited
                        .iter()
                        .any(|forbidden| token.eq_ignore_ascii_case(forbidden)),
                    "{} exact-resume command contains prohibited token {token}",
                    provider["id"]
                );
            }
        }
    }
}

#[test]
fn exact_resume_failure_stays_visible_and_never_falls_back_to_fresh() {
    let document = matrix();
    let claude = provider(&document, "claude_code");
    assert_eq!(claude["exactResume"]["command"][0], "--resume");
    assert!(CLAUDE_HELP
        .split_whitespace()
        .any(|token| token == "--resume"));
    assert!(CLAUDE_RESUME_NOT_FOUND.contains("No conversation found"));
    assert!(!CLAUDE_RESUME_NOT_FOUND.contains("--continue"));
    assert!(!CLAUDE_RESUME_NOT_FOUND.contains("--last"));

    let codex = provider(&document, "codex");
    assert_eq!(codex["exactResume"]["command"][0], "resume");
    assert!(CODEX_RESUME_HELP
        .lines()
        .any(|line| line.split_whitespace().eq([
            "Usage:",
            "codex",
            "resume",
            "[OPTIONS]",
            "[SESSION_ID]",
            "[PROMPT]"
        ])));
    assert!(!CODEX_RESUME_LAST_ONLY.contains("[SESSION_ID]"));
    assert!(CODEX_RESUME_LAST_ONLY.contains("--last"));

    let cursor = provider(&document, "cursor");
    assert_eq!(cursor["exactResume"]["supported"], false);
    assert_eq!(
        cursor["exactResume"]["error"],
        "UnsupportedCapability(ExactResume)"
    );

    let resume_failure: Value =
        serde_json::from_str(RESUME_FAILURE_FIXTURE).expect("resume failure fixture");
    match decide_strict_resume(&resume_failure).unwrap() {
        ResumeOutcome::FailedVisible { reason } => {
            assert_eq!(reason, StrictResumeFailure::NotFound);
        }
        other => panic!("exact-resume failure must stay visible, got {other:?}"),
    }

    let adapter = CursorAdapter::new();
    let executable =
        ProviderExecutable::new(PathBuf::from("C:/bin/cursor-agent"), [0x44; 32]).unwrap();
    let session = ProviderSessionId::new("chat-id-must-not-be-inferred").unwrap();
    assert!(matches!(
        adapter.build_launch(LaunchProviderRequest::new(
            executable.open_for_launch().unwrap(),
            None,
            Some(session),
        )),
        Err(devmanager::providers::ProviderError::UnsupportedCapability(
            ProviderCapability::ExactResume
        ))
    ));
}

#[test]
fn api_key_and_ambiguous_auth_never_become_subscription() {
    assert_eq!(
        classify_claude_auth(CLAUDE_AUTH_SUBSCRIPTION),
        "authenticated_subscription"
    );
    assert_eq!(classify_claude_auth(CLAUDE_AUTH_API_KEY), "unknown");
    assert_eq!(classify_claude_auth(CLAUDE_AUTH_AMBIGUOUS), "unknown");
    assert_eq!(
        classify_codex_auth(CODEX_LOGIN_SUBSCRIPTION),
        "authenticated_subscription"
    );
    assert_eq!(classify_codex_auth(CODEX_LOGIN_API_KEY), "unknown");
    assert_eq!(classify_codex_auth(CODEX_LOGIN_AMBIGUOUS), "unknown");

    let cursor: Value = serde_json::from_str(CURSOR_CONTRACT).unwrap();
    assert_eq!(cursor["claims_auth"], false);
    assert_eq!(cursor["capabilities"]["auth_state"], "Unknown");
    assert_eq!(cursor["mode"], "fixture_only");
    assert_eq!(cursor["launches_provider"], false);
}

#[test]
fn quota_is_unsupported_without_an_official_probe_contract() {
    let document = matrix();
    for id in ["claude_code", "codex", "cursor"] {
        let provider = provider(&document, id);
        assert_eq!(provider["quota"]["officialProbe"], false);
        assert_eq!(provider["quota"]["record"], "unsupported");
    }
    assert_eq!(
        provider(&document, "cursor")["quota"]["observe"],
        "unsupported"
    );
    assert_eq!(
        provider(&document, "codex")["quota"]["observe"],
        "unsupported"
    );

    let cursor: Value = serde_json::from_str(CURSOR_CONTRACT).unwrap();
    assert_eq!(cursor["capabilities"]["observe_quota"], "Unsupported");
}

#[test]
fn smoke_output_redacts_sensitive_fields_and_keeps_required_shape() {
    let document = redaction();
    let sensitive = &document["sensitive"];
    let forbidden = json!({
        "prompt": sensitive["prompt"],
        "response": sensitive["response"],
        "credential": sensitive["credential"],
        "absolute_user_path": sensitive["absolute_user_path"],
        "session_id": sensitive["session_id"]
    });
    assert!(reject_smoke_sensitive_payload(&forbidden).is_err());

    let safe = json!({
        "schemaVersion": 1,
        "mode": "fixture",
        "providers": ["claude_code", "codex", "cursor"],
        "checks": [{"id": "matrix.loaded", "status": "pass"}],
        "launchedProviders": false,
        "residueCount": 0,
        "disposition": "pass"
    });
    reject_smoke_sensitive_payload(&safe).expect("bounded smoke result shape must be publishable");
}

#[test]
fn fixture_mode_script_does_not_launch_and_leaves_zero_residue() {
    let (code, stdout, stderr) = invoke_provider_smoke(&[]);
    assert!(
        stderr.trim().is_empty() || !stderr.to_ascii_lowercase().contains("secret"),
        "stderr must not leak secrets: {stderr}"
    );
    let result = parse_result_json(&stdout);
    assert_eq!(code, 0, "fixture mode must pass: {stdout}");
    assert_eq!(result["schemaVersion"], 1);
    assert_eq!(result["mode"], "fixture");
    assert_eq!(result["launchedProviders"], false);
    assert_eq!(result["residueCount"], 0);
    assert_eq!(result["disposition"], "pass");
    reject_smoke_sensitive_payload(&result).expect("fixture smoke JSON must stay redacted");

    let redaction = redaction();
    let rendered = result.to_string();
    for key in [
        "prompt",
        "response",
        "credential",
        "absolute_user_path",
        "session_id",
    ] {
        let leaked = redaction["sensitive"][key].as_str().unwrap();
        assert!(!rendered.contains(leaked), "fixture result leaked {key}");
    }
    assert!(!rendered.contains("C:/Users/"));
    assert!(!rendered.contains("C:\\Users\\"));
    assert!(!rendered.contains("sk-ant-"));
    assert!(!rendered.contains("sess_0123456789abcdef"));

    let checks = result["checks"].as_array().expect("checks");
    assert!(checks.iter().all(|check| check["status"] == "pass"));
    assert_eq!(result["providers"].as_array().unwrap().len(), 3);
}

#[test]
fn fixture_mode_rejects_production_and_relative_profile_roots() {
    let production = Path::new(r"C:\Users\Public\AppData\Roaming\com.userfirst.devmanager");
    let (code, stdout, _) = invoke_provider_smoke(&[
        "-IsolatedProfile",
        production.to_str().unwrap(),
        "-IAcknowledgeIsolatedNonproductionProfile",
    ]);
    let result = parse_result_json(&stdout);
    assert_eq!(code, 1);
    assert_eq!(result["disposition"], "rejected");
    assert_eq!(result["launchedProviders"], false);
    assert!(
        result["rejection"] == "production-profile"
            || result["checks"]
                .as_array()
                .map(|checks| checks.iter().any(|check| {
                    check["id"] == "profile.production" && check["status"] == "rejected"
                }))
                .unwrap_or(false)
    );

    let (relative_code, relative_stdout, _) =
        invoke_provider_smoke(&["-IsolatedProfile", "relative-profile"]);
    let relative = parse_result_json(&relative_stdout);
    assert_eq!(relative_code, 1);
    assert_eq!(relative["disposition"], "rejected");
    assert_eq!(relative["launchedProviders"], false);
}

#[test]
fn script_source_keeps_live_probe_policy_closed() {
    let script =
        fs::read_to_string(repo_root().join("scripts/native-next/Invoke-ProviderSmoke.ps1"))
            .expect("smoke script");
    for token in [
        "create-chat",
        "--print",
        "--continue",
        "--last",
        "ANTHROPIC_API_KEY",
        "launchedProviders",
        "residueCount",
        "schemaVersion",
    ] {
        assert!(script.contains(token), "script must mention {token}");
    }
    assert!(!script.contains("fixture-only smoke runtime is unimplemented"));
}
