import { describe, expect, it, vi } from "vitest";

import {
  CONNECT_HOST_PUBLICATION_KEY,
  CONNECT_IDENTITY_HOLD,
  CONNECT_IDENTITY_RECORD_SCHEMA,
  CONNECT_IDENTITY_RECORD_VERSION,
  ConnectBrowserIdentityHoldError,
  bootstrapConnect,
  bootstrapCrossOriginConnect,
  connectDevicePublicId,
  createConnectHandshakeMaterialFactory,
  installConnectDocumentPublication,
  loadOrCreateConnectIdentity,
  parseX25519Pkcs8PrivateKey,
  readConnectHostPublication,
  type ConnectIdentityStorage,
  type PersistedConnectIdentity,
} from "./identity";
import { CONNECT_STORE_CONFIGURATION_KEY } from "./storeAdapter";
import type { ConnectCryptoRuntime } from "./crypto";
import { parseConnectGreeting } from "./transport";
import { HostTrustHoldError } from "./hostTrust";

const marker = {
  transport: "connect" as const,
  endpoint: "/api/connect",
  generation: 7,
  protocolMajor: 1,
  protocolMinor: 0,
};

function realCrypto() {
  const value = globalThis.crypto;
  if (!value?.subtle) {
    throw new Error("Node WebCrypto is required for custody tests");
  }
  // Node's WebCrypto exposes CryptoKey; custody prefers shape+decrypt over instanceof.
  expect(typeof globalThis.CryptoKey).toBe("function");
  return value;
}

function memoryIdentityStorage(
  initial: PersistedConnectIdentity | null = null,
): ConnectIdentityStorage & {
  dump(): unknown;
  seed(record: unknown): void;
} {
  let record: unknown = initial;
  return {
    async load() {
      return (record as PersistedConnectIdentity | null) ?? null;
    },
    async putIfAbsent(next) {
      if (record != null) return record as PersistedConnectIdentity;
      record = next;
      return next;
    },
    async clear() {
      record = null;
    },
    dump() {
      return record;
    },
    seed(next) {
      record = next;
    },
  };
}

function pairedFetch(): typeof fetch {
  return (async () =>
    new Response(JSON.stringify({ ok: true }), {
      status: 200,
    })) as typeof fetch;
}

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

function runtimeFixture(
  handshakeCtor?: ConnectCryptoRuntime["WasmConnectHandshake"],
): ConnectCryptoRuntime {
  return {
    WasmConnectHandshake:
      handshakeCtor ??
      (vi.fn(function WasmConnectHandshake() {
        return {
          write_message: () => new Uint8Array([0x02]),
          read_message: () => {},
          is_finished: () => true,
          finish: () => ({
            seal: () => new Uint8Array([1]),
            open: () => new Uint8Array([1]),
            free: vi.fn(),
          }),
          free: vi.fn(),
        };
      }) as unknown as ConnectCryptoRuntime["WasmConnectHandshake"]),
    connect_protocol_major: () => 1,
    connect_noise_pattern: (firstPairing) =>
      firstPairing
        ? "Noise_XX_25519_ChaChaPoly_BLAKE2s"
        : "Noise_IK_25519_ChaChaPoly_BLAKE2s",
    encode_connect_envelope_json: () => new Uint8Array([1]),
    decode_connect_envelope_json: () => "{}",
    encode_connect_payload_json: () => new Uint8Array([1]),
    decode_connect_payload_json: () => "{}",
  };
}

describe("Connect browser identity bootstrap", () => {
  it("installs inert host identity metadata before asynchronous bootstrap", () => {
    const publicMarker = { ...marker, hostPublicId: "01234567-89ab-7000-8000-000000000017", hostPublicKey: "ab".repeat(32) };
    const source = (raw: string) => ({ querySelector: () => ({ getAttribute: () => raw }) }) as unknown as Pick<Document, "querySelector">;
    const host: Record<string, unknown> = {};
    expect(installConnectDocumentPublication(source(JSON.stringify(publicMarker)), host)).toBe(true);
    expect(readConnectHostPublication(host)).toEqual(publicMarker);
    expect(host[CONNECT_STORE_CONFIGURATION_KEY]).toEqual({ transport: "connect" });
    for (const raw of ["invalid", "x".repeat(4097), JSON.stringify({ ...publicMarker, hostPublicKey: "00".repeat(32) }),
      JSON.stringify({ ...marker, hostPublicId: publicMarker.hostPublicId })]) {
      installConnectDocumentPublication(source(raw), host);
      expect(readConnectHostPublication(host)).toBeNull();
      expect(host[CONNECT_STORE_CONFIGURATION_KEY]).toEqual({ transport: "connect" });
    }
  });
  it("accepts only the bounded current host publication marker", () => {
    expect(
      readConnectHostPublication({ [CONNECT_HOST_PUBLICATION_KEY]: marker }),
    ).toEqual(marker);
    expect(
      readConnectHostPublication({
        [CONNECT_HOST_PUBLICATION_KEY]: { ...marker, endpoint: "/api/ws" },
      }),
    ).toBeNull();
    expect(
      readConnectHostPublication({
        [CONNECT_HOST_PUBLICATION_KEY]: { ...marker, generation: 0 },
      }),
    ).toBeNull();
  });

  it("does not install a transport without a host publication", async () => {
    const host: Record<string, unknown> = {};
    expect(await bootstrapConnect({ host })).toBeNull();
    expect(host[CONNECT_STORE_CONFIGURATION_KEY]).toBeUndefined();
  });

  it("publishes Connect and HOLDs on malformed markers instead of legacy fallback", async () => {
    const host: Record<string, unknown> = {
      [CONNECT_HOST_PUBLICATION_KEY]: {
        transport: "connect",
        endpoint: "/api/ws",
        generation: 7,
        protocolMajor: 1,
        protocolMinor: 0,
      },
    };
    await expect(bootstrapConnect({ host })).rejects.toMatchObject({
      code: CONNECT_IDENTITY_HOLD,
    });
    expect(host[CONNECT_STORE_CONFIGURATION_KEY]).toEqual({
      transport: "connect",
    });
    expect(
      (host[CONNECT_STORE_CONFIGURATION_KEY] as { connectTransport?: unknown })
        .connectTransport,
    ).toBeUndefined();

    const badVersion: Record<string, unknown> = {
      [CONNECT_STORE_CONFIGURATION_KEY]: {
        transport: "connect",
        endpoint: "/api/connect",
        generation: 3,
        protocolMajor: 2,
        protocolMinor: 0,
      },
    };
    await expect(bootstrapConnect({ host: badVersion })).rejects.toMatchObject({
      code: CONNECT_IDENTITY_HOLD,
    });
    expect(badVersion[CONNECT_STORE_CONFIGURATION_KEY]).toEqual({
      transport: "connect",
    });
  });

  it("publishes Connect selection before pairing fails so legacy cannot win", async () => {
    const host: Record<string, unknown> = {
      [CONNECT_HOST_PUBLICATION_KEY]: marker,
    };
    const fetcher = async () => new Response(null, { status: 401 });
    await expect(
      bootstrapConnect({ host, fetch: fetcher }),
    ).rejects.toMatchObject({
      code: CONNECT_IDENTITY_HOLD,
    });
    expect(host[CONNECT_STORE_CONFIGURATION_KEY]).toMatchObject({
      transport: "connect",
      endpoint: "/api/connect",
      generation: 7,
    });
    expect(
      (host[CONNECT_STORE_CONFIGURATION_KEY] as { connectTransport?: unknown })
        .connectTransport,
    ).toBeUndefined();
  });

  it("pins complete host metadata before making the pairing request", async () => {
    const host: Record<string, unknown> = {
      [CONNECT_HOST_PUBLICATION_KEY]: {
        ...marker,
        hostPublicId: "01234567-89ab-7000-8000-000000000017",
        hostPublicKey: "ab".repeat(32),
      },
    };
    const fetcher = vi.fn(async () => new Response(null, { status: 401 }));
    await expect(
      bootstrapConnect({
        host,
        fetch: fetcher,
        location: { protocol: "https:", host: "phone.example.test" },
        hostTrustStorage: {
          pin: async () => {
            throw new HostTrustHoldError("changed host key");
          },
        },
      }),
    ).rejects.toBeInstanceOf(HostTrustHoldError);
    expect(fetcher).not.toHaveBeenCalled();
    expect(host[CONNECT_STORE_CONFIGURATION_KEY]).toMatchObject({
      transport: "connect",
    });
  });

  it("uses a typed hold rather than a plaintext fallback when WebCrypto cannot create X25519", async () => {
    const host: Record<string, unknown> = {
      [CONNECT_HOST_PUBLICATION_KEY]: marker,
    };
    const storage = memoryIdentityStorage();
    const crypto = {
      subtle: {
        generateKey: async () => {
          throw new Error("X25519 unavailable");
        },
      },
      getRandomValues<T extends ArrayBufferView | null>(array: T): T {
        return array;
      },
    } as unknown as Crypto;
    await expect(
      bootstrapConnect({
        host,
        fetch: pairedFetch(),
        crypto,
        storage,
        cryptoLoader: async () => runtimeFixture(),
      }),
    ).rejects.toBeInstanceOf(ConnectBrowserIdentityHoldError);
    expect(host[CONNECT_STORE_CONFIGURATION_KEY]).toMatchObject({
      transport: "connect",
      endpoint: "/api/connect",
      generation: 7,
    });
    expect(storage.dump()).toBeNull();
  });

  it("bootstraps readiness from CONNECT_STORE_CONFIGURATION_KEY alone", async () => {
    const host: Record<string, unknown> = {
      [CONNECT_STORE_CONFIGURATION_KEY]: { ...marker },
    };
    const storage = memoryIdentityStorage();
    const socket = new FakeConnectSocket();
    const handshakeCtor = vi.fn(function Handshake() {
      return {
        write_message: () => new Uint8Array([0x02]),
        read_message: () => {},
        is_finished: () => false,
        finish: () => ({
          seal: () => new Uint8Array([1]),
          open: () => new Uint8Array([1]),
        }),
      };
    });
    const loader = vi.fn(async () =>
      runtimeFixture(
        handshakeCtor as unknown as ConnectCryptoRuntime["WasmConnectHandshake"],
      ),
    );
    const handle = await bootstrapConnect({
      host,
      fetch: pairedFetch(),
      storage,
      crypto: realCrypto(),
      cryptoLoader: loader,
      location: { protocol: "http:", host: "example.test" },
      transportOptions: {
        socketFactory: () => socket,
      },
    });
    expect(handle).not.toBeNull();
    expect(host[CONNECT_STORE_CONFIGURATION_KEY]).toMatchObject({
      transport: "connect",
      endpoint: "/api/connect",
      generation: 7,
      protocolMajor: 1,
      protocolMinor: 0,
    });
    expect(
      (host[CONNECT_STORE_CONFIGURATION_KEY] as { connectTransport?: unknown })
        .connectTransport,
    ).toBe(handle!.transport);

    await handle!.transport.start();
    socket.emitOpen();
    socket.emit(connectGreeting());
    await expect.poll(() => handshakeCtor.mock.calls.length).toBe(1);
    handle!.stop();
  });

  it("bootstraps through greeting with a persisted device claim, never a static-key device id", async () => {
    const host: Record<string, unknown> = {
      [CONNECT_HOST_PUBLICATION_KEY]: marker,
    };
    const storage = memoryIdentityStorage();
    const socket = new FakeConnectSocket();
    const handshakeCtor = vi.fn(function Handshake(
      _pattern: string,
      _firstPairing: boolean,
      _role: number,
      privateKey: Uint8Array,
      localPublic: Uint8Array,
      expectedRemote: Uint8Array | undefined,
      hostPublicId: Uint8Array,
      devicePublicId: Uint8Array | undefined,
    ) {
      expect(privateKey.byteLength).toBe(32);
      expect(localPublic.byteLength).toBe(32);
      expect(expectedRemote).toBeUndefined();
      expect(hostPublicId.byteLength).toBe(16);
      expect(devicePublicId?.byteLength).toBe(16);
      return {
        write_message: () => new Uint8Array([0x02]),
        read_message: () => {},
        is_finished: () => false,
        finish: () => ({
          seal: () => new Uint8Array([1]),
          open: () => new Uint8Array([1]),
        }),
      };
    });
    const loader = vi.fn(async () =>
      runtimeFixture(
        handshakeCtor as unknown as ConnectCryptoRuntime["WasmConnectHandshake"],
      ),
    );
    const handle = await bootstrapConnect({
      host,
      fetch: pairedFetch(),
      storage,
      crypto: realCrypto(),
      cryptoLoader: loader,
      location: { protocol: "http:", host: "example.test" },
      transportOptions: {
        socketFactory: () => socket,
      },
    });
    expect(handle).not.toBeNull();
    expect(loader).toHaveBeenCalledTimes(1);
    const published = host[CONNECT_STORE_CONFIGURATION_KEY] as Record<
      string,
      unknown
    >;
    expect(published.connectTransport).toBe(handle!.transport);
    expect(published.connectRequest).toBeUndefined();

    await handle!.transport.start();
    socket.emitOpen();
    const greeting = connectGreeting();
    expect(parseConnectGreeting(greeting)).not.toBeNull();
    socket.emit(greeting);
    await expect
      .poll(() => handshakeCtor.mock.calls.length)
      .toBe(1);
    const args = handshakeCtor.mock.calls[0]!;
    expect(args[3].byteLength).toBe(32);
    expect(args[4].byteLength).toBe(32);
    expect(args[7]).toEqual(connectDevicePublicId(handle!.identity.deviceId));
    handle!.stop();
  });

  it("preserves both persisted custody ID formats without treating them as canonical IDs", () => {
    const id = "00000001-0002-4003-8004-000000000005";
    expect(connectDevicePublicId(id)).toEqual(
      connectDevicePublicId("connect-00000001000240038004000000000005"),
    );
    expect(connectDevicePublicId(id)).toHaveLength(16);
    for (const invalid of ["", "legacy-device", "00".repeat(32), "connect-" + "00".repeat(16)]) {
      expect(() => connectDevicePublicId(invalid)).toThrow(ConnectBrowserIdentityHoldError);
    }
  });

  it("uses transportOptions.cryptoLoader when top-level cryptoLoader is omitted", async () => {
    const host: Record<string, unknown> = {
      [CONNECT_HOST_PUBLICATION_KEY]: marker,
    };
    const storage = memoryIdentityStorage();
    const loader = vi.fn(async () => runtimeFixture());
    const handle = await bootstrapConnect({
      host,
      fetch: pairedFetch(),
      storage,
      crypto: realCrypto(),
      transportOptions: {
        cryptoLoader: loader,
        location: { protocol: "http:", host: "example.test" },
      },
    });
    expect(handle).not.toBeNull();
    expect(loader).toHaveBeenCalledTimes(1);
    handle!.stop();
  });

  it("keeps Connect selected when WASM is incompatible after custody succeeds", async () => {
    const host: Record<string, unknown> = {
      [CONNECT_HOST_PUBLICATION_KEY]: marker,
    };
    const storage = memoryIdentityStorage();
    await expect(
      bootstrapConnect({
        host,
        fetch: pairedFetch(),
        storage,
        crypto: realCrypto(),
        cryptoLoader: async () => {
          throw new Error("wasm-missing");
        },
      }),
    ).rejects.toMatchObject({ code: "browser-e2e-transport-held" });
    expect(host[CONNECT_STORE_CONFIGURATION_KEY]).toMatchObject({
      transport: "connect",
      generation: 7,
    });
    expect(
      (host[CONNECT_STORE_CONFIGURATION_KEY] as { connectTransport?: unknown })
        .connectTransport,
    ).toBeUndefined();
  });
});

describe("Connect wrapped identity custody", () => {
  it("preserves key material across higher then lower host generations without rewrite", async () => {
    const storage = memoryIdentityStorage();
    const first = await loadOrCreateConnectIdentity(9, {
      storage,
      crypto: realCrypto(),
    });
    const before = storage.dump() as PersistedConnectIdentity;
    const ciphertextBefore = new Uint8Array(before.ciphertext).slice();
    const ivBefore = new Uint8Array(before.iv).slice();
    const second = await loadOrCreateConnectIdentity(3, {
      storage,
      crypto: realCrypto(),
    });
    const after = storage.dump() as PersistedConnectIdentity;
    expect(second.deviceId).toBe(first.deviceId);
    expect(Array.from(second.publicKey)).toEqual(Array.from(first.publicKey));
    expect(second.hostGeneration).toBe(3);
    expect(after.hostGeneration).toBe(9);
    expect(Array.from(new Uint8Array(after.ciphertext))).toEqual(
      Array.from(ciphertextBefore),
    );
    expect(Array.from(new Uint8Array(after.iv))).toEqual(Array.from(ivBefore));
    const firstPrivate = await first.unwrapPrivateKey();
    const secondPrivate = await second.unwrapPrivateKey();
    expect(Array.from(secondPrivate)).toEqual(Array.from(firstPrivate));
    firstPrivate.fill(0);
    secondPrivate.fill(0);
  });

  it("persists only wrapped ciphertext and rejects metadata swaps", async () => {
    const storage = memoryIdentityStorage();
    const custody = await loadOrCreateConnectIdentity(1, {
      storage,
      crypto: realCrypto(),
    });
    const record = storage.dump() as PersistedConnectIdentity;
    storage.seed({
      ...record,
      deviceId: "swapped-device-id",
    });
    await expect(
      loadOrCreateConnectIdentity(1, { storage, crypto: realCrypto() }),
    ).rejects.toMatchObject({ code: CONNECT_IDENTITY_HOLD });
    const untouched = await custody.unwrapPrivateKey();
    expect(untouched.byteLength).toBe(32);
    untouched.fill(0);
  });

  it("rejects ciphertext tampering and invalid wrapping keys", async () => {
    const storage = memoryIdentityStorage();
    await loadOrCreateConnectIdentity(1, { storage, crypto: realCrypto() });
    const record = storage.dump() as PersistedConnectIdentity;
    const tamperedCipher = new Uint8Array(record.ciphertext);
    tamperedCipher[0] ^= 0xff;
    storage.seed({
      ...record,
      ciphertext: tamperedCipher.buffer.slice(
        tamperedCipher.byteOffset,
        tamperedCipher.byteOffset + tamperedCipher.byteLength,
      ),
    });
    await expect(
      loadOrCreateConnectIdentity(1, { storage, crypto: realCrypto() }),
    ).rejects.toMatchObject({ code: CONNECT_IDENTITY_HOLD });

    const fresh = memoryIdentityStorage();
    await loadOrCreateConnectIdentity(1, {
      storage: fresh,
      crypto: realCrypto(),
    });
    const valid = fresh.dump() as PersistedConnectIdentity;
    const hmacKey = await realCrypto().subtle.generateKey(
      { name: "HMAC", hash: "SHA-256" },
      false,
      ["sign", "verify"],
    );
    fresh.seed({ ...valid, wrappingKey: hmacKey });
    await expect(
      loadOrCreateConnectIdentity(1, { storage: fresh, crypto: realCrypto() }),
    ).rejects.toMatchObject({ code: CONNECT_IDENTITY_HOLD });
  });

  it("fails closed on opaque v1 records instead of rotating them", async () => {
    const storage = memoryIdentityStorage();
    const v1 = {
      version: 1,
      deviceId: "legacy-device",
      publicKey: new Uint8Array(32).fill(7).buffer,
      privateCryptoKey: {} as CryptoKey,
      hostGeneration: 1,
      createdAt: 1,
    };
    storage.seed(v1);
    await expect(
      loadOrCreateConnectIdentity(1, { storage, crypto: realCrypto() }),
    ).rejects.toMatchObject({
      code: CONNECT_IDENTITY_HOLD,
      message: expect.stringContaining("explicit repair"),
    });
    expect(storage.dump()).toMatchObject({
      version: 1,
      deviceId: "legacy-device",
    });
  });

  it("holds when storage is unavailable and never prints key bytes", async () => {
    const storage: ConnectIdentityStorage = {
      load: async () => {
        throw new Error("disk-private-key-aabbccddeeff");
      },
      putIfAbsent: async (record) => record,
      clear: async () => {},
    };
    await expect(
      loadOrCreateConnectIdentity(1, { storage, crypto: realCrypto() }),
    ).rejects.toSatisfy((error: unknown) => {
      expect(error).toBeInstanceOf(ConnectBrowserIdentityHoldError);
      expect(String(error)).not.toContain("aabbccddeeff");
      return true;
    });
  });

  it("parses WebCrypto PKCS8 strictly and rejects noncanonical long lengths", async () => {
    const keyPair = (await realCrypto().subtle.generateKey(
      { name: "X25519" },
      true,
      ["deriveBits"],
    )) as CryptoKeyPair;
    const pkcs8 = new Uint8Array(
      await realCrypto().subtle.exportKey("pkcs8", keyPair.privateKey),
    );
    const parsed = parseX25519Pkcs8PrivateKey(pkcs8);
    expect(parsed.byteLength).toBe(32);
    const padded = new Uint8Array(pkcs8.byteLength + 1);
    padded.set(pkcs8);
    expect(() => parseX25519Pkcs8PrivateKey(padded)).toThrow(
      ConnectBrowserIdentityHoldError,
    );
    const truncated = pkcs8.slice(0, pkcs8.byteLength - 1);
    expect(() => parseX25519Pkcs8PrivateKey(truncated)).toThrow(
      ConnectBrowserIdentityHoldError,
    );
    // Nonminimal long-form length 0x82 0x00 0x2e for a 46-byte sequence body.
    const noncanonical = new Uint8Array([
      0x30,
      0x82,
      0x00,
      0x2e,
      ...pkcs8.slice(2),
    ]);
    expect(() => parseX25519Pkcs8PrivateKey(noncanonical)).toThrow(
      ConnectBrowserIdentityHoldError,
    );
    parsed.fill(0);
  });

  it("commits storage before returning and resolves first-create races with putIfAbsent", async () => {
    let stored: PersistedConnectIdentity | null = null;
    const order: string[] = [];
    const storage: ConnectIdentityStorage = {
      async load() {
        return stored;
      },
      async putIfAbsent(record) {
        order.push("put");
        if (stored) return stored;
        stored = record;
        return record;
      },
      async clear() {
        stored = null;
      },
    };
    const [a, b] = await Promise.all([
      loadOrCreateConnectIdentity(1, {
        storage,
        crypto: realCrypto(),
        locks: null,
      }),
      loadOrCreateConnectIdentity(1, {
        storage,
        crypto: realCrypto(),
        locks: null,
      }),
    ]);
    expect(a.deviceId).toBe(b.deviceId);
    expect(order.length).toBeGreaterThanOrEqual(1);
    expect(Array.from(a.publicKey)).toEqual(Array.from(b.publicKey));
    const committed = await storage.load();
    expect(committed).toEqual(
      expect.objectContaining({
        version: CONNECT_IDENTITY_RECORD_VERSION,
        schema: CONNECT_IDENTITY_RECORD_SCHEMA,
      }),
    );
  });

  it("authenticates a putIfAbsent winner even when the loser created a different wrap", async () => {
    const storage = memoryIdentityStorage();
    const winner = await loadOrCreateConnectIdentity(1, {
      storage,
      crypto: realCrypto(),
    });
    const corruptLoser: ConnectIdentityStorage = {
      async load() {
        return null;
      },
      async putIfAbsent() {
        // Simulate losing the create race to an already-valid winner.
        return storage.dump() as PersistedConnectIdentity;
      },
      async clear() {},
    };
    const resolved = await loadOrCreateConnectIdentity(1, {
      storage: corruptLoser,
      crypto: realCrypto(),
      locks: null,
    });
    expect(resolved.deviceId).toBe(winner.deviceId);
    const privateKey = await resolved.unwrapPrivateKey();
    expect(privateKey.byteLength).toBe(32);
    privateKey.fill(0);
  });

  it("builds a handshake factory that yields wipeable temporary private bytes", async () => {
    const storage = memoryIdentityStorage();
    const custody = await loadOrCreateConnectIdentity(1, {
      storage,
      crypto: realCrypto(),
    });
    const factory = createConnectHandshakeMaterialFactory(custody);
    const material = await factory();
    expect(material.privateKey.byteLength).toBe(32);
    material.privateKey.fill(0);
    expect(material.privateKey.every((value) => value === 0)).toBe(true);
    const again = await factory();
    expect(again.privateKey.some((value) => value !== 0)).toBe(true);
    again.privateKey.fill(0);
  });
});

describe("bootstrapCrossOriginConnect", () => {
  const remoteDescriptor = {
    hostPublicId: "01234567-89ab-7000-8000-000000000002",
    hostPublicKey: "cd".repeat(32),
    origin: "https://studio.example",
    label: "Studio",
    generation: 4,
    protocolMajor: 1 as const,
    protocolMinor: 0,
    isPageHost: false,
  };

  it("pins trust to host B origin before pair fetch and never retargets page A", async () => {
    const pins: Array<{ origin: string; hostPublicId: string }> = [];
    const fetcher = vi.fn(async () => {
      throw new Error("should not fetch after pin hold");
    });
    await expect(
      bootstrapCrossOriginConnect({
        descriptor: remoteDescriptor,
        grant: { grant: "grant-token" },
        fetch: fetcher,
        storage: memoryIdentityStorage(),
        crypto: realCrypto(),
        cryptoLoader: async () => runtimeFixture(),
        hostTrustStorage: {
          pin: async (record) => {
            pins.push({
              origin: record.origin,
              hostPublicId: record.hostPublicId,
            });
            throw new HostTrustHoldError("pin held");
          },
        },
      }),
    ).rejects.toBeInstanceOf(HostTrustHoldError);
    expect(pins).toEqual([
      {
        origin: "https://studio.example",
        hostPublicId: remoteDescriptor.hostPublicId,
      },
    ]);
    expect(fetcher).not.toHaveBeenCalled();
  });

  it("rejects a foreign hostPublicId from pair response before opening a socket", async () => {
    const storage = memoryIdentityStorage();
    const sockets: unknown[] = [];
    await expect(
      bootstrapCrossOriginConnect({
        descriptor: remoteDescriptor,
        grant: { grant: "grant-token" },
        storage,
        crypto: realCrypto(),
        cryptoLoader: async () => runtimeFixture(),
        hostTrustStorage: {
          pin: async (record) => record,
        },
        fetch: (async () => {
          const bytes = new TextEncoder().encode(
            JSON.stringify({
              attachTicket: "ticket-1",
              expiresAtEpochMs: Date.now() + 30_000,
              hostPublicId: "01234567-89ab-7000-8000-000000000099",
              clientId: "01234567-89ab-7000-8000-000000000088",
            }),
          );
          const stream = new ReadableStream<Uint8Array>({
            start(controller) {
              controller.enqueue(bytes);
              controller.close();
            },
          });
          return new Response(stream, { status: 200 });
        }) as typeof fetch,
        transportOptions: {
          socketFactory: (url) => {
            sockets.push(url);
            return new FakeConnectSocket();
          },
        },
      }),
    ).rejects.toMatchObject({ code: CONNECT_IDENTITY_HOLD });
    expect(sockets).toHaveLength(0);
    const dumped = storage.dump() as PersistedConnectIdentity | null;
    expect(dumped && JSON.stringify(dumped)).not.toMatch(/privateKey|grant-token|ticket-1/);
  });

  it("resumes a known pin without pair fetch when no grant is supplied", async () => {
    const storage = memoryIdentityStorage();
    const fetcher = vi.fn(async () => new Response(null, { status: 500 }));
    const handle = await bootstrapCrossOriginConnect({
      descriptor: remoteDescriptor,
      storage,
      crypto: realCrypto(),
      cryptoLoader: async () => runtimeFixture(),
      hostTrustStorage: { pin: async (record) => record },
      fetch: fetcher,
      transportOptions: {
        socketFactory: () => new FakeConnectSocket(),
        firstPairing: true,
      },
    });
    expect(fetcher).not.toHaveBeenCalled();
    expect(handle.marker.hostPublicId).toBe(remoteDescriptor.hostPublicId);
    handle.stop();
  });

  it("bounds pair response body under one absolute deadline", async () => {
    const storage = memoryIdentityStorage();
    await expect(
      bootstrapCrossOriginConnect({
        descriptor: remoteDescriptor,
        grant: { grant: "grant-token" },
        storage,
        crypto: realCrypto(),
        cryptoLoader: async () => runtimeFixture(),
        hostTrustStorage: { pin: async (record) => record },
        pairDeadlineMs: 30,
        fetch: (async () => {
          const stream = new ReadableStream({
            start(controller) {
              controller.enqueue(new TextEncoder().encode('{"attachTicket":"'));
              // Never closes — deadline must cancel the reader.
            },
          });
          return new Response(stream, { status: 200 });
        }) as typeof fetch,
      }),
    ).rejects.toMatchObject({ code: CONNECT_IDENTITY_HOLD });
  });

  it("fail-closes pair body read when Response has no stream reader", async () => {
    const storage = memoryIdentityStorage();
    await expect(
      bootstrapCrossOriginConnect({
        descriptor: remoteDescriptor,
        grant: { grant: "grant-token" },
        storage,
        crypto: realCrypto(),
        cryptoLoader: async () => runtimeFixture(),
        hostTrustStorage: { pin: async (record) => record },
        fetch: (async () =>
          ({
            ok: true,
            status: 200,
            body: null,
          }) as unknown as Response) as typeof fetch,
      }),
    ).rejects.toMatchObject({ code: CONNECT_IDENTITY_HOLD });
  });
});
