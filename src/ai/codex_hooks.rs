//! Codex hooks tap: relays Codex lifecycle/tool/approval hook payloads into
//! DevManager's semantic journal without altering how the Codex TUI runs.
//! Mirrors the Claude hooks relay (`claude_hooks.rs`).

use crate::ai::claude_hooks::is_valid_loopback_relay_url_for;
use std::io::Read;
use std::process::ExitCode;
use std::time::Duration;

pub const CODEX_HOOK_RELAY_PATH: &str = "/internal/codex-hook";
pub const MAX_CODEX_HOOK_BODY_BYTES: usize = 256 * 1024;

pub fn run_codex_hook_relay(endpoint: &str, nonce: &str, body: &[u8]) -> ExitCode {
    if body.len() > MAX_CODEX_HOOK_BODY_BYTES
        || !is_valid_loopback_relay_url_for(endpoint, CODEX_HOOK_RELAY_PATH)
    {
        return ExitCode::SUCCESS;
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(125)))
        .max_redirects(0)
        .proxy(None)
        .build()
        .into();
    let _ = agent
        .post(endpoint)
        .header("content-type", "application/json")
        .header("x-devmanager-codex-nonce", nonce)
        .send(body);
    ExitCode::SUCCESS
}

pub fn run_codex_hook_relay_subcommand<R: Read>(args: &[String], reader: R) -> Option<ExitCode> {
    if args.first().map(String::as_str) != Some("codex-hook-relay") {
        return None;
    }
    let [_, url_flag, endpoint, nonce_flag, nonce] = args else {
        return Some(ExitCode::SUCCESS);
    };
    if url_flag != "--url" || nonce_flag != "--nonce" {
        return Some(ExitCode::SUCCESS);
    }
    let mut body = Vec::new();
    let mut limited = reader.take((MAX_CODEX_HOOK_BODY_BYTES + 1) as u64);
    if limited.read_to_end(&mut body).is_err() || body.len() > MAX_CODEX_HOOK_BODY_BYTES {
        return Some(ExitCode::SUCCESS);
    }
    Some(run_codex_hook_relay(endpoint, nonce, &body))
}

#[cfg(test)]
mod relay_cli_tests {
    use super::*;

    #[test]
    fn ignores_other_subcommands() {
        assert!(run_codex_hook_relay_subcommand(&["claude-hook-relay".into()], &b""[..]).is_none());
        assert!(run_codex_hook_relay_subcommand(&[], &b""[..]).is_none());
    }

    #[test]
    fn malformed_args_exit_success_without_posting() {
        let args = vec!["codex-hook-relay".to_string(), "--url".to_string()];
        assert!(run_codex_hook_relay_subcommand(&args, &b"{}"[..]).is_some());
    }

    #[test]
    fn codex_relay_path_is_validated() {
        assert!(is_valid_loopback_relay_url_for(
            "http://127.0.0.1:1234/internal/codex-hook",
            CODEX_HOOK_RELAY_PATH
        ));
        assert!(!is_valid_loopback_relay_url_for(
            "http://127.0.0.1:1234/internal/claude-hook",
            CODEX_HOOK_RELAY_PATH
        ));
    }
}
