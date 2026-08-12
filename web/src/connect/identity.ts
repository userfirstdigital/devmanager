import {
  CONNECT_STORE_CONFIGURATION_KEY,
} from "./storeAdapter";
import type {
  ConnectBrowserTransport,
  ConnectBrowserTransportOptions,
} from "./transport";

/** The native host publishes this marker separately from the store handoff. */
export const CONNECT_HOST_PUBLICATION_KEY =
  "__DEVMANAGER_CONNECT_HOST__" as const;
export const CONNECT_IDENTITY_DB_NAME = "devmanager.connect.identity" as const;
export const CONNECT_IDENTITY_DB_VERSION = 1 as const;
export const CONNECT_IDENTITY_RECORD_KEY = "device" as const;

export const CONNECT_IDENTITY_HOLD = "browser-connect-identity-held" as const;

const IDENTITY_STORE_NAME = "identity";
const CONNECT_ENDPOINT = "/api/connect";

export class ConnectBrowserIdentityHoldError extends Error {
  readonly code = CONNECT_IDENTITY_HOLD;

  constructor(message: string) {
    super(message);
    this.name = "ConnectBrowserIdentityHoldError";
  }
}

export interface ConnectHostPublication {
  transport: "connect";
  endpoint: string;
  generation: number;
  protocolMajor: number;
  protocolMinor: number;
}

/** Only public material and the host generation leave this module. */
export interface ConnectDeviceIdentity {
  deviceId: string;
  publicKey: Uint8Array;
  /** Opaque key custody; the current WASM adapter cannot consume this yet. */
  privateCryptoKey: CryptoKey;
  hostGeneration: number;
}

interface PersistedConnectIdentity {
  version: 1;
  deviceId: string;
  publicKey: ArrayBuffer;
  /** Non-extractable X25519 key retained as the browser's device identity. */
  privateCryptoKey: CryptoKey;
  hostGeneration: number;
  createdAt: number;
}

export interface ConnectIdentityStorage {
  load(): Promise<PersistedConnectIdentity | null>;
  save(record: PersistedConnectIdentity): Promise<void>;
  clear(): Promise<void>;
}

export interface ConnectIdentityCrypto {
  readonly subtle: SubtleCrypto;
  getRandomValues<T extends ArrayBufferView | null>(array: T): T;
  randomUUID?(): string;
}

export interface ConnectIdentityOptions {
  storage?: ConnectIdentityStorage;
  crypto?: ConnectIdentityCrypto;
  now?: () => number;
}

export interface ConnectBootstrapOptions extends ConnectIdentityOptions {
  host?: unknown;
  fetch?: typeof globalThis.fetch;
  transportOptions?: Partial<ConnectBrowserTransportOptions>;
  location?: { protocol: string; host: string };
}

export interface ConnectBootstrapHandle {
  readonly marker: ConnectHostPublication;
  readonly transport: ConnectBrowserTransport;
  stop(): void;
}

type RuntimeHost = Record<string, unknown>;

function isRecord(value: unknown): value is RuntimeHost {
  return typeof value === "object" && value !== null;
}

function asBytes(value: ArrayBuffer | Uint8Array): Uint8Array {
  return value instanceof Uint8Array ? value.slice() : new Uint8Array(value).slice();
}

function copyBytes(value: Uint8Array): ArrayBuffer {
  return value.slice().buffer;
}

function hold(message: string): never {
  throw new ConnectBrowserIdentityHoldError(message);
}

function defaultCrypto(): ConnectIdentityCrypto {
  const value = globalThis.crypto;
  if (!value?.subtle || typeof value.getRandomValues !== "function") {
    return hold("WebCrypto is unavailable; Connect identity is held");
  }
  return value;
}

function makeDeviceId(crypto: ConnectIdentityCrypto): string {
  if (typeof crypto.randomUUID === "function") return crypto.randomUUID();
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return `connect-${Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("")}`;
}

function validatePublication(value: unknown): ConnectHostPublication | null {
  if (!isRecord(value) || value.transport !== "connect") return null;
  const endpoint = value.endpoint;
  const generation = value.generation;
  const protocolMajor = value.protocolMajor;
  const protocolMinor = value.protocolMinor;
  if (
    typeof endpoint !== "string" ||
    endpoint !== CONNECT_ENDPOINT ||
    typeof generation !== "number" ||
    !Number.isSafeInteger(generation) ||
    generation <= 0 ||
    typeof protocolMajor !== "number" ||
    protocolMajor !== 1 ||
    typeof protocolMinor !== "number" ||
    !Number.isSafeInteger(protocolMinor) ||
    protocolMinor < 0
  ) {
    return null;
  }
  return { transport: "connect", endpoint, generation, protocolMajor, protocolMinor };
}

/** Read only the bounded, non-secret marker emitted by the native host. */
export function readConnectHostPublication(
  host: unknown = globalThis,
): ConnectHostPublication | null {
  if (!isRecord(host)) return null;
  return (
    validatePublication(host[CONNECT_HOST_PUBLICATION_KEY]) ??
    validatePublication(host[CONNECT_STORE_CONFIGURATION_KEY])
  );
}

function openRequest<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error ?? new Error("IndexedDB request failed"));
  });
}

function transactionDone(transaction: IDBTransaction): Promise<void> {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve();
    transaction.onerror = () => reject(transaction.error ?? new Error("IndexedDB transaction failed"));
    transaction.onabort = () => reject(transaction.error ?? new Error("IndexedDB transaction aborted"));
  });
}

/** IndexedDB is deliberately the only default persistence mechanism. */
export function createIndexedDbIdentityStorage(
  indexedDb: IDBFactory = globalThis.indexedDB,
): ConnectIdentityStorage {
  if (!indexedDb) return hold("IndexedDB is unavailable; Connect identity is held");
  let databasePromise: Promise<IDBDatabase> | null = null;
  const database = (): Promise<IDBDatabase> => {
    databasePromise ??= new Promise((resolve, reject) => {
      const request = indexedDb.open(
        CONNECT_IDENTITY_DB_NAME,
        CONNECT_IDENTITY_DB_VERSION,
      );
      request.onupgradeneeded = () => {
        if (!request.result.objectStoreNames.contains(IDENTITY_STORE_NAME)) {
          request.result.createObjectStore(IDENTITY_STORE_NAME);
        }
      };
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error ?? new Error("IndexedDB open failed"));
      request.onblocked = () => reject(new Error("IndexedDB identity store blocked"));
    });
    return databasePromise;
  };
  return {
    async load() {
      const db = await database();
      const tx = db.transaction(IDENTITY_STORE_NAME, "readonly");
      const done = transactionDone(tx);
      const value = await openRequest<PersistedConnectIdentity | undefined>(
        tx.objectStore(IDENTITY_STORE_NAME).get(CONNECT_IDENTITY_RECORD_KEY),
      );
      await done;
      return value ?? null;
    },
    async save(record) {
      const db = await database();
      const tx = db.transaction(IDENTITY_STORE_NAME, "readwrite");
      const done = transactionDone(tx);
      tx.objectStore(IDENTITY_STORE_NAME).put(record, CONNECT_IDENTITY_RECORD_KEY);
      await done;
    },
    async clear() {
      const db = await database();
      const tx = db.transaction(IDENTITY_STORE_NAME, "readwrite");
      const done = transactionDone(tx);
      tx.objectStore(IDENTITY_STORE_NAME).delete(CONNECT_IDENTITY_RECORD_KEY);
      await done;
    },
  };
}

async function createIdentity(
  crypto: ConnectIdentityCrypto,
  storage: ConnectIdentityStorage,
  hostGeneration: number,
  now: () => number,
): Promise<ConnectDeviceIdentity> {
  let keyPair: CryptoKeyPair;
  try {
    keyPair = (await crypto.subtle.generateKey(
      { name: "X25519" },
      false,
      ["deriveBits"],
    )) as CryptoKeyPair;
  } catch {
    return hold("This browser cannot create the X25519 Connect identity");
  }
  let publicKey: Uint8Array;
  try {
    publicKey = asBytes(await crypto.subtle.exportKey("raw", keyPair.publicKey));
  } catch {
    return hold("Connect X25519 public identity export is unavailable");
  }
  try {
    const record: PersistedConnectIdentity = {
      version: 1,
      deviceId: makeDeviceId(crypto),
      publicKey: copyBytes(publicKey),
      privateCryptoKey: keyPair.privateKey,
      hostGeneration,
      createdAt: now(),
    };
    await storage.save(record);
    return {
      deviceId: record.deviceId,
      publicKey,
      privateCryptoKey: keyPair.privateKey,
      hostGeneration,
    };
  } catch {
    return hold("Connect browser identity could not be persisted");
  }
}

export async function loadOrCreateConnectIdentity(
  hostGeneration: number,
  options: ConnectIdentityOptions = {},
): Promise<ConnectDeviceIdentity> {
  const crypto = options.crypto ?? defaultCrypto();
  const storage = options.storage ?? createIndexedDbIdentityStorage();
  const now = options.now ?? Date.now;
  let record: PersistedConnectIdentity | null;
  try {
    record = await storage.load();
  } catch {
    return hold("Connect browser identity storage could not be opened");
  }
  if (!record) return createIdentity(crypto, storage, hostGeneration, now);
  if (
    record.version !== 1 ||
    !record.deviceId ||
    record.hostGeneration > hostGeneration ||
    record.publicKey.byteLength !== 32 ||
    !record.privateCryptoKey
  ) {
    return hold("Connect browser identity generation is stale or invalid");
  }
  if (record.hostGeneration !== hostGeneration) {
    await storage.save({ ...record, hostGeneration });
  }
  return {
    deviceId: record.deviceId,
    publicKey: asBytes(record.publicKey),
    privateCryptoKey: record.privateCryptoKey,
    hostGeneration,
  };
}

async function assertPaired(fetcher: typeof globalThis.fetch): Promise<void> {
  let response: Response;
  try {
    response = await fetcher("/api/me", {
      credentials: "include",
      cache: "no-store",
    });
  } catch {
    return hold("Connect pairing status could not be checked");
  }
  if (response.status === 401) return hold("Connect browser pairing is required");
  if (!response.ok) return hold("Connect pairing status was rejected");
}

function publishConnectHold(
  host: RuntimeHost,
  marker: ConnectHostPublication,
): void {
  host[CONNECT_STORE_CONFIGURATION_KEY] = {
    transport: "connect" as const,
    endpoint: marker.endpoint,
    generation: marker.generation,
    protocolMajor: marker.protocolMajor,
    protocolMinor: marker.protocolMinor,
  };
}

/**
 * Build the browser transport only after the same-origin pair cookie is
 * proven valid. A missing/invalid host publication or unsupported secure
 * identity is a typed HOLD; the function never installs a legacy transport.
 */
export async function bootstrapConnect(
  options: ConnectBootstrapOptions = {},
): Promise<ConnectBootstrapHandle | null> {
  const candidateHost = options.host ?? globalThis;
  if (!isRecord(candidateHost)) return null;
  const host: RuntimeHost = candidateHost;
  const marker = readConnectHostPublication(host);
  if (!marker) return null;
  await assertPaired(options.fetch ?? globalThis.fetch.bind(globalThis));
  try {
    await loadOrCreateConnectIdentity(marker.generation, options);
  } catch (error) {
    if (error instanceof ConnectBrowserIdentityHoldError) {
      // Keep Connect selected after authenticated pairing. The store will
      // report its typed HOLD instead of silently opening plaintext /api/ws.
      publishConnectHold(host, marker);
    }
    throw error;
  }
  // The current generated WASM constructor accepts raw X25519 bytes only.
  // Passing an export of a non-extractable CryptoKey would violate custody, so
  // hold until that boundary accepts the opaque key directly.
  return hold(
    "Connect WASM key custody is held: it does not yet accept a non-extractable browser key",
  );
}
