import { beforeEach, describe, expect, it, vi } from "vitest";

import type {
  ComposerAccepted,
  SemanticEvent,
  SemanticJournalState,
  SemanticReplayDescriptor,
  SemanticReplayPage,
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
    readonly request = vi.fn(async () => ({ ok: true, payload: null }));
    readonly wake = vi.fn();
    readonly setVisibility = vi.fn();
    readonly resetRuntime = vi.fn();
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

import { useStore } from "./index";

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

function journal(events: SemanticEvent[]): SemanticJournalState {
  return {
    stableSessionKey: "tab:a",
    oldestSequence: events[0]?.sequence ?? 0,
    latestSequence: events[events.length - 1]?.sequence ?? 0,
    cursorRolledOver: false,
    events,
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

  it("applies immutable pages in order, deduplicates repeats, and advances gaps", () => {
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
      1, 2, 3, 4, 6, 7,
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

  it("refresh and control affordances never emit legacy focus/subscribe/control frames", () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    useStore.getState().setActiveSession("tab:a");
    const client = wsClientState.instance;
    client?.wake.mockClear();

    useStore.getState().refreshActiveConnection();
    useStore.getState().takeControl();
    useStore.getState().releaseControl();

    expect(client?.wake).toHaveBeenCalledTimes(2);
    expect(client?.setVisibility).toHaveBeenCalledWith(false);
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
});

describe("composer acknowledgement and retries", () => {
  it("keeps the draft through rejection and reuses the mutation id on retry", async () => {
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
    expect(useStore.getState().pendingMutations["tab:a"]?.mutationId).toBe(
      firstMutationId,
    );

    const retry = useStore.getState().submitComposer("tab:a", "hello");
    expect(client?.submitComposer.mock.calls[1]?.[0].mutationId).toBe(firstMutationId);
    second.resolve({
      mutationId: firstMutationId ?? "",
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

  it("retains the draft and mutation id when host mutation capacity is full", async () => {
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
    expect(useStore.getState().pendingMutations["tab:a"]?.mutationId).toBe(
      mutationId,
    );
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

  it("adds the automatic lease generation to raw input and safe actions", () => {
    useStore.getState().init();
    useStore.getState().applySnapshot(makeSnapshot());
    const client = wsClientState.instance;

    useStore.getState().sendInput("pty-a", "ls\r");
    useStore.getState().sendAction({ type: "startServer", command_id: "command-1" });

    expect(client?.ensureWriterLease).toHaveBeenCalledTimes(1);
    expect(client?.send).toHaveBeenNthCalledWith(1, {
      type: "input",
      sessionId: "pty-a",
      text: "ls\r",
      expectedLeaseGeneration: 7,
    });
    expect(client?.send).toHaveBeenCalledTimes(1);
    expect(client?.request).toHaveBeenCalledWith({
      type: "startServer",
      command_id: "command-1",
    });
  });
});
