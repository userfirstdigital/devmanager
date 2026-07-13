import { beforeEach, describe, expect, it, vi } from "vitest";

import type {
  ComposerAccepted,
  SemanticEvent,
  SemanticReplayDescriptor,
  SemanticReplayPage,
  WebActionResult,
  WebWorkspaceSnapshot,
} from "../api/types";

const { wsClientState, MockWsClient } = vi.hoisted(() => {
  const state: { instance: MockWsClient | null } = { instance: null };

  class MockWsClient {
    readonly callbacks: {
      onStatus(status: unknown): void;
      onMessage(message: unknown): void;
      onSessionOutput(frame: unknown): void;
      getResumeContext?(): unknown;
    };
    readonly start = vi.fn(async () => {});
    readonly stop = vi.fn();
    readonly send = vi.fn((_frame: { type: string }) => true);
    readonly sendWithWriterLease = vi.fn((_frame: { type: string }) => true);
    readonly request = vi.fn(
      async (): Promise<WebActionResult> => ({ ok: true, payload: null }),
    );
    readonly wake = vi.fn();
    readonly foreground = vi.fn();
    readonly setVisibility = vi.fn();
    readonly resetRuntime = vi.fn();
    readonly discardWriterFramesForSession = vi.fn();
    readonly ensureWriterLease = vi.fn();
    readonly cancelComposer = vi.fn();
    readonly leaseState = vi.fn(() => ({
      ownerClientInstanceId: null,
      generation: 0,
      expiresAtEpochMs: null,
      youAreOwner: false,
    }));
    readonly submitComposer = vi.fn(async (submission: { mutationId: string }) => ({
      mutationId: submission.mutationId,
      stableSessionKey: "tab:a",
      acceptedSequence: 1,
      leaseGeneration: 1,
    }));

    constructor(callbacks: MockWsClient["callbacks"]) {
      this.callbacks = callbacks;
      state.instance = this;
    }
  }

  return { wsClientState: state, MockWsClient };
});

vi.mock("../api/ws", () => ({
  WsClient: MockWsClient,
  isTransientComposerRejection: (code: string) =>
    [
      "leaseBusy",
      "staleGeneration",
      "nativeControllerActive",
      "mutationInFlight",
    ].includes(code),
}));

import {
  type BoundedSemanticJournalState,
  MAX_SEMANTIC_BYTES_PER_SESSION,
  MAX_SEMANTIC_EVENTS_PER_SESSION,
  useStore,
} from "./index";

const writerLease = {
  ownerClientInstanceId: "tab-client",
  generation: 7,
  expiresAtEpochMs: 10_000,
  youAreOwner: true,
};

function makeSnapshot(
  overrides: Partial<WebWorkspaceSnapshot> = {},
): WebWorkspaceSnapshot {
  return {
    webProtocolVersion: 2,
    runtimeInstanceId: "runtime-1",
    revision: 1,
    serverId: "server-1",
    projects: [
      {
        id: "project-1",
        name: "Project",
        color: "#123456",
        folders: [
          {
            id: "folder-1",
            name: "Folder",
            commands: [
              {
                id: "command-1",
                label: "Server",
                port: 3000,
                status: "Running",
              },
            ],
          },
        ],
      },
    ],
    sshConnections: [
      {
        id: "ssh-1",
        label: "SSH",
        host: "host.example",
        port: 22,
        username: "dev",
      },
    ],
    tabs: [
      {
        id: "a",
        kind: "claude",
        projectId: "project-1",
        commandId: null,
        sessionId: "pty-a",
        connectionId: null,
        label: "Claude",
      },
    ],
    sessions: [
      {
        sessionId: "pty-a",
        stableSessionKey: "tab:a",
        kind: "claude",
        status: "Running",
        projectId: "project-1",
        commandId: null,
        tabId: "a",
        dimensions: { cols: 100, rows: 30, cell_width: 10, cell_height: 20 },
        lastActivityEpochMs: 123,
        attention: "unread",
        attentionCount: 2,
        adapterHealth: "healthy",
        rawRequired: false,
        oldestSequence: 1,
        latestSequence: 2,
      },
    ],
    portStatuses: [
      { port: 3000, inUse: true, pid: 123, processName: "node" },
    ],
    writerLease,
    ...overrides,
  };
}

function outputEvent(sequence: number, text = `event-${sequence}`): SemanticEvent {
  return {
    stableSessionKey: "tab:a",
    sequence,
    occurredAtEpochMs: sequence * 100,
    source: "claude",
    kind: "output",
    stream: "stdout",
    text,
  };
}

function journal(events: SemanticEvent[]): BoundedSemanticJournalState {
  return {
    stableSessionKey: "tab:a",
    oldestSequence: events[0]?.sequence ?? 0,
    latestSequence: events[events.length - 1]?.sequence ?? 0,
    cursorRolledOver: false,
    events,
    retainedBytes: events.reduce(
      (total, event) =>
        total + new TextEncoder().encode(JSON.stringify(event)).byteLength,
      0,
    ),
  };
}

function replayDescriptor(
  overrides: Partial<SemanticReplayDescriptor> = {},
): SemanticReplayDescriptor {
  return {
    replayId: 10,
    stableSessionKey: "tab:a",
    fromSequence: 2,
    throughSequence: 7,
    rollover: false,
    ...overrides,
  };
}

function replayPage(
  overrides: Partial<SemanticReplayPage> = {},
): SemanticReplayPage {
  return {
    ...replayDescriptor(),
    nextSequence: 7,
    complete: true,
    events: [],
    ...overrides,
  };
}

function storageMock() {
  const values = new Map<string, string>();
  return {
    getItem: vi.fn((key: string) => values.get(key) ?? null),
    setItem: vi.fn((key: string, value: string) => values.set(key, value)),
    removeItem: vi.fn((key: string) => values.delete(key)),
  };
}

function deferred<T>() {
  let resolve: (value: T) => void = () => {};
  let reject: (reason?: unknown) => void = () => {};
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

function resetStore(): void {
  useStore.setState(useStore.getInitialState(), true);
  wsClientState.instance = null;
}

beforeEach(() => {
  vi.restoreAllMocks();
  vi.stubGlobal("localStorage", storageMock());
  vi.stubGlobal("crypto", { randomUUID: vi.fn(() => "mutation-uuid") });
  resetStore();
});

describe("host runtime reconciliation", () => {
  it("atomically clears every runtime-derived slice before applying a new runtime", () => {
    useStore.getState().init();
    useStore.setState({
      runtimeInstanceId: "runtime-old",
      revision: 9,
      activeSessionKey: "tab:old",
      journals: { "tab:old": journal([outputEvent(1)]) },
      semanticReplay: {
        replayId: 1,
        stableSessionKey: "tab:old",
        fromSequence: 1,
        throughSequence: 2,
        rollover: false,
        nextSequence: 1,
      },
      drafts: { "tab:old": "unfinished" },
      unread: { "tab:old": 4 },
      pendingRoute: "/session/tab/old",
      pendingMutations: {
        "tab:old": {
          mutationId: "mutation-old",
          stableSessionKey: "tab:old",
          text: "unfinished",
          attachments: [],
        },
      },
      rawTerminal: {
        ...useStore.getState().rawTerminal,
        activeStreamSessionId: "pty-old",
        streamSessionIdByStableKey: { "tab:old": "pty-old" },
        pendingTerminalFrames: new Map([["pty-old", []]]),
      },
    });

    useStore.getState().applySnapshot(
      makeSnapshot({ runtimeInstanceId: "runtime-new", revision: 1, sessions: [] }),
    );

    const state = useStore.getState();
    expect(state.runtimeInstanceId).toBe("runtime-new");
    expect(state.workspace?.runtimeInstanceId).toBe("runtime-new");
    expect(state.journals).toEqual({});
    expect(state.semanticReplay).toBeNull();
    expect(state.drafts).toEqual({});
    expect(state.unread).toEqual({});
    expect(state.activeSessionKey).toBeNull();
    expect(state.pendingRoute).toBeNull();
    expect(state.pendingMutations).toEqual({});
    expect(state.rawTerminal.activeStreamSessionId).toBeNull();
    expect(state.rawTerminal.streamSessionIdByStableKey).toEqual({});
    expect(state.rawTerminal.pendingTerminalFrames.size).toBe(0);
    expect(wsClientState.instance?.resetRuntime).toHaveBeenCalledTimes(1);
  });

  it("fails closed and exposes a compatibility diagnostic for an unsupported protocol", () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.setState({
      activeSessionKey: "tab:a",
      journals: { "tab:a": journal([outputEvent(1), outputEvent(2)]) },
      drafts: { "tab:a": "do not render" },
      unread: { "tab:a": 2 },
      pendingRoute: "/session/tab/a",
      pendingMutations: {
        "tab:a": {
          mutationId: "mutation-old-protocol",
          stableSessionKey: "tab:a",
          text: "do not send",
          attachments: [],
        },
      },
    });

    useStore.getState().applySnapshot(
      makeSnapshot({
        webProtocolVersion: 99,
        runtimeInstanceId: "runtime-new",
      }),
    );

    const state = useStore.getState();
    expect(state.compatibilityDiagnostic).toEqual({
      expectedProtocolVersion: 2,
      receivedProtocolVersion: 99,
    });
    expect(state.status).toMatchObject({ kind: "closed" });
    expect(state.workspace).toBeNull();
    expect(state.snapshot).toBeNull();
    expect(state.runtimeInstanceId).toBeNull();
    expect(state.sessions).toEqual({});
    expect(state.writerLease.youAreOwner).toBe(false);
    expect(state.activeSessionKey).toBeNull();
    expect(state.journals).toEqual({});
    expect(state.drafts).toEqual({});
    expect(state.unread).toEqual({});
    expect(state.pendingRoute).toBeNull();
    expect(state.pendingMutations).toEqual({});
    expect(state.rawTerminal.pendingTerminalFrames.size).toBe(0);
    expect(state.client).toBeNull();
    expect(wsClientState.instance?.resetRuntime).toHaveBeenCalledWith(
      expect.stringContaining("protocol 99"),
    );
    expect(wsClientState.instance?.stop).toHaveBeenCalledTimes(1);
  });

  it("preserves stable-key journals and drafts across same-runtime snapshots", () => {
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.setState({
      journals: { "tab:a": journal([outputEvent(1)]) },
      drafts: { "tab:a": "same runtime" },
    });

    useStore.getState().applySnapshot(makeSnapshot({ revision: 2 }));

    expect(useStore.getState().journals["tab:a"]?.events).toHaveLength(1);
    expect(useStore.getState().drafts["tab:a"]).toBe("same runtime");
    expect(useStore.getState().revision).toBe(2);
  });

  it("removes every session and PTY scoped value omitted by a same-runtime snapshot", () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    const goneEvent = { ...outputEvent(1), stableSessionKey: "tab:gone" };
    useStore.setState((state) => ({
      activeSessionKey: "tab:gone",
      journals: {
        "tab:a": journal([outputEvent(1)]),
        "tab:gone": {
          ...journal([goneEvent]),
          stableSessionKey: "tab:gone",
        },
      },
      semanticReplay: {
        ...replayDescriptor({ stableSessionKey: "tab:gone" }),
        nextSequence: 2,
      },
      semanticGapKeys: new Set(["tab:a", "tab:gone"]),
      drafts: { "tab:a": "keep", "tab:gone": "remove" },
      unread: { "tab:a": 2, "tab:gone": 3 },
      pendingMutations: {
        "tab:a": {
          mutationId: "keep-mutation",
          stableSessionKey: "tab:a",
          text: "keep",
          attachments: [],
        },
        "tab:gone": {
          mutationId: "gone-mutation",
          stableSessionKey: "tab:gone",
          text: "remove",
          attachments: [],
        },
      },
      rawTerminal: {
        ...state.rawTerminal,
        activeStreamSessionId: "pty-gone",
        streamSessionIdByStableKey: {
          "tab:a": "pty-a",
          "tab:gone": "pty-gone",
        },
        terminalSubscribers: new Map([
          ["pty-a", new Set()],
          ["pty-gone", new Set()],
        ]),
        pendingTerminalFrames: new Map([
          ["pty-a", []],
          ["pty-gone", []],
        ]),
        bootstrapSubscribers: new Map([
          ["pty-a", new Set()],
          ["pty-gone", new Set()],
        ]),
        pendingBootstraps: new Map([
          ["pty-a", {} as never],
          ["pty-gone", {} as never],
        ]),
      },
    }));

    useStore.getState().applySnapshot(makeSnapshot({ revision: 2 }));

    const state = useStore.getState();
    expect(Object.keys(state.journals)).toEqual(["tab:a"]);
    expect(Object.keys(state.drafts)).toEqual(["tab:a"]);
    expect(Object.keys(state.pendingMutations)).toEqual(["tab:a"]);
    expect(Object.keys(state.unread)).toEqual(["tab:a"]);
    expect([...state.semanticGapKeys]).toEqual(["tab:a"]);
    expect(state.semanticReplay).toBeNull();
    expect(state.activeSessionKey).toBeNull();
    expect(state.rawTerminal.activeStreamSessionId).toBeNull();
    expect([...state.rawTerminal.terminalSubscribers.keys()]).toEqual(["pty-a"]);
    expect([...state.rawTerminal.pendingTerminalFrames.keys()]).toEqual(["pty-a"]);
    expect([...state.rawTerminal.bootstrapSubscribers.keys()]).toEqual(["pty-a"]);
    expect([...state.rawTerminal.pendingBootstraps.keys()]).toEqual(["pty-a"]);
    expect(wsClientState.instance?.cancelComposer).toHaveBeenCalledWith(
      "gone-mutation",
      "session removed by host",
    );
    expect(
      wsClientState.instance?.discardWriterFramesForSession,
    ).toHaveBeenCalledWith("pty-gone");
  });

  it.each([true, false])(
    "purges an %sactive removed session immediately and rejects its late raw frames",
    (removedSessionWasActive) => {
      useStore.getState().init();
      const base = makeSnapshot();
      const removedSession = {
        ...base.sessions[0],
        sessionId: "pty-b",
        stableSessionKey: "tab:b",
        tabId: "b",
      };
      useStore.getState().applySnapshot(
        makeSnapshot({
          tabs: [
            ...base.tabs,
            {
              ...base.tabs[0],
              id: "b",
              sessionId: "pty-b",
              label: "Claude B",
            },
          ],
          sessions: [...base.sessions, removedSession],
        }),
      );
      const removedEvent = {
        ...outputEvent(3),
        stableSessionKey: "tab:b",
      };
      useStore.setState((state) => ({
        activeSessionKey: removedSessionWasActive ? "tab:b" : "tab:a",
        journals: {
          "tab:a": journal([outputEvent(1), outputEvent(2)]),
          "tab:b": {
            ...journal([removedEvent]),
            stableSessionKey: "tab:b",
          },
        },
        drafts: { "tab:a": "keep", "tab:b": "remove" },
        unread: { "tab:a": 1, "tab:b": 2 },
        semanticGapKeys: new Set(["tab:a", "tab:b"]),
        semanticGapSequences: { "tab:a": 4, "tab:b": 5 },
        semanticReplay: {
          ...replayDescriptor({ stableSessionKey: "tab:b" }),
          nextSequence: 2,
        },
        pendingMutations: {
          "tab:b": {
            mutationId: "mutation-b",
            stableSessionKey: "tab:b",
            text: "remove",
            attachments: [],
          },
        },
        rawTerminal: {
          ...state.rawTerminal,
          activeStreamSessionId: removedSessionWasActive ? "pty-b" : "pty-a",
          terminalSubscribers: new Map([
            ["pty-a", new Set()],
            ["pty-b", new Set()],
          ]),
          pendingTerminalFrames: new Map([
            ["pty-a", []],
            ["pty-b", []],
          ]),
          bootstrapSubscribers: new Map([
            ["pty-a", new Set()],
            ["pty-b", new Set()],
          ]),
          pendingBootstraps: new Map([
            ["pty-a", {} as never],
            ["pty-b", {} as never],
          ]),
        },
      }));

      wsClientState.instance?.callbacks.onMessage({
        type: "sessionRemoved",
        sessionId: "pty-b",
      });
      wsClientState.instance?.callbacks.onSessionOutput({
        sessionId: "pty-b",
        chunkSeq: 99,
        bytes: new Uint8Array([1, 2, 3]),
      });

      const state = useStore.getState();
      expect(state.activeSessionKey).toBe(
        removedSessionWasActive ? null : "tab:a",
      );
      expect(state.sessions["tab:b"]).toBeUndefined();
      expect(state.workspace?.sessions.some((session) => session.sessionId === "pty-b"))
        .toBe(false);
      expect(state.journals["tab:b"]).toBeUndefined();
      expect(state.drafts["tab:b"]).toBeUndefined();
      expect(state.unread["tab:b"]).toBeUndefined();
      expect(state.semanticGapKeys.has("tab:b")).toBe(false);
      expect(state.semanticGapSequences["tab:b"]).toBeUndefined();
      expect(state.semanticReplay).toBeNull();
      expect(state.pendingMutations["tab:b"]).toBeUndefined();
      expect(state.rawTerminal.streamSessionIdByStableKey["tab:b"]).toBeUndefined();
      expect(state.rawTerminal.terminalSubscribers.has("pty-b")).toBe(false);
      expect(state.rawTerminal.pendingTerminalFrames.has("pty-b")).toBe(false);
      expect(state.rawTerminal.bootstrapSubscribers.has("pty-b")).toBe(false);
      expect(state.rawTerminal.pendingBootstraps.has("pty-b")).toBe(false);
      expect(wsClientState.instance?.cancelComposer).toHaveBeenCalledWith(
        "mutation-b",
        "session removed by host",
      );
      expect(
        wsClientState.instance?.discardWriterFramesForSession,
      ).toHaveBeenCalledWith("pty-b");
    },
  );

  it("closes only raw PTY state while retaining the stable semantic session", () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.setState((state) => ({
      activeSessionKey: "tab:a",
      journals: { "tab:a": journal([outputEvent(1), outputEvent(2)]) },
      drafts: { "tab:a": "keep final draft" },
      pendingMutations: {
        "tab:a": {
          mutationId: "mutation-a",
          stableSessionKey: "tab:a",
          text: "keep final draft",
          attachments: [],
        },
      },
      rawTerminal: {
        ...state.rawTerminal,
        activeStreamSessionId: "pty-a",
        terminalSubscribers: new Map([["pty-a", new Set()]]),
        pendingTerminalFrames: new Map([["pty-a", []]]),
        bootstrapSubscribers: new Map([["pty-a", new Set()]]),
        pendingBootstraps: new Map([["pty-a", {} as never]]),
      },
    }));

    wsClientState.instance?.callbacks.onMessage({
      type: "sessionClosed",
      sessionId: "pty-a",
    });

    const state = useStore.getState();
    expect(state.activeSessionKey).toBe("tab:a");
    expect(state.sessions["tab:a"]).toBeDefined();
    expect(state.journals["tab:a"]?.events).toHaveLength(2);
    expect(state.drafts["tab:a"]).toBe("keep final draft");
    expect(state.pendingMutations["tab:a"]?.mutationId).toBe("mutation-a");
    expect(state.rawTerminal.activeStreamSessionId).toBeNull();
    expect(state.rawTerminal.streamSessionIdByStableKey["tab:a"]).toBeUndefined();
    expect(state.rawTerminal.terminalSubscribers.has("pty-a")).toBe(false);
    expect(state.rawTerminal.pendingTerminalFrames.has("pty-a")).toBe(false);
    expect(state.rawTerminal.bootstrapSubscribers.has("pty-a")).toBe(false);
    expect(state.rawTerminal.pendingBootstraps.has("pty-a")).toBe(false);
    expect(wsClientState.instance?.cancelComposer).not.toHaveBeenCalled();
    expect(
      wsClientState.instance?.discardWriterFramesForSession,
    ).toHaveBeenCalledWith("pty-a");
  });

  it("trims journals to host retention and the browser memory cap", () => {
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.setState({
      journals: {
        "tab:a": journal(Array.from({ length: 6 }, (_, index) => outputEvent(index + 1))),
      },
    });
    useStore.getState().applySnapshot(
      makeSnapshot({
        revision: 2,
        sessions: [
          {
            ...makeSnapshot().sessions[0],
            oldestSequence: 4,
            latestSequence: 6,
          },
        ],
      }),
    );
    expect(
      useStore.getState().journals["tab:a"]?.events.map((event) => event.sequence),
    ).toEqual([4, 5, 6]);

    const total = MAX_SEMANTIC_EVENTS_PER_SESSION + 25;
    useStore.setState({
      journals: {
        "tab:a": journal(
          Array.from({ length: total }, (_, index) => outputEvent(index + 1)),
        ),
      },
    });
    useStore.getState().applySnapshot(
      makeSnapshot({
        revision: 3,
        sessions: [
          {
            ...makeSnapshot().sessions[0],
            oldestSequence: 1,
            latestSequence: total,
          },
        ],
      }),
    );
    let events = useStore.getState().journals["tab:a"]?.events ?? [];
    expect(events).toHaveLength(MAX_SEMANTIC_EVENTS_PER_SESSION);
    expect(events[0]?.sequence).toBe(26);

    useStore.getState().appendSemanticEvent(outputEvent(total + 1));
    events = useStore.getState().journals["tab:a"]?.events ?? [];
    expect(events).toHaveLength(MAX_SEMANTIC_EVENTS_PER_SESSION);
    expect(events[0]?.sequence).toBe(27);
    expect(useStore.getState().journals["tab:a"]?.oldestSequence).toBe(27);
    expect(useStore.getState().journals["tab:a"]?.latestSequence).toBe(total + 1);
  });

  it("reuses a bounded journal when host retention has not advanced", () => {
    useStore.getState().applySnapshot(makeSnapshot());
    const existing = journal([outputEvent(1), outputEvent(2)]);
    useStore.setState({ journals: { "tab:a": existing } });

    useStore.getState().applySnapshot(makeSnapshot({ revision: 2 }));

    expect(useStore.getState().journals["tab:a"]).toBe(existing);
  });

  it("also bounds retained semantic history by encoded bytes", () => {
    const eventCount = 64;
    const largeText = "x".repeat(64 * 1_024);
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.setState({
      journals: {
        "tab:a": journal(
          Array.from({ length: eventCount }, (_, index) =>
            outputEvent(index + 1, largeText),
          ),
        ),
      },
    });
    useStore.getState().applySnapshot(
      makeSnapshot({
        revision: 2,
        sessions: [
          {
            ...makeSnapshot().sessions[0],
            oldestSequence: 1,
            latestSequence: eventCount,
          },
        ],
      }),
    );

    let journalState = useStore.getState().journals["tab:a"];
    const retainedBytes = journalState?.events.reduce(
      (total, event) => total + new TextEncoder().encode(JSON.stringify(event)).byteLength,
      0,
    );
    expect(retainedBytes).toBeLessThanOrEqual(MAX_SEMANTIC_BYTES_PER_SESSION);
    expect(journalState?.events.length).toBeLessThan(eventCount);
    expect(journalState?.oldestSequence).toBe(
      journalState?.events[0]?.sequence,
    );
    expect(journalState?.latestSequence).toBe(eventCount);

    const oldestBeforeAppend = journalState?.oldestSequence ?? 0;
    useStore.getState().appendSemanticEvent(outputEvent(eventCount + 1, largeText));
    journalState = useStore.getState().journals["tab:a"];
    const appendedBytes = journalState?.events.reduce(
      (total, event) => total + new TextEncoder().encode(JSON.stringify(event)).byteLength,
      0,
    );
    expect(appendedBytes).toBeLessThanOrEqual(MAX_SEMANTIC_BYTES_PER_SESSION);
    expect(journalState?.oldestSequence).toBeGreaterThan(oldestBeforeAppend);
    expect(journalState?.oldestSequence).toBe(
      journalState?.events[0]?.sequence,
    );
    expect(journalState?.latestSequence).toBe(eventCount + 1);
  });

  it("drops the connection-scoped writer lease on close without losing retry state", () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.setState({
      drafts: { "tab:a": "retry me" },
      pendingMutations: {
        "tab:a": {
          mutationId: "mutation-a",
          stableSessionKey: "tab:a",
          text: "retry me",
          attachments: [],
        },
      },
    });

    wsClientState.instance?.callbacks.onStatus({
      kind: "closed",
      reason: "offline",
    });

    expect(useStore.getState().writerLease).toEqual({
      ownerClientInstanceId: null,
      generation: 0,
      expiresAtEpochMs: null,
      youAreOwner: false,
    });
    expect(useStore.getState().workspace?.writerLease.youAreOwner).toBe(false);
    expect(useStore.getState().drafts["tab:a"]).toBe("retry me");
    expect(useStore.getState().pendingMutations["tab:a"]?.mutationId).toBe(
      "mutation-a",
    );

    useStore.getState().applySnapshot(makeSnapshot({ revision: 2 }));
    wsClientState.instance?.callbacks.onStatus({ kind: "connecting" });
    expect(useStore.getState().writerLease.youAreOwner).toBe(false);
    expect(useStore.getState().writerLease.generation).toBe(0);
  });

  it("does not persist snapshots, journals, or drafts", () => {
    const storage = storageMock();
    vi.stubGlobal("localStorage", storage);
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.getState().setDraft("tab:a", "draft");
    useStore.getState().beginSemanticReplay(
      replayDescriptor({ fromSequence: 0, throughSequence: 1 }),
    );
    useStore.getState().applySemanticReplayPage(
      replayPage({
        fromSequence: 0,
        throughSequence: 1,
        nextSequence: 1,
        events: [outputEvent(1)],
      }),
    );

    const persistedKeys = storage.setItem.mock.calls.map(([key]) => key);
    expect(persistedKeys).not.toContain("devmanager-workspace");
    expect(persistedKeys).not.toContain("devmanager-journals");
    expect(persistedKeys).not.toContain("devmanager-drafts");
    expect(persistedKeys).not.toContain("devmanager-active-terminal");
  });

  it("ignores an old runtime composer settlement even when ids collide", async () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    const client = wsClientState.instance;
    const oldRuntime = deferred<ComposerAccepted>();
    const newRuntime = deferred<ComposerAccepted>();
    client?.submitComposer
      .mockReturnValueOnce(oldRuntime.promise)
      .mockReturnValueOnce(newRuntime.promise);

    const oldSubmission = useStore.getState().submitComposer("tab:a", "old");
    useStore.getState().applySnapshot(
      makeSnapshot({ runtimeInstanceId: "runtime-2", revision: 1 }),
    );
    const newSubmission = useStore.getState().submitComposer("tab:a", "new");
    const collidedId = client?.submitComposer.mock.calls[1]?.[0].mutationId ?? "";

    oldRuntime.resolve({
      mutationId: collidedId,
      stableSessionKey: "tab:a",
      acceptedSequence: 3,
      leaseGeneration: 7,
    });
    await oldSubmission;

    expect(useStore.getState().drafts["tab:a"]).toBe("new");
    expect(useStore.getState().pendingMutations["tab:a"]?.mutationId).toBe(
      collidedId,
    );

    newRuntime.resolve({
      mutationId: collidedId,
      stableSessionKey: "tab:a",
      acceptedSequence: 1,
      leaseGeneration: 7,
    });
    await newSubmission;
  });

  it("ignores resolved and rejected action promises from an old runtime", async () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    const client = wsClientState.instance;
    const resolved = deferred<{ ok: boolean; message: string; payload: null }>();
    client?.request.mockReturnValueOnce(resolved.promise);

    useStore.getState().sendAction({ type: "stopAllServers" });
    useStore.getState().applySnapshot(
      makeSnapshot({ runtimeInstanceId: "runtime-2", revision: 1 }),
    );
    resolved.resolve({ ok: false, message: "old failure", payload: null });
    await resolved.promise;
    await Promise.resolve();
    expect(useStore.getState().lastError).toBeNull();

    const rejected = deferred<{ ok: boolean; payload: null }>();
    client?.request.mockReturnValueOnce(rejected.promise);
    useStore.getState().sendAction({ type: "stopAllServers" });
    useStore.getState().applySnapshot(
      makeSnapshot({ runtimeInstanceId: "runtime-3", revision: 1 }),
    );
    rejected.reject(new Error("stale rejection"));
    await rejected.promise.catch(() => {});
    await Promise.resolve();
    expect(useStore.getState().lastError).toBeNull();
  });

  it("does not navigate from an old runtime AI launch result", async () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    const client = wsClientState.instance;
    const result = deferred<{
      ok: boolean;
      message: null;
      payload: {
        type: "aiTab";
        tabId: string;
        projectId: string;
        tabType: "claude";
        sessionId: string;
        label: null;
      };
    }>();
    client?.request.mockReturnValueOnce(result.promise);

    const launch = useStore.getState().launchAiTab("project-1", "claude");
    useStore.getState().applySnapshot(
      makeSnapshot({ runtimeInstanceId: "runtime-2", sessions: [], tabs: [] }),
    );
    result.resolve({
      ok: true,
      message: null,
      payload: {
        type: "aiTab",
        tabId: "old-tab",
        projectId: "project-1",
        tabType: "claude",
        sessionId: "old-pty",
        label: null,
      },
    });
    await launch;

    expect(useStore.getState().activeSessionKey).toBeNull();
    expect(
      useStore.getState().rawTerminal.streamSessionIdByStableKey["tab:old-tab"],
    ).toBeUndefined();
  });
});

describe("semantic journal reconciliation", () => {
  it("accepts the Rust enum's snake-case flattened semantic fields", () => {
    const event: SemanticEvent = {
      stableSessionKey: "tab:a",
      sequence: 1,
      occurredAtEpochMs: 100,
      source: "claude",
      kind: "assistantMessage",
      message_id: "message-1",
      text: "hello",
      streaming: false,
    };

    useStore.getState().appendSemanticEvent(event);

    expect(useStore.getState().journals["tab:a"]?.events[0]).toMatchObject({
      message_id: "message-1",
    });
  });

  it("applies immutable pages in order and trusts their authoritative cursor", () => {
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.setState({ journals: { "tab:a": journal([outputEvent(1), outputEvent(2)]) } });

    useStore.getState().beginSemanticReplay(replayDescriptor());
    const first = replayPage({
      nextSequence: 4,
      complete: false,
      events: [outputEvent(3), outputEvent(4)],
    });
    useStore.getState().applySemanticReplayPage(first);
    useStore.getState().applySemanticReplayPage(first);
    useStore.getState().applySemanticReplayPage(
      replayPage({
        fromSequence: 4,
        events: [outputEvent(6)],
      }),
    );
    useStore.getState().appendSemanticEvent(outputEvent(7, "repeated-live"));

    const reconciled = useStore.getState().journals["tab:a"];
    expect(reconciled?.events.map((event) => event.sequence)).toEqual([
      1, 2, 3, 4, 6,
    ]);
    expect(reconciled?.latestSequence).toBe(7);
    expect(useStore.getState().semanticReplay).toBeNull();
  });

  it("clears stale local history as soon as the descriptor reports rollover", () => {
    useStore.setState({ journals: { "tab:a": journal([outputEvent(1), outputEvent(2)]) } });

    useStore.getState().beginSemanticReplay(
      replayDescriptor({ throughSequence: 6, rollover: true }),
    );
    expect(useStore.getState().journals["tab:a"]?.events).toEqual([]);
    useStore.getState().applySemanticReplayPage(
      replayPage({
        throughSequence: 6,
        nextSequence: 6,
        rollover: true,
        events: [outputEvent(5), outputEvent(6)],
      }),
    );

    const reconciled = useStore.getState().journals["tab:a"];
    expect(reconciled?.events.map((event) => event.sequence)).toEqual([5, 6]);
    expect(reconciled?.oldestSequence).toBe(5);
    expect(reconciled?.latestSequence).toBe(6);
    expect(reconciled?.cursorRolledOver).toBe(true);
  });

  it("ignores stale and out-of-order pages without corrupting the active replay", () => {
    useStore.setState({
      journals: {
        "tab:a": journal([outputEvent(1), outputEvent(2)]),
      },
    });

    useStore.getState().beginSemanticReplay(replayDescriptor());
    useStore.getState().applySemanticReplayPage(
      replayPage({ replayId: 9, events: [outputEvent(3)] }),
    );
    useStore.getState().applySemanticReplayPage(
      replayPage({ fromSequence: 4, events: [outputEvent(5)] }),
    );

    expect(
      useStore.getState().journals["tab:a"]?.events.map((event) => event.sequence),
    ).toEqual([1, 2]);
    expect(useStore.getState().semanticReplay?.nextSequence).toBe(2);
  });

  it("starts from ResumeState and consumes following FIFO page messages", () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    const client = wsClientState.instance;

    client?.callbacks.onMessage({
      type: "resumeState",
      runtimeInstanceId: "runtime-1",
      revision: 1,
      hardReset: false,
      route: "/session/tab/a",
      desiredSessionKey: "tab:a",
      workspace: null,
      semanticReplay: replayDescriptor({ fromSequence: 0, throughSequence: 2 }),
      writerLease,
    });
    expect(useStore.getState().semanticReplay?.replayId).toBe(10);

    client?.callbacks.onMessage({
      type: "semanticReplayPage",
      ...replayPage({
        fromSequence: 0,
        throughSequence: 2,
        nextSequence: 2,
        events: [outputEvent(1), outputEvent(2)],
      }),
    });
    expect(
      useStore.getState().journals["tab:a"]?.events.map((event) => event.sequence),
    ).toEqual([1, 2]);
    expect(useStore.getState().semanticReplay).toBeNull();
  });

  it("buffers ordered live events during replay and flushes them exactly once", () => {
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.setState({
      activeSessionKey: "tab:a",
      journals: { "tab:a": journal([outputEvent(1), outputEvent(2)]) },
    });
    useStore.getState().beginSemanticReplay(
      replayDescriptor({ fromSequence: 2, throughSequence: 4 }),
    );
    useStore.getState().applySemanticReplayPage(
      replayPage({
        fromSequence: 2,
        throughSequence: 4,
        nextSequence: 3,
        complete: false,
        events: [outputEvent(3)],
      }),
    );

    useStore.getState().appendSemanticEvent(outputEvent(5, "live-five"));
    useStore.getState().appendSemanticEvent(outputEvent(5, "duplicate-five"));
    useStore.getState().appendSemanticEvent(outputEvent(6, "live-six"));
    expect(
      useStore.getState().journals["tab:a"]?.events.map((event) => event.sequence),
    ).toEqual([1, 2, 3]);

    useStore.getState().applySemanticReplayPage(
      replayPage({
        fromSequence: 3,
        throughSequence: 4,
        nextSequence: 4,
        complete: true,
        events: [outputEvent(4)],
      }),
    );

    const events = useStore.getState().journals["tab:a"]?.events ?? [];
    expect(events.map((event) => event.sequence)).toEqual([1, 2, 3, 4, 5, 6]);
    expect(events.find((event) => event.sequence === 5)).toMatchObject({
      text: "live-five",
    });
    expect(useStore.getState().semanticReplay).toBeNull();
  });

  it("removes the retained event replaced by a newer live semantic event", () => {
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.setState({
      activeSessionKey: "tab:a",
      journals: {
        "tab:a": journal([outputEvent(1), outputEvent(2), outputEvent(3)]),
      },
    });

    useStore.getState().appendSemanticEvent({
      ...outputEvent(4, "replacement"),
      replacesSequence: 2,
    });

    expect(
      useStore.getState().journals["tab:a"]?.events.map((event) => event.sequence),
    ).toEqual([1, 3, 4]);
  });

  it("applies replacement metadata carried by a replay page", () => {
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.setState({
      activeSessionKey: "tab:a",
      journals: { "tab:a": journal([outputEvent(1), outputEvent(2)]) },
    });
    useStore.getState().beginSemanticReplay(
      replayDescriptor({ fromSequence: 2, throughSequence: 4 }),
    );

    useStore.getState().applySemanticReplayPage(
      replayPage({
        fromSequence: 2,
        throughSequence: 4,
        nextSequence: 4,
        complete: true,
        events: [
          outputEvent(3),
          { ...outputEvent(4, "replacement"), replacesSequence: 2 },
        ],
      }),
    );

    expect(
      useStore.getState().journals["tab:a"]?.events.map((event) => event.sequence),
    ).toEqual([1, 3, 4]);
  });

  it("requests one atomic replay for a live gap without advancing the cursor", () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.setState({
      activeSessionKey: "tab:a",
      journals: { "tab:a": journal([outputEvent(1), outputEvent(2)]) },
    });
    const client = wsClientState.instance;
    client?.wake.mockClear();

    useStore.getState().appendSemanticEvent(outputEvent(4));
    useStore.getState().appendSemanticEvent(outputEvent(5));

    expect(
      useStore.getState().journals["tab:a"]?.events.map((event) => event.sequence),
    ).toEqual([1, 2]);
    expect(useStore.getState().journals["tab:a"]?.latestSequence).toBe(2);
    expect(client?.callbacks.getResumeContext?.()).toMatchObject({
      semanticAfterSequence: 2,
    });
    expect(client?.wake).toHaveBeenCalledTimes(1);

    useStore.getState().applyResumeState({
      runtimeInstanceId: "runtime-1",
      revision: 1,
      hardReset: false,
      route: "/session/tab/a",
      desiredSessionKey: "tab:a",
      workspace: null,
      semanticReplay: null,
      writerLease,
    });
    useStore.getState().appendSemanticEvent(outputEvent(6));
    expect(client?.wake).toHaveBeenCalledTimes(1);
    expect(useStore.getState().journals["tab:a"]?.latestSequence).toBe(6);
    expect(useStore.getState().journals["tab:a"]?.cursorRolledOver).toBe(true);
    expect([...useStore.getState().semanticGapKeys]).toEqual([]);

    useStore.getState().appendSemanticEvent(outputEvent(8));
    expect(useStore.getState().journals["tab:a"]?.latestSequence).toBe(6);
    expect(client?.wake).toHaveBeenCalledTimes(2);
  });

  it("rebases a gap with no local journal when Resume has no replay", () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.setState({ activeSessionKey: "tab:a", journals: {} });
    const client = wsClientState.instance;
    client?.wake.mockClear();

    useStore.getState().appendSemanticEvent(outputEvent(4));
    useStore.getState().appendSemanticEvent(outputEvent(5));
    expect(useStore.getState().journals["tab:a"]).toBeUndefined();
    expect(client?.wake).toHaveBeenCalledTimes(1);

    useStore.getState().applyResumeState({
      runtimeInstanceId: "runtime-1",
      revision: 1,
      hardReset: false,
      route: "/session/tab/a",
      desiredSessionKey: "tab:a",
      workspace: null,
      semanticReplay: null,
      writerLease,
    });
    expect(useStore.getState().journals["tab:a"]).toMatchObject({
      oldestSequence: 0,
      latestSequence: 5,
      cursorRolledOver: true,
      events: [],
    });

    useStore.getState().appendSemanticEvent(outputEvent(6));
    expect(useStore.getState().journals["tab:a"]?.latestSequence).toBe(6);
    expect(client?.wake).toHaveBeenCalledTimes(1);
  });
});

describe("resume ownership and stable session identity", () => {
  it("builds Resume from the stable route and semantic cursor", () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.setState({ journals: { "tab:a": journal([outputEvent(12)]) } });

    useStore.getState().setActiveSession("pty-a");

    const client = wsClientState.instance;
    expect(useStore.getState().activeSessionKey).toBe("tab:a");
    expect(useStore.getState().rawTerminal.activeStreamSessionId).toBe("pty-a");
    expect(client?.callbacks.getResumeContext?.()).toMatchObject({
      seenRuntimeInstanceId: "runtime-1",
      seenRevision: 1,
      route: "/session/tab/a",
      desiredSessionKey: "tab:a",
      semanticAfterSequence: 12,
      wantsWriterLease: true,
    });
    expect(client?.wake).toHaveBeenCalledTimes(1);
    expect(client?.send).not.toHaveBeenCalledWith(
      expect.objectContaining({
        type: expect.stringMatching(/focus|subscribe|takeControl|claimControl/i),
      }),
    );
  });

  it("foreground recovery never lies about visibility or emits legacy control frames", () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.getState().setActiveSession("tab:a");
    const client = wsClientState.instance;
    client?.wake.mockClear();

    useStore.getState().refreshActiveConnection();
    useStore.getState().foregroundConnection();

    expect(client?.wake).toHaveBeenCalledTimes(1);
    expect(client?.foreground).toHaveBeenCalledTimes(1);
    expect(client?.setVisibility).not.toHaveBeenCalledWith(false);
    const sentTypes = client?.send.mock.calls.map(([frame]) => frame.type) ?? [];
    expect(sentTypes).not.toEqual(
      expect.arrayContaining([
        "focusSession",
        "subscribeSessions",
        "unsubscribeSessions",
        "takeControl",
        "claimControlIfAvailable",
        "releaseControl",
      ]),
    );
  });

  it("keeps PTY stream ids inside the raw-terminal slice", () => {
    useStore.getState().applySnapshot(makeSnapshot());
    const state = useStore.getState();

    expect(state.sessions["tab:a"]?.stableSessionKey).toBe("tab:a");
    expect(state.rawTerminal.streamSessionIdByStableKey["tab:a"]).toBe("pty-a");
    expect("activeSessionId" in state).toBe(false);
  });

  it("does not emit a second Resume when opening the already-selected AI tab", async () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.getState().setActiveSession("pty-a");
    const client = wsClientState.instance;
    client?.wake.mockClear();

    await useStore.getState().openAiTab("a");

    expect(client?.wake).not.toHaveBeenCalled();
    expect(useStore.getState().activeSessionKey).toBe("tab:a");
  });
});

describe("composer acknowledgement and retries", () => {
  it("keeps the draft but gives a retry a new id after terminal rejection", async () => {
    vi.stubGlobal("crypto", {
      randomUUID: vi
        .fn()
        .mockReturnValueOnce("mutation-first")
        .mockReturnValueOnce("mutation-retry"),
    });
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    const client = wsClientState.instance;
    const first = deferred<ComposerAccepted>();
    const second = deferred<ComposerAccepted>();
    client?.submitComposer
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise);

    const firstAttempt = useStore.getState().submitComposer("tab:a", "hello");
    const firstMutationId = client?.submitComposer.mock.calls[0]?.[0].mutationId;
    expect(useStore.getState().drafts["tab:a"]).toBe("hello");

    first.reject(new Error("websocket closed"));
    await expect(firstAttempt).rejects.toThrow("websocket closed");
    expect(useStore.getState().drafts["tab:a"]).toBe("hello");
    expect(useStore.getState().pendingMutations["tab:a"]).toBeUndefined();

    const retry = useStore.getState().submitComposer("tab:a", "hello");
    const retryMutationId = client?.submitComposer.mock.calls[1]?.[0].mutationId;
    expect(retryMutationId).not.toBe(firstMutationId);
    second.resolve({
      mutationId: retryMutationId ?? "",
      stableSessionKey: "tab:a",
      acceptedSequence: 3,
      leaseGeneration: 7,
    });
    await retry;
    expect(useStore.getState().drafts["tab:a"]).toBe("");
    expect(useStore.getState().pendingMutations).toEqual({});
  });

  it("clears only the accepted submitted draft, not text edited while pending", async () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    const client = wsClientState.instance;
    const accepted = deferred<ComposerAccepted>();
    client?.submitComposer.mockReturnValueOnce(accepted.promise);

    const submission = useStore.getState().submitComposer("tab:a", "original");
    useStore.getState().setDraft("tab:a", "edited");
    const mutationId = client?.submitComposer.mock.calls[0]?.[0].mutationId ?? "";
    accepted.resolve({
      mutationId,
      stableSessionKey: "tab:a",
      acceptedSequence: 3,
      leaseGeneration: 7,
    });
    await submission;

    expect(useStore.getState().drafts["tab:a"]).toBe("edited");
    expect(useStore.getState().pendingMutations).toEqual({});
  });

  it("retains the draft but clears the mutation id when host capacity rejects it", async () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    const client = wsClientState.instance;
    const capacity = deferred<ComposerAccepted>();
    client?.submitComposer.mockReturnValueOnce(capacity.promise);

    const submission = useStore.getState().submitComposer("tab:a", "try later");
    const mutationId = client?.submitComposer.mock.calls[0]?.[0].mutationId;
    capacity.reject(
      Object.assign(new Error("mutation capacity reached"), {
        code: "capacityExceeded",
      }),
    );
    await expect(submission).rejects.toMatchObject({
      code: "capacityExceeded",
    });

    expect(useStore.getState().drafts["tab:a"]).toBe("try later");
    expect(mutationId).toBeDefined();
    expect(useStore.getState().pendingMutations["tab:a"]).toBeUndefined();
  });

  it("cancels the old automatic retry when edited content becomes a new mutation", async () => {
    vi.stubGlobal("crypto", {
      randomUUID: vi
        .fn()
        .mockReturnValueOnce("mutation-first")
        .mockReturnValueOnce("mutation-edited"),
    });
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    const client = wsClientState.instance;
    const first = deferred<ComposerAccepted>();
    const second = deferred<ComposerAccepted>();
    client?.submitComposer
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise);

    const firstAttempt = useStore.getState().submitComposer("tab:a", "first");
    const firstMutationId = client?.submitComposer.mock.calls[0]?.[0].mutationId;
    useStore.getState().setDraft("tab:a", "edited");
    expect(client?.cancelComposer).toHaveBeenCalledWith(
      firstMutationId,
      "composer mutation superseded",
    );
    expect(useStore.getState().pendingMutations["tab:a"]).toBeUndefined();

    const secondAttempt = useStore.getState().submitComposer("tab:a", "edited");
    const secondMutationId = client?.submitComposer.mock.calls[1]?.[0].mutationId;

    expect(secondMutationId).not.toBe(firstMutationId);

    first.resolve({
      mutationId: firstMutationId ?? "",
      stableSessionKey: "tab:a",
      acceptedSequence: 3,
      leaseGeneration: 7,
    });
    second.resolve({
      mutationId: secondMutationId ?? "",
      stableSessionKey: "tab:a",
      acceptedSequence: 4,
      leaseGeneration: 7,
    });
    await firstAttempt;
    await secondAttempt;
    expect(useStore.getState().drafts["tab:a"]).toBe("");
  });
});

describe("safe compatibility and raw terminal IO", () => {
  it("derives the old terminal view from the safe flat DTO", () => {
    useStore.getState().applySnapshot(makeSnapshot());

    const state = useStore.getState();
    expect(state.workspace?.projects[0]?.folders[0]?.commands[0]?.label).toBe("Server");
    expect(state.snapshot?.appState.config.projects[0]?.name).toBe("Project");
    expect(state.snapshot?.runtimeState.sessions["pty-a"]?.stable_session_key).toBe(
      "tab:a",
    );
    expect(JSON.stringify(state.snapshot)).not.toContain("command\"");
    expect(JSON.stringify(state.snapshot)).not.toContain("password");
  });

  it("stages raw input and safe actions for automatic writer acquisition", () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    const client = wsClientState.instance;

    useStore.getState().sendInput("pty-a", "ls\r");
    useStore.getState().pasteImage("pty-a", {
      mimeType: "image/png",
      fileName: "screen.png",
      dataBase64: "aGVsbG8=",
    });
    useStore.getState().sendResize("pty-a", 30, 100);
    useStore.getState().sendAction({ type: "startServer", command_id: "command-1" });

    expect(client?.sendWithWriterLease).toHaveBeenNthCalledWith(1, {
      type: "input",
      sessionId: "pty-a",
      text: "ls\r",
    });
    expect(client?.sendWithWriterLease).toHaveBeenNthCalledWith(2, {
      type: "pasteImage",
      sessionId: "pty-a",
      mimeType: "image/png",
      fileName: "screen.png",
      dataBase64: "aGVsbG8=",
    });
    expect(client?.sendWithWriterLease).toHaveBeenNthCalledWith(3, {
      type: "resize",
      sessionId: "pty-a",
      rows: 30,
      cols: 100,
    });
    expect(client?.send).not.toHaveBeenCalled();
    expect(client?.request).toHaveBeenCalledWith({
      type: "startServer",
      command_id: "command-1",
    });
  });
});
