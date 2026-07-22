// @vitest-environment jsdom

import { cleanup, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  WEB_PROTOCOL_VERSION,
  type WebSessionSummary,
  type WebWorkspaceSnapshot,
} from "../api/types";
import { SessionsScreen } from "./SessionsScreen";

afterEach(() => {
  cleanup();
});

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
    lastActivityEpochMs: Date.now() - 45_000,
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
                status: "Stopped",
              },
            ],
          },
        ],
      },
    ],
    sshConnections: [],
    tabs: [
      {
        id: "live-ai",
        kind: "claude",
        projectId: "project-devmanager",
        commandId: null,
        sessionId: "pty-tab:live-ai",
        connectionId: null,
        label: "Claude 6",
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

describe("SessionsScreen", () => {
  it("renders Live now before Recent and exposes title, project, provider, state, and activity", () => {
    const onNavigate = vi.fn();
    const live = session("tab:live-ai", {
      title: "Fix sessions ordering",
      attention: "needsInput",
      lastActivityEpochMs: Date.now() - 20_000,
    });
    const endedFailed = session("tab:old-fail", {
      status: "Failed",
      attention: "failed",
      attentionCount: 3,
      lastActivityEpochMs: Date.now() - 3_600_000,
      title: "Old crash",
    });
    const stoppedServer = session("server:web", {
      status: "Stopped",
      lastActivityEpochMs: Date.now() - 120_000,
    });

    render(
      <SessionsScreen
        workspace={workspace([endedFailed, stoppedServer, live])}
        onNavigate={onNavigate}
      />,
    );

    const headings = screen.getAllByRole("heading", { level: 2 }).map((node) => node.textContent);
    expect(headings[0]).toMatch(/live now/i);
    expect(headings.indexOf(headings.find((text) => /recent/i.test(text ?? "")) ?? "")).toBeGreaterThan(
      headings.findIndex((text) => /live now/i.test(text ?? "")),
    );
    expect(screen.queryByRole("heading", { name: /needs attention/i })).toBeNull();

    const liveRow = screen.getByRole("button", {
      name: /Fix sessions ordering, DevManager, Claude, Needs input,/i,
    });
    const copy = liveRow.querySelector(".dm-session-copy");
    expect(copy?.children).toHaveLength(2);
    expect(copy?.children[0]?.textContent).toMatch(/Fix sessions ordering/);
    expect(copy?.children[0]?.textContent).toMatch(/ago|Now/i);
    expect(copy?.children[1]?.textContent).toMatch(/DevManager/);
    expect(copy?.children[1]?.textContent).toMatch(/Claude/);
    expect(copy?.children[1]?.textContent).toMatch(/Needs input/);
    expect(within(liveRow).getByText("DevManager").closest(".dm-session-secondary")).not.toBeNull();

    const recentSection = screen.getByRole("heading", { name: /recent/i }).closest("section");
    expect(recentSection).not.toBeNull();
    expect(within(recentSection as HTMLElement).getByText("Old crash").isConnected).toBe(true);
    expect(within(recentSection as HTMLElement).getByText("Web app").isConnected).toBe(true);
    expect(
      within(recentSection as HTMLElement).queryByLabelText(/unread updates/i),
    ).toBeNull();
  });
});
