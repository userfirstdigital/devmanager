import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { RemoteAction } from "./types";
import { WEB_PROTOCOL_VERSION } from "./types";
import { CLIENT_WEB_BUILD_ID } from "../pwa/buildCompatibility";
import { WsClient } from "./ws";

const resumeContext = {
  seenRuntimeInstanceId: "runtime-1",
  seenRevision: 7,
  route: "/session/tab/tab-1",
  desiredSessionKey: "tab:tab-1",
  rawSessionId: "pty-tab-1",
  semanticAfterSequence: 12,
  visible: true,
  wantsWriterLease: true,
};

function jsonFrames(socket: FakeWebSocket): Array<Record<string, unknown>> {
  return socket.sent.map((frame) => JSON.parse(frame) as Record<string, unknown>);
}

function clientCallbacks(overrides: Record<string, unknown> = {}) {
  return {
    onStatus: vi.fn(),
    onMessage: vi.fn(),
    onSessionOutput: vi.fn(),
    getResumeContext: vi.fn(() => resumeContext),
    ...overrides,
  };
}

class FakeWebSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSED = 3;
  static instances: FakeWebSocket[] = [];

  readonly sent: string[] = [];
  readonly url: string;
  readyState = FakeWebSocket.CONNECTING;
  binaryType = "";
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;

  constructor(url: string) {
    this.url = url;
    FakeWebSocket.instances.push(this);
  }

  send(data: string): void {
    this.sent.push(data);
  }

  close(): void {
    this.readyState = FakeWebSocket.CLOSED;
    this.onclose?.({ code: 1006, reason: "" } as CloseEvent);
  }

  emitOpen(sendHello = true): void {
    this.readyState = FakeWebSocket.OPEN;
    this.onopen?.({} as Event);
    if (sendHello) {
      this.emitMessage(
        JSON.stringify({
          type: "hello",
          clientId: "web-client",
          serverId: "server-1",
          protocolVersion: WEB_PROTOCOL_VERSION,
          webBuildId: CLIENT_WEB_BUILD_ID,
        }),
      );
    }
  }

  emitMessage(data: string | ArrayBuffer): void {
    this.onmessage?.({ data } as MessageEvent);
  }

  emitClose(reason = "network lost"): void {
    this.readyState = FakeWebSocket.CLOSED;
    this.onclose?.({ code: 1006, reason } as CloseEvent);
  }
}

describe("WsClient request handling", () => {
  it("uses the raw-stream Resume protocol version", () => {
    expect(WEB_PROTOCOL_VERSION).toBe(3);
  });

  beforeEach(() => {
    FakeWebSocket.instances = [];
    vi.stubGlobal("window", globalThis);
    vi.stubGlobal("location", { protocol: "http:", host: "example.test" });
    vi.stubGlobal("localStorage", {
      getItem: vi.fn(() => null),
      setItem: vi.fn(),
    });
    vi.stubGlobal("sessionStorage", {
      getItem: vi.fn(() => null),
      setItem: vi.fn(),
    });
    vi.stubGlobal("crypto", {
      randomUUID: vi.fn(() => "browser-install-uuid"),
    });
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: true, status: 200 }),
    );
    vi.stubGlobal("WebSocket", FakeWebSocket);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("does not open, resume, lease, or mutate before the mandatory hello", async () => {
    const callbacks = clientCallbacks();
    const client = new WsClient(callbacks);
    await client.start();
    const socket = FakeWebSocket.instances[0];

    socket.emitOpen(false);
    void client.request({ type: "stopAllServers" }).catch(() => undefined);
    client.sendWithWriterLease({
      type: "input",
      sessionId: "pty-a",
      text: "must wait",
    });

    expect(callbacks.onStatus).not.toHaveBeenCalledWith({ kind: "open" });
    expect(jsonFrames(socket)).toEqual([]);

    socket.emitMessage(
      JSON.stringify({ type: "snapshot", workspace: { revision: 1 } }),
    );
    expect(socket.readyState).toBe(FakeWebSocket.CLOSED);
    expect(callbacks.onMessage).not.toHaveBeenCalled();
  });

  it.each(["null", '{"type":"hello","protocolVersion":"2"}']) (
    "closes safely for a malformed first frame: %s",
    async (frame) => {
      const callbacks = clientCallbacks();
      const client = new WsClient(callbacks);
      await client.start();
      const socket = FakeWebSocket.instances[0];
      socket.emitOpen(false);

      expect(() => socket.emitMessage(frame)).not.toThrow();
      expect(socket.readyState).toBe(FakeWebSocket.CLOSED);
      expect(callbacks.onStatus).not.toHaveBeenCalledWith({ kind: "open" });
    },
  );

  it.each([
    {
      name: "protocol",
      hello: {
        type: "hello",
        clientId: "web-client",
        serverId: "server-1",
        protocolVersion: WEB_PROTOCOL_VERSION + 1,
        webBuildId: CLIENT_WEB_BUILD_ID,
      },
      failure: "protocolMismatch",
    },
    {
      name: "build",
      hello: {
        type: "hello",
        clientId: "web-client",
        serverId: "server-1",
        protocolVersion: WEB_PROTOCOL_VERSION,
        webBuildId: "different-host-build",
      },
      failure: "buildMismatch",
    },
  ])("rejects a $name mismatch before opening", async ({ hello, failure }) => {
    const onHelloFailure = vi.fn();
    const callbacks = clientCallbacks({ onHelloFailure });
    const client = new WsClient(callbacks);
    await client.start();
    const socket = FakeWebSocket.instances[0];

    socket.emitOpen(false);
    socket.emitMessage(JSON.stringify(hello));

    expect(callbacks.onStatus).not.toHaveBeenCalledWith({ kind: "open" });
    expect(jsonFrames(socket)).toEqual([]);
    expect(onHelloFailure).toHaveBeenCalledWith(
      expect.objectContaining({ kind: failure }),
    );
    expect(socket.readyState).toBe(FakeWebSocket.CLOSED);
  });

  it("uses compatible-build recovery when protocol and build both differ", async () => {
    const onHelloFailure = vi.fn();
    const callbacks = clientCallbacks({ onHelloFailure });
    const client = new WsClient(callbacks);
    await client.start();
    const socket = FakeWebSocket.instances[0];

    socket.emitOpen(false);
    socket.emitMessage(
      JSON.stringify({
        type: "hello",
        clientId: "web-client",
        serverId: "server-1",
        protocolVersion: WEB_PROTOCOL_VERSION + 1,
        webBuildId: "different-host-build",
      }),
    );

    expect(onHelloFailure).toHaveBeenCalledWith({
      kind: "buildMismatch",
      expectedBuildId: CLIENT_WEB_BUILD_ID,
      receivedBuildId: "different-host-build",
    });
    expect(callbacks.onStatus).not.toHaveBeenCalledWith({ kind: "open" });
    expect(jsonFrames(socket)).toEqual([]);
    expect(socket.readyState).toBe(FakeWebSocket.CLOSED);
  });

  it("opens and resumes only after a compatible hello", async () => {
    const callbacks = clientCallbacks();
    const client = new WsClient(callbacks);
    await client.start();
    const socket = FakeWebSocket.instances[0];

    socket.emitOpen(false);
    expect(jsonFrames(socket)).toEqual([]);
    socket.emitMessage(
      JSON.stringify({
        type: "hello",
        clientId: "web-client",
        serverId: "server-1",
        protocolVersion: WEB_PROTOCOL_VERSION,
        webBuildId: CLIENT_WEB_BUILD_ID,
      }),
    );

    expect(callbacks.onStatus).toHaveBeenLastCalledWith({ kind: "open" });
    expect(jsonFrames(socket)).toEqual([
      expect.objectContaining({ type: "resume" }),
    ]);
  });

  it("sends request frames and resolves matching responses", async () => {
    const client = new WsClient(clientCallbacks());

    await client.start();
    const socket = FakeWebSocket.instances[0];
    expect(socket).toBeDefined();
    expect(socket.url).toBe(
      "ws://example.test/api/ws?browserInstallId=browser-install-uuid",
    );
    socket.emitOpen();
    socket.emitMessage(
      JSON.stringify({
        type: "writerLeaseState",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 2,
          expiresAtEpochMs: 10_000,
          youAreOwner: true,
        },
      }),
    );

    const action: RemoteAction = { type: "closeTab", tab_id: "tab-1" };
    const requestPromise = (
      client as unknown as { request(action: RemoteAction): Promise<unknown> }
    ).request(action);

    expect(jsonFrames(socket).filter((frame) => frame.type === "resume")).toHaveLength(1);
    const frames = jsonFrames(socket);
    expect(frames[frames.length - 1]).toEqual({
      type: "request",
      id: 1,
      action,
      expectedLeaseGeneration: 2,
    });

    socket.emitMessage(
      JSON.stringify({
        type: "response",
        id: 1,
        result: { ok: true, message: "opened", payload: null },
      }),
    );

    await expect(requestPromise).resolves.toEqual({
      ok: true,
      message: "opened",
      payload: null,
    });
  });

  it("stages raw input until the authoritative writer generation arrives", async () => {
    const client = new WsClient(clientCallbacks());
    await client.start();
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen();

    expect(
      client.sendWithWriterLease({
        type: "input",
        sessionId: "pty-a",
        text: "hello",
        inputKind: "paste",
      }),
    ).toBe(true);
    expect(jsonFrames(socket).filter((frame) => frame.type === "input")).toEqual([]);

    socket.emitMessage(
      JSON.stringify({
        type: "writerLeaseState",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 12,
          expiresAtEpochMs: 10_000,
          youAreOwner: true,
        },
      }),
    );
    socket.emitMessage(
      JSON.stringify({
        type: "writerLeaseState",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 12,
          expiresAtEpochMs: 10_000,
          youAreOwner: true,
        },
      }),
    );

    expect(jsonFrames(socket).filter((frame) => frame.type === "input")).toEqual([
      {
        type: "input",
        sessionId: "pty-a",
        text: "hello",
        inputKind: "paste",
        expectedLeaseGeneration: 12,
      },
    ]);
  });

  it("drops staged raw input when the host runtime resets", async () => {
    const client = new WsClient(clientCallbacks());
    await client.start();
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen();
    client.sendWithWriterLease({
      type: "input",
      sessionId: "pty-a",
      text: "old runtime",
    });

    client.resetRuntime();
    socket.emitMessage(
      JSON.stringify({
        type: "writerLeaseState",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 13,
          expiresAtEpochMs: 10_000,
          youAreOwner: true,
        },
      }),
    );

    expect(jsonFrames(socket).filter((frame) => frame.type === "input")).toEqual([]);
  });

  it("sends exactly one atomic resume frame whenever a socket opens", async () => {
    const client = new WsClient(clientCallbacks());

    await client.start();
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen();

    expect(jsonFrames(socket)).toEqual([
      {
        type: "resume",
        ...resumeContext,
        clientInstanceId: "browser-install-uuid",
      },
    ]);
  });

  it("defaults atomic resume to no raw terminal subscription", async () => {
    const client = new WsClient({
      onStatus: vi.fn(),
      onMessage: vi.fn(),
      onSessionOutput: vi.fn(),
    });

    await client.start();
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen();

    expect(jsonFrames(socket)).toEqual([
      expect.objectContaining({
        type: "resume",
        desiredSessionKey: null,
        rawSessionId: null,
      }),
    ]);
  });

  it("uses one sessionStorage client instance id for every client in the tab", async () => {
    let stored: string | null = null;
    const storage = {
      getItem: vi.fn(() => stored),
      setItem: vi.fn((_key: string, value: string) => {
        stored = value;
      }),
    };
    vi.stubGlobal("sessionStorage", storage);
    const first = new WsClient(clientCallbacks());
    const second = new WsClient(clientCallbacks());

    await first.start();
    FakeWebSocket.instances[0]?.emitOpen();
    await second.start();
    FakeWebSocket.instances[1]?.emitOpen();

    const firstResume = jsonFrames(FakeWebSocket.instances[0] ?? ({} as FakeWebSocket))[0];
    const secondResume = jsonFrames(FakeWebSocket.instances[1] ?? ({} as FakeWebSocket))[0];
    expect(firstResume?.clientInstanceId).toBe(secondResume?.clientInstanceId);
    expect(storage.setItem).toHaveBeenCalledTimes(1);
  });

  it("keeps one in-memory tab id when sessionStorage is unavailable", async () => {
    vi.stubGlobal("sessionStorage", {
      getItem: vi.fn(() => {
        throw new Error("blocked");
      }),
      setItem: vi.fn(() => {
        throw new Error("blocked");
      }),
    });
    let nextId = 0;
    vi.stubGlobal("crypto", {
      randomUUID: vi.fn(() => `generated-${++nextId}`),
    });
    const first = new WsClient(clientCallbacks());
    const second = new WsClient(clientCallbacks());

    await first.start();
    FakeWebSocket.instances[0]?.emitOpen();
    await second.start();
    FakeWebSocket.instances[1]?.emitOpen();

    const firstResume = jsonFrames(FakeWebSocket.instances[0] ?? ({} as FakeWebSocket))[0];
    const secondResume = jsonFrames(FakeWebSocket.instances[1] ?? ({} as FakeWebSocket))[0];
    expect(firstResume?.clientInstanceId).toBe(secondResume?.clientInstanceId);
  });

  it("falls back when reading the sessionStorage property itself throws", async () => {
    Object.defineProperty(globalThis, "sessionStorage", {
      configurable: true,
      get() {
        throw new DOMException("blocked", "SecurityError");
      },
    });

    expect(() => new WsClient(clientCallbacks())).not.toThrow();
  });

  it("resolves and rejects composer submissions by mutation id", async () => {
    const client = new WsClient(clientCallbacks());
    await client.start();
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen();
    socket.emitMessage(
      JSON.stringify({
        type: "writerLeaseState",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 9,
          expiresAtEpochMs: 9_000,
          youAreOwner: true,
        },
      }),
    );

    const submit = client as unknown as {
      submitComposer(input: {
        mutationId: string;
        stableSessionKey: string;
        text: string;
        attachments: never[];
      }): Promise<unknown>;
    };
    const accepted = submit.submitComposer({
      mutationId: "mutation-accepted",
      stableSessionKey: "tab:tab-1",
      text: "hello",
      attachments: [],
    });
    const frames = jsonFrames(socket);
    expect(frames[frames.length - 1]).toEqual({
      type: "composerSubmit",
      mutationId: "mutation-accepted",
      stableSessionKey: "tab:tab-1",
      text: "hello",
      attachments: [],
      expectedLeaseGeneration: 9,
    });
    socket.emitMessage(
      JSON.stringify({
        type: "composerAccepted",
        mutationId: "mutation-accepted",
        stableSessionKey: "tab:tab-1",
        acceptedSequence: 13,
        leaseGeneration: 9,
      }),
    );
    await expect(accepted).resolves.toMatchObject({
      mutationId: "mutation-accepted",
      acceptedSequence: 13,
    });

    const rejected = submit.submitComposer({
      mutationId: "mutation-rejected",
      stableSessionKey: "tab:tab-1",
      text: "again",
      attachments: [],
    });
    socket.emitMessage(
      JSON.stringify({
        type: "composerRejected",
        mutationId: "mutation-rejected",
        code: "mutationConflict",
        message: "mutation content changed",
        writerLease: {
          ownerClientInstanceId: null,
          generation: 10,
          expiresAtEpochMs: null,
          youAreOwner: false,
        },
      }),
    );
    await expect(rejected).rejects.toMatchObject({
      mutationId: "mutation-rejected",
      code: "mutationConflict",
      message: "mutation content changed",
    });

    const capacityRejected = submit.submitComposer({
      mutationId: "mutation-capacity",
      stableSessionKey: "tab:tab-1",
      text: "later",
      attachments: [],
    });
    socket.emitMessage(
      JSON.stringify({
        type: "composerRejected",
        mutationId: "mutation-capacity",
        code: "capacityExceeded",
        message: "retry after older mutations expire",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 10,
          expiresAtEpochMs: 10_000,
          youAreOwner: true,
        },
      }),
    );
    await expect(capacityRejected).rejects.toMatchObject({
      mutationId: "mutation-capacity",
      code: "capacityExceeded",
    });
  });

  it("rejects every pending mutation when the host runtime changes", async () => {
    const client = new WsClient(clientCallbacks());
    await client.start();
    FakeWebSocket.instances[0]?.emitOpen();

    let actionRejected = false;
    let composerRejected = false;
    void client
      .request({ type: "stopAllServers" })
      .catch(() => {
        actionRejected = true;
      });
    void client
      .submitComposer({
        mutationId: "mutation-reset",
        stableSessionKey: "tab:tab-1",
        text: "hello",
        attachments: [],
      })
      .catch(() => {
        composerRejected = true;
      });

    client.resetRuntime("host runtime changed");
    await Promise.resolve();

    expect(actionRejected).toBe(true);
    expect(composerRejected).toBe(true);
  });

  it("applies the authoritative hard-reset lease after runtime cleanup", async () => {
    let client: WsClient;
    const onMessage = vi.fn((message: unknown) => {
      const inbound = message as { type?: string; hardReset?: boolean };
      if (inbound.type === "resumeState" && inbound.hardReset) {
        client.resetRuntime("host runtime changed");
      }
    });
    client = new WsClient(clientCallbacks({ onMessage }));
    await client.start();
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen();
    socket.emitMessage(
      JSON.stringify({
        type: "resumeState",
        runtimeInstanceId: "runtime-new",
        revision: 1,
        hardReset: true,
        route: "/sessions",
        desiredSessionKey: null,
        workspace: null,
        semanticReplay: null,
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 44,
          expiresAtEpochMs: 20_000,
          youAreOwner: true,
        },
      }),
    );
    socket.sent.length = 0;

    expect(
      client.sendWithWriterLease({
        type: "input",
        sessionId: "pty-a",
        text: "uses authoritative lease",
      }),
    ).toBe(true);

    expect(jsonFrames(socket)).toEqual([
      {
        type: "input",
        sessionId: "pty-a",
        text: "uses authoritative lease",
        expectedLeaseGeneration: 44,
      },
    ]);
  });

  it("shares a bounded pending-work count across raw frames and actions", async () => {
    const client = new WsClient(clientCallbacks());
    await client.start();
    FakeWebSocket.instances[0]?.emitOpen();
    for (let index = 0; index < 256; index += 1) {
      expect(
        client.sendWithWriterLease({
          type: "input",
          sessionId: "pty-a",
          text: "x",
        }),
      ).toBe(true);
    }

    let rejection: unknown;
    void client.request({ type: "stopAllServers" }).catch((error: unknown) => {
      rejection = error;
    });
    await Promise.resolve();

    expect(rejection).toMatchObject({
      message: expect.stringContaining("Too much outbound work"),
    });
  });

  it("bounds staged raw work by encoded bytes and resets the accounting", async () => {
    const client = new WsClient(clientCallbacks());
    await client.start();
    FakeWebSocket.instances[0]?.emitOpen();
    const largeBase64 = "a".repeat(7 * 1_024 * 1_024);
    const image = {
      type: "pasteImage" as const,
      sessionId: "pty-a",
      mimeType: "image/png" as const,
      fileName: "large.png",
      dataBase64: largeBase64,
    };

    expect(client.sendWithWriterLease(image)).toBe(true);
    expect(client.sendWithWriterLease(image)).toBe(false);

    client.resetRuntime("host runtime changed");
    expect(client.sendWithWriterLease(image)).toBe(true);
  });

  it("rejects a composer payload that exceeds the shared byte budget", async () => {
    const client = new WsClient(clientCallbacks());
    await client.start();
    FakeWebSocket.instances[0]?.emitOpen();
    let rejection: unknown;
    void client
      .submitComposer({
        mutationId: "mutation-too-large",
        stableSessionKey: "tab:tab-1",
        text: "image",
        attachments: [
          {
            mimeType: "image/png",
            fileName: "too-large.png",
            dataBase64: "a".repeat(9 * 1_024 * 1_024),
          },
        ],
      })
      .catch((error: unknown) => {
        rejection = error;
      });
    await Promise.resolve();

    expect(rejection).toMatchObject({
      message: expect.stringContaining("Too much outbound work"),
    });
  });

  it("coalesces staged resizes to the latest dimensions per session", async () => {
    const client = new WsClient(clientCallbacks());
    await client.start();
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen();
    client.sendWithWriterLease({
      type: "resize",
      sessionId: "pty-a",
      rows: 24,
      cols: 80,
    });
    client.sendWithWriterLease({
      type: "resize",
      sessionId: "pty-a",
      rows: 30,
      cols: 100,
    });

    socket.emitMessage(
      JSON.stringify({
        type: "writerLeaseState",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 45,
          expiresAtEpochMs: 20_000,
          youAreOwner: true,
        },
      }),
    );

    expect(jsonFrames(socket).filter((frame) => frame.type === "resize")).toEqual([
      {
        type: "resize",
        sessionId: "pty-a",
        rows: 30,
        cols: 100,
        expectedLeaseGeneration: 45,
      },
    ]);
  });

  it("discards staged raw work for a closed session only", async () => {
    const client = new WsClient(clientCallbacks());
    await client.start();
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen();
    client.sendWithWriterLease({
      type: "input",
      sessionId: "pty-a",
      text: "discard",
    });
    client.sendWithWriterLease({
      type: "input",
      sessionId: "pty-b",
      text: "keep",
    });

    client.discardWriterFramesForSession("pty-a");
    socket.emitMessage(
      JSON.stringify({
        type: "writerLeaseState",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 46,
          expiresAtEpochMs: 20_000,
          youAreOwner: true,
        },
      }),
    );

    expect(jsonFrames(socket).filter((frame) => frame.type === "input")).toEqual([
      {
        type: "input",
        sessionId: "pty-b",
        text: "keep",
        expectedLeaseGeneration: 46,
      },
    ]);
  });

  it("discards a staged interrupt with the exact closed stable session", async () => {
    const client = new WsClient(clientCallbacks());
    await client.start();
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen();
    client.sendWithWriterLease({
      type: "interruptSession",
      stableSessionKey: "tab:a",
    });
    client.sendWithWriterLease({
      type: "interruptSession",
      stableSessionKey: "tab:b",
    });

    client.discardWriterFramesForSession("pty-a", "tab:a");
    socket.emitMessage(
      JSON.stringify({
        type: "writerLeaseState",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 47,
          expiresAtEpochMs: 20_000,
          youAreOwner: true,
        },
      }),
    );

    expect(
      jsonFrames(socket).filter((frame) => frame.type === "interruptSession"),
    ).toEqual([
      {
        type: "interruptSession",
        stableSessionKey: "tab:b",
        expectedLeaseGeneration: 47,
      },
    ]);
  });

  it("requests a visible writer lease for actions without replaying the action", async () => {
    const client = new WsClient(clientCallbacks());
    await client.start();
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen();
    socket.sent.length = 0;

    const result = client.request({ type: "stopAllServers" });
    expect(jsonFrames(socket).map((frame) => frame.type)).toEqual([
      "acquireWriterLease",
    ]);

    socket.emitMessage(
      JSON.stringify({
        type: "writerLeaseState",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 3,
          expiresAtEpochMs: 10_000,
          youAreOwner: true,
        },
      }),
    );
    expect(jsonFrames(socket).map((frame) => frame.type)).toEqual([
      "acquireWriterLease",
      "request",
    ]);
    expect(jsonFrames(socket)[1]).toMatchObject({
      expectedLeaseGeneration: 3,
    });
    expect(
      jsonFrames(socket).filter((frame) => frame.type === "request"),
    ).toHaveLength(1);
    socket.emitMessage(
      JSON.stringify({
        type: "response",
        id: 1,
        result: { ok: true, message: null, payload: null },
      }),
    );
    await expect(result).resolves.toMatchObject({ ok: true });
  });
});

describe("WsClient reconnect wake handling", () => {
  beforeEach(() => {
    FakeWebSocket.instances = [];
    vi.useFakeTimers();
    vi.setSystemTime(0);
    vi.stubGlobal("window", globalThis);
    vi.stubGlobal("location", { protocol: "http:", host: "example.test" });
    vi.stubGlobal("localStorage", {
      getItem: vi.fn(() => null),
      setItem: vi.fn(),
    });
    vi.stubGlobal("sessionStorage", {
      getItem: vi.fn(() => null),
      setItem: vi.fn(),
    });
    vi.stubGlobal("crypto", {
      randomUUID: vi.fn(() => "browser-install-uuid"),
    });
    vi.stubGlobal("WebSocket", FakeWebSocket);
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("closes a transport that stays silent instead of waiting forever for hello", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: true, status: 200 }),
    );
    const callbacks = clientCallbacks();
    const client = new WsClient(callbacks);
    await client.start();
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen(false);

    await vi.advanceTimersByTimeAsync(5_000);

    expect(socket.readyState).toBe(FakeWebSocket.CLOSED);
    expect(callbacks.onStatus).not.toHaveBeenCalledWith({ kind: "open" });
  });

  it("wake retries immediately instead of waiting for reconnect backoff", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn()
        .mockRejectedValueOnce(new Error("offline"))
        .mockResolvedValue({ ok: true, status: 200 }),
    );
    const client = new WsClient(clientCallbacks());

    await client.start();
    expect(FakeWebSocket.instances).toHaveLength(0);

    client.wake();
    await Promise.resolve();
    await Promise.resolve();

    expect(FakeWebSocket.instances).toHaveLength(1);
  });

  it("does not create duplicate sockets while a start is already in flight", async () => {
    let resolveFetch: (value: { ok: boolean; status: number }) => void = () => {};
    const fetchMock = vi.fn(
      () =>
        new Promise((resolve) => {
          resolveFetch = resolve;
        }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const client = new WsClient(clientCallbacks());

    const firstStart = client.start();
    await client.start();
    resolveFetch({ ok: true, status: 200 });
    await firstStart;

    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(FakeWebSocket.instances).toHaveLength(1);
  });

  it("replaces an open socket that has gone stale after backgrounding", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: true, status: 200 }),
    );
    const onMessage = vi.fn();
    const onStatus = vi.fn();
    const client = new WsClient(clientCallbacks({ onMessage, onStatus }));

    await client.start();
    FakeWebSocket.instances[0]?.emitOpen();
    FakeWebSocket.instances[0]?.emitMessage(
      JSON.stringify({
        type: "writerLeaseState",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 2,
          expiresAtEpochMs: 40_000,
          youAreOwner: true,
        },
      }),
    );
    let actionRejected = false;
    void client.request({ type: "stopAllServers" }).catch(() => {
      actionRejected = true;
    });
    vi.setSystemTime(31_000);

    client.wake();
    await Promise.resolve();
    await Promise.resolve();

    expect(FakeWebSocket.instances).toHaveLength(2);
    expect(actionRejected).toBe(true);

    const stale = FakeWebSocket.instances[0];
    stale?.emitMessage(JSON.stringify({ type: "error", message: "stale" }));
    stale?.onclose?.({ code: 1006, reason: "stale close" } as CloseEvent);
    expect(onMessage).not.toHaveBeenCalledWith(
      expect.objectContaining({ message: "stale" }),
    );
    expect(onStatus).not.toHaveBeenCalledWith(
      expect.objectContaining({ reason: "stale close" }),
    );
  });

  it("wake sends one resume immediately on a healthy open socket", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: true, status: 200 }),
    );
    const client = new WsClient(clientCallbacks());
    await client.start();
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen();
    socket.sent.length = 0;

    client.wake();

    expect(jsonFrames(socket)).toEqual([
      expect.objectContaining({ type: "resume" }),
    ]);
  });

  it("coalesces one foreground return burst without superseding its replay", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: true, status: 200 }),
    );
    const client = new WsClient(clientCallbacks());
    await client.start();
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen();

    // visibilitychange, focus, pageshow, and online can all describe the same
    // foreground transition. The socket-open Resume is already sufficient.
    client.setVisibility(true);
    client.foreground();
    client.foreground();
    client.foreground();
    expect(
      jsonFrames(socket).filter((frame) => frame.type === "resume"),
    ).toHaveLength(1);

    // A later foreground episode still performs recovery.
    await vi.advanceTimersByTimeAsync(1_001);
    client.foreground();
    client.foreground();
    expect(
      jsonFrames(socket).filter((frame) => frame.type === "resume"),
    ).toHaveLength(2);
  });

  it("never suppresses the visible transition after a socket opened hidden", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: true, status: 200 }),
    );
    let visible = false;
    const client = new WsClient(
      clientCallbacks({
        getResumeContext: vi.fn(() => ({
          ...resumeContext,
          visible,
          wantsWriterLease: visible,
        })),
      }),
    );
    await client.start();
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen();

    visible = true;
    client.setVisibility(true);

    const resumes = jsonFrames(socket).filter((frame) => frame.type === "resume");
    expect(resumes).toHaveLength(2);
    expect(resumes[0]).toMatchObject({ visible: false, wantsWriterLease: false });
    expect(resumes[1]).toMatchObject({ visible: true, wantsWriterLease: true });
  });

  it("retries a lease-busy composer send after the guard with one mutation id", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: true, status: 200 }),
    );
    const client = new WsClient(clientCallbacks());
    await client.start();
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen();
    socket.emitMessage(
      JSON.stringify({
        type: "writerLeaseState",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 4,
          expiresAtEpochMs: 8_000,
          youAreOwner: true,
        },
      }),
    );

    let rejected = false;
    const submission = client
      .submitComposer({
        mutationId: "mutation-guard",
        stableSessionKey: "tab:tab-1",
        text: "send once",
        attachments: [],
      })
      .catch((error: unknown) => {
        rejected = true;
        throw error;
      });
    socket.emitMessage(
      JSON.stringify({
        type: "composerRejected",
        mutationId: "mutation-guard",
        code: "leaseBusy",
        message: "active input guard",
        writerLease: {
          ownerClientInstanceId: "other-tab",
          generation: 5,
          expiresAtEpochMs: 8_700,
          youAreOwner: false,
        },
      }),
    );
    await Promise.resolve();
    expect(rejected).toBe(false);

    await vi.advanceTimersByTimeAsync(250);
    const afterGuard = jsonFrames(socket);
    expect(afterGuard[afterGuard.length - 1]).toMatchObject({
      type: "acquireWriterLease",
      clientInstanceId: "browser-install-uuid",
      visible: true,
    });
    socket.emitMessage(
      JSON.stringify({
        type: "writerLeaseState",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 6,
          expiresAtEpochMs: 9_500,
          youAreOwner: true,
        },
      }),
    );

    const sends = jsonFrames(socket).filter(
      (frame) => frame.type === "composerSubmit",
    );
    expect(sends).toHaveLength(2);
    expect(new Set(sends.map((frame) => frame.mutationId))).toEqual(
      new Set(["mutation-guard"]),
    );
    expect(sends[1]?.expectedLeaseGeneration).toBe(6);

    socket.emitMessage(
      JSON.stringify({
        type: "composerAccepted",
        mutationId: "mutation-guard",
        stableSessionKey: "tab:tab-1",
        acceptedSequence: 20,
        leaseGeneration: 6,
      }),
    );
    await expect(submission).resolves.toMatchObject({ acceptedSequence: 20 });
  });

  it.each([
    "staleGeneration",
    "nativeControllerActive",
    "mutationInFlight",
  ] as const)("keeps the same promise for transient %s rejection", async (code) => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: true, status: 200 }),
    );
    const client = new WsClient(clientCallbacks());
    await client.start();
    const socket = FakeWebSocket.instances[0];
    socket.emitOpen();
    socket.emitMessage(
      JSON.stringify({
        type: "writerLeaseState",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 7,
          expiresAtEpochMs: 8_000,
          youAreOwner: true,
        },
      }),
    );
    let rejected = false;
    const submission = client
      .submitComposer({
        mutationId: `mutation-${code}`,
        stableSessionKey: "tab:tab-1",
        text: code,
        attachments: [],
      })
      .catch((error: unknown) => {
        rejected = true;
        throw error;
      });
    socket.emitMessage(
      JSON.stringify({
        type: "composerRejected",
        mutationId: `mutation-${code}`,
        code,
        message: "retry",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 8,
          expiresAtEpochMs: 9_000,
          youAreOwner: true,
        },
      }),
    );
    await Promise.resolve();
    expect(rejected).toBe(false);

    expect(
      jsonFrames(socket).filter((frame) => frame.type === "composerSubmit"),
    ).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(249);
    expect(
      jsonFrames(socket).filter((frame) => frame.type === "composerSubmit"),
    ).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    const sends = jsonFrames(socket).filter(
      (frame) => frame.type === "composerSubmit",
    );
    expect(sends).toHaveLength(2);
    expect(sends[1]?.mutationId).toBe(`mutation-${code}`);
    socket.emitMessage(
      JSON.stringify({
        type: "composerAccepted",
        mutationId: `mutation-${code}`,
        stableSessionKey: "tab:tab-1",
        acceptedSequence: 21,
        leaseGeneration: 8,
      }),
    );
    await expect(submission).resolves.toMatchObject({ acceptedSequence: 21 });
  });

  it("resends an unacknowledged composer mutation after reconnect Resume ownership", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: true, status: 200 }),
    );
    const client = new WsClient(clientCallbacks());
    await client.start();
    const firstSocket = FakeWebSocket.instances[0];
    firstSocket.emitOpen();
    firstSocket.emitMessage(
      JSON.stringify({
        type: "writerLeaseState",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 11,
          expiresAtEpochMs: 8_000,
          youAreOwner: true,
        },
      }),
    );
    let rejected = false;
    const submission = client
      .submitComposer({
        mutationId: "mutation-reconnect",
        stableSessionKey: "tab:tab-1",
        text: "survive reconnect",
        attachments: [],
      })
      .catch((error: unknown) => {
        rejected = true;
        throw error;
      });
    firstSocket.emitClose();
    await Promise.resolve();
    expect(rejected).toBe(false);

    await vi.advanceTimersByTimeAsync(1_000);
    await Promise.resolve();
    const secondSocket = FakeWebSocket.instances[1];
    expect(secondSocket).toBeDefined();
    secondSocket.emitOpen();
    secondSocket.emitMessage(
      JSON.stringify({
        type: "resumeState",
        runtimeInstanceId: "runtime-1",
        revision: 7,
        hardReset: false,
        route: "/session/tab/tab-1",
        desiredSessionKey: "tab:tab-1",
        workspace: null,
        semanticReplay: null,
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 12,
          expiresAtEpochMs: 10_000,
          youAreOwner: true,
        },
      }),
    );

    const retrySends = jsonFrames(secondSocket).filter(
      (frame) => frame.type === "composerSubmit",
    );
    expect(retrySends).toHaveLength(1);
    expect(retrySends[0]).toMatchObject({
      mutationId: "mutation-reconnect",
      expectedLeaseGeneration: 12,
    });
    secondSocket.emitMessage(
      JSON.stringify({
        type: "composerAccepted",
        mutationId: "mutation-reconnect",
        stableSessionKey: "tab:tab-1",
        acceptedSequence: 22,
        leaseGeneration: 12,
      }),
    );
    await expect(submission).resolves.toMatchObject({ acceptedSequence: 22 });
  });

  it("keeps an unsent action staged across reconnect, then sends it once", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: true, status: 200 }),
    );
    const client = new WsClient(clientCallbacks());
    await client.start();
    const firstSocket = FakeWebSocket.instances[0];
    firstSocket.emitOpen();
    firstSocket.sent.length = 0;

    let rejected = false;
    const result = client.request({ type: "stopAllServers" }).catch((error) => {
      rejected = true;
      throw error;
    });
    expect(jsonFrames(firstSocket).map((frame) => frame.type)).toEqual([
      "acquireWriterLease",
    ]);
    firstSocket.emitClose();
    await Promise.resolve();
    expect(rejected).toBe(false);

    await vi.advanceTimersByTimeAsync(1_000);
    const secondSocket = FakeWebSocket.instances[1];
    secondSocket.emitOpen();
    secondSocket.emitMessage(
      JSON.stringify({
        type: "resumeState",
        runtimeInstanceId: "runtime-1",
        revision: 7,
        hardReset: false,
        route: "/sessions",
        desiredSessionKey: null,
        workspace: null,
        semanticReplay: null,
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 30,
          expiresAtEpochMs: 12_000,
          youAreOwner: true,
        },
      }),
    );
    const requests = jsonFrames(secondSocket).filter(
      (frame) => frame.type === "request",
    );
    expect(requests).toHaveLength(1);
    expect(requests[0]?.expectedLeaseGeneration).toBe(30);
    secondSocket.emitMessage(
      JSON.stringify({
        type: "response",
        id: 1,
        result: { ok: true, message: null, payload: null },
      }),
    );
    await expect(result).resolves.toMatchObject({ ok: true });
  });

  it("rejects a sent action on disconnect and never replays it", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: true, status: 200 }),
    );
    const client = new WsClient(clientCallbacks());
    await client.start();
    const firstSocket = FakeWebSocket.instances[0];
    firstSocket.emitOpen();
    firstSocket.emitMessage(
      JSON.stringify({
        type: "writerLeaseState",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 31,
          expiresAtEpochMs: 12_000,
          youAreOwner: true,
        },
      }),
    );
    const result = client.request({ type: "stopAllServers" });
    expect(
      jsonFrames(firstSocket).filter((frame) => frame.type === "request"),
    ).toHaveLength(1);
    firstSocket.emitClose();
    await expect(result).rejects.toThrow("websocket closed");

    await vi.advanceTimersByTimeAsync(1_000);
    const secondSocket = FakeWebSocket.instances[1];
    secondSocket.emitOpen();
    secondSocket.emitMessage(
      JSON.stringify({
        type: "writerLeaseState",
        writerLease: {
          ownerClientInstanceId: "browser-install-uuid",
          generation: 32,
          expiresAtEpochMs: 14_000,
          youAreOwner: true,
        },
      }),
    );
    expect(
      jsonFrames(secondSocket).filter((frame) => frame.type === "request"),
    ).toHaveLength(0);
  });

  it.each(["stop", "runtime reset"] as const)(
    "cancels bounded composer retry timers on %s",
    async (operation) => {
      vi.stubGlobal(
        "fetch",
        vi.fn().mockResolvedValue({ ok: true, status: 200 }),
      );
      const client = new WsClient(clientCallbacks());
      await client.start();
      const socket = FakeWebSocket.instances[0];
      socket.emitOpen();
      socket.emitMessage(
        JSON.stringify({
          type: "writerLeaseState",
          writerLease: {
            ownerClientInstanceId: "browser-install-uuid",
            generation: 40,
            expiresAtEpochMs: 10_000,
            youAreOwner: true,
          },
        }),
      );
      const outcome = client
        .submitComposer({
          mutationId: `mutation-${operation}`,
          stableSessionKey: "tab:tab-1",
          text: "cancel retry",
          attachments: [],
        })
        .catch((error: unknown) => error);
      socket.emitMessage(
        JSON.stringify({
          type: "composerRejected",
          mutationId: `mutation-${operation}`,
          code: "mutationInFlight",
          message: "wait",
          writerLease: {
            ownerClientInstanceId: "browser-install-uuid",
            generation: 40,
            expiresAtEpochMs: 10_000,
            youAreOwner: true,
          },
        }),
      );

      if (operation === "stop") client.stop();
      else client.resetRuntime("host runtime changed");
      await expect(outcome).resolves.toBeInstanceOf(Error);
      await vi.advanceTimersByTimeAsync(10_000);

      expect(
        jsonFrames(socket).filter((frame) => frame.type === "composerSubmit"),
      ).toHaveLength(1);
    },
  );
});
