import type {
  SemanticEvent,
  SessionStatus,
  WebSessionKind,
  WebSessionSummary,
  WebWorkspaceSnapshot,
} from "../api/types";

type DemoEventPayload<E extends SemanticEvent = SemanticEvent> = E extends SemanticEvent
  ? Omit<E, "stableSessionKey" | "sequence" | "occurredAtEpochMs" | "source">
  : never;

function demoEvent(
  stableSessionKey: string,
  sequence: number,
  occurredAtEpochMs: number,
  event: DemoEventPayload,
): SemanticEvent {
  return {
    stableSessionKey,
    sequence,
    occurredAtEpochMs,
    source: stableSessionKey.includes("mobile-ui") ? "claude" : "system",
    ...event,
  } as SemanticEvent;
}

export function makeDemoEvents(stableSessionKey: string, now = Date.now()): SemanticEvent[] {
  if (stableSessionKey === "tab:mobile-ui") {
    return [
      demoEvent(stableSessionKey, 79, now - 90_000, {
        kind: "userMessage",
        text: "Make the remote interface feel completely native on iPhone, including seamless return after multitasking.",
      }),
      demoEvent(stableSessionKey, 80, now - 72_000, {
        kind: "assistantMessage",
        message_id: "demo-message",
        text: "I’ve built the session home around projects and recent activity. The current session now returns exactly where you left it, while the DevManager host remains the source of truth.",
        streaming: false,
      }),
      demoEvent(stableSessionKey, 81, now - 48_000, {
        kind: "tool",
        tool_id: "demo-tool",
        name: "Run web tests",
        state: "completed",
        summary: "101 tests passed, including runtime resume and composer behavior.",
      }),
      demoEvent(stableSessionKey, 82, now - 34_000, {
        kind: "diff",
        item_id: "demo-diff",
        unified_diff: "+ Native semantic timeline\n+ Runtime-scoped drafts\n+ Automatic terminal fallback",
      }),
      demoEvent(stableSessionKey, 83, now - 12_000, {
        kind: "question",
        question_id: "demo-question",
        prompt: "The native session experience is ready for a visual pass.",
        choices: ["Review now", "Keep working"],
      }),
    ];
  }
  if (stableSessionKey.startsWith("server:")) {
    return [
      demoEvent(stableSessionKey, 17, now - 52_000, {
        kind: "status",
        state: "Server started",
        detail: "Watching for changes",
      }),
      demoEvent(stableSessionKey, 18, now - 31_000, {
        kind: "output",
        stream: "stdout",
        text: "Local: http://localhost:5199/\nready in 284 ms\n",
      }),
    ];
  }
  return [
    demoEvent(stableSessionKey, 1, now - 60_000, {
      kind: "command",
      command_id: "demo-command",
      text: "git status --short",
      exit_code: 0,
    }),
    demoEvent(stableSessionKey, 2, now - 58_000, {
      kind: "output",
      stream: "stdout",
      text: "Working tree clean\n",
    }),
  ];
}

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
