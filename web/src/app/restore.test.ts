import { describe, expect, it } from "vitest";

import type { WebWorkspaceSnapshot } from "../api/types";
import { resolveColdStart, type SavedRoute } from "./restore";
import type { AppRoute } from "./router";

function snapshot(
  runtimeInstanceId = "runtime-a",
  stableSessionKey: string | null = "tab:tab-a",
): WebWorkspaceSnapshot {
  return {
    webProtocolVersion: 2,
    runtimeInstanceId,
    revision: 1,
    serverId: "server",
    projects: [],
    sshConnections: [],
    tabs: [],
    sessions: stableSessionKey
      ? [
          {
            sessionId: "pty-a",
            stableSessionKey,
            kind: stableSessionKey.startsWith("server:") ? "server" : "claude",
            status: "Running",
            projectId: "project-a",
            commandId: null,
            tabId: "tab-a",
            dimensions: { cols: 80, rows: 24, cell_width: 10, cell_height: 20 },
            lastActivityEpochMs: 10,
            attention: "none",
            attentionCount: 0,
            adapterHealth: "healthy",
            rawRequired: false,
            oldestSequence: 1,
            latestSequence: 3,
          },
        ]
      : [],
    portStatuses: [],
    writerLease: {
      ownerClientInstanceId: null,
      generation: 0,
      expiresAtEpochMs: null,
      youAreOwner: false,
    },
  };
}

const sessions: AppRoute = { name: "sessions" };
const saved: SavedRoute = {
  runtimeInstanceId: "runtime-a",
  route: { name: "session", kind: "tab", id: "tab-a" },
};

describe("installed route restoration", () => {
  it("never restores in a normal browser tab", () => {
    expect(
      resolveColdStart(sessions, saved, {
        standalone: false,
        snapshot: snapshot(),
        launchEligible: true,
      }),
    ).toEqual(sessions);
  });

  it("restores a valid session only for an eligible installed cold launch", () => {
    expect(
      resolveColdStart(sessions, saved, {
        standalone: true,
        snapshot: snapshot(),
        launchEligible: true,
      }),
    ).toEqual(saved.route);
  });

  it("lets an explicit deep link win over saved state", () => {
    const deepLink: AppRoute = {
      name: "session",
      kind: "server",
      id: "server-a",
    };
    expect(
      resolveColdStart(deepLink, saved, {
        standalone: true,
        snapshot: snapshot(),
        launchEligible: false,
      }),
    ).toEqual(deepLink);
  });

  it("accepts a pushed deep link only for the runtime that created it", () => {
    const deepLink: AppRoute = {
      name: "session",
      kind: "tab",
      id: "tab-a",
    };
    expect(
      resolveColdStart(sessions, saved, {
        standalone: true,
        snapshot: snapshot("runtime-a"),
        launchEligible: false,
        notificationRuntimeInstanceId: "runtime-a",
        notificationRoute: deepLink,
      }),
    ).toEqual(deepLink);
    expect(
      resolveColdStart(sessions, saved, {
        standalone: true,
        snapshot: snapshot("runtime-new", "tab:tab-a"),
        launchEligible: false,
        notificationRuntimeInstanceId: "runtime-a",
        notificationRoute: deepLink,
      }),
    ).toEqual(sessions);
  });

  it("rejects a route from an old runtime or a removed session", () => {
    expect(
      resolveColdStart(sessions, saved, {
        standalone: true,
        snapshot: snapshot("runtime-new"),
        launchEligible: true,
      }),
    ).toEqual(sessions);
    expect(
      resolveColdStart(sessions, saved, {
        standalone: true,
        snapshot: snapshot("runtime-a", null),
        launchEligible: true,
      }),
    ).toEqual(sessions);
  });
});
