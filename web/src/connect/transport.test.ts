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
    WasmConnectHandshake: FakeConnectWasmHandshake as unknown as ConnectCryptoRuntime["WasmConnectHandshake"],
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
    expect(
      parseAdvertisedRelayUrl("https://relay.example/connect"),
    ).toBeNull();
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
      1,
      0,
      0,
      0,
      0,
      0,
      0,
      0,
      7,
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
    const hello = JSON.parse(new TextDecoder().decode(helloFrame.ciphertext)) as {
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
    const resync = JSON.parse(new TextDecoder().decode(resyncFrame.ciphertext)) as {
      sequence: number;
      payloadKind: number;
    };
    expect(resync).toMatchObject({ sequence: 2, payloadKind: 15 });

    const completion = {
      ...helloResponse,
      sequence: 4,
      payloadKind: 15,
      payloadBase64: encodeBase64Json({
        channel_sequence: 1,
        newest_sequence: 3,
        reason: "gap",
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
    expect(envelopes).toEqual([1, 15]);
  });

  it("generates protocol-valid v7 request identities", () => {
    expect(createConnectRequestId()).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i,
    );
  });
});
