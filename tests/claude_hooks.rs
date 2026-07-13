use devmanager::ai::claude_hooks::{
    is_valid_loopback_relay_url, prepare_claude_launch_overlay, quote_shell_argument,
    run_hook_relay, run_hook_relay_subcommand, ClaudeHookRegistry, ClaudeHookRelayListener,
    ClaudeReducer, ClaudeReducerLimits, ClaudeRegistryEvent, ClaudeRegistryLimits, ClaudeShellKind,
    RelayIngestStatus, MAX_CLAUDE_HOOK_BODY_BYTES,
};
use devmanager::remote::presentation::{
    SemanticAdapterHealth, SemanticEventKind, SemanticRetention, SemanticToolState,
    StableSessionKey,
};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn fixture(name: &str) -> &'static [u8] {
    match name {
        "session_start" => include_bytes!("fixtures/claude_hooks/session_start.json"),
        "prompt" => include_bytes!("fixtures/claude_hooks/prompt.json"),
        "message" => include_bytes!("fixtures/claude_hooks/message_display.json"),
        "pre_tool_a" => include_bytes!("fixtures/claude_hooks/pre_tool_a.json"),
        "pre_tool_b" => include_bytes!("fixtures/claude_hooks/pre_tool_b.json"),
        "post_tool_b" => include_bytes!("fixtures/claude_hooks/post_tool_b.json"),
        "post_tool_failure" => {
            include_bytes!("fixtures/claude_hooks/post_tool_failure.json")
        }
        "pre_tool_failed" => include_bytes!("fixtures/claude_hooks/pre_tool_failed.json"),
        "permission" => include_bytes!("fixtures/claude_hooks/permission_request.json"),
        "notification" => include_bytes!("fixtures/claude_hooks/notification.json"),
        "elicitation" => include_bytes!("fixtures/claude_hooks/elicitation.json"),
        "stop" => include_bytes!("fixtures/claude_hooks/stop.json"),
        "stop_failure" => include_bytes!("fixtures/claude_hooks/stop_failure.json"),
        "session_end" => include_bytes!("fixtures/claude_hooks/session_end.json"),
        _ => panic!("unknown fixture {name}"),
    }
}

fn reducer() -> ClaudeReducer {
    ClaudeReducer::new(
        StableSessionKey::from_tab("claude-tab"),
        ClaudeReducerLimits {
            max_tool_records: 8,
            max_deduplication_keys: 32,
        },
    )
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devmanager-claude-hook-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn parallel_tools_reduce_by_tool_use_id_and_replay_is_deduplicated() {
    let mut reducer = reducer();

    reducer.apply_json(fixture("pre_tool_a"), 10);
    reducer.apply_json(fixture("pre_tool_b"), 11);
    let completed = reducer.apply_json(fixture("post_tool_b"), 12);
    let replay = reducer.apply_json(fixture("post_tool_b"), 13);

    assert_eq!(
        reducer.tool("tool-a").expect("tool a").state,
        SemanticToolState::Running
    );
    assert_eq!(
        reducer.tool("tool-b").expect("tool b").state,
        SemanticToolState::Completed
    );
    assert_eq!(completed.drafts.len(), 1);
    assert!(replay.drafts.is_empty());

    reducer.apply_json(fixture("post_tool_failure"), 14);
    reducer.apply_json(fixture("pre_tool_failed"), 15);
    assert_eq!(
        reducer.tool("tool-failed").expect("failed tool").state,
        SemanticToolState::Failed,
        "a late PreToolUse must not downgrade an observed failure"
    );
}

#[test]
fn known_lifecycle_fixtures_normalize_without_leaking_provider_metadata() {
    let mut reducer = reducer();
    let mut drafts = Vec::new();
    for (index, name) in [
        "session_start",
        "prompt",
        "message",
        "permission",
        "notification",
        "elicitation",
        "stop",
        "stop_failure",
        "session_end",
    ]
    .into_iter()
    .enumerate()
    {
        drafts.extend(reducer.apply_json(fixture(name), index as u64).drafts);
    }

    assert!(drafts
        .iter()
        .any(|draft| matches!(&draft.kind, SemanticEventKind::UserMessage { text } if text == "Please inspect the reducer")));
    assert!(drafts.iter().any(|draft| matches!(
        &draft.kind,
        SemanticEventKind::AssistantMessage { text, streaming: true, .. }
            if text == "I am checking it now."
    )));
    assert!(drafts.iter().any(|draft| {
        matches!(
            &draft.kind,
            SemanticEventKind::AssistantMessage {
                streaming: true,
                ..
            }
        ) && draft.retention == SemanticRetention::Verbose
    }));
    assert!(drafts.iter().any(|draft| matches!(
        &draft.kind,
        SemanticEventKind::Question { prompt, .. }
            if prompt == "Claude requests permission to use Bash"
    )));
    assert!(drafts.iter().any(|draft| matches!(
        &draft.kind,
        SemanticEventKind::Question { prompt, .. }
            if prompt == "Choose a deployment region"
    )));
    assert!(drafts.iter().any(|draft| matches!(
        &draft.kind,
        SemanticEventKind::Error { message } if message.contains("rate_limit")
    )));
    assert!(drafts.iter().any(|draft| matches!(
        &draft.kind,
        SemanticEventKind::Status { state, detail }
            if state == "ended" && detail.as_deref() == Some("prompt_input_exit")
    )));

    let rendered = format!("{drafts:?}");
    for forbidden in [
        "SECRET_TRANSCRIPT_PATH_SENTINEL",
        "SECRET_CWD_SENTINEL",
        "SECRET_COMMAND_SENTINEL",
        "SECRET_PERMISSION_SENTINEL",
        "SECRET_ELICITATION_RESPONSE_SENTINEL",
    ] {
        assert!(!rendered.contains(forbidden), "leaked {forbidden}");
    }
}

#[test]
fn malformed_and_unknown_hooks_are_fail_open_and_bounded() {
    let mut reducer = ClaudeReducer::new(
        StableSessionKey::from_tab("claude-tab"),
        ClaudeReducerLimits {
            max_tool_records: 2,
            max_deduplication_keys: 3,
        },
    );

    let malformed = reducer.apply_json(br#"{"hook_event_name":"PreToolUse""#, 1);
    let unknown = reducer.apply_json(br#"{"hook_event_name":"FutureHook","extra":true}"#, 2);
    for index in 0..8 {
        let event = format!(
            r#"{{"hook_event_name":"PreToolUse","tool_use_id":"tool-{index}","tool_name":"Read","tool_input":{{"file_path":"SECRET"}}}}"#
        );
        reducer.apply_json(event.as_bytes(), index + 10);
    }

    assert!(malformed.drafts.is_empty());
    assert!(malformed.degraded);
    assert!(unknown.drafts.is_empty());
    assert!(!unknown.degraded);
    assert!(reducer.tool_record_count() <= 2);
    assert!(reducer.deduplication_key_count() <= 3);
}

#[test]
fn huge_unicode_provider_text_stays_below_the_semantic_event_limit() {
    let mut reducer = reducer();
    let message = "🦀".repeat(20_000);
    let body = serde_json::json!({
        "hook_event_name": "MessageDisplay",
        "message_id": "large-message",
        "message": message,
    });

    let outcome = reducer.apply_json(&serde_json::to_vec(&body).unwrap(), 1);

    assert_eq!(outcome.drafts.len(), 1);
    let encoded = serde_json::to_vec(&outcome.drafts[0].kind).unwrap();
    assert!(encoded.len() <= 64 * 1024, "{} bytes", encoded.len());
    assert!(matches!(
        &outcome.drafts[0].kind,
        SemanticEventKind::AssistantMessage { text, .. }
            if text.ends_with("[truncated by DevManager]")
    ));
}

#[test]
fn heavily_escaped_provider_text_stays_below_the_semantic_event_limit() {
    let mut reducer = reducer();
    let body = serde_json::json!({
        "hook_event_name": "MessageDisplay",
        "message_id": "escaped-message",
        "message": "\0".repeat(20_000),
    });

    let outcome = reducer.apply_json(&serde_json::to_vec(&body).unwrap(), 1);

    assert_eq!(outcome.drafts.len(), 1);
    let encoded = serde_json::to_vec(&outcome.drafts[0].kind).unwrap();
    assert!(encoded.len() <= 64 * 1024, "{} bytes", encoded.len());
    assert!(matches!(
        &outcome.drafts[0].kind,
        SemanticEventKind::AssistantMessage { text, .. }
            if text.ends_with("[truncated by DevManager]")
    ));
}

#[test]
fn relay_url_validation_rejects_ambiguous_or_non_loopback_authorities() {
    assert!(is_valid_loopback_relay_url(
        "http://127.0.0.1:43873/internal/claude-hook"
    ));
    assert!(is_valid_loopback_relay_url(
        "http://[::1]:43873/internal/claude-hook"
    ));
    for invalid in [
        "https://127.0.0.1:43873/internal/claude-hook",
        "http://localhost:43873/internal/claude-hook",
        "http://127.0.0.2:43873/internal/claude-hook",
        "http://127.0.0.1/internal/claude-hook",
        "http://127.0.0.1:notaport/internal/claude-hook",
        "http://127.0.0.1:80@evil.example/internal/claude-hook",
        "http://evil.example@127.0.0.1:43873/internal/claude-hook",
        "http://127.0.0.1:43873/other",
        "http://127.0.0.1:43873/internal/claude-hook?nonce=secret",
        "http://127.0.0.1:43873/internal/claude-hook#fragment",
    ] {
        assert!(!is_valid_loopback_relay_url(invalid), "accepted {invalid}");
    }
}

#[test]
fn default_registry_ttl_preserves_idle_all_day_sessions() {
    assert!(ClaudeRegistryLimits::default().registration_ttl >= Duration::from_secs(24 * 60 * 60));
}

#[test]
fn registry_authenticates_loopback_nonce_caps_bodies_and_expires_entries() {
    let now = Instant::now();
    let registry = ClaudeHookRegistry::with_limits(ClaudeRegistryLimits {
        max_registrations: 2,
        max_body_bytes: 1024,
        registration_ttl: Duration::from_secs(30),
        reducer: ClaudeReducerLimits::default(),
    });
    let registration = registry
        .register_at(StableSessionKey::from_tab("claude-tab"), now)
        .expect("registration");
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5000);
    let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 5000);

    assert_eq!(
        registry.ingest_at(remote, &registration.nonce, fixture("prompt"), now, 1_000),
        RelayIngestStatus::Rejected
    );
    assert_eq!(
        registry.ingest_at(loopback, "wrong-nonce", fixture("prompt"), now, 1_000),
        RelayIngestStatus::Rejected
    );
    assert_eq!(
        registry.ingest_at(loopback, &registration.nonce, &vec![b'x'; 1025], now, 1_000,),
        RelayIngestStatus::BodyTooLarge
    );
    assert!(matches!(
        registry.ingest_at(
            loopback,
            &registration.nonce,
            fixture("prompt"),
            now + Duration::from_secs(20),
            1_020,
        ),
        RelayIngestStatus::Accepted(_)
    ));
    assert!(matches!(
        registry.ingest_at(
            loopback,
            &registration.nonce,
            fixture("notification"),
            now + Duration::from_secs(40),
            1_040,
        ),
        RelayIngestStatus::Accepted(_)
    ));
    assert_eq!(
        registry.ingest_at(
            loopback,
            &registration.nonce,
            fixture("prompt"),
            now + Duration::from_secs(71),
            1_071,
        ),
        RelayIngestStatus::Expired
    );
    assert_eq!(registry.registration_count(), 0);
}

#[test]
fn registry_uses_injected_unix_epoch_for_semantic_drafts() {
    let now = Instant::now();
    let registry = ClaudeHookRegistry::default();
    let registration = registry
        .register_at(StableSessionKey::from_tab("claude-tab"), now)
        .unwrap();
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5000);

    let RelayIngestStatus::Accepted(outcome) = registry.ingest_at(
        loopback,
        &registration.nonce,
        fixture("prompt"),
        now,
        1_799_999_999_123,
    ) else {
        panic!("hook was not accepted");
    };

    assert_eq!(outcome.drafts[0].occurred_at_epoch_ms, 1_799_999_999_123);
}

#[test]
fn recognized_commands_generate_exec_form_hooks_and_cleanup_with_registration() {
    let temp = TempDir::new("recognized");
    let registry = ClaudeHookRegistry::default();
    let executable = Path::new("C:/Program Files/DevManager/devmanager.exe");
    let overlay = prepare_claude_launch_overlay(
        &registry,
        StableSessionKey::from_tab("claude-tab"),
        "npx -y @anthropic-ai/claude-code@2.1.207 --model sonnet",
        ClaudeShellKind::Posix,
        executable,
        "http://127.0.0.1:43873/internal/claude-hook",
        temp.path(),
        Instant::now(),
    );

    assert_eq!(overlay.health, SemanticAdapterHealth::Healthy);
    let registration = overlay.registration.as_ref().expect("registration");
    let settings_path = overlay.settings_path.as_ref().expect("settings path");
    assert!(settings_path.is_file());
    assert!(overlay.startup_command.contains("--settings '"));

    let settings: serde_json::Value =
        serde_json::from_slice(&fs::read(settings_path).unwrap()).unwrap();
    let hooks = settings["hooks"].as_object().expect("hooks object");
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PermissionRequest",
        "PermissionDenied",
        "PostToolUse",
        "PostToolUseFailure",
        "PostToolBatch",
        "Notification",
        "MessageDisplay",
        "Elicitation",
        "ElicitationResult",
        "SubagentStart",
        "SubagentStop",
        "TaskCreated",
        "TaskCompleted",
        "PreCompact",
        "PostCompact",
        "Stop",
        "StopFailure",
        "SessionEnd",
    ] {
        assert!(hooks.contains_key(event), "missing {event}");
        let command = &hooks[event][0]["hooks"][0];
        assert_eq!(command["type"], "command");
        assert_eq!(command["command"], executable.display().to_string());
        assert_eq!(command["args"][0], "claude-hook-relay");
        assert_eq!(command["args"][1], "--url");
        assert_eq!(command["args"][2], overlay.endpoint);
        assert_eq!(command["args"][3], "--nonce");
        assert_eq!(command["args"][4], registration.nonce);
        assert_eq!(command["async"], true);
    }
    let serialized = serde_json::to_string(&settings).unwrap();
    assert!(!serialized.contains("permissionDecision"));
    assert!(!serialized.contains("SECRET"));

    assert!(registry.unregister(&registration.nonce).is_some());
    assert!(!settings_path.exists());
}

#[test]
fn registry_capacity_eviction_removes_ephemeral_settings_and_reports_the_nonce() {
    let temp = TempDir::new("eviction");
    let registry = Arc::new(ClaudeHookRegistry::with_limits(ClaudeRegistryLimits {
        max_registrations: 1,
        ..ClaudeRegistryLimits::default()
    }));
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    registry.set_event_handler(Some(Arc::new(move |event| {
        observed.lock().unwrap().push(event);
    })));
    let overlay = prepare_claude_launch_overlay(
        &registry,
        StableSessionKey::from_tab("first-tab"),
        "claude",
        ClaudeShellKind::PowerShell,
        Path::new("C:/DevManager/devmanager.exe"),
        "http://127.0.0.1:43873/internal/claude-hook",
        temp.path(),
        Instant::now(),
    );
    let first = overlay.registration.expect("first registration");
    let settings_path = overlay.settings_path.expect("first settings path");

    registry
        .register_at(StableSessionKey::from_tab("second-tab"), Instant::now())
        .unwrap();

    assert!(!settings_path.exists());
    assert!(events.lock().unwrap().iter().any(|event| matches!(
        event,
        ClaudeRegistryEvent::RegistrationDropped { nonce, .. } if nonce == &first.nonce
    )));
}

#[test]
fn existing_settings_are_merged_without_overwriting_the_user_file() {
    let temp = TempDir::new("merge");
    let user_settings = temp.path().join("user settings.json");
    let original = serde_json::json!({
        "model": "sonnet",
        "permissions": { "allow": ["Read"] },
        "hooks": {
            "Stop": [{
                "hooks": [{ "type": "command", "command": "user-stop-hook" }]
            }]
        }
    });
    fs::write(
        &user_settings,
        serde_json::to_vec_pretty(&original).unwrap(),
    )
    .unwrap();
    let startup = format!(
        "claude --settings \"{}\" --verbose",
        user_settings.display()
    );
    let registry = ClaudeHookRegistry::default();

    let overlay = prepare_claude_launch_overlay(
        &registry,
        StableSessionKey::from_tab("claude-tab"),
        &startup,
        ClaudeShellKind::PowerShell,
        Path::new("C:/DevManager/devmanager.exe"),
        "http://127.0.0.1:43873/internal/claude-hook",
        temp.path(),
        Instant::now(),
    );

    assert_eq!(overlay.health, SemanticAdapterHealth::Healthy);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&user_settings).unwrap()).unwrap(),
        original,
        "the user's settings file must remain untouched"
    );
    let merged: serde_json::Value = serde_json::from_slice(
        &fs::read(overlay.settings_path.as_ref().expect("overlay path")).unwrap(),
    )
    .unwrap();
    assert_eq!(merged["model"], "sonnet");
    assert_eq!(merged["permissions"]["allow"][0], "Read");
    assert_eq!(
        merged["hooks"]["Stop"][0]["hooks"][0]["command"],
        "user-stop-hook"
    );
    assert!(merged["hooks"]["Stop"].as_array().unwrap().len() >= 2);
    assert!(overlay.startup_command.contains(" --verbose"));
    assert!(!overlay
        .startup_command
        .contains(&user_settings.display().to_string()));
}

#[test]
fn wrappers_operators_and_invalid_settings_fall_back_unchanged() {
    let temp = TempDir::new("fallback");
    let registry = ClaudeHookRegistry::default();
    for command in [
        "my-wrapper claude --model sonnet",
        "env FOO=bar claude",
        "claude && echo unsafe",
        "claude # settings would be commented out",
        "claude\nother-command",
        "powershell -Command claude",
        "npx some-other-package",
        "claude --settings",
    ] {
        let overlay = prepare_claude_launch_overlay(
            &registry,
            StableSessionKey::from_tab("claude-tab"),
            command,
            ClaudeShellKind::PowerShell,
            Path::new("C:/DevManager/devmanager.exe"),
            "http://127.0.0.1:43873/internal/claude-hook",
            temp.path(),
            Instant::now(),
        );
        assert_eq!(overlay.startup_command, command);
        assert_eq!(overlay.health, SemanticAdapterHealth::Degraded);
        assert!(overlay.registration.is_none());
        assert!(overlay.settings_path.is_none());
    }
    let posix_substitution = prepare_claude_launch_overlay(
        &registry,
        StableSessionKey::from_tab("claude-tab"),
        "claude `other-command`",
        ClaudeShellKind::Posix,
        Path::new("C:/DevManager/devmanager.exe"),
        "http://127.0.0.1:43873/internal/claude-hook",
        temp.path(),
        Instant::now(),
    );
    assert_eq!(posix_substitution.startup_command, "claude `other-command`");
    assert_eq!(posix_substitution.health, SemanticAdapterHealth::Degraded);
    assert!(posix_substitution.registration.is_none());
    assert_eq!(registry.registration_count(), 0);
}

#[test]
fn shell_quoting_is_platform_specific_and_lossless() {
    let value = "C:/Temp/O'Brien/settings file.json";
    assert_eq!(
        quote_shell_argument(value, ClaudeShellKind::Posix),
        "'C:/Temp/O'\\''Brien/settings file.json'"
    );
    assert_eq!(
        quote_shell_argument(value, ClaudeShellKind::PowerShell),
        "'C:/Temp/O''Brien/settings file.json'"
    );
    assert_eq!(
        quote_shell_argument(value, ClaudeShellKind::Cmd),
        "\"C:/Temp/O'Brien/settings file.json\""
    );
}

#[test]
fn cmd_settings_paths_with_expansion_markers_fall_back_unchanged() {
    let temp = TempDir::new("cmd-unsafe-path");
    let unsafe_root = temp.path().join("settings-%TEMP%-!");
    let registry = ClaudeHookRegistry::default();
    let startup = "claude --model sonnet";

    let overlay = prepare_claude_launch_overlay(
        &registry,
        StableSessionKey::from_tab("claude-tab"),
        startup,
        ClaudeShellKind::Cmd,
        Path::new("C:/DevManager/devmanager.exe"),
        "http://127.0.0.1:43873/internal/claude-hook",
        &unsafe_root,
        Instant::now(),
    );

    assert_eq!(overlay.startup_command, startup);
    assert_eq!(overlay.health, SemanticAdapterHealth::Degraded);
    assert!(overlay.registration.is_none());
    assert!(overlay.settings_path.is_none());
    assert_eq!(registry.registration_count(), 0);
    assert!(!unsafe_root.exists());
}

#[test]
fn loopback_listener_authenticates_caps_and_dispatches_after_unlock() {
    let registry = Arc::new(ClaudeHookRegistry::with_limits(ClaudeRegistryLimits {
        max_registrations: 4,
        max_body_bytes: 1024,
        registration_ttl: Duration::from_secs(30),
        reducer: ClaudeReducerLimits::default(),
    }));
    let events = Arc::new(Mutex::new(Vec::<ClaudeRegistryEvent>::new()));
    let callback_registry = registry.clone();
    let callback_events = events.clone();
    registry.set_event_handler(Some(Arc::new(move |event| {
        // This would deadlock if registry callbacks ran under the registry lock.
        let _ = callback_registry.registration_count();
        callback_events.lock().unwrap().push(event);
    })));
    let listener = ClaudeHookRelayListener::start(registry.clone()).expect("listener");
    let registration = registry
        .register_at(StableSessionKey::from_tab("claude-tab"), Instant::now())
        .unwrap();

    let accepted = ureq::post(listener.endpoint())
        .header("x-devmanager-claude-nonce", &registration.nonce)
        .header("content-type", "application/json")
        .send(fixture("prompt"));
    assert_eq!(accepted.unwrap().status().as_u16(), 204);
    wait_for(Duration::from_secs(2), || {
        !events.lock().unwrap().is_empty()
    });
    assert!(events.lock().unwrap().iter().any(|event| matches!(
        event,
        ClaudeRegistryEvent::Semantic(draft)
            if draft.occurred_at_epoch_ms > 1_700_000_000_000
    )));

    let rejected = ureq::post(listener.endpoint())
        .header("x-devmanager-claude-nonce", "wrong")
        .send(fixture("prompt"));
    assert!(matches!(rejected, Err(ureq::Error::StatusCode(401))));
    let oversized = ureq::post(listener.endpoint())
        .header("x-devmanager-claude-nonce", &registration.nonce)
        .send(vec![b'x'; 1025]);
    assert!(matches!(oversized, Err(ureq::Error::StatusCode(413))));

    let malformed = ureq::post(listener.endpoint())
        .header("x-devmanager-claude-nonce", &registration.nonce)
        .send(br#"{"hook_event_name":"PreToolUse""#);
    assert_eq!(malformed.unwrap().status().as_u16(), 204);
    wait_for(Duration::from_secs(2), || {
        events.lock().unwrap().iter().any(|event| {
            matches!(
                event,
                ClaudeRegistryEvent::AdapterHealth {
                    health: SemanticAdapterHealth::Degraded,
                    ..
                }
            )
        })
    });

    let ended = ureq::post(listener.endpoint())
        .header("x-devmanager-claude-nonce", &registration.nonce)
        .send(fixture("session_end"));
    assert_eq!(ended.unwrap().status().as_u16(), 204);
    wait_for(Duration::from_secs(2), || {
        registry.registration_count() == 0
    });
}

fn wait_for(timeout: Duration, predicate: impl Fn() -> bool) {
    let started = Instant::now();
    while !predicate() {
        assert!(started.elapsed() < timeout, "condition timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn relay_failure_is_always_fail_open() {
    let code = run_hook_relay(
        "http://127.0.0.1:9/internal/claude-hook",
        "not-a-real-nonce",
        br#"{}"#,
    );
    assert_eq!(code, ExitCode::SUCCESS);
}

#[test]
fn relay_subcommand_is_exact_bounded_and_never_launches_the_gui_path() {
    assert!(run_hook_relay_subcommand(&["ordinary".to_string()], &b""[..]).is_none());
    assert_eq!(
        run_hook_relay_subcommand(
            &[
                "claude-hook-relay".to_string(),
                "--url".to_string(),
                "http://127.0.0.1:9/internal/claude-hook".to_string(),
                "--nonce".to_string(),
                "nonce".to_string(),
            ],
            &br#"{}"#[..],
        ),
        Some(ExitCode::SUCCESS)
    );
    assert_eq!(
        run_hook_relay_subcommand(
            &["claude-hook-relay".to_string(), "--url".to_string()],
            &br#"{}"#[..],
        ),
        Some(ExitCode::SUCCESS),
        "a malformed relay invocation must still exit instead of opening DevManager"
    );
    assert_eq!(
        run_hook_relay_subcommand(
            &[
                "claude-hook-relay".to_string(),
                "--url".to_string(),
                "http://127.0.0.1:9/internal/claude-hook".to_string(),
                "--nonce".to_string(),
                "nonce".to_string(),
            ],
            vec![b'x'; MAX_CLAUDE_HOOK_BODY_BYTES + 1].as_slice(),
        ),
        Some(ExitCode::SUCCESS)
    );
}
