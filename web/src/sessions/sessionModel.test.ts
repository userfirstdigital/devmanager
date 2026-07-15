import { describe, expect, it } from "vitest";

import type {
  WebSessionSummary,
  WebWorkspaceSnapshot,
} from "../api/types";
import { WEB_PROTOCOL_VERSION } from "../api/types";
import { describeSession, groupSessions } from "./sessionModel";

function session(
  stableSessionKey: string,
  overrides: Partial<WebSessionSummary> = {},
): WebSessionSummary {
  return {
    sessionId: `pty-${stableSessionKey}`,
    stableSessionKey,
    kind: stableSessionKey.startsWith("server:") ? "server" : "claude",
    status: "Running",
    projectId: "project-devmanager",
    commandId: stableSessionKey.startsWith("server:")
      ? stableSessionKey.slice("server:".length)
      : null,
    tabId: stableSessionKey.startsWith("tab:")
      ? stableSessionKey.slice("tab:".length)
      : null,
    dimensions: { cols: 80, rows: 24, cell_width: 10, cell_height: 20 },
    lastActivityEpochMs: 1_000,
    attention: "none",
    attentionCount: 0,
    adapterHealth: "healthy",
    rawRequired: false,
    oldestSequence: 1,
    latestSequence: 2,
    ...overrides,
  };
}

function workspace(sessions: WebSessionSummary[]): WebWorkspaceSnapshot {
  return {
    webProtocolVersion: WEB_PROTOCOL_VERSION,
    runtimeInstanceId: "runtime",
    revision: 1,
    serverId: "server",
    projects: [
      {
        id: "project-devmanager",
        name: "DevManager",
        color: "#5b5bd6",
        folders: [
          {
            id: "web-folder",
            name: "Web",
            commands: [
              {
                id: "web",
                label: "Web app",
                port: 5199,
                status: "Running",
              },
            ],
          },
        ],
      },
    ],
    sshConnections: [],
    tabs: [
      {
        id: "claude-a",
        kind: "claude",
        projectId: "project-devmanager",
        commandId: null,
        sessionId: "pty-tab:claude-a",
        connectionId: null,
        label: "Native mobile UI",
      },
    ],
    sessions,
    portStatuses: [],
    writerLease: {
      ownerClientInstanceId: null,
      generation: 0,
      expiresAtEpochMs: null,
      youAreOwner: false,
    },
  };
}

describe("session presentation", () => {
  it("always includes the project and resolves human labels", () => {
    const ai = describeSession(
      workspace([session("tab:claude-a")]),
      session("tab:claude-a"),
    );
    expect(ai).toMatchObject({
      label: "Native mobile UI",
      projectName: "DevManager",
      kindLabel: "Claude",
      stateLabel: "Open",
      route: { name: "session", kind: "tab", id: "claude-a" },
    });

    const server = describeSession(
      workspace([session("server:web")]),
      session("server:web"),
    );
    expect(server).toMatchObject({
      label: "Web app",
      projectName: "DevManager",
      kindLabel: "Server",
    });
  });

  it("groups without duplicates and sorts every section by activity", () => {
    const needsInput = session("tab:needs", {
      attention: "needsInput",
      attentionCount: 2,
      lastActivityEpochMs: 4_000,
    });
    const failed = session("tab:failed", {
      attention: "failed",
      status: "Failed",
      lastActivityEpochMs: 5_000,
    });
    const newestActive = session("tab:new-active", {
      lastActivityEpochMs: 3_000,
    });
    const olderActive = session("tab:old-active", {
      lastActivityEpochMs: 2_000,
    });
    const recent = session("server:web", {
      status: "Stopped",
      lastActivityEpochMs: 1_000,
    });

    const groups = groupSessions(
      workspace([recent, olderActive, needsInput, newestActive, failed]),
    );
    expect(groups.needsAttention.map((item) => item.stableSessionKey)).toEqual([
      "tab:failed",
      "tab:needs",
    ]);
    expect(groups.active.map((item) => item.stableSessionKey)).toEqual([
      "tab:new-active",
      "tab:old-active",
    ]);
    expect(groups.recent.map((item) => item.stableSessionKey)).toEqual([
      "server:web",
    ]);
    expect(
      [...groups.needsAttention, ...groups.active, ...groups.recent].map(
        (item) => item.stableSessionKey,
      ),
    ).toHaveLength(5);
    expect(groups.active[0]).toMatchObject({ projectName: "DevManager" });
  });

  it("uses explicit, non-secret fallbacks when configuration is missing", () => {
    const orphan = session("tab:orphan", {
      projectId: "gone",
      kind: "ssh",
    });
    expect(describeSession(workspace([orphan]), orphan)).toMatchObject({
      label: "SSH session",
      projectName: "Project unavailable",
      stateLabel: "Connected",
    });
  });
});
