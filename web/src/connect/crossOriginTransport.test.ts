import { describe, expect, it, vi } from "vitest";

import {
  buildConnectCrossOriginEndpoint,
  ConnectBrowserTransport,
  CONNECT_CROSS_ORIGIN_MAGIC,
  CONNECT_CROSS_ORIGIN_PATH,
  parseConnectCrossOriginEndpoint,
} from "./transport";
import type { ConnectCryptoRuntime } from "./crypto";

function fixtureKey(fill: number): Uint8Array {
  return new Uint8Array(32).fill(fill);
}

function greetingFrame(hostPublicId = new Uint8Array(16).fill(7)): Uint8Array {
  const bytes = new Uint8Array(5 + 16 + 16 + 16);
  bytes.set(new TextEncoder().encode("DMCN1"));
  bytes.set(hostPublicId, 5);
  bytes.set(new Uint8Array(16).fill(8), 21);
  bytes.set(new Uint8Array(16).fill(9), 37);
  // Force UUID v7 nibble pattern in host id for uuidFromBytes on routeId
  bytes[21 + 6] = (bytes[21 + 6]! & 0x0f) | 0x70;
  bytes[21 + 8] = (bytes[21 + 8]! & 0x3f) | 0x80;
  bytes[37 + 6] = (bytes[37 + 6]! & 0x0f) | 0x70;
  bytes[37 + 8] = (bytes[37 + 8]! & 0x3f) | 0x80;
  bytes[5 + 6] = (bytes[5 + 6]! & 0x0f) | 0x70;
  bytes[5 + 8] = (bytes[5 + 8]! & 0x3f) | 0x80;
  return bytes;
}

function cryptoRuntime(): ConnectCryptoRuntime {
  return {
    connect_protocol_major: () => 1,
    connect_noise_pattern: (firstPairing) =>
      firstPairing
        ? "Noise_XX_25519_ChaChaPoly_BLAKE2s"
        : "Noise_IK_25519_ChaChaPoly_BLAKE2s",
    encode_connect_envelope_json: (input) => new TextEncoder().encode(input),
    decode_connect_envelope_json: (input) => new TextDecoder().decode(input),
    encode_connect_payload_json: (input) => new TextEncoder().encode(input),
    decode_connect_payload_json: (input) => new TextDecoder().decode(input),
    WasmConnectHandshake: class {
      write_message() {
        return new Uint8Array([1]);
      }
      read_message() {}
      is_finished() {
        return false;
      }
      finish() {
        return {
          seal: () => new Uint8Array(),
          open: () => new Uint8Array(),
        };
      }
    },
  };
}

describe("cross-origin Connect endpoint", () => {
  it("accepts only exact wss path without URL secrets", () => {
    expect(
      parseConnectCrossOriginEndpoint(
        `wss://studio.example${CONNECT_CROSS_ORIGIN_PATH}`,
        "https://studio.example",
      ),
    ).toBe(`wss://studio.example${CONNECT_CROSS_ORIGIN_PATH}`);
    expect(
      parseConnectCrossOriginEndpoint(
        `wss://studio.example${CONNECT_CROSS_ORIGIN_PATH}?ticket=secret`,
        "https://studio.example",
      ),
    ).toBeNull();
    expect(
      parseConnectCrossOriginEndpoint(
        `ws://studio.example${CONNECT_CROSS_ORIGIN_PATH}`,
        "https://studio.example",
      ),
    ).toBeNull();
    expect(buildConnectCrossOriginEndpoint("http://127.0.0.1:9")).toBeNull();
  });
});

describe("ConnectBrowserTransport cross-origin prelude", () => {
  it("sends DMCX1 ticket once then resume on wake reconnect", async () => {
    const sent: Uint8Array[] = [];
    let socketHandler: {
      onopen: ((event: unknown) => void) | null;
      onmessage: ((event: { data: unknown }) => void) | null;
      onclose: ((event: { code?: number; reason?: string }) => void) | null;
    } | null = null;
    const hostId = new Uint8Array(16).fill(7);
    hostId[6] = (hostId[6]! & 0x0f) | 0x70;
    hostId[8] = (hostId[8]! & 0x3f) | 0x80;

    const transport = new ConnectBrowserTransport({
      firstPairing: true,
      privateKey: fixtureKey(1),
      localPublic: fixtureKey(2),
      expectedRemote: fixtureKey(3),
      hostPublicId: hostId,
      explicitTarget: {
        origin: "https://studio.example",
        endpoint: `wss://studio.example${CONNECT_CROSS_ORIGIN_PATH}`,
      },
      crossOriginTicket: "one-use-ticket",
      cryptoLoader: async () => cryptoRuntime(),
      socketFactory: () => {
        const socket = {
          readyState: 1,
          binaryType: "arraybuffer" as BinaryType,
          onopen: null as ((event: unknown) => void) | null,
          onmessage: null as ((event: { data: unknown }) => void) | null,
          onclose: null as ((event: { code?: number; reason?: string }) => void) | null,
          onerror: null as ((event: unknown) => void) | null,
          send(data: Uint8Array) {
            sent.push(Uint8Array.from(data));
          },
          close() {},
        };
        socketHandler = socket;
        return socket;
      },
    });

    await transport.start();
    socketHandler!.onopen?.({});
    expect(new TextDecoder().decode(sent[0]!)).toBe(CONNECT_CROSS_ORIGIN_MAGIC);
    expect(JSON.parse(new TextDecoder().decode(sent[1]!))).toEqual({
      type: "ticket",
      ticket: "one-use-ticket",
    });

    // Ticket is one-shot. Production wake only replaces a ready/resyncing
    // channel; reconnect after suspend (idle) exercises resume admission.
    const firstSocket = socketHandler;
    transport.suspend();
    expect(transport.state().kind).toBe("idle");
    transport.wake();
    await expect.poll(() => socketHandler !== firstSocket).toBe(true);
    socketHandler!.onopen?.({});
    const admissions = sent
      .map((bytes) => {
        try {
          return JSON.parse(new TextDecoder().decode(bytes));
        } catch {
          return null;
        }
      })
      .filter(Boolean) as Array<{ type: string }>;
    expect(admissions.filter((item) => item.type === "ticket")).toHaveLength(1);
    expect(admissions.some((item) => item.type === "resume")).toBe(true);
    transport.stop();
  });

  it("denies a wrong host public id before Noise material is used", async () => {
    const factory = vi.fn(async () => ({
      privateKey: fixtureKey(9),
      localPublic: fixtureKey(2),
    }));
    let socketHandler: {
      onopen: ((event: unknown) => void) | null;
      onmessage: ((event: { data: unknown }) => void) | null;
    } | null = null;
    const expectedHost = new Uint8Array(16).fill(1);
    expectedHost[6] = (expectedHost[6]! & 0x0f) | 0x70;
    expectedHost[8] = (expectedHost[8]! & 0x3f) | 0x80;
    const foreignHost = new Uint8Array(16).fill(2);
    foreignHost[6] = (foreignHost[6]! & 0x0f) | 0x70;
    foreignHost[8] = (foreignHost[8]! & 0x3f) | 0x80;

    const transport = new ConnectBrowserTransport({
      firstPairing: true,
      localPublic: fixtureKey(2),
      handshakeMaterialFactory: factory,
      hostPublicId: expectedHost,
      expectedRemote: fixtureKey(3),
      explicitTarget: {
        origin: "https://studio.example",
        endpoint: `wss://studio.example${CONNECT_CROSS_ORIGIN_PATH}`,
      },
      cryptoLoader: async () => cryptoRuntime(),
      socketFactory: () => {
        const socket = {
          readyState: 1,
          binaryType: "arraybuffer" as BinaryType,
          onopen: null as ((event: unknown) => void) | null,
          onmessage: null as ((event: { data: unknown }) => void) | null,
          onclose: null as ((event: { code?: number; reason?: string }) => void) | null,
          onerror: null as ((event: unknown) => void) | null,
          send() {},
          close() {},
        };
        socketHandler = socket;
        return socket;
      },
    });

    await transport.start();
    socketHandler!.onopen?.({});
    await socketHandler!.onmessage?.({ data: greetingFrame(foreignHost).buffer });
    await Promise.resolve();
    expect(factory).not.toHaveBeenCalled();
    expect(transport.state().kind).toBe("closed");
    transport.stop();
  });

  it("keeps Noise XX for ticket and resume (production browser fleet pattern)", async () => {
    const patterns: boolean[] = [];
    const runtime = cryptoRuntime();
    const original = runtime.connect_noise_pattern.bind(runtime);
    runtime.connect_noise_pattern = (firstPairing: boolean) => {
      patterns.push(firstPairing);
      return original(firstPairing);
    };
    // Exhaustive: resume admission still requests XX, never IK.
    expect(runtime.connect_noise_pattern(true)).toBe(
      "Noise_XX_25519_ChaChaPoly_BLAKE2s",
    );
    expect(patterns.every(Boolean)).toBe(true);
  });
});
