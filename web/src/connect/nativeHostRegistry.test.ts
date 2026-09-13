// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import { createMemoryNativeCacheStore } from "./nativeCache";
import { NativeHostRegistry } from "./nativeHostRegistry";
import type { NativeFleetHostDescriptor } from "./fleetDescriptor";
import type { ConnectBootstrapHandle } from "./identity";
import type { ConnectBrowserTransport } from "./transport";
import { DeferredNativeTransport } from "./deferredNativeTransport";
import { buildSubmitProviderInputSendNow } from "./nativeProtocol";
import { HostTrustHoldError } from "./hostTrust";
import type { ConnectConnectionState } from "./transport";

const PAGE_ID = "01234567-89ab-7000-8000-000000000001";
const REMOTE_ID = "01234567-89ab-7000-8000-000000000002";
const TASK = "01234567-89ab-7000-8000-0000000000aa";
const CLIENT = "01234567-89ab-7000-8000-0000000000cc";
const COMMAND = "01234567-89ab-7000-8000-0000000000dd";
const AGENT = "01234567-89ab-7000-8000-0000000000ee";

function descriptor(
  overrides: Partial<NativeFleetHostDescriptor> &
    Pick<NativeFleetHostDescriptor, "hostPublicId" | "isPageHost">,
): NativeFleetHostDescriptor {
  return {
    hostPublicKey: overrides.isPageHost ? "aa".repeat(32) : "bb".repeat(32),
    origin: overrides.isPageHost ? "http://127.0.0.1:8787" : "https://studio.example",
    label: overrides.isPageHost ? "Page" : "Studio",
    generation: 1,
    protocolMajor: 1,
    protocolMinor: 0,
    ...overrides,
  };
}

function fakeHandle(hostPublicId: string): ConnectBootstrapHandle {
  const transport = {
    start: vi.fn(async () => undefined),
    stop: vi.fn(),
    suspend: vi.fn(),
    wake: vi.fn(),
    subscribe: vi.fn((listener: (state: { kind: string }) => void) => {
      listener({ kind: "ready" });
      return () => undefined;
    }),
    subscribeEnvelope: vi.fn(() => () => undefined),
    request: vi.fn(),
  };
  return {
    marker: {
      transport: "connect",
      endpoint: "/api/connect",
      generation: 1,
      protocolMajor: 1,
      protocolMinor: 0,
      hostPublicId,
    },
    identity: {
      deviceId: "connect-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      publicKey: new Uint8Array(32).fill(1),
      hostGeneration: 1,
    },
    transport: transport as unknown as ConnectBrowserTransport,
    suspend: () => transport.suspend(),
    stop: () => transport.stop(),
  };
}

/** Real Deferred-attached port with synchronous wake/state emissions. */
function controllablePort(options: {
  onWake?: (emit: (state: ConnectConnectionState) => void) => "resume" | "reconnect" | "held" | "start";
  initialState?: ConnectConnectionState;
}) {
  let stateListener: ((state: ConnectConnectionState) => void) | null = null;
  const emit = (state: ConnectConnectionState) => stateListener?.(state);
  const port = {
    start: vi.fn(async () => undefined),
    stop: vi.fn(),
    suspend: vi.fn(),
    wake: vi.fn(() => {
      if (options.onWake) return options.onWake(emit);
      emit({ kind: "connecting" });
      return "reconnect" as const;
    }),
    requestResync: vi.fn(() => false),
    request: vi.fn(),
    subscribe: vi.fn((listener: (state: ConnectConnectionState) => void) => {
      stateListener = listener;
      listener(options.initialState ?? { kind: "ready" });
      return () => {
        stateListener = null;
      };
    }),
    subscribeEnvelope: vi.fn(() => () => undefined),
    emit,
  };
  return port;
}

function handleFromPort(
  hostPublicId: string,
  port: ReturnType<typeof controllablePort>,
): ConnectBootstrapHandle {
  return {
    marker: {
      transport: "connect",
      endpoint: "/api/connect",
      generation: 1,
      protocolMajor: 1,
      protocolMinor: 0,
      hostPublicId,
    },
    identity: {
      deviceId: "connect-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      publicKey: new Uint8Array(32).fill(1),
      hostGeneration: 1,
    },
    transport: port as unknown as ConnectBrowserTransport,
    suspend: () => port.suspend(),
    stop: () => port.stop(),
  };
}

function setDocumentVisible(visible: boolean): void {
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    get: () => (visible ? "visible" : "hidden"),
  });
}

const registries: NativeHostRegistry[] = [];

function track(registry: NativeHostRegistry): NativeHostRegistry {
  registries.push(registry);
  return registry;
}

afterEach(() => {
  setDocumentVisible(true);
  while (registries.length > 0) {
    registries.pop()?.stop();
  }
});

describe("NativeHostRegistry corrections", () => {
  it("does not wake or attach before sessionStarted; delayed hydrate stays cache-first", async () => {
    setDocumentVisible(true);
    let resolveHydrate!: () => void;
    const hydrateGate = new Promise<void>((resolve) => {
      resolveHydrate = resolve;
    });
    const pageCache = createMemoryNativeCacheStore(() => 20);
    await pageCache.putTasks(PAGE_ID, [
      {
        taskId: TASK,
        revision: 1,
        actionEpoch: 1,
        title: "Page task",
        lifecycle: "open",
        projectId: null,
        environmentId: null,
        createdAtMs: null,
        connectivity: null,
        attention: null,
        activity: null,
        primaryAgentId: null,
        updatedAtMs: 10,
      },
    ]);
    const bootstrapPage = vi.fn(async () => fakeHandle(PAGE_ID));
    const registry = track(
      new NativeHostRegistry({
        hosts: [descriptor({ hostPublicId: PAGE_ID, isPageHost: true })],
        createCache: () => {
          const store = createMemoryNativeCacheStore();
          const loadHost = pageCache.loadHost.bind(pageCache);
          store.loadHost = async (id) => {
            await hydrateGate;
            return loadHost(id);
          };
          return store;
        },
        bootstrapPageHost: bootstrapPage,
      }),
    );
    registry.start();
    registry.start(); // idempotent
    expect(bootstrapPage).not.toHaveBeenCalled();
    resolveHydrate?.();
    await expect.poll(() => registry.get(PAGE_ID)?.hydrationKnown === true).toBe(true);
    await expect.poll(() => bootstrapPage.mock.calls.length).toBe(1);
    expect(registry.get(PAGE_ID)?.session.view().tasks.size).toBe(1);
  });

  it("cancels late bootstrap after StrictMode stop", async () => {
    setDocumentVisible(true);
    let attachResolve!: (handle: ConnectBootstrapHandle) => void;
    const pending = new Promise<ConnectBootstrapHandle>((resolve) => {
      attachResolve = resolve;
    });
    const registry = track(
      new NativeHostRegistry({
        hosts: [descriptor({ hostPublicId: PAGE_ID, isPageHost: true })],
        createCache: () => createMemoryNativeCacheStore(),
        bootstrapPageHost: async () => pending,
      }),
    );
    registry.start();
    await expect.poll(() => registry.get(PAGE_ID)?.hydrationKnown === true).toBe(true);
    registry.stop();
    const handle = fakeHandle(PAGE_ID);
    attachResolve?.(handle);
    await Promise.resolve();
    await Promise.resolve();
    expect(handle.transport.stop).toHaveBeenCalled();
  });

  it("keeps Pair reachable after unauthenticated attach and replaces only that transport", async () => {
    setDocumentVisible(true);
    const first = fakeHandle(REMOTE_ID);
    const second = fakeHandle(REMOTE_ID);
    let calls = 0;
    const registry = track(
      new NativeHostRegistry({
        hosts: [
          descriptor({ hostPublicId: PAGE_ID, isPageHost: true }),
          descriptor({ hostPublicId: REMOTE_ID, isPageHost: false }),
        ],
        createCache: () => createMemoryNativeCacheStore(),
        bootstrapPageHost: async () => fakeHandle(PAGE_ID),
        bootstrapRemoteHost: async (_entry, grant) => {
          calls += 1;
          if (calls === 1) {
            expect(grant).toBeNull();
            return first;
          }
          expect(grant?.grant).toBe("grant-1");
          return second;
        },
      }),
    );
    registry.start();
    await expect
      .poll(() => registry.get(REMOTE_ID)?.transportAttached === true)
      .toBe(true);
    const entry = registry.get(REMOTE_ID)!;
    expect(entry.transport.isAttached()).toBe(true);
    entry.session.fenceTransportReplacement();
    expect(entry.session.view().connectionStatus).toBe("connecting");
    registry.submitPairGrant(REMOTE_ID, { grant: "grant-1" });
    await expect.poll(() => calls).toBe(2);
    expect(first.transport.stop).toHaveBeenCalled();
    expect(entry.transport.isAttached()).toBe(true);
  });

  it("allows known-pin resume retry without a second grant after attach", async () => {
    setDocumentVisible(true);
    let calls = 0;
    const registry = track(
      new NativeHostRegistry({
        hosts: [descriptor({ hostPublicId: REMOTE_ID, isPageHost: false })],
        documentRemoteOrigins: new Set(["https://studio.example"]),
        createCache: () => createMemoryNativeCacheStore(),
        bootstrapRemoteHost: async (_entry, grant) => {
          calls += 1;
          expect(grant).toBeNull();
          return fakeHandle(REMOTE_ID);
        },
      }),
    );
    registry.start();
    await expect.poll(() => calls).toBe(1);
    registry.retryHost(REMOTE_ID);
    await expect.poll(() => calls).toBe(2);
  });

  it("suspends a just-started session on hidden/pagehide during hydrate and skips bootstrap until foreground", async () => {
    setDocumentVisible(true);
    let resolveHydrate!: () => void;
    const hydrateGate = new Promise<void>((resolve) => {
      resolveHydrate = resolve;
    });
    const bootstrapRemote = vi.fn(async () => fakeHandle(REMOTE_ID));
    const registry = track(
      new NativeHostRegistry({
        hosts: [
          descriptor({ hostPublicId: PAGE_ID, isPageHost: true }),
          descriptor({ hostPublicId: REMOTE_ID, isPageHost: false }),
        ],
        documentRemoteOrigins: new Set(["https://studio.example"]),
        createCache: () => {
          const store = createMemoryNativeCacheStore();
          const inner = store.loadHost.bind(store);
          store.loadHost = async (id) => {
            if (id === REMOTE_ID) await hydrateGate;
            return inner(id);
          };
          return store;
        },
        bootstrapPageHost: async () => fakeHandle(PAGE_ID),
        bootstrapRemoteHost: bootstrapRemote,
      }),
    );
    registry.start();
    await expect.poll(() => registry.get(PAGE_ID)?.hydrationKnown === true).toBe(true);
    setDocumentVisible(false);
    window.dispatchEvent(new Event("pagehide"));
    resolveHydrate?.();
    await expect.poll(() => registry.get(REMOTE_ID)?.hydrationKnown === true).toBe(true);
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(bootstrapRemote).not.toHaveBeenCalled();
    setDocumentVisible(true);
    document.dispatchEvent(new Event("visibilitychange"));
    await expect.poll(() => bootstrapRemote.mock.calls.length).toBe(1);
  });
});

describe("DeferredNativeTransport.replace", () => {
  it("detaches previous callbacks before wiring the replacement", async () => {
    const deferred = new DeferredNativeTransport();
    const first = {
      start: vi.fn(async () => undefined),
      stop: vi.fn(),
      subscribe: vi.fn(() => () => undefined),
      subscribeEnvelope: vi.fn(() => () => undefined),
      request: vi.fn(),
    };
    const second = {
      start: vi.fn(async () => undefined),
      stop: vi.fn(),
      subscribe: vi.fn(() => () => undefined),
      subscribeEnvelope: vi.fn(() => () => undefined),
      request: vi.fn(),
    };
    // start() awaits attach — do not await start before attach (deadlock).
    const started = deferred.start();
    await deferred.attach(first);
    await started;
    await deferred.replace(second);
    expect(first.stop).toHaveBeenCalled();
    expect(second.subscribe).toHaveBeenCalled();
    expect(second.start).toHaveBeenCalled();
    deferred.stop();
  });
});

describe("NativeHostRegistry.reconcileHosts", () => {
  const REMOTE_B = "01234567-89ab-7000-8000-000000000003";
  const TASK_SHARED = "01234567-89ab-7000-8000-0000000000bb";

  it("fences a removed host, keeps unaffected session objects, and hydrates new hosts", async () => {
    setDocumentVisible(true);
    const remoteA = descriptor({ hostPublicId: REMOTE_ID, isPageHost: false });
    const remoteB = descriptor({
      hostPublicId: REMOTE_B,
      isPageHost: false,
      origin: "https://lab.example",
      hostPublicKey: "cc".repeat(32),
    });
    const page = descriptor({ hostPublicId: PAGE_ID, isPageHost: true });
    const bootstraps = new Map<string, number>();
    const registry = track(
      new NativeHostRegistry({
        hosts: [page, remoteA],
        documentRemoteOrigins: new Set([remoteA.origin, remoteB.origin]),
        createCache: () => createMemoryNativeCacheStore(),
        bootstrapPageHost: async () => fakeHandle(PAGE_ID),
        bootstrapRemoteHost: async (entry) => {
          bootstraps.set(
            entry.descriptor.hostPublicId,
            (bootstraps.get(entry.descriptor.hostPublicId) ?? 0) + 1,
          );
          return fakeHandle(entry.descriptor.hostPublicId);
        },
      }),
    );
    const pageSession = registry.get(PAGE_ID)!.session;
    const remoteSession = registry.get(REMOTE_ID)!.session;
    registry.start();
    await expect
      .poll(() => registry.get(REMOTE_ID)?.transportAttached === true)
      .toBe(true);

    const epochBefore = registry.membershipEpoch();
    const result = registry.reconcileHosts([page, remoteB]);
    expect(result).toEqual({
      ok: true,
      membershipChanged: true,
      heldPin: false,
    });
    expect(registry.membershipEpoch()).toBe(epochBefore + 1);
    expect(registry.get(REMOTE_ID)).toBeUndefined();
    expect(registry.get(PAGE_ID)?.session).toBe(pageSession);
    await expect
      .poll(() => registry.get(REMOTE_B)?.transportAttached === true)
      .toBe(true);
    expect(bootstraps.get(REMOTE_B)).toBe(1);
    expect(remoteSession.view().connectionStatus).toBe("stopped");
    await expect(remoteSession.sendText(TASK, "should not send")).resolves.toEqual({
      ok: false,
      reason: "not_ready",
    });
  });

  it("HOLDs pin rotation with fence+stop, preserves durable cache/outbox, blocks late attach", async () => {
    setDocumentVisible(true);
    const page = descriptor({ hostPublicId: PAGE_ID, isPageHost: true });
    const remote = descriptor({ hostPublicId: REMOTE_ID, isPageHost: false });
    const remoteCache = createMemoryNativeCacheStore();
    await remoteCache.putOutbox({
      hostPublicId: REMOTE_ID,
      commandId: COMMAND,
      taskId: TASK,
      clientId: CLIENT,
      status: "pending",
      updatedAtMs: 1,
      issuedAtMs: 1,
      text: "pending",
      commandPayload: buildSubmitProviderInputSendNow({
        authority: {
          hostPublicId: REMOTE_ID,
          clientId: CLIENT,
          requestId: "01234567-89ab-7000-8000-0000000000ff",
        },
        commandId: COMMAND,
        text: "pending",
        issuedAtMs: 1,
        fence: {
          hostPublicId: REMOTE_ID,
          clientId: CLIENT,
          taskId: TASK,
          taskRevision: 1,
          actionEpoch: 1,
          agentSessionId: AGENT,
          runtimeGeneration: 1,
          agentLifecycle: "open",
          providerKind: "codex",
          providerSessionId: null,
          currentTurn: null,
          openQuestion: null,
          openApproval: null,
          pendingWaitCommandIds: [],
        },
      }).payload,
    });

    let resolveLate!: (handle: ConnectBootstrapHandle) => void;
    let bootstraps = 0;
    const registry = track(
      new NativeHostRegistry({
        hosts: [page, remote],
        documentRemoteOrigins: new Set([remote.origin]),
        createCache: (id) =>
          id === REMOTE_ID ? remoteCache : createMemoryNativeCacheStore(),
        bootstrapPageHost: async () => fakeHandle(PAGE_ID),
        bootstrapRemoteHost: async () => {
          bootstraps += 1;
          if (bootstraps === 1) return fakeHandle(REMOTE_ID);
          return new Promise((resolve) => {
            resolveLate = resolve;
          });
        },
      }),
    );
    registry.start();
    await expect
      .poll(() => registry.get(REMOTE_ID)?.transportAttached === true)
      .toBe(true);
    const entry = registry.get(REMOTE_ID)!;
    const session = entry.session;
    registry.retryHost(REMOTE_ID);
    await expect.poll(() => bootstraps).toBe(2);

    const result = registry.reconcileHosts([
      page,
      { ...remote, hostPublicKey: "dd".repeat(32) },
    ]);
    expect(result.ok).toBe(true);
    if (result.ok) expect(result.heldPin).toBe(true);
    expect(registry.get(REMOTE_ID)?.session).toBe(session);
    expect(registry.get(REMOTE_ID)?.pairingState).toBe("held");
    expect(session.view().connectionStatus).toBe("stopped");
    await expect(session.sendText(TASK, "nope")).resolves.toEqual({
      ok: false,
      reason: "not_ready",
    });
    const snapshot = await remoteCache.loadHost(REMOTE_ID);
    expect(snapshot.outbox.some((row) => row.commandId === COMMAND)).toBe(true);

    const late = fakeHandle(REMOTE_ID);
    resolveLate?.(late);
    await Promise.resolve();
    await Promise.resolve();
    expect(late.transport.stop).toHaveBeenCalled();
    expect(entry.transport.isAttached()).toBe(false);
  });

  it("closes a stale bootstrap when the same host is removed and re-added as a new entry", async () => {
    setDocumentVisible(true);
    const page = descriptor({ hostPublicId: PAGE_ID, isPageHost: true });
    const remote = descriptor({ hostPublicId: REMOTE_ID, isPageHost: false });
    let resolveFirst!: (handle: ConnectBootstrapHandle) => void;
    let calls = 0;
    const registry = track(
      new NativeHostRegistry({
        hosts: [page, remote],
        documentRemoteOrigins: new Set([remote.origin]),
        createCache: () => createMemoryNativeCacheStore(),
        bootstrapPageHost: async () => fakeHandle(PAGE_ID),
        bootstrapRemoteHost: async () => {
          calls += 1;
          if (calls === 1) {
            return new Promise((resolve) => {
              resolveFirst = resolve;
            });
          }
          return fakeHandle(REMOTE_ID);
        },
      }),
    );
    registry.start();
    await expect.poll(() => calls).toBe(1);
    const oldEntry = registry.get(REMOTE_ID)!;
    expect(
      registry.reconcileHosts([page]).ok,
    ).toBe(true);
    expect(registry.get(REMOTE_ID)).toBeUndefined();
    expect(registry.reconcileHosts([page, remote]).ok).toBe(true);
    const newEntry = registry.get(REMOTE_ID)!;
    expect(newEntry).not.toBe(oldEntry);
    await expect.poll(() => calls).toBe(2);
    await expect.poll(() => newEntry.transportAttached === true).toBe(true);

    const stale = fakeHandle(REMOTE_ID);
    resolveFirst?.(stale);
    await Promise.resolve();
    await Promise.resolve();
    expect(stale.transport.stop).toHaveBeenCalled();
    expect(newEntry.transport.isAttached()).toBe(true);
    expect(newEntry.session).not.toBe(oldEntry.session);
  });

  it("keeps equal TaskIds isolated across two host sessions after reconcile", async () => {
    setDocumentVisible(true);
    const page = descriptor({ hostPublicId: PAGE_ID, isPageHost: true });
    const remoteA = descriptor({ hostPublicId: REMOTE_ID, isPageHost: false });
    const remoteB = descriptor({
      hostPublicId: REMOTE_B,
      isPageHost: false,
      origin: "https://lab.example",
      hostPublicKey: "cc".repeat(32),
    });
    const cacheA = createMemoryNativeCacheStore(() => 20);
    const cacheB = createMemoryNativeCacheStore(() => 20);
    await cacheA.putTasks(REMOTE_ID, [
      {
        taskId: TASK_SHARED,
        revision: 1,
        actionEpoch: 1,
        title: "A task",
        lifecycle: "open",
        projectId: null,
        environmentId: null,
        createdAtMs: null,
        connectivity: null,
        attention: null,
        activity: null,
        primaryAgentId: null,
        updatedAtMs: 10,
      },
    ]);
    await cacheB.putTasks(REMOTE_B, [
      {
        taskId: TASK_SHARED,
        revision: 1,
        actionEpoch: 1,
        title: "B task",
        lifecycle: "open",
        projectId: null,
        environmentId: null,
        createdAtMs: null,
        connectivity: null,
        attention: null,
        activity: null,
        primaryAgentId: null,
        updatedAtMs: 20,
      },
    ]);
    const registry = track(
      new NativeHostRegistry({
        hosts: [page],
        documentRemoteOrigins: new Set([remoteA.origin, remoteB.origin]),
        createCache: (id) => {
          if (id === REMOTE_ID) return cacheA;
          if (id === REMOTE_B) return cacheB;
          return createMemoryNativeCacheStore();
        },
        bootstrapPageHost: async () => fakeHandle(PAGE_ID),
        bootstrapRemoteHost: async (entry) =>
          fakeHandle(entry.descriptor.hostPublicId),
      }),
    );
    registry.start();
    await expect.poll(() => registry.get(PAGE_ID)?.hydrationKnown).toBe(true);
    expect(registry.reconcileHosts([page, remoteA, remoteB]).ok).toBe(true);
    await expect.poll(() => registry.get(REMOTE_ID)?.hydrationKnown).toBe(true);
    await expect.poll(() => registry.get(REMOTE_B)?.hydrationKnown).toBe(true);
    const titleA = registry.get(REMOTE_ID)!.session.view().tasks.get(TASK_SHARED)
      ?.title;
    const titleB = registry.get(REMOTE_B)!.session.view().tasks.get(TASK_SHARED)
      ?.title;
    expect(titleA).toBe("A task");
    expect(titleB).toBe("B task");
    expect(registry.get(REMOTE_ID)!.session).not.toBe(
      registry.get(REMOTE_B)!.session,
    );
  });
});

describe("NativeHostRegistry wake/reconnect/hold lifecycle", () => {
  const FOREIGN_ID = "01234567-89ab-7000-8000-000000000099";

  it("keeps bootstrap count at 1 across foreground wakes while calling existing transport.wake", async () => {
    setDocumentVisible(true);
    const port = controllablePort({
      onWake: (emit) => {
        emit({ kind: "connecting" });
        return "reconnect";
      },
    });
    const bootstrapPage = vi.fn(async () => handleFromPort(PAGE_ID, port));
    const registry = track(
      new NativeHostRegistry({
        hosts: [descriptor({ hostPublicId: PAGE_ID, isPageHost: true })],
        createCache: () => createMemoryNativeCacheStore(),
        bootstrapPageHost: bootstrapPage,
      }),
    );
    registry.start();
    await expect.poll(() => registry.get(PAGE_ID)?.transportAttached === true).toBe(true);
    expect(bootstrapPage).toHaveBeenCalledTimes(1);

    for (let i = 0; i < 3; i += 1) {
      setDocumentVisible(false);
      window.dispatchEvent(new Event("pagehide"));
      setDocumentVisible(true);
      document.dispatchEvent(new Event("visibilitychange"));
    }
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(bootstrapPage).toHaveBeenCalledTimes(1);
    expect(port.wake.mock.calls.length).toBeGreaterThanOrEqual(3);

    const replacement = controllablePort({});
    bootstrapPage.mockImplementation(async () => handleFromPort(PAGE_ID, replacement));
    registry.retryHost(PAGE_ID);
    await expect.poll(() => bootstrapPage.mock.calls.length).toBe(2);
    expect(port.stop).toHaveBeenCalled();
    expect(registry.get(PAGE_ID)?.transport.isAttached()).toBe(true);
  });

  it("treats ordinary reconnecting as degraded without pairing_required", async () => {
    setDocumentVisible(true);
    const port = controllablePort({
      onWake: (emit) => {
        emit({ kind: "reconnecting" });
        return "reconnect";
      },
    });
    const registry = track(
      new NativeHostRegistry({
        hosts: [
          descriptor({ hostPublicId: PAGE_ID, isPageHost: true }),
          descriptor({ hostPublicId: REMOTE_ID, isPageHost: false }),
        ],
        documentRemoteOrigins: new Set(["https://studio.example"]),
        createCache: () => createMemoryNativeCacheStore(),
        bootstrapPageHost: async () => fakeHandle(PAGE_ID),
        bootstrapRemoteHost: async () => handleFromPort(REMOTE_ID, port),
      }),
    );
    registry.start();
    await expect
      .poll(() => registry.get(REMOTE_ID)?.transportAttached === true)
      .toBe(true);
    expect(registry.get(REMOTE_ID)?.pairingState).toBe("transport_attached");

    setDocumentVisible(false);
    window.dispatchEvent(new Event("pagehide"));
    setDocumentVisible(true);
    document.dispatchEvent(new Event("visibilitychange"));
    await expect
      .poll(() => registry.get(REMOTE_ID)?.session.view().connectionStatus === "degraded")
      .toBe(true);
    expect(registry.get(REMOTE_ID)?.pairingState).toBe("transport_attached");
    expect(registry.snapshots().find((s) => s.descriptor.hostPublicId === REMOTE_ID)?.pairingState).not.toBe(
      "pairing_required",
    );
  });

  it("HOLDs on HostTrustHoldError after forced bootstrap: stopped, not sendable, cache kept", async () => {
    setDocumentVisible(true);
    const remoteCache = createMemoryNativeCacheStore();
    await remoteCache.putDraft(REMOTE_ID, {
      taskId: TASK,
      text: "keep-me",
      updatedAtMs: 5,
    });
    await remoteCache.putOutbox({
      hostPublicId: REMOTE_ID,
      commandId: COMMAND,
      taskId: TASK,
      clientId: CLIENT,
      status: "pending",
      updatedAtMs: 1,
      issuedAtMs: 1,
      text: "pending",
      commandPayload: buildSubmitProviderInputSendNow({
        authority: {
          hostPublicId: REMOTE_ID,
          clientId: CLIENT,
          requestId: "01234567-89ab-7000-8000-0000000000ff",
        },
        commandId: COMMAND,
        text: "pending",
        issuedAtMs: 1,
        fence: {
          hostPublicId: REMOTE_ID,
          clientId: CLIENT,
          taskId: TASK,
          taskRevision: 1,
          actionEpoch: 1,
          agentSessionId: AGENT,
          runtimeGeneration: 1,
          agentLifecycle: "open",
          providerKind: "codex",
          providerSessionId: null,
          currentTurn: null,
          openQuestion: null,
          openApproval: null,
          pendingWaitCommandIds: [],
        },
      }).payload,
    });

    const firstPort = controllablePort({});
    let calls = 0;
    const registry = track(
      new NativeHostRegistry({
        hosts: [
          descriptor({ hostPublicId: PAGE_ID, isPageHost: true }),
          descriptor({ hostPublicId: REMOTE_ID, isPageHost: false }),
        ],
        documentRemoteOrigins: new Set(["https://studio.example"]),
        createCache: (id) =>
          id === REMOTE_ID ? remoteCache : createMemoryNativeCacheStore(),
        bootstrapPageHost: async () => fakeHandle(PAGE_ID),
        bootstrapRemoteHost: async () => {
          calls += 1;
          if (calls === 1) return handleFromPort(REMOTE_ID, firstPort);
          throw new HostTrustHoldError("host trust pin mismatch");
        },
      }),
    );
    registry.start();
    await expect
      .poll(() => registry.get(REMOTE_ID)?.transportAttached === true)
      .toBe(true);
    registry.retryHost(REMOTE_ID);
    await expect
      .poll(() => registry.get(REMOTE_ID)?.pairingState === "held")
      .toBe(true);
    const session = registry.get(REMOTE_ID)!.session;
    expect(session.view().connectionStatus).toBe("stopped");
    await expect(session.sendText(TASK, "blocked")).resolves.toEqual({
      ok: false,
      reason: "not_ready",
    });
    expect(firstPort.stop).toHaveBeenCalled();
    const snap = await remoteCache.loadHost(REMOTE_ID);
    expect(snap.drafts.some((d) => d.taskId === TASK && d.text === "keep-me")).toBe(true);
    expect(snap.outbox.some((row) => row.commandId === COMMAND)).toBe(true);
  });

  it("does not HOLD a replacement entry when a stale wrong-host bootstrap settles", async () => {
    setDocumentVisible(true);
    const page = descriptor({ hostPublicId: PAGE_ID, isPageHost: true });
    const remote = descriptor({ hostPublicId: REMOTE_ID, isPageHost: false });
    let resolveFirst!: (handle: ConnectBootstrapHandle) => void;
    let calls = 0;
    const registry = track(
      new NativeHostRegistry({
        hosts: [page, remote],
        documentRemoteOrigins: new Set([remote.origin]),
        createCache: () => createMemoryNativeCacheStore(),
        bootstrapPageHost: async () => fakeHandle(PAGE_ID),
        bootstrapRemoteHost: async () => {
          calls += 1;
          if (calls === 1) {
            return new Promise((resolve) => {
              resolveFirst = resolve;
            });
          }
          return fakeHandle(REMOTE_ID);
        },
      }),
    );
    registry.start();
    await expect.poll(() => calls).toBe(1);
    const oldEntry = registry.get(REMOTE_ID)!;
    expect(registry.reconcileHosts([page]).ok).toBe(true);
    expect(registry.reconcileHosts([page, remote]).ok).toBe(true);
    const replacement = registry.get(REMOTE_ID)!;
    expect(replacement).not.toBe(oldEntry);
    await expect.poll(() => replacement.transportAttached === true).toBe(true);

    resolveFirst(fakeHandle(FOREIGN_ID));
    await Promise.resolve();
    await Promise.resolve();
    expect(replacement.pairingState).not.toBe("held");
    expect(replacement.transportAttached).toBe(true);
    expect(replacement.session.view().connectionStatus).not.toBe("stopped");
  });
});
