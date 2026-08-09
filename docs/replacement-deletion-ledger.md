# Replacement Deletion Ledger

This ledger is the dependency-safe Phase 11.1 cutover contract. The fenced JSON
document is canonical and is consumed by
`scripts/native-next/Invoke-CutoverAudit.ps1` and `tests/cutover_contract.rs`.
Every row remains `HOLD` until its prerequisite phase/gates and evidence are
actually green. `READY` means the replacement is approved for the deletion
slice; `DELETED` additionally requires the legacy path and all tracked
references to be absent. This foundation makes no deletion claim.

The audit uses `git ls-files` as the tracked universe and `rg` fixed-string line
scans for source/package/document references. Only this ledger is an allowed
self-reference. An exact `session.json` file is path-only evidence and is never
opened or hashed by the audit.

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
    "maxMatchesPerRow": 20
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
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Explicit Phase 11 cutover approval after merged-tree parity evidence"
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
          "devmanager-next.exe",
          "DEVMANAGER_PROFILE"
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
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Explicit approval is required before removing the development entry identity"
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
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Correlated current-generation provider identity evidence and explicit approval"
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
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Runtime ownership and no-session-write proof require explicit approval"
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
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Task Cockpit navigation parity and explicit approval"
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
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Task/Connect web route parity and explicit deletion approval"
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
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "The old session fixture remains HOLD until deletion evidence is approved"
    },
    {
      "id": "legacy-tauri-archive",
      "area": "archived-desktop-implementation",
      "legacy": {
        "path": "zz-archive/tauri-react-v0.1.11",
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
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Archive deletion is allowed only after release-candidate evidence and explicit approval"
    },
    {
      "id": "legacy-updater-module",
      "area": "update-metadata-and-handoff",
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
        "path": "src/client/cli.rs",
        "symbol": "UpdateCommand"
      },
      "prerequisites": [
        "gate-release-candidate"
      ],
      "evidence": {
        "commands": [
          "cargo test --test updater -- --nocapture",
          "pwsh scripts/native-next/Invoke-CutoverAudit.ps1 -Mode Parity"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-11/release-candidate.json"
        ]
      },
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Signed metadata, client/host identity, rollback, and update matrix approval"
    },
    {
      "id": "legacy-updater-tests",
      "area": "update-contract-evidence",
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
        "path": "tests/fixtures/latest.json",
        "symbol": "latest fixture"
      },
      "prerequisites": [
        "gate-release-candidate"
      ],
      "evidence": {
        "commands": [
          "cargo test --test updater -- --nocapture",
          "rg -n -F latest.json src tests web --glob !docs/replacement-deletion-ledger.md"
        ],
        "artifacts": [
          ".devmanager-next/evidence/phase-11/release-candidate.json"
        ]
      },
      "status": "HOLD",
      "approvalRequired": true,
      "approvalRequirement": "Update-contract replacement evidence and explicit approval"
    }
  ]
}
```

Current state: `HOLD`. The repository intentionally still contains the
legacy paths, references, development documentation, and unproven evidence
artifacts. A nonzero audit result is therefore the honest result until earlier
phase gates and the explicit Phase 11 approval exist.
