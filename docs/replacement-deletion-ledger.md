# Replacement Deletion Ledger

This ledger is the dependency-safe Phase 11.1 source-level rip-and-replace
cutover contract. The fenced JSON document is canonical and is consumed by
`scripts/native-next/Invoke-CutoverAudit.ps1` and `tests/cutover_contract.rs`.
This is a deletion cutover, not a permanent dual-UI or compatibility layer:
one GPUI desktop entry (`src/main.rs`) plus one durable host
(`src/bin/devmanager-host.rs`) are the only product processes after approval.
Final semantics: sole `devmanager` GPUI entry; durable `devmanager-host`
attach/detach/full quit; no `devmanager-next` binary; no backward-compatibility
shell; no Codex rollout JSONL tailer as a conversation identity/transcript
source; no `zz-archive` desktop tree; production profile only on the signed
release path; atomic two-binary updater/package identity; and explicit manual
publication approval.

Phase 11 deletes old runtime paths once owning lanes produce evidence; it does
not permanently `HOLD` them as a compatibility layer. A still-present delete
row stays `HOLD` only as a deferred deletion until its prerequisite
phase/gates and evidence are actually green. Declared tests, E2E proof, and
production impact are included only when an exact tracked binding exists.
Missing or unverified fields stay HOLD blockers. No row may claim assumed,
partial, or compile-only evidence. `HOLD` on a present path is a deferred
delete, never a keep. `READY` means the replacement is approved for the
deletion or handoff slice. `DELETED` is valid only for `cutoverAction: delete`
rows whose legacy path and the full deletion set are absent; an already-absent
path must not remain `HOLD`. Handoff rows never become `DELETED`; they become
`READY` when required binaries, packager tokens, and update files exist and
their prerequisites are green. This foundation makes no deletion, install,
publish, or user-data claim. The product desktop entry is already
`run_native_shell` from `src/main.rs`; `src/app/mod.rs` remains a deferred
legacy source compiled by `pub mod app` because unowned tests still
`include_str!` that file (named on the `legacy-app-runtime` HOLD). Host
`serve_request` stays an integration-test compatibility seam because
`tests/ipc_protocol.rs` links the library without `--cfg test`; production
host serving uses `serve_duplex`.

The audit uses `git ls-files` as the tracked universe. Candidate mode uses a
bounded internal fixed-string scanner; fixture mode may use a PATH-confined `rg`
shim after fixture authority. Only this ledger is an allowed self-reference.
Intentional historical references are allowlisted in
`referencePolicy.intentionalHistoricalReferencePaths`. An exact `session.json`
file is path-only evidence and is never opened or hashed by the audit. The
audit remaps `%APPDATA%` beneath the worktree evidence root, does not set
`DEVMANAGER_PROFILE`, and never observes or mutates the installed DevManager
process or production `config.json` / `remote.json`.

```json cutover-contract
{
  "schemaVersion": 1,
  "contractId": "phase-11.1-cutover",
  "ledgerPath": "docs/replacement-deletion-ledger.md",
  "statusModel": [
    "HOLD",
    "READY",
    "DELETED"
  ],
  "referencePolicy": {
    "trackedUniverse": "git-ls-files",
    "referenceScanner": "rg --fixed-strings --line-number",
    "allowedLedgerSelfReferences": [
      "docs/replacement-deletion-ledger.md"
    ],
    "protectedFileBasenames": [
      "session.json"
    ],
    "maxMatchesPerRow": 20,
    "intentionalHistoricalReferencePaths": [
      "docs/replacement-deletion-ledger.md",
      "docs/superpowers/plans/2026-08-04-phase-11-cutover-release.md",
      "tests/cutover_contract.rs",
      "scripts/native-next/Invoke-CutoverAudit.ps1",
      "scripts/native-next/NativeNext.ps1",
      "scripts/native-next/Isolation.ps1"
    ]
  },
  "productEntrypoints": {
    "desktopClient": {
      "id": "gpui-desktop-client",
      "path": "src/main.rs",
      "symbol": "main",
      "role": "gpui-client",
      "forbiddenDispatch": [
        "devmanager::app::run",
        "app::run"
      ]
    },
    "durableHost": {
      "id": "durable-host",
      "path": "src/bin/devmanager-host.rs",
      "symbol": "main",
      "role": "durable-host",
      "lifecycle": [
        "attach",
        "detach",
        "full-quit"
      ]
    }
  },
  "compatibilityPolicy": {
    "permanentDualUi": false,
    "backwardCompatibilityMode": false,
    "forbiddenRuntimeSwitches": [
      "new_ui",
      "use_old",
      "old_runtime",
      "compatibility_mode"
    ],
    "scanPaths": [
      "src",
      "Cargo.toml"
    ]
  },
  "deletionPolicy": {
    "permanentHoldForbidden": true,
    "action": "delete",
    "readyRequiresOwningLaneEvidence": true,
    "deletedRequiresPathAndDeletionSetAbsent": true
  },
  "deferredDeletionPaths": [
    "src/app/mod.rs",
    "src/services/process_manager.rs",
    "src/terminal/session.rs",
    "src/ai/codex_cli.rs",
    "src/browser/pane.rs",
    "src/state/",
    "src/models/config.rs",
    "src/persistence/mod.rs",
    "src/services/session_manager.rs",
    "src/remote/mod.rs",
    "src/remote/web/bridge.rs",
    "src/remote/web/lease.rs",
    "src/sidebar/",
    "src/workspace/editor_ui.rs",
    "src/ai/claude_hooks.rs",
    "src/workspace/mod.rs",
    "tests/legacy_loader.rs",
    "tests/fixtures/legacy-session.json"
  ],
  "hostCompatibility": {
    "serveRequest": {
      "path": "src/host/ipc.rs",
      "symbol": "HostConnection::serve_request",
      "kind": "integration-test-seam",
      "cfgTestGated": false,
      "reason": "tests/ipc_protocol.rs links the library without cfg(test); production host uses serve_duplex",
      "productionSymbol": "HostConnection::serve_duplex",
      "productionCaller": "src/bin/devmanager-host.rs"
    }
  },
  "packagingHandoff": {
    "requiredBinaries": [
      "devmanager.exe",
      "devmanager-host.exe"
    ],
    "atomicTwoBinaryIdentity": true,
    "packagerManifest": "Cargo.toml",
    "requiredManifestTokens": [
      "devmanager-host"
    ],
    "requiredFiles": [
      "src/updater/handoff.rs",
      "src/host/update.rs",
      "tests/update_contract.rs",
      "tests/package_contract.rs"
    ],
    "forbidInstallOrPublish": true
  },
  "profileIsolation": {
    "productionRootName": "com.userfirst.devmanager",
    "evidenceRoot": ".devmanager-next/evidence",
    "forbidSettingDevmanagerProfile": true,
    "remapAppData": true,
    "productionProfileOnlyInSignedRelease": true
  },
  "installedAppPolicy": {
    "touchInstalledApp": false,
    "hashProductionFiles": false,
    "openSessionJson": false,
    "installPublishDeleteUserData": false
  },
  "publicationPolicy": {
    "requireExplicitManualApproval": true,
    "forbidAutomatedPublish": true
  },
  "prerequisiteNodes": [
    {
      "id": "phase-01-domain-store",
      "kind": "phase",
      "status": "HOLD",
      "dependsOn": [],
      "evidence": [
        ".devmanager-next/evidence/phase-01/verification.json"
      ]
    },
    {
      "id": "phase-02-host-ipc",
      "kind": "phase",
      "status": "HOLD",
      "dependsOn": [
        "phase-01-domain-store"
      ],
      "evidence": [
        ".devmanager-next/evidence/phase-02/verification.json"
      ]
    },
    {
      "id": "phase-03-zero-orphan",
      "kind": "phase",
      "status": "HOLD",
      "dependsOn": [
        "phase-02-host-ipc"
      ],
      "evidence": [
        ".devmanager-next/evidence/phase-03/verification.json"
      ]
    },
    {
      "id": "phase-04-provider-conformance",
      "kind": "phase",
      "status": "HOLD",
      "dependsOn": [
        "phase-03-zero-orphan"
      ],
      "evidence": [
        ".devmanager-next/evidence/phase-04/verification.json"
      ]
    },
    {
      "id": "phase-05-task-cockpit",
      "kind": "phase",
      "status": "HOLD",
      "dependsOn": [
        "phase-01-domain-store"
      ],
      "evidence": [
        ".devmanager-next/evidence/phase-05/verification.json"
      ]
    },
    {
      "id": "phase-06-workspace-config",
      "kind": "phase",
      "status": "HOLD",
      "dependsOn": [
        "phase-01-domain-store"
      ],
      "evidence": [
        ".devmanager-next/evidence/phase-06/verification.json"
      ]
    },
    {
      "id": "phase-07-prompt-library",
      "kind": "phase",
      "status": "HOLD",
      "dependsOn": [
        "phase-05-task-cockpit"
      ],
      "evidence": [
        ".devmanager-next/evidence/phase-07/verification.json"
      ]
    },
    {
      "id": "phase-08-browser-conformance",
      "kind": "phase",
      "status": "HOLD",
      "dependsOn": [
        "phase-03-zero-orphan"
      ],
      "evidence": [
        ".devmanager-next/evidence/phase-08/verification.json"
      ]
    },
    {
      "id": "phase-09-connect-realtime",
      "kind": "phase",
      "status": "HOLD",
      "dependsOn": [
        "phase-02-host-ipc"
      ],
      "evidence": [
        ".devmanager-next/evidence/phase-09/verification.json"
      ]
    },
    {
      "id": "phase-10-organization",
      "kind": "phase",
      "status": "HOLD",
      "dependsOn": [
        "phase-07-prompt-library",
        "phase-09-connect-realtime"
      ],
      "evidence": [
        ".devmanager-next/evidence/phase-10/verification.json"
      ]
    },
    {
      "id": "gate-phase11-approval",
      "kind": "gate",
      "status": "HOLD",
      "dependsOn": [
        "phase-04-provider-conformance",
        "phase-05-task-cockpit",
        "phase-06-workspace-config",
        "phase-07-prompt-library",
        "phase-08-browser-conformance",
        "phase-09-connect-realtime",
        "phase-10-organization"
      ],
      "evidence": [
        ".devmanager-next/evidence/phase-11/approval.json"
      ]
    },
    {
      "id": "gate-release-candidate",
      "kind": "gate",
      "status": "HOLD",
      "dependsOn": [
        "gate-phase11-approval"
      ],
      "evidence": [
        ".devmanager-next/evidence/phase-11/release-candidate.json"
      ]
    }
  ],
  "forbiddenEntrypoints": [
    {
      "id": "legacy-devmanager-next",
      "path": "src/bin/devmanager-next.rs",
      "tokens": [
        "devmanager-next",
        "devmanager-next.exe"
      ]
    }
  ],
  "rows": [
    {
      "id": "legacy-app-runtime",
      "area": "desktop-entry-runtime",
      "legacy": {
        "path": "src/app/mod.rs",
        "symbols": [
          "devmanager::app::run",
          "AppState",
          "mod app"
        ],
        "tokens": [
          "crate::app"
        ]
      },
      "replacementOwner": {
        "path": "src/main.rs",
        "symbol": "main"
      },
      "prerequisites": [
        "phase-05-task-cockpit",
        "phase-06-workspace-config",
        "phase-10-organization"
      ],
      "evidence": {
        "commands": [
          "cargo test --test cutover_contract parity_ -- --nocapture",
          "pwsh scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-10 -Recipe phase-10-contract"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-10/verification.json"
        ]
      },
      "deletionSet": [
        "src/app/mod.rs"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "HOLD: src/app/mod.rs, src/app/chrome.rs, src/app/process_monitor.rs, and src/lib.rs `pub mod app` remain because unowned tests still compile-time include that source (`include_str!(\"../src/app/mod.rs\")` in tests/browser_pane.rs, tests/config_service.rs, tests/browser_secret_prompt.rs, tests/browser_workflow_*.rs, tests/diagnostics_lifecycle.rs, tests/terminal_pending_annotations.rs, tests/browser_replay_repair.rs, tests/browser_host.rs, tests/browser_attachment_lifecycle.rs). Native entry is src/main.rs `run_native_shell` and does not call app::run. Deleting the tree without those unowned test edits would break the test compile contract."
    },
    {
      "id": "legacy-next-entrypoint",
      "area": "desktop-entry-identity",
      "legacy": {
        "path": "src/bin/devmanager-next.rs",
        "symbols": [
          "main",
          "devmanager-next"
        ],
        "tokens": [
          "devmanager-next",
          "devmanager-next.exe"
        ]
      },
      "replacementOwner": {
        "path": "src/main.rs",
        "symbol": "main"
      },
      "prerequisites": [
        "phase-05-task-cockpit",
        "gate-phase11-approval"
      ],
      "evidence": {
        "commands": [
          "rg -n -F -e devmanager-next -e devmanager-next.exe --glob !docs/replacement-deletion-ledger.md .",
          "cargo test --test cutover_contract entry_ -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-05/verification.json",
          ".devmanager-next/evidence/phase-11/approval.json"
        ]
      },
      "deletionSet": [
        "src/bin/devmanager-next.rs"
      ],
      "cutoverAction": "delete",
      "status": "DELETED",
      "approvalRequired": true,
      "approvalRequirement": "The development entry identity is already absent; remaining historical references are not a product binary"
    },
    {
      "id": "legacy-process-manager",
      "area": "process-ownership",
      "legacy": {
        "path": "src/services/process_manager.rs",
        "symbols": [
          "ProcessManager",
          "process_manager",
          "set_active_session"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "src/process/mod.rs",
        "symbol": "ProcessRegistry"
      },
      "prerequisites": [
        "phase-03-zero-orphan",
        "phase-04-provider-conformance",
        "phase-08-browser-conformance"
      ],
      "evidence": {
        "commands": [
          "cargo test --test process_supervisor -- --nocapture",
          "pwsh scripts/native-next/Invoke-PhaseGate.ps1 -Phase phase-03 -Recipe phase-03-process-supervisor"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-03/verification.json",
          ".devmanager-next/evidence/phase-08/verification.json"
        ]
      },
      "deletionSet": [
        "src/services/process_manager.rs"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Process and provider/browser zero-orphan evidence plus explicit cutover approval"
    },
    {
      "id": "legacy-terminal-session",
      "area": "terminal-ownership",
      "legacy": {
        "path": "src/terminal/session.rs",
        "symbols": [
          "TerminalSession",
          "TerminalSessionView",
          "terminal::session"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "src/host/mod.rs",
        "symbol": "HostRuntime"
      },
      "prerequisites": [
        "phase-02-host-ipc",
        "phase-03-zero-orphan"
      ],
      "evidence": {
        "commands": [
          "cargo test --test host_lifecycle -- --nocapture",
          "cargo test --test process_supervisor -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-02/verification.json",
          ".devmanager-next/evidence/phase-03/verification.json"
        ]
      },
      "deletionSet": [
        "src/terminal/session.rs"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Host ownership, attach/detach, and zero-orphan gates must be approved"
    },
    {
      "id": "legacy-codex-cli",
      "area": "provider-launch",
      "legacy": {
        "path": "src/ai/codex_cli.rs",
        "symbols": [
          "CodexConfigOverride",
          "codex_cli"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "src/domain/agent.rs",
        "symbol": "AgentIdentity"
      },
      "prerequisites": [
        "phase-04-provider-conformance"
      ],
      "evidence": {
        "commands": [
          "cargo test --test browser_provider -- --nocapture",
          "cargo test --test claude_hooks -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-04/verification.json"
        ]
      },
      "deletionSet": [
        "src/ai/codex_cli.rs"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Provider-native identity and launch conformance must be green before deletion"
    },
    {
      "id": "legacy-codex-rollout",
      "area": "provider-conversation-identity",
      "legacy": {
        "path": "src/ai/codex_rollout.rs",
        "symbols": [
          "CodexRolloutReducer",
          "CodexRolloutTailer",
          "codex_rollout"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "src/domain/agent.rs",
        "symbol": "ProviderSessionIdentity"
      },
      "prerequisites": [
        "phase-04-provider-conformance",
        "gate-phase11-approval"
      ],
      "evidence": {
        "commands": [
          "cargo test --test claude_hooks -- --nocapture",
          "cargo test --test cutover_contract parity_provider_identity -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-04/verification.json",
          ".devmanager-next/evidence/phase-11/approval.json"
        ]
      },
      "deletionSet": [
        "src/ai/codex_rollout.rs"
      ],
      "cutoverAction": "delete",
      "status": "DELETED",
      "approvalRequired": true,
      "approvalRequirement": "The Codex JSONL tailer is absent. Provider conversation identity remains correlated current-generation SessionStart hook data via bind_runtime_provider_session_id; no cwd/timestamp/transcript inference replacement was added."
    },
    {
      "id": "legacy-browser-pane",
      "area": "browser-ownership",
      "legacy": {
        "path": "src/browser/pane.rs",
        "symbols": [
          "BrowserPaneContext",
          "BrowserPaneTransient",
          "BrowserPaneModel",
          "BrowserPaneActions",
          "render_browser_pane"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "src/browser/host/mod.rs",
        "symbol": "BrowserHost"
      },
      "prerequisites": [
        "phase-05-task-cockpit",
        "phase-08-browser-conformance"
      ],
      "evidence": {
        "commands": [
          "cargo test --test browser_replay -- --nocapture",
          "cargo test --test browser_host -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-08/verification.json"
        ]
      },
      "deletionSet": [
        "src/browser/pane.rs"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Task-scoped browser ownership and replay evidence must be approved"
    },
    {
      "id": "legacy-state-read-model",
      "area": "task-and-runtime-state",
      "legacy": {
        "path": "src/state/",
        "symbols": [
          "RuntimeState",
          "AppState",
          "mod state"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "src/kernel/snapshot.rs",
        "symbol": "Snapshot"
      },
      "prerequisites": [
        "phase-01-domain-store",
        "phase-05-task-cockpit",
        "phase-06-workspace-config"
      ],
      "evidence": {
        "commands": [
          "cargo test --test task_state -- --nocapture",
          "cargo test --test kernel_store -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-01/verification.json",
          ".devmanager-next/evidence/phase-05/verification.json"
        ]
      },
      "deletionSet": [
        "src/state/"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Single-source Task/runtime projection evidence and explicit approval"
    },
    {
      "id": "legacy-config-session-model",
      "area": "configuration-and-session-model",
      "legacy": {
        "path": "src/models/config.rs",
        "symbols": [
          "SessionState",
          "SessionTab"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "src/config/mod.rs",
        "symbol": "ConfigFacade"
      },
      "prerequisites": [
        "phase-01-domain-store",
        "phase-06-workspace-config",
        "gate-phase11-approval"
      ],
      "evidence": {
        "commands": [
          "cargo test --test config_persistence -- --nocapture",
          "cargo test --test config_legacy -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-01/verification.json",
          ".devmanager-next/evidence/phase-11/approval.json"
        ]
      },
      "deletionSet": [
        "src/models/config.rs"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Supported config/remote preservation and intentional session omission require explicit approval"
    },
    {
      "id": "legacy-session-persistence",
      "area": "session-json-prohibition",
      "legacy": {
        "path": "src/persistence/mod.rs",
        "symbols": [
          "SESSION_FILE_NAME",
          "session_path",
          "session.json"
        ],
        "tokens": [
          "legacy-session"
        ]
      },
      "replacementOwner": {
        "path": "src/config/paths.rs",
        "symbol": "AppPaths"
      },
      "prerequisites": [
        "phase-01-domain-store",
        "gate-phase11-approval"
      ],
      "evidence": {
        "commands": [
          "rg -n -F session.json src tests docs --glob !docs/replacement-deletion-ledger.md",
          "cargo test --test cutover_contract session_json -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-01/verification.json",
          ".devmanager-next/evidence/phase-11/approval.json"
        ]
      },
      "deletionSet": [
        "src/persistence/mod.rs"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "The exact session.json omission contract and fresh-start evidence require explicit approval"
    },
    {
      "id": "legacy-session-manager",
      "area": "runtime-session-save",
      "legacy": {
        "path": "src/services/session_manager.rs",
        "symbols": [
          "SessionManager",
          "session_manager",
          "save_session"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "src/host/mod.rs",
        "symbol": "HostRuntime"
      },
      "prerequisites": [
        "phase-02-host-ipc",
        "phase-03-zero-orphan",
        "gate-phase11-approval"
      ],
      "evidence": {
        "commands": [
          "cargo test --test host_lifecycle -- --nocapture",
          "cargo test --test config_persistence -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-02/verification.json",
          ".devmanager-next/evidence/phase-11/approval.json"
        ]
      },
      "deletionSet": [
        "src/services/session_manager.rs"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "HOLD: src/services/session_manager.rs remains because src/services/mod.rs still `pub use session_manager::{ConfigImportMode, SessionManager}`, src/app/mod.rs still calls SessionManager::load_workspace/load_session/save_config/save_session/export_config_dialog, tests/config_persistence.rs uses SessionManager::apply_import_mode, and tests/config_service.rs include_str! the file. src/config/mod.rs has no ConfigImportMode/apply_import_mode owner. Migrating the helper would touch unowned config/services/test modules and could alter import/export behavior."
    },
    {
      "id": "legacy-remote-snapshot",
      "area": "connect-snapshot-ownership",
      "legacy": {
        "path": "src/remote/mod.rs",
        "symbols": [
          "RemoteWorkspaceSnapshot",
          "current_snapshot",
          "update_snapshot"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "src/remote/presentation.rs",
        "symbol": "RemotePresentation"
      },
      "prerequisites": [
        "phase-09-connect-realtime"
      ],
      "evidence": {
        "commands": [
          "cargo test --test protocol_contract -- --nocapture",
          "cargo test --test ipc_protocol -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-09/verification.json"
        ]
      },
      "deletionSet": [
        "src/remote/mod.rs"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Realtime snapshot/event ownership and explicit cutover approval"
    },
    {
      "id": "legacy-remote-bridge",
      "area": "connect-web-bridge",
      "legacy": {
        "path": "src/remote/web/bridge.rs",
        "symbols": [
          "remote::web::bridge",
          "web::bridge",
          "mod bridge"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "src/remote/web/action.rs",
        "symbol": "WebAction"
      },
      "prerequisites": [
        "phase-09-connect-realtime",
        "phase-10-organization"
      ],
      "evidence": {
        "commands": [
          "cargo test --test protocol_contract -- --nocapture",
          "cargo test --test browser_gateway -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-09/verification.json",
          ".devmanager-next/evidence/phase-10/verification.json"
        ]
      },
      "deletionSet": [
        "src/remote/web/bridge.rs"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Connect authorization/realtime parity and explicit approval"
    },
    {
      "id": "legacy-remote-lease",
      "area": "connect-control-ownership",
      "legacy": {
        "path": "src/remote/web/lease.rs",
        "symbols": [
          "RemoteLease",
          "lease",
          "control lease"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "src/remote/client_pool.rs",
        "symbol": "ClientPool"
      },
      "prerequisites": [
        "phase-09-connect-realtime",
        "phase-10-organization"
      ],
      "evidence": {
        "commands": [
          "cargo test --test host_lock -- --nocapture",
          "cargo test --test protocol_contract -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-09/verification.json",
          ".devmanager-next/evidence/phase-10/verification.json"
        ]
      },
      "deletionSet": [
        "src/remote/web/lease.rs"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Authorization and identity-preserving Connect evidence require explicit approval"
    },
    {
      "id": "legacy-sidebar",
      "area": "task-cockpit-navigation",
      "legacy": {
        "path": "src/sidebar/",
        "symbols": [
          "mod sidebar",
          "crate::sidebar",
          "Sidebar"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "src/client/model.rs",
        "symbol": "ClientModel"
      },
      "prerequisites": [
        "phase-05-task-cockpit"
      ],
      "evidence": {
        "commands": [
          "cargo test --test task_state -- --nocapture",
          "cargo test --test cutover_contract parity_ui -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-05/verification.json"
        ]
      },
      "deletionSet": [
        "src/sidebar/"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "HOLD: src/sidebar/ remains because src/lib.rs still has `pub mod sidebar` and src/app/mod.rs still has `use crate::sidebar` plus sidebar::sidebar_width_px. Native shell/UI does not import the module. Persistence field `sidebarCollapsed` is a data contract in src/persistence/mod.rs and must remain. Cannot delete while the held app runtime still compiles against it."
    },
    {
      "id": "legacy-editor-ui",
      "area": "configuration-editor-ui",
      "legacy": {
        "path": "src/workspace/editor_ui.rs",
        "symbols": [
          "mod editor_ui",
          "editor_ui::"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "src/config/mod.rs",
        "symbol": "ConfigFacade"
      },
      "prerequisites": [
        "phase-06-workspace-config"
      ],
      "evidence": {
        "commands": [
          "cargo test --test config_persistence -- --nocapture",
          "cargo test --test ssh_restore -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-06/verification.json"
        ]
      },
      "deletionSet": [
        "src/workspace/editor_ui.rs"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Configuration/sidebar behavior parity and explicit approval"
    },
    {
      "id": "legacy-provider-prompt-hooks",
      "area": "prompt-and-provider-events",
      "legacy": {
        "path": "src/ai/claude_hooks.rs",
        "symbols": [
          "UserPromptSubmit",
          "hook_event_name",
          "prompt"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "src/domain/command.rs",
        "symbol": "Command"
      },
      "prerequisites": [
        "phase-04-provider-conformance",
        "phase-07-prompt-library"
      ],
      "evidence": {
        "commands": [
          "cargo test --test claude_hooks -- --nocapture",
          "cargo test --test task_state -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-04/verification.json",
          ".devmanager-next/evidence/phase-07/verification.json"
        ]
      },
      "deletionSet": [
        "src/ai/claude_hooks.rs"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Provider prompt identity, personal prompt, and manual-chain evidence require approval"
    },
    {
      "id": "legacy-settings-connect-prompt-surface",
      "area": "connect-prompt-command-center-surface",
      "legacy": {
        "path": "src/workspace/mod.rs",
        "symbols": [
          "RemoteTopTab",
          "remote_pairing_token",
          "ConnectRemoteHost",
          "Prompt",
          "Chain"
        ],
        "tokens": [
          "personal prompt",
          "organization prompt",
          "prompt chain"
        ]
      },
      "replacementOwner": {
        "path": "src/client/model.rs",
        "symbol": "ClientModel"
      },
      "prerequisites": [
        "phase-06-workspace-config",
        "phase-07-prompt-library",
        "phase-09-connect-realtime",
        "phase-10-organization"
      ],
      "evidence": {
        "commands": [
          "cargo test --test config_persistence -- --nocapture",
          "cargo test --test protocol_contract -- --nocapture",
          "cargo test --test cutover_contract parity_prompt_connect_ui -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-07/verification.json",
          ".devmanager-next/evidence/phase-09/verification.json",
          ".devmanager-next/evidence/phase-10/verification.json"
        ]
      },
      "deletionSet": [
        "src/workspace/mod.rs"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Personal/organization prompt, manual chain, Connect identity, and Command Center parity approval"
    },
    {
      "id": "legacy-web-sessions",
      "area": "web-session-routes",
      "legacy": {
        "path": "web/src/sessions/",
        "symbols": [
          "SessionScreen",
          "sessionModel",
          "sessions/"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "web/src/app/router.ts",
        "symbol": "router"
      },
      "prerequisites": [
        "phase-05-task-cockpit",
        "phase-09-connect-realtime"
      ],
      "evidence": {
        "commands": [
          "npm test -- --runInBand",
          "cargo test --test protocol_contract -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-05/verification.json",
          ".devmanager-next/evidence/phase-09/verification.json"
        ]
      },
      "deletionSet": [
        "web/src/sessions/"
      ],
      "cutoverAction": "delete",
      "status": "DELETED",
      "approvalRequired": true,
      "approvalRequirement": "The old web sessions tree is already absent; remaining Task/Connect routes live under web/src/tasks and web/src/connect"
    },
    {
      "id": "legacy-loader-test",
      "area": "legacy-import-coverage",
      "legacy": {
        "path": "tests/legacy_loader.rs",
        "symbols": [
          "legacy_loader",
          "ConfigImportMode"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "tests/config_persistence.rs",
        "symbol": "config_round_trip"
      },
      "prerequisites": [
        "phase-01-domain-store",
        "gate-phase11-approval"
      ],
      "evidence": {
        "commands": [
          "cargo test --test config_persistence -- --nocapture",
          "cargo test --test cutover_contract session_json -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-01/verification.json",
          ".devmanager-next/evidence/phase-11/approval.json"
        ]
      },
      "deletionSet": [
        "tests/legacy_loader.rs"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Legacy import coverage must be replaced by an explicit fresh-start negative contract"
    },
    {
      "id": "legacy-session-fixture",
      "area": "legacy-session-fixtures",
      "legacy": {
        "path": "tests/fixtures/legacy-session.json",
        "symbols": [
          "legacy-session.json",
          "SessionState"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "tests/fixtures/legacy-config.json",
        "symbol": "config fixture"
      },
      "prerequisites": [
        "phase-01-domain-store",
        "gate-phase11-approval"
      ],
      "evidence": {
        "commands": [
          "cargo test --test cutover_contract session_json -- --nocapture",
          "cargo test --test config_persistence -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-01/verification.json",
          ".devmanager-next/evidence/phase-11/approval.json"
        ]
      },
      "deletionSet": [
        "tests/fixtures/legacy-session.json"
      ],
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "The old session fixture remains HOLD until deletion evidence is approved"
    },
    {
      "id": "legacy-tauri-archive",
      "area": "archived-desktop-implementation",
      "legacy": {
        "path": "zz-archive/tauri-react-v0.1.11/",
        "symbols": [
          "tauri-react-v0.1.11",
          "src-tauri"
        ],
        "tokens": [
          "tauri"
        ]
      },
      "replacementOwner": {
        "path": "src/main.rs",
        "symbol": "main"
      },
      "prerequisites": [
        "gate-release-candidate"
      ],
      "evidence": {
        "commands": [
          "rg -n -F tauri --glob !docs/replacement-deletion-ledger.md .",
          "pwsh scripts/native-next/Invoke-CutoverAudit.ps1 -Mode Parity"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-11/release-candidate.json"
        ]
      },
      "deletionSet": [
        "zz-archive/tauri-react-v0.1.11/"
      ],
      "cutoverAction": "delete",
      "status": "DELETED",
      "approvalRequired": true,
      "approvalRequirement": "The archived Tauri React desktop tree is absent. Package/scanner contracts keep the `zz-archive` exclusion/skip name and do not require the directory to exist."
    },
    {
      "id": "handoff-updater-module",
      "area": "update-metadata-and-handoff",
      "cutoverAction": "handoff",
      "legacy": {
        "path": "src/updater/mod.rs",
        "symbols": [
          "UpdateManifest",
          "latest.json",
          "check_for_updates"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "src/updater/handoff.rs",
        "symbol": "UpdateHandoff"
      },
      "prerequisites": [
        "gate-release-candidate"
      ],
      "evidence": {
        "commands": [
          "cargo test --test cutover_contract entry_ -- --nocapture",
          "cargo test --test update_contract -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-11/release-candidate.json"
        ]
      },
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Signed metadata, matching client/host identity, rollback, and update matrix approval. This module is completed in place; it is not deleted."
    },
    {
      "id": "handoff-update-contract",
      "area": "update-contract-evidence",
      "cutoverAction": "handoff",
      "legacy": {
        "path": "tests/updater.rs",
        "symbols": [
          "UpdateManifest",
          "latest.json",
          "updater"
        ],
        "tokens": []
      },
      "replacementOwner": {
        "path": "tests/update_contract.rs",
        "symbol": "update_contract"
      },
      "prerequisites": [
        "gate-release-candidate"
      ],
      "evidence": {
        "commands": [
          "cargo test --test update_contract -- --nocapture",
          "cargo test --test package_contract -- --nocapture"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-11/release-candidate.json"
        ]
      },
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Update-contract and package-contract evidence plus explicit approval. Do not install, publish, or delete user data from this phase."
    }
  ]
}
```

Current state: overall `HOLD` with four completed deletions. Final product
entry is sole GPUI `src/main.rs` (`run_native_shell` after hook relays and
debug `--ui-preview`) plus durable `devmanager-host` attach/detach/full quit.
`src/bin/devmanager-next.rs`, `web/src/sessions/`, `src/ai/codex_rollout.rs`,
and `zz-archive/tauri-react-v0.1.11/` are absent and therefore `DELETED`; they
must not return as a binary, compatibility shell, identity/transcript source,
or permanent HOLD. `src/app/mod.rs`, `src/sidebar/`,
`src/services/session_manager.rs`, and the other `deferredDeletionPaths`
remain present because named remaining dependents still compile against them.
Host `serve_request` remains a documented integration-test seam; production
uses `serve_duplex`. Packaging/update handoff files (`src/updater/handoff.rs`,
`tests/update_contract.rs`, `tests/package_contract.rs`) exist, but signed
release production-profile proof and explicit Phase 11 approval do not.
Remaining integrated prerequisites are every phase-01 through phase-10 node
plus `gate-phase11-approval` and `gate-release-candidate`. A nonzero audit
result is the honest result until those gates and explicit manual Phase 11
approval exist. This contract does not install, publish, or delete user data.
