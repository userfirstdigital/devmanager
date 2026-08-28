import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { buildWebSocketUrl } from "../lib/browserIdentity";
import {
  MAX_INBOUND_BINARY_BYTES,
  MAX_INBOUND_TEXT_BYTES,
  MAX_PENDING_OUTBOUND_BYTES,
  MAX_PENDING_OUTBOUND_ITEMS,
  allowsRawTerminal,
  buildConnectWebSocketUrl,
  classifyInboundFrame,
  inboundTextByteLength,
  isRawTerminalWriterFrame,
  parseAdvertisedRelayUrl,
  selectConnectRoute,
  CONNECT_BROWSER_E2E_HOLD,
  connectBrowserTransportState,
  decodeConnectSealedFrame,
  encodeConnectSealedFrame,
  ConnectBrowserTransport,
  createConnectRequestId,
  parseConnectGreeting,
} from "./transport";
import type {
  ConnectCryptoRuntime,
  ConnectWasmHandshake,
  ConnectWasmTransport,
} from "./crypto";

const location = { protocol: "http:", host: "example.test" };

const connectLimits = {
  max_physical_frame_bytes: 1 * 1024 * 1024,
  max_reassembled_message_bytes: 16 * 1024 * 1024,
  max_page_items: 1_000,
  max_page_encoded_bytes: 512 * 1024,
  max_chunk_bytes: 256 * 1024,
  max_cumulative_bytes: 16 * 1024 * 1024,
};

function fixtureUuidBytes(tail: number): Uint8Array {
  const bytes = new Uint8Array(16);
  bytes[0] = 0x01;
  bytes[1] = 0x23;
  bytes[2] = 0x45;
  bytes[3] = 0x67;
  bytes[4] = 0x89;
  bytes[5] = 0xab;
  bytes[6] = 0x70;
  bytes[8] = 0x80;
  bytes[15] = tail;
  return bytes;
}

function connectGreeting(): Uint8Array {
  const bytes = new Uint8Array(53);
  bytes.set(new TextEncoder().encode("DMCN1"));
  bytes.set(fixtureUuidBytes(0x11), 5);
  bytes.set(fixtureUuidBytes(0x12), 21);
  bytes.set(fixtureUuidBytes(0x13), 37);
  return bytes;
}

function encodeBase64Json(value: unknown): string {
  const bytes = new TextEncoder().encode(JSON.stringify(value));
  return btoa(String.fromCharCode(...bytes));
}

class FakeConnectSocket {
  readonly sent: Uint8Array[] = [];
  readyState = 0;
  binaryType: BinaryType = "arraybuffer";
  onopen: ((event: unknown) => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: ((event: { code?: number; reason?: string }) => void) | null = null;
  onerror: ((event: unknown) => void) | null = null;

  send(data: Uint8Array): void {
    this.sent.push(data.slice());
  }

  close(): void {
    this.readyState = 3;
    this.onclose?.({ code: 1000, reason: "closed" });
  }

  emitOpen(): void {
    this.readyState = 1;
    this.onopen?.({});
  }

  emit(data: Uint8Array): void {
    this.onmessage?.({ data });
  }
}

class FakeConnectWasmTransport implements ConnectWasmTransport {
  seal(sequence: bigint, nonce: Uint8Array, plaintext: Uint8Array): Uint8Array {
    return encodeConnectSealedFrame({
      version: 1,
      sequence,
      nonce,
      ciphertext: plaintext,
      tag: new Uint8Array(32),
    });
  }

  open(encoded: Uint8Array): Uint8Array {
    return decodeConnectSealedFrame(encoded).ciphertext;
  }
}

class FakeConnectWasmHandshake implements ConnectWasmHandshake {
  private finished = false;
  write_message(): Uint8Array {
    return new Uint8Array([0x02]);
  }
  read_message(_encoded: Uint8Array): void {
    this.finished = true;
  }
  is_finished(): boolean {
    return this.finished;
  }
  finish(): ConnectWasmTransport {
    return new FakeConnectWasmTransport();
  }
}

function fakeConnectRuntime(): ConnectCryptoRuntime {
  return {
    WasmConnectHandshake:
      FakeConnectWasmHandshake as unknown as ConnectCryptoRuntime["WasmConnectHandshake"],
    connect_protocol_major: () => 1,
    connect_noise_pattern: (firstPairing) =>
      firstPairing
        ? "Noise_XX_25519_ChaChaPoly_BLAKE2s"
        : "Noise_IK_25519_ChaChaPoly_BLAKE2s",
    encode_connect_envelope_json: (input) => new TextEncoder().encode(input),
    decode_connect_envelope_json: (input) => new TextDecoder().decode(input),
    encode_connect_payload_json: (input) => new TextEncoder().encode(input),
    decode_connect_payload_json: (input) => new TextDecoder().decode(input),
  };
}

describe("selectConnectRoute", () => {
  beforeEach(() => {
    vi.stubGlobal("localStorage", {
      getItem: vi.fn(() => "browser-install-uuid"),
      setItem: vi.fn(),
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("prefers the existing host WebSocket when direct is available", () => {
    const route = selectConnectRoute({
      preferDirect: true,
      directAvailable: true,
      location,
    });

    expect(route).toEqual({
      kind: "direct",
      url: buildWebSocketUrl(location),
      reason: "preferredDirect",
    });
    expect(allowsRawTerminal(route)).toBe(true);
  });

  it("uses a valid host-advertised relay when direct is unavailable", () => {
    const hostUrl = buildWebSocketUrl(location);
    const relayUrl = "wss://relay.example.test/connect";
    const route = selectConnectRoute({
      preferDirect: true,
      directAvailable: false,
      relayUrl,
      location,
    });

    expect(route).toEqual({
      kind: "relay",
      url: relayUrl,
      reason: "directUnavailable",
    });
    expect(allowsRawTerminal(route)).toBe(false);
    if (route.kind === "relay") {
      expect(route.url).not.toBe(hostUrl);
    }
  });

  it("fails closed when relay metadata is absent or invalid", () => {
    expect(
      selectConnectRoute({
        preferDirect: true,
        directAvailable: false,
        location,
      }),
    ).toEqual({ kind: "noRoute", reason: "advertisedRelayAbsent" });
    const route = selectConnectRoute({
      preferDirect: false,
      directAvailable: true,
      relayUrl: "wss://relay.example/undocumented",
      location,
    });

    expect(route).toEqual({
      kind: "relay",
      url: "wss://relay.example/undocumented",
      reason: "explicitFallback",
    });
    expect(
      selectConnectRoute({
        preferDirect: false,
        directAvailable: true,
        relayUrl: buildWebSocketUrl(location),
        location,
      }),
    ).toEqual({ kind: "noRoute", reason: "advertisedRelayInvalid" });
    expect(parseAdvertisedRelayUrl("https://relay.example/connect")).toBeNull();
    expect(
      parseAdvertisedRelayUrl("wss://user:secret@relay.example/connect"),
    ).toBeNull();
    expect(
      parseAdvertisedRelayUrl("wss://relay.example/connect?t=PAIRCODE"),
    ).toBeNull();
  });
});

describe("browser Connect transport boundary", () => {
  it("uses the dedicated /api/connect endpoint", () => {
    expect(buildConnectWebSocketUrl(location)).toBe(
      "ws://example.test/api/connect",
    );
  });

  it("keeps Connect visibly held until the Rust/WASM leaf is ready", () => {
    expect(connectBrowserTransportState()).toEqual({
      kind: "held",
      code: CONNECT_BROWSER_E2E_HOLD,
      reason: expect.stringContaining("will not downgrade to /api/ws"),
    });
  });

  it("uses the native sealed-frame wire layout without browser crypto", () => {
    const encoded = encodeConnectSealedFrame({
      version: 1,
      sequence: 7n,
      nonce: new Uint8Array(16).fill(0x11),
      ciphertext: new Uint8Array([0x22, 0x33]),
      tag: new Uint8Array(32).fill(0x44),
    });
    expect(Array.from(encoded.slice(0, 9))).toEqual([
      1, 0, 0, 0, 0, 0, 0, 0, 7,
    ]);
    expect(decodeConnectSealedFrame(encoded)).toMatchObject({
      version: 1,
      sequence: 7n,
    });
  });

  it("binds the DMCN1 greeting to nonzero fixed-width identifiers", () => {
    const greeting = new Uint8Array(53);
    greeting.set(new TextEncoder().encode("DMCN1"));
    greeting.fill(0x71, 5, 21);
    greeting.fill(0x72, 21, 37);
    greeting.fill(0x73, 37, 53);
    expect(parseConnectGreeting(greeting)).toMatchObject({
      hostPublicId: expect.any(Uint8Array),
      routeId: expect.any(Uint8Array),
      sessionId: expect.any(Uint8Array),
    });
    expect(parseConnectGreeting(greeting.slice(0, 52))).toBeNull();
  });
});

describe("inbound frame bounds", () => {
  it("accepts a well-formed text frame under the host outbound cap", () => {
    expect(classifyInboundFrame({ channel: "text", byteLength: 128 })).toBe(
      "ok",
    );
    expect(
      classifyInboundFrame({
        channel: "binary",
        byteLength: 1024,
        frameType: 0x01,
      }),
    ).toBe("ok");
  });

  it("rejects oversized inbound text and binary without inspecting payload text", () => {
    expect(
      classifyInboundFrame({
        channel: "text",
        byteLength: MAX_INBOUND_TEXT_BYTES + 1,
      }),
    ).toBe("oversized");
    expect(
      classifyInboundFrame({
        channel: "binary",
        byteLength: MAX_INBOUND_BINARY_BYTES + 1,
        frameType: 0x01,
      }),
    ).toBe("oversized");
    expect(inboundTextByteLength("hello")).toBe(5);
  });

  it("rejects malformed binary that is too short or unknown", () => {
    expect(
      classifyInboundFrame({
        channel: "binary",
        byteLength: 4,
        frameType: 0x01,
      }),
    ).toBe("malformed");
    expect(
      classifyInboundFrame({
        channel: "binary",
        byteLength: 32,
        frameType: 0x99,
      }),
    ).toBe("malformed");
  });
});

describe("pending queue bounds and raw-terminal classification", () => {
  it("matches the live client pending-work budget", () => {
    expect(MAX_PENDING_OUTBOUND_ITEMS).toBe(256);
    expect(MAX_PENDING_OUTBOUND_BYTES).toBe(8 * 1_024 * 1_024);
  });

  it("treats only PTY writer frames as raw terminal traffic", () => {
    expect(
      isRawTerminalWriterFrame({ type: "input", sessionId: "pty-a" }),
    ).toBe(true);
    expect(
      isRawTerminalWriterFrame({ type: "pasteImage", sessionId: "pty-a" }),
    ).toBe(true);
    expect(
      isRawTerminalWriterFrame({ type: "resize", sessionId: "pty-a" }),
    ).toBe(true);
    expect(
      isRawTerminalWriterFrame({
        type: "interruptSession",
        stableSessionKey: "tab:a",
      }),
    ).toBe(false);
  });
});

describe("Connect channel sequencing and identity fences", () => {
  it("binds the first sealed Hello to the server greeting route and session", async () => {
    const socket = new FakeConnectSocket();
    const transport = new ConnectBrowserTransport({
      firstPairing: true,
      privateKey: new Uint8Array(32).fill(1),
      localPublic: new Uint8Array(32).fill(2),
      location,
      cryptoLoader: async () => fakeConnectRuntime(),
      socketFactory: () => socket,
    });
    await transport.start();
    socket.emitOpen();
    socket.emit(connectGreeting());
    socket.emit(new Uint8Array([0x03]));
    const sealed = decodeConnectSealedFrame(socket.sent[socket.sent.length - 1]!);
    const hello = JSON.parse(new TextDecoder().decode(sealed.ciphertext));
    expect(hello.connectionId).toBe("01234567-89ab-7000-8000-000000000012");
    expect(hello.sessionId).toBe("01234567-89ab-7000-8000-000000000013");
    expect(hello.channelId).not.toBe(hello.connectionId);
    transport.stop();
  });

  it("does not deliver or advance an out-of-order frame before explicit resync", async () => {
    const socket = new FakeConnectSocket();
    const envelopes: number[] = [];
    const transport = new ConnectBrowserTransport({
      firstPairing: true,
      privateKey: new Uint8Array(32).fill(1),
      localPublic: new Uint8Array(32).fill(2),
      location,
      cryptoLoader: async () => fakeConnectRuntime(),
      socketFactory: () => socket,
      onEnvelope: (envelope) => envelopes.push(envelope.payloadKind),
    });

    await transport.start();
    socket.emitOpen();
    socket.emit(connectGreeting());
    socket.emit(new Uint8Array([0x03]));

    const helloFrame = decodeConnectSealedFrame(
      socket.sent[socket.sent.length - 1] as Uint8Array,
    );
    const hello = JSON.parse(
      new TextDecoder().decode(helloFrame.ciphertext),
    ) as {
      connectionId: string;
      sessionId: string;
      channelId: string;
    };
    const helloResponse = {
      protocolMajor: 1,
      protocolMinor: 0,
      connectionId: hello.connectionId,
      sessionId: hello.sessionId,
      channelId: hello.channelId,
      channel: "critical",
      sequence: 1,
      requestId: null,
      operationId: null,
      limits: connectLimits,
      compression: "none",
      privacyClass: "local_only",
      payloadKind: 1,
      payloadVersion: 1,
      payloadBase64: encodeBase64Json({
        capabilities: 0,
        limits: connectLimits,
        privacy_class: "local_only",
        client_id: hello.connectionId,
      }),
    };
    const wasmTransport = new FakeConnectWasmTransport();
    socket.emit(
      wasmTransport.seal(
        1n,
        new Uint8Array(16).fill(3),
        new TextEncoder().encode(JSON.stringify(helloResponse)),
      ),
    );
    expect(transport.state()).toEqual({ kind: "ready" });

    const gap = {
      ...helloResponse,
      sequence: 3,
      payloadKind: 18,
      payloadBase64: encodeBase64Json({ request_id: hello.connectionId }),
    };
    socket.emit(
      wasmTransport.seal(
        3n,
        new Uint8Array(16).fill(4),
        new TextEncoder().encode(JSON.stringify(gap)),
      ),
    );
    expect(transport.state()).toEqual({ kind: "resyncing" });
    expect(envelopes).toEqual([1]);

    const resyncFrame = decodeConnectSealedFrame(
      socket.sent[socket.sent.length - 1] as Uint8Array,
    );
    const resync = JSON.parse(
      new TextDecoder().decode(resyncFrame.ciphertext),
    ) as {
      sequence: number;
      payloadKind: number;
      requestId: string;
    };
    expect(resync).toMatchObject({ sequence: 2, payloadKind: 15 });
    expect(resync.requestId).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i,
    );

    const completion = {
      ...helloResponse,
      sequence: 4,
      requestId: resync.requestId,
      payloadKind: 18,
      payloadBase64: encodeBase64Json({
        request_id: resync.requestId,
        outcome: {
          ok: {
            snapshot_page: {
              page: {
                snapshot_id: "01234567-89ab-7012-8000-000000000013",
                through_sequence: 3,
                section: "tasks",
                after_item: null,
                items: [],
                encoded_bytes: 1,
                next_cursor: null,
              },
            },
          },
        },
      }),
    };
    socket.emit(
      wasmTransport.seal(
        4n,
        new Uint8Array(16).fill(5),
        new TextEncoder().encode(JSON.stringify(completion)),
      ),
    );
    expect(transport.state()).toEqual({ kind: "ready" });
    expect(envelopes).toEqual([1, 18]);
  });

  it("does not treat an unrelated QueryReply as the resync snapshot", async () => {
    const socket = new FakeConnectSocket();
    const envelopes: number[] = [];
    const transport = new ConnectBrowserTransport({
      firstPairing: true,
      privateKey: new Uint8Array(32).fill(1),
      localPublic: new Uint8Array(32).fill(2),
      location,
      cryptoLoader: async () => fakeConnectRuntime(),
      socketFactory: () => socket,
      onEnvelope: (envelope) => envelopes.push(envelope.payloadKind),
    });

    await transport.start();
    socket.emitOpen();
    socket.emit(connectGreeting());
    socket.emit(new Uint8Array([0x03]));

    const helloFrame = decodeConnectSealedFrame(
      socket.sent[socket.sent.length - 1] as Uint8Array,
    );
    const hello = JSON.parse(
      new TextDecoder().decode(helloFrame.ciphertext),
    ) as {
      connectionId: string;
      sessionId: string;
      channelId: string;
    };
    const helloResponse = {
      protocolMajor: 1,
      protocolMinor: 0,
      connectionId: hello.connectionId,
      sessionId: hello.sessionId,
      channelId: hello.channelId,
      channel: "critical",
      sequence: 1,
      requestId: null,
      operationId: null,
      limits: connectLimits,
      compression: "none",
      privacyClass: "local_only",
      payloadKind: 1,
      payloadVersion: 1,
      payloadBase64: encodeBase64Json({
        capabilities: 0,
        limits: connectLimits,
        privacy_class: "local_only",
        client_id: hello.connectionId,
      }),
    };
    const wasmTransport = new FakeConnectWasmTransport();
    socket.emit(
      wasmTransport.seal(
        1n,
        new Uint8Array(16).fill(3),
        new TextEncoder().encode(JSON.stringify(helloResponse)),
      ),
    );

    const gap = {
      ...helloResponse,
      sequence: 3,
      payloadKind: 18,
      payloadBase64: encodeBase64Json({
        request_id: "01234567-89ab-7012-8000-000000000099",
        outcome: { err: { unavailable: { reason: "not-the-resync" } } },
      }),
    };
    socket.emit(
      wasmTransport.seal(
        3n,
        new Uint8Array(16).fill(4),
        new TextEncoder().encode(JSON.stringify(gap)),
      ),
    );
    expect(transport.state()).toEqual({ kind: "resyncing" });
    expect(envelopes).toEqual([1]);

    const resyncFrame = decodeConnectSealedFrame(
      socket.sent[socket.sent.length - 1] as Uint8Array,
    );
    const resync = JSON.parse(
      new TextDecoder().decode(resyncFrame.ciphertext),
    ) as {
      requestId: string;
    };
    const unrelated = {
      ...helloResponse,
      sequence: 4,
      requestId: "01234567-89ab-7012-8000-000000000099",
      payloadKind: 18,
      payloadBase64: encodeBase64Json({
        request_id: "01234567-89ab-7012-8000-000000000099",
        outcome: {
          ok: {
            snapshot_page: {
              page: {
                snapshot_id: "01234567-89ab-7012-8000-000000000013",
                through_sequence: 3,
                section: "tasks",
                after_item: null,
                items: [],
                encoded_bytes: 1,
                next_cursor: null,
              },
            },
          },
        },
      }),
    };
    socket.emit(
      wasmTransport.seal(
        4n,
        new Uint8Array(16).fill(5),
        new TextEncoder().encode(JSON.stringify(unrelated)),
      ),
    );
    expect(transport.state()).toEqual({ kind: "resyncing" });
    expect(envelopes).toEqual([1]);
    expect(resync.requestId).not.toBe(unrelated.requestId);
  });

  it("generates protocol-valid v7 request identities", () => {
    expect(createConnectRequestId()).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i,
    );
  });
});

describe("Connect private handshake material factory", () => {
  it("unwraps per handshake, wipes temporary bytes, and ignores stop races", async () => {
    const socket = new FakeConnectSocket();
    const privateBytes = new Uint8Array(32).fill(9);
    let resolveMaterial:
      | ((value: { privateKey: Uint8Array; localPublic: Uint8Array }) => void)
      | null = null;
    const materialPromise = new Promise<{
      privateKey: Uint8Array;
      localPublic: Uint8Array;
    }>((resolve) => {
      resolveMaterial = resolve;
    });
    const runtime: ConnectCryptoRuntime = {
      ...fakeConnectRuntime(),
      WasmConnectHandshake: vi.fn(function Handshake() {
        return {
          write_message: () => new Uint8Array([0x02]),
          read_message: () => {},
          is_finished: () => false,
          finish: () => new FakeConnectWasmTransport(),
          free: vi.fn(),
        };
      }) as unknown as ConnectCryptoRuntime["WasmConnectHandshake"],
    };
    const transport = new ConnectBrowserTransport({
      firstPairing: true,
      localPublic: new Uint8Array(32).fill(2),
      handshakeMaterialFactory: async () => materialPromise,
      location,
      cryptoLoader: async () => runtime,
      socketFactory: () => socket,
    });

    await transport.start();
    socket.emitOpen();
    socket.emit(connectGreeting());
    await Promise.resolve();
    transport.stop();
    resolveMaterial!({
      privateKey: privateBytes,
      localPublic: new Uint8Array(32).fill(2),
    });
    await Promise.resolve();
    await Promise.resolve();
    expect(transport.state()).toEqual({
      kind: "closed",
      reason: "Connect transport stopped",
    });
    expect(privateBytes.every((value) => value === 0)).toBe(true);
    expect(runtime.WasmConnectHandshake).not.toHaveBeenCalled();
  });

  it("rejects constructing a transport with both fixture bytes and a factory", () => {
    expect(
      () =>
        new ConnectBrowserTransport({
          firstPairing: true,
          privateKey: new Uint8Array(32).fill(1),
          localPublic: new Uint8Array(32).fill(2),
          handshakeMaterialFactory: async () => ({
            privateKey: new Uint8Array(32).fill(1),
          }),
        }),
    ).toThrow(/exactly one/);
  });

  it("rejects a 32-byte static key as devicePublicId at the constructor boundary", () => {
    expect(
      () =>
        new ConnectBrowserTransport({
          firstPairing: true,
          privateKey: new Uint8Array(32).fill(1),
          localPublic: new Uint8Array(32).fill(2),
          devicePublicId: new Uint8Array(32).fill(3),
        }),
    ).toThrow(/device public id/);
    expect(
      () =>
        new ConnectBrowserTransport({
          firstPairing: true,
          privateKey: new Uint8Array(32).fill(1),
          localPublic: new Uint8Array(32).fill(2),
          devicePublicId: new Uint8Array(16).fill(3),
        }),
    ).not.toThrow();
  });

  it("does not let an old rejected unwrap close a newer reconnect socket", async () => {
    const sockets: FakeConnectSocket[] = [];
    let rejectMaterial: ((error: Error) => void) | null = null;
    let resolveMaterial:
      | ((value: {
          privateKey: Uint8Array;
          localPublic: Uint8Array;
        }) => void)
      | null = null;
    let materialCalls = 0;
    const transport = new ConnectBrowserTransport({
      firstPairing: true,
      localPublic: new Uint8Array(32).fill(2),
      handshakeMaterialFactory: async () => {
        materialCalls += 1;
        if (materialCalls === 1) {
          return new Promise((_, reject) => {
            rejectMaterial = reject;
          });
        }
        return new Promise((resolve) => {
          resolveMaterial = resolve;
        });
      },
      location,
      cryptoLoader: async () => fakeConnectRuntime(),
      socketFactory: () => {
        const socket = new FakeConnectSocket();
        sockets.push(socket);
        return socket;
      },
    });

    await transport.start();
    sockets[0]!.emitOpen();
    sockets[0]!.emit(connectGreeting());
    await Promise.resolve();
    sockets[0]!.close();
    await Promise.resolve();
    await transport.start();
    sockets[1]!.emitOpen();
    sockets[1]!.emit(connectGreeting());
    await Promise.resolve();
    rejectMaterial!(new Error("old unwrap rejected"));
    await Promise.resolve();
    await Promise.resolve();
    expect(transport.state().kind).not.toBe("closed");
    expect(sockets[1]!.readyState).toBe(1);

    const secondKey = new Uint8Array(32).fill(5);
    resolveMaterial!({
      privateKey: secondKey,
      localPublic: new Uint8Array(32).fill(2),
    });
    await Promise.resolve();
    await Promise.resolve();
    expect(secondKey.every((value) => value === 0)).toBe(true);
    expect(sockets[1]!.sent.length).toBeGreaterThan(0);
    transport.stop();
  });

  it("does not let an old resolved unwrap publish onto a newer reconnect socket", async () => {
    const sockets: FakeConnectSocket[] = [];
    let resolveOld:
      | ((value: {
          privateKey: Uint8Array;
          localPublic: Uint8Array;
        }) => void)
      | null = null;
    let resolveNew:
      | ((value: {
          privateKey: Uint8Array;
          localPublic: Uint8Array;
        }) => void)
      | null = null;
    let materialCalls = 0;
    const handshakeCtor = vi.fn(function Handshake() {
      return {
        write_message: () => new Uint8Array([0x02]),
        read_message: () => {},
        is_finished: () => false,
        finish: () => new FakeConnectWasmTransport(),
      };
    });
    const transport = new ConnectBrowserTransport({
      firstPairing: true,
      localPublic: new Uint8Array(32).fill(2),
      handshakeMaterialFactory: async () => {
        materialCalls += 1;
        if (materialCalls === 1) {
          return new Promise((resolve) => {
            resolveOld = resolve;
          });
        }
        return new Promise((resolve) => {
          resolveNew = resolve;
        });
      },
      location,
      cryptoLoader: async () => ({
        ...fakeConnectRuntime(),
        WasmConnectHandshake:
          handshakeCtor as unknown as ConnectCryptoRuntime["WasmConnectHandshake"],
      }),
      socketFactory: () => {
        const socket = new FakeConnectSocket();
        sockets.push(socket);
        return socket;
      },
    });

    await transport.start();
    sockets[0]!.emitOpen();
    sockets[0]!.emit(connectGreeting());
    await Promise.resolve();
    sockets[0]!.close();
    await Promise.resolve();
    await transport.start();
    sockets[1]!.emitOpen();
    sockets[1]!.emit(connectGreeting());
    await Promise.resolve();

    const oldKey = new Uint8Array(32).fill(8);
    resolveOld!({
      privateKey: oldKey,
      localPublic: new Uint8Array(32).fill(2),
    });
    await Promise.resolve();
    await Promise.resolve();
    expect(oldKey.every((value) => value === 0)).toBe(true);
    expect(handshakeCtor).not.toHaveBeenCalled();

    const newKey = new Uint8Array(32).fill(7);
    resolveNew!({
      privateKey: newKey,
      localPublic: new Uint8Array(32).fill(2),
    });
    await Promise.resolve();
    await Promise.resolve();
    expect(handshakeCtor).toHaveBeenCalledTimes(1);
    expect(newKey.every((value) => value === 0)).toBe(true);
    transport.stop();
  });

  it("wipes factory private bytes when the handshake constructor throws", async () => {
    const socket = new FakeConnectSocket();
    const privateBytes = new Uint8Array(32).fill(4);
    const transport = new ConnectBrowserTransport({
      firstPairing: true,
      localPublic: new Uint8Array(32).fill(2),
      handshakeMaterialFactory: async () => ({
        privateKey: privateBytes,
        localPublic: new Uint8Array(32).fill(2),
      }),
      location,
      cryptoLoader: async () => ({
        ...fakeConnectRuntime(),
        WasmConnectHandshake: vi.fn(() => {
          throw new Error("constructor rejected");
        }) as unknown as ConnectCryptoRuntime["WasmConnectHandshake"],
      }),
      socketFactory: () => socket,
    });
    await transport.start();
    socket.emitOpen();
    socket.emit(connectGreeting());
    await Promise.resolve();
    await Promise.resolve();
    expect(privateBytes.every((value) => value === 0)).toBe(true);
    expect(transport.state()).toEqual({
      kind: "closed",
      reason: "Connect protocol rejected",
    });
  });

  it("does not apply an old loader rejection after stop or a newer generation", async () => {
    let rejectLoader: ((error: Error) => void) | null = null;
    const transport = new ConnectBrowserTransport({
      firstPairing: true,
      privateKey: new Uint8Array(32).fill(1),
      localPublic: new Uint8Array(32).fill(2),
      location,
      cryptoLoader: () =>
        new Promise((_, reject) => {
          rejectLoader = reject;
        }),
      socketFactory: () => new FakeConnectSocket(),
    });
    const starting = transport.start();
    await Promise.resolve();
    transport.stop();
    rejectLoader!(new Error("old loader rejected"));
    await starting;
    expect(transport.state()).toEqual({
      kind: "closed",
      reason: "Connect transport stopped",
    });

    let resolveOld: ((runtime: ConnectCryptoRuntime) => void) | null = null;
    let resolveNew: ((runtime: ConnectCryptoRuntime) => void) | null = null;
    let loads = 0;
    const sockets: FakeConnectSocket[] = [];
    const raced = new ConnectBrowserTransport({
      firstPairing: true,
      privateKey: new Uint8Array(32).fill(1),
      localPublic: new Uint8Array(32).fill(2),
      location,
      cryptoLoader: () => {
        loads += 1;
        if (loads === 1) {
          return new Promise((resolve) => {
            resolveOld = resolve;
          });
        }
        return new Promise((resolve) => {
          resolveNew = resolve;
        });
      },
      socketFactory: () => {
        const socket = new FakeConnectSocket();
        sockets.push(socket);
        return socket;
      },
    });
    const first = raced.start();
    await Promise.resolve();
    raced.suspend();
    const second = raced.start();
    await Promise.resolve();
    resolveOld!(fakeConnectRuntime());
    await first;
    expect(sockets).toHaveLength(0);
    resolveNew!(fakeConnectRuntime());
    await second;
    expect(sockets).toHaveLength(1);
    expect(raced.state().kind).toBe("connecting");
    raced.stop();
  });

  it("suspends for pagehide then allows wake/start without permanent stop", async () => {
    const sockets: FakeConnectSocket[] = [];
    const transport = new ConnectBrowserTransport({
      firstPairing: true,
      privateKey: new Uint8Array(32).fill(1),
      localPublic: new Uint8Array(32).fill(2),
      location,
      cryptoLoader: async () => fakeConnectRuntime(),
      socketFactory: () => {
        const socket = new FakeConnectSocket();
        sockets.push(socket);
        return socket;
      },
    });
    await transport.start();
    sockets[0]!.emitOpen();
    transport.suspend();
    expect(transport.state()).toEqual({ kind: "idle" });
    expect(sockets[0]!.readyState).toBe(3);
    expect(transport.wake()).toBe("start");
    await expect.poll(() => sockets.length).toBeGreaterThanOrEqual(2);
    transport.stop();
    expect(transport.wake()).toBe("held");
  });

  it("replaces a ready channel after long background instead of no-op start", async () => {
    const sockets: FakeConnectSocket[] = [];
    const transport = new ConnectBrowserTransport({
      firstPairing: true,
      privateKey: new Uint8Array(32).fill(1),
      localPublic: new Uint8Array(32).fill(2),
      location,
      cryptoLoader: async () => fakeConnectRuntime(),
      socketFactory: () => {
        const socket = new FakeConnectSocket();
        sockets.push(socket);
        return socket;
      },
    });
    await transport.start();
    sockets[0]!.emitOpen();
    sockets[0]!.emit(connectGreeting());
    await expect.poll(() => sockets[0]!.sent.length).toBeGreaterThan(0);
    sockets[0]!.emit(new Uint8Array([0x03]));
    await expect.poll(() => sockets[0]!.sent.length).toBeGreaterThan(1);
    const helloFrame = decodeConnectSealedFrame(
      sockets[0]!.sent[sockets[0]!.sent.length - 1] as Uint8Array,
    );
    const hello = JSON.parse(
      new TextDecoder().decode(helloFrame.ciphertext),
    ) as {
      connectionId: string;
      sessionId: string;
      channelId: string;
    };
    const wasmTransport = new FakeConnectWasmTransport();
    sockets[0]!.emit(
      wasmTransport.seal(
        1n,
        new Uint8Array(16).fill(3),
        new TextEncoder().encode(
          JSON.stringify({
            protocolMajor: 1,
            protocolMinor: 0,
            connectionId: hello.connectionId,
            sessionId: hello.sessionId,
            channelId: hello.channelId,
            channel: "critical",
            sequence: 1,
            requestId: null,
            operationId: null,
            limits: connectLimits,
            compression: "none",
            privacyClass: "local_only",
            payloadKind: 1,
            payloadVersion: 1,
            payloadBase64: encodeBase64Json({
              capabilities: 0,
              limits: connectLimits,
              privacy_class: "local_only",
            }),
          }),
        ),
      ),
    );
    expect(transport.state()).toEqual({ kind: "ready" });
    expect(transport.wake({ hiddenDurationMs: 10_000 })).toBe("reconnect");
    await expect.poll(() => sockets.length).toBeGreaterThan(1);
    transport.stop();
  });

  it("does not open a new socket when wake follows protocol rejection", async () => {
    const sockets: FakeConnectSocket[] = [];
    const transport = new ConnectBrowserTransport({
      firstPairing: true,
      privateKey: new Uint8Array(32).fill(1),
      localPublic: new Uint8Array(32).fill(2),
      location,
      cryptoLoader: async () => fakeConnectRuntime(),
      socketFactory: () => {
        const socket = new FakeConnectSocket();
        sockets.push(socket);
        return socket;
      },
    });
    await transport.start();
    sockets[0]!.emitOpen();
    sockets[0]!.emit(new Uint8Array([0x00]));
    await expect.poll(() => transport.state().kind).toBe("closed");
    expect(transport.state()).toEqual({
      kind: "closed",
      reason: "Connect protocol rejected",
    });
    const before = sockets.length;
    expect(transport.wake()).toBe("held");
    await Promise.resolve();
    expect(sockets.length).toBe(before);
    transport.stop();
  });
});
