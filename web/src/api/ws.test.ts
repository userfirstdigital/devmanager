import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { RemoteAction } from "./types";
import { WsClient } from "./ws";

class FakeWebSocket {
  static readonly OPEN = 1;
  static instances: FakeWebSocket[] = [];

  readonly sent: string[] = [];
  readonly url: string;
  readyState = FakeWebSocket.OPEN;
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

  close(): void {}

  emitOpen(): void {
    this.onopen?.({} as Event);
  }

  emitMessage(data: string | ArrayBuffer): void {
    this.onmessage?.({ data } as MessageEvent);
  }
}

describe("WsClient request handling", () => {
  beforeEach(() => {
    FakeWebSocket.instances = [];
    vi.stubGlobal("window", globalThis);
    vi.stubGlobal("location", { protocol: "http:", host: "example.test" });
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

  it("sends request frames and resolves matching responses", async () => {
    const client = new WsClient({
      onStatus: vi.fn(),
      onMessage: vi.fn(),
      onSessionOutput: vi.fn(),
    });

    await client.start();
    const socket = FakeWebSocket.instances[0];
    expect(socket).toBeDefined();
    socket.emitOpen();

    const action: RemoteAction = { type: "closeTab", tab_id: "tab-1" };
    const requestPromise = (
      client as unknown as { request(action: RemoteAction): Promise<unknown> }
    ).request(action);

    expect(socket.sent).toHaveLength(1);
    expect(JSON.parse(socket.sent[0] ?? "")).toEqual({
      type: "request",
      id: 1,
      action,
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
});
