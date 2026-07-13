import type {
  SessionStatus,
  WebSessionKind,
  WebSessionSummary,
  WebWorkspaceSnapshot,
} from "../api/types";

function session(
  stableSessionKey: string,
  kind: WebSessionKind,
  status: SessionStatus,
  projectId: string,
  lastActivityEpochMs: number,
  options: Partial<WebSessionSummary> = {},
): WebSessionSummary {
  return {
    sessionId: `demo-${stableSessionKey.replace(":", "-")}`,
    stableSessionKey,
    kind,
    status,
    projectId,
    commandId: stableSessionKey.startsWith("server:")
      ? stableSessionKey.slice("server:".length)
      : null,
    tabId: stableSessionKey.startsWith("tab:")
      ? stableSessionKey.slice("tab:".length)
      : null,
    dimensions: { cols: 100, rows: 30, cell_width: 10, cell_height: 20 },
    lastActivityEpochMs,
    attention: "none",
    attentionCount: 0,
    adapterHealth: "healthy",
    rawRequired: false,
    oldestSequence: 1,
    latestSequence: 20,
    ...options,
  };
}

export function makeDemoWorkspace(now = Date.now()): WebWorkspaceSnapshot {
  return {
    webProtocolVersion: 2,
    runtimeInstanceId: "demo-native-mobile-runtime",
    revision: 12,
    serverId: "demo-host",
    projects: [
      {
        id: "devmanager",
        name: "DevManager",
        color: "#0a84ff",
        folders: [
          {
            id: "apps",
            name: "Apps",
            commands: [
              { id: "web", label: "Web interface", port: 5199, status: "Running" },
              { id: "desktop", label: "Desktop app", port: null, status: "Stopped" },
            ],
          },
        ],
      },
      {
        id: "househunter",
        name: "House Hunter",
        color: "#ff9f0a",
        folders: [
          {
            id: "services",
            name: "Services",
            commands: [
              { id: "hh-api", label: "API", port: 3000, status: "Failed" },
            ],
          },
        ],
      },
    ],
    sshConnections: [
      {
        id: "staging",
        label: "Staging server",
        host: "dev.example.test",
        port: 22,
        username: "developer",
      },
    ],
    tabs: [
      {
        id: "mobile-ui",
        kind: "claude",
        projectId: "devmanager",
        commandId: null,
        sessionId: "demo-tab-mobile-ui",
        connectionId: null,
        label: "Native mobile interface",
      },
      {
        id: "resume-engine",
        kind: "codex",
        projectId: "devmanager",
        commandId: null,
        sessionId: "demo-tab-resume-engine",
        connectionId: null,
        label: "Seamless resume",
      },
      {
        id: "staging-tab",
        kind: "ssh",
        projectId: "househunter",
        commandId: null,
        sessionId: "demo-tab-staging-tab",
        connectionId: "staging",
        label: "Staging server",
      },
    ],
    sessions: [
      session("tab:mobile-ui", "claude", "Running", "devmanager", now - 12_000, {
        attention: "needsInput",
        attentionCount: 1,
        latestSequence: 84,
      }),
      session("server:hh-api", "server", "Failed", "househunter", now - 18 * 60_000, {
        attention: "failed",
        attentionCount: 1,
      }),
      session("tab:resume-engine", "codex", "Running", "devmanager", now - 3 * 60_000),
      session("server:web", "server", "Running", "devmanager", now - 11 * 60_000),
      session("tab:staging-tab", "ssh", "Exited", "househunter", now - 2 * 60 * 60_000),
    ],
    portStatuses: [
      { port: 5199, inUse: true, pid: 1234, processName: "vite" },
      { port: 3000, inUse: false, pid: null, processName: null },
    ],
    writerLease: {
      ownerClientInstanceId: "demo-phone",
      generation: 3,
      expiresAtEpochMs: now + 8_000,
      youAreOwner: true,
    },
  };
}
