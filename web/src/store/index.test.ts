import { beforeEach, describe, expect, it, vi } from "vitest";

import type {
  RemoteActionResult,
  RemoteWorkspaceSnapshot,
  TerminalScreenSnapshot,
  TerminalSessionView,
} from "../api/types";

const { wsClientState, MockWsClient } = vi.hoisted(() => {
  const state: { instance: MockWsClient | null } = { instance: null };

  class MockWsClient {
    readonly send = vi.fn(() => true);
    readonly request = vi.fn<(action: unknown) => Promise<RemoteActionResult>>();
    readonly start = vi.fn(async () => {});
    readonly stop = vi.fn();
  readonly callbacks: {
    onStatus(status: unknown): void;
    onMessage(message: unknown): void;
    onSessionOutput(frame: unknown): void;
  };

    constructor(callbacks: {
      onStatus(status: unknown): void;
      onMessage(message: unknown): void;
      onSessionOutput(frame: unknown): void;
    }) {
      this.callbacks = callbacks;
      state.instance = this;
    }
  }

  return { wsClientState: state, MockWsClient };
});

vi.mock("../api/ws", () => ({
  WsClient: MockWsClient,
}));

import { useStore } from "./index";

function makeSnapshot(): RemoteWorkspaceSnapshot {
  return {
    appState: {
      config: {
        version: 1,
        projects: [
          {
            id: "project-1",
            name: "Project 1",
            rootPath: "C:\\Code\\project-1",
            folders: [],
            color: null,
            pinned: null,
            notes: null,
            createdAt: "2026-01-01T00:00:00.000Z",
            updatedAt: "2026-01-01T00:00:00.000Z",
          },
        ],
      },
      open_tabs: [],
      active_tab_id: null,
      sidebar_collapsed: false,
      collapsed_projects: [],
      window_bounds: null,
    },
    runtimeState: {
      sessions: {},
      active_session_id: null,
      debug_enabled: false,
    },
    sessionViews: {},
    portStatuses: {},
    controllerClientId: "web-client",
    youHaveControl: true,
    serverId: "server-1",
  };
}

function makeExistingAiTabSnapshot(): RemoteWorkspaceSnapshot {
  const snapshot = makeSnapshot();
  snapshot.appState.open_tabs = [
    {
      id: "claude-tab-1",
      type: "claude",
      projectId: "project-1",
      commandId: null,
      ptySessionId: "claude-session-old",
      label: "Claude 1",
      sshConnectionId: null,
    },
  ];
  snapshot.runtimeState.sessions["claude-session-old"] = {
    session_id: "claude-session-old",
    pid: 321,
    status: "Failed",
    session_kind: null,
    command_id: null,
    project_id: "project-1",
    tab_id: "claude-tab-1",
    exit_code: null,
    title: "Claude 1",
    dimensions: { cols: 100, rows: 30, cell_width: 10, cell_height: 20 },
  };
  return snapshot;
}

function makeScreen(): TerminalScreenSnapshot {
  return {
    lines: [],
    cursor: null,
    display_offset: 0,
    history_size: 0,
    total_lines: 0,
    rows: 24,
    cols: 80,
    mode: {
      alternate_screen: false,
      app_cursor: false,
      bracketed_paste: false,
      focus_in_out: false,
      mouse_report_click: false,
      mouse_drag: false,
      mouse_motion: false,
      sgr_mouse: false,
      utf8_mouse: false,
      alternate_scroll: false,
    },
  };
}

function makeSessionView(sessionId: string): TerminalSessionView {
  return {
    runtime: {
      session_id: sessionId,
      pid: 123,
      status: "Starting",
      session_kind: null,
      command_id: null,
      project_id: "project-1",
      tab_id: "claude-tab-2",
      exit_code: null,
      title: "Claude 1",
      dimensions: { cols: 100, rows: 30, cell_width: 10, cell_height: 20 },
    },
    screen: makeScreen(),
  };
}

describe("web AI tab actions", () => {
  beforeEach(() => {
    wsClientState.instance = null;
    useStore.setState({
      status: { kind: "idle" },
      snapshot: makeSnapshot(),
      activeProjectId: "project-1",
      activeSessionId: null,
      collapsedProjects: new Set<string>(),
      lastError: null,
      client: null,
      terminalSubscribers: new Map(),
      bootstrapSubscribers: new Map(),
      pendingBootstraps: new Map(),
      pendingLaunches: [],
    });
    useStore.getState().init();
  });

  it("launchAiTab uses the returned aiTab payload immediately", async () => {
    const client = wsClientState.instance;
    expect(client).toBeTruthy();
    client?.request.mockResolvedValue({
      ok: true,
      message: "Opened claude-session-2",
      payload: {
        type: "aiTab",
        tab_id: "claude-tab-2",
        project_id: "project-1",
        tab_type: "claude",
        session_id: "claude-session-2",
        label: "Claude 1",
        session_view: makeSessionView("claude-session-2"),
      },
    });

    await useStore.getState().launchAiTab("project-1", "claude");

    expect(client?.request).toHaveBeenCalledWith({
      type: "launchAi",
      project_id: "project-1",
      tab_type: "claude",
      dimensions: { cols: 100, rows: 30, cell_width: 10, cell_height: 20 },
    });
    expect(client?.send).toHaveBeenCalledWith({
      type: "subscribeSessions",
      sessionIds: ["claude-session-2"],
    });

    const state = useStore.getState();
    expect(state.activeSessionId).toBe("claude-session-2");
    expect(state.snapshot?.appState.open_tabs).toContainEqual(
      expect.objectContaining({
        id: "claude-tab-2",
        type: "claude",
        projectId: "project-1",
        ptySessionId: "claude-session-2",
        label: "Claude 1",
      }),
    );
    expect(state.snapshot?.runtimeState.sessions["claude-session-2"]).toEqual(
      makeSessionView("claude-session-2").runtime,
    );
    expect(state.pendingBootstraps.get("claude-session-2")?.screen.rows).toBe(24);
  });

  it("openAiTab follows the host-returned session id for existing tabs", async () => {
    useStore.setState({
      snapshot: makeExistingAiTabSnapshot(),
      activeSessionId: "claude-session-old",
      pendingBootstraps: new Map(),
    });

    const client = wsClientState.instance;
    expect(client).toBeTruthy();
    client?.request.mockResolvedValue({
      ok: true,
      message: "Opened claude-session-new",
      payload: {
        type: "aiTab",
        tab_id: "claude-tab-1",
        project_id: "project-1",
        tab_type: "claude",
        session_id: "claude-session-new",
        label: "Claude 1",
        session_view: makeSessionView("claude-session-new"),
      },
    });

    await useStore.getState().openAiTab("claude-tab-1");

    expect(client?.request).toHaveBeenCalledWith({
      type: "openAiTab",
      tab_id: "claude-tab-1",
      dimensions: { cols: 100, rows: 30, cell_width: 10, cell_height: 20 },
    });
    expect(client?.send).toHaveBeenNthCalledWith(1, {
      type: "unsubscribeSessions",
      sessionIds: ["claude-session-old"],
    });
    expect(client?.send).toHaveBeenNthCalledWith(2, {
      type: "subscribeSessions",
      sessionIds: ["claude-session-new"],
    });

    const state = useStore.getState();
    expect(state.activeSessionId).toBe("claude-session-new");
    expect(
      state.snapshot?.appState.open_tabs.find((tab) => tab.id === "claude-tab-1")
        ?.ptySessionId,
    ).toBe("claude-session-new");
  });

  it("buffers session output that arrives before the terminal subscribes", () => {
    const client = wsClientState.instance;
    expect(client).toBeTruthy();

    client?.callbacks.onSessionOutput({
      sessionId: "claude-session-3",
      chunkSeq: 1,
      bytes: new Uint8Array([65, 66, 67]),
    });

    const seen: Uint8Array[] = [];
    const unsubscribe = useStore
      .getState()
      .subscribeTerminal("claude-session-3", (frame) => seen.push(frame.bytes));

    expect(seen).toHaveLength(1);
    expect(Array.from(seen[0] ?? new Uint8Array())).toEqual([65, 66, 67]);

    unsubscribe();
  });
});
