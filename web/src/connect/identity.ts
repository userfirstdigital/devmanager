import { CONNECT_STORE_CONFIGURATION_KEY } from "./storeAdapter";
import { protocolUuid } from "./hostOutput";
import { NATIVE_BROWSER_CAPABILITIES } from "./nativeProtocol";
import { assertHostTrust, type HostTrustStorage } from "./hostTrust";
import { resolveConnectCrypto, type ConnectCryptoLoader } from "./crypto";
import {
  type NativeFleetHostDescriptor,
  parseConnectFleetDescriptors,
  readConnectFleetMetaJson,
} from "./fleetDescriptor";
import {
  BoundedResponseError,
  readBoundedResponseText,
  withResponseAbort,
} from "./boundedResponse";
import {
  ConnectBrowserTransport,
  buildConnectCrossOriginEndpoint,
  type ConnectBrowserTransportOptions,
  type ConnectHandshakeMaterialFactory,
} from "./transport";

export { readBoundedResponseText, BoundedResponseError } from "./boundedResponse";

/** The native host publishes this marker separately from the store handoff. */
export const CONNECT_HOST_PUBLICATION_KEY =
  "__DEVMANAGER_CONNECT_HOST__" as const;
export const CONNECT_IDENTITY_DB_NAME = "devmanager.connect.identity" as const;
export const CONNECT_IDENTITY_DB_VERSION = 1 as const;
export const CONNECT_IDENTITY_RECORD_KEY = "device" as const;
export const CONNECT_IDENTITY_RECORD_VERSION = 2 as const;
export const CONNECT_IDENTITY_RECORD_SCHEMA =
  "connect-device-wrapped-v1" as const;
export const CONNECT_IDENTITY_LOCK_NAME =
  "devmanager.connect.identity.custody" as const;

export const CONNECT_IDENTITY_HOLD = "browser-connect-identity-held" as const;

const IDENTITY_STORE_NAME = "identity";
const CONNECT_ENDPOINT = "/api/connect";
const X25519_PUBLIC_BYTES = 32;
const X25519_PRIVATE_BYTES = 32;
const AES_GCM_IV_BYTES = 12;
const AES_GCM_KEY_BITS = 256;
/** AES-GCM ciphertext length for a 32-byte plaintext with the 128-bit tag. */
const WRAPPED_PRIVATE_CIPHERTEXT_BYTES = X25519_PRIVATE_BYTES + 16;
/** Bound UTF-8 deviceId before AAD allocation. */
const MAX_DEVICE_ID_BYTES = 128;

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
  hostPublicId?: string;
  hostPublicKey?: string;
}

/** Pairing is recoverable in the Connect shell, but never a legacy fallback. */
export class ConnectPairingRequiredError extends ConnectBrowserIdentityHoldError {
  constructor() {
    super("Connect browser pairing is required");
    this.name = "ConnectPairingRequiredError";
  }
}

/** Public device identity only; private bytes never leave the custody boundary. */
export interface ConnectDeviceIdentity {
  deviceId: string;
  publicKey: Uint8Array;
  hostGeneration: number;
}

/**
 * Durable wrapped custody record. IndexedDB may retain the non-extractable
 * AES wrapping key beside authenticated ciphertext; raw X25519 private bytes
 * are never persisted.
 */
export interface PersistedConnectIdentity {
  version: typeof CONNECT_IDENTITY_RECORD_VERSION;
  schema: typeof CONNECT_IDENTITY_RECORD_SCHEMA;
  deviceId: string;
  publicKey: ArrayBuffer;
  ciphertext: ArrayBuffer;
  iv: ArrayBuffer;
  wrappingKey: CryptoKey;
  hostGeneration: number;
  createdAt: number;
}

export interface ConnectIdentityStorage {
  load(): Promise<PersistedConnectIdentity | null>;
  /**
   * Insert `record` only when absent. Returns the durable winner so concurrent
   * first-create races cannot silently publish inconsistent device identities.
   */
  putIfAbsent(
    record: PersistedConnectIdentity,
  ): Promise<PersistedConnectIdentity>;
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
  /**
   * Optional Web Locks frontier. When omitted, `navigator.locks` is used when
   * present; when locks are unavailable, storage `putIfAbsent` remains the
   * sole race boundary and callers must not assume cross-tab mutual exclusion.
   */
  locks?: LockManager | null;
}

export interface ConnectBootstrapOptions extends ConnectIdentityOptions {
  host?: unknown;
  fetch?: typeof globalThis.fetch;
  transportOptions?: Partial<ConnectBrowserTransportOptions>;
  location?: { protocol: string; host: string };
  cryptoLoader?: ConnectCryptoLoader;
  /** Test seam only; production uses the durable IndexedDB host-trust store. */
  hostTrustStorage?: HostTrustStorage;
}

export interface ConnectCrossOriginPairGrant {
  /** Owner-generated one-time grant string; never persisted by the browser. */
  grant: string;
  label?: string;
}

export interface ConnectCrossOriginBootstrapOptions extends ConnectBootstrapOptions {
  descriptor: NativeFleetHostDescriptor;
  /** Optional one-time pairing grant from the host-status Pair UI. */
  grant?: ConnectCrossOriginPairGrant | null;
  /** Test seam: absolute deadline for pair POST / handshake admission. */
  pairDeadlineMs?: number;
}

export interface ConnectBootstrapHandle {
  readonly marker: ConnectHostPublication;
  readonly identity: ConnectDeviceIdentity;
  readonly transport: ConnectBrowserTransport;
  /** Reversible pagehide/bfcache suspension; does not permanently stop. */
  suspend(): void;
  stop(): void;
}

/** In-memory custody material used to build one handshake without re-reading IDB. */
export interface ConnectDeviceCustody extends ConnectDeviceIdentity {
  unwrapPrivateKey(): Promise<Uint8Array>;
}

export type RuntimeHost = Record<string, unknown>;

function isRecord(value: unknown): value is RuntimeHost {
  return typeof value === "object" && value !== null;
}

function asBytes(value: ArrayBuffer | Uint8Array): Uint8Array {
  return value instanceof Uint8Array
    ? value.slice()
    : new Uint8Array(value).slice();
}

function copyBytes(value: Uint8Array): ArrayBuffer {
  return value.slice().buffer;
}

function wipeBytes(bytes: Uint8Array | null | undefined): void {
  if (!bytes) return;
  bytes.fill(0);
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

/** Noise's logical device claim is the persisted random 16-byte custody ID.
 * It is NOT the host-assigned canonical DeviceId (UUIDv7), nor a public key.
 * Both formats are emitted by makeDeviceId; never rotate existing custody/AAD.
 */
export function connectDevicePublicId(deviceId: string): Uint8Array {
  const hex = /^connect-[0-9a-f]{32}$/i.test(deviceId)
    ? deviceId.slice(8)
    : /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(deviceId)
      ? deviceId.replace(/-/g, "")
      : null;
  if (!hex || /^0{32}$/.test(hex)) {
    return hold("Stored Connect device identifier needs explicit repair");
  }
  return Uint8Array.from(hex.match(/../g)!, (byte) => parseInt(byte, 16));
}

function validatePublication(value: unknown): ConnectHostPublication | null {
  if (!isRecord(value) || value.transport !== "connect") return null;
  const endpoint = value.endpoint;
  const generation = value.generation;
  const protocolMajor = value.protocolMajor;
  const protocolMinor = value.protocolMinor;
  const hostPublicId = value.hostPublicId === undefined ? undefined : protocolUuid(value.hostPublicId);
  const hostPublicKey = value.hostPublicKey;
  if ((hostPublicId === undefined) !== (hostPublicKey === undefined) || hostPublicId === null ||
      (hostPublicKey !== undefined && (typeof hostPublicKey !== "string" ||
        !/^[0-9a-f]{64}$/.test(hostPublicKey) || /^0{64}$/.test(hostPublicKey)))) return null;
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
  return {
    transport: "connect",
    endpoint,
    generation,
    protocolMajor,
    protocolMinor,
    ...(hostPublicId ? { hostPublicId, hostPublicKey: hostPublicKey as string } : {}),
  };
}

/** Install inert host metadata synchronously, before any store or network work. */
export function installConnectDocumentPublication(
  documentSource: Pick<Document, "querySelector"> = document,
  host: RuntimeHost = globalThis as unknown as RuntimeHost,
): boolean {
  const element = documentSource.querySelector('meta[name="devmanager-connect"]');
  if (!element) return false;
  // Presence itself is authoritative transport selection; malformed data must
  // not expose a legacy endpoint while the native host is unavailable.
  host[CONNECT_HOST_PUBLICATION_KEY] = { transport: "connect" };
  const raw = element.getAttribute("content");
  if (raw && raw.length <= 4096) {
    try {
      const marker = validatePublication(JSON.parse(raw));
      if (marker) host[CONNECT_HOST_PUBLICATION_KEY] = marker;
    } catch { /* fail closed */ }
  }
  publishConnectSelected(host);
  return true;
}

/** True when a runtime marker explicitly selects Connect, even if malformed. */
export function hasExplicitConnectSelection(host: RuntimeHost): boolean {
  const hostMarker = host[CONNECT_HOST_PUBLICATION_KEY];
  const storeMarker = host[CONNECT_STORE_CONFIGURATION_KEY];
  return (
    (isRecord(hostMarker) && hostMarker.transport === "connect") ||
    (isRecord(storeMarker) && storeMarker.transport === "connect")
  );
}

/** Native cache-first entry requires a complete public host binding. */
export function hasCompleteConnectHostBinding(
  marker: ConnectHostPublication | null,
): marker is ConnectHostPublication & {
  hostPublicId: string;
  hostPublicKey: string;
} {
  return typeof marker?.hostPublicId === "string" && typeof marker.hostPublicKey === "string";
}

function connectOrigin(location: { protocol: string; host: string }): string {
  try {
    const origin = new URL(`${location.protocol}//${location.host}`).origin;
    if (origin === "null") return hold("Connect host origin is invalid");
    return origin;
  } catch {
    return hold("Connect host origin is invalid");
  }
}

function publishConnectSelected(host: RuntimeHost): void {
  host[CONNECT_STORE_CONFIGURATION_KEY] = { transport: "connect" as const };
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
    request.onerror = () =>
      reject(request.error ?? new Error("IndexedDB request failed"));
  });
}

function transactionDone(transaction: IDBTransaction): Promise<void> {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve();
    transaction.onerror = () =>
      reject(transaction.error ?? new Error("IndexedDB transaction failed"));
    transaction.onabort = () =>
      reject(transaction.error ?? new Error("IndexedDB transaction aborted"));
  });
}

/** IndexedDB is deliberately the only default persistence mechanism. */
export function createIndexedDbIdentityStorage(
  indexedDb: IDBFactory = globalThis.indexedDB,
): ConnectIdentityStorage {
  if (!indexedDb) {
    return hold("IndexedDB is unavailable; Connect identity is held");
  }
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
      request.onerror = () =>
        reject(request.error ?? new Error("IndexedDB open failed"));
      request.onblocked = () =>
        reject(new Error("IndexedDB identity store blocked"));
    });
    return databasePromise;
  };
  return {
    async load() {
      const db = await database();
      const tx = db.transaction(IDENTITY_STORE_NAME, "readonly");
      const done = transactionDone(tx);
      const value = await openRequest<unknown>(
        tx.objectStore(IDENTITY_STORE_NAME).get(CONNECT_IDENTITY_RECORD_KEY),
      );
      await done;
      return structuralNormalizeRecord(value);
    },
    async putIfAbsent(record) {
      const db = await database();
      const tx = db.transaction(IDENTITY_STORE_NAME, "readwrite");
      const store = tx.objectStore(IDENTITY_STORE_NAME);
      const done = transactionDone(tx);
      const existing = await openRequest<unknown>(
        store.get(CONNECT_IDENTITY_RECORD_KEY),
      );
      if (existing !== undefined) {
        await done;
        const normalized = structuralNormalizeRecord(existing);
        if (!normalized) {
          return hold(
            "Connect browser identity is corrupt and requires explicit repair",
          );
        }
        return normalized;
      }
      store.put(record, CONNECT_IDENTITY_RECORD_KEY);
      await done;
      return record;
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

/**
 * Strict PKCS#8 parse for WebCrypto X25519 private keys.
 * Rejects trailing data, unexpected attributes, nonminimal DER lengths, and
 * any OID other than 1.3.101.110. Validates framing before allocating private
 * bytes; never assumes a fixed slice offset into an opaque buffer.
 */
export function parseX25519Pkcs8PrivateKey(pkcs8: Uint8Array): Uint8Array {
  let offset = 0;
  const readByte = (): number => {
    if (offset >= pkcs8.byteLength) {
      return hold("Connect X25519 PKCS8 private key is truncated");
    }
    const value = pkcs8[offset]!;
    offset += 1;
    return value;
  };
  /** X25519 PKCS8 uses only short-form lengths; reject all long-form encodings. */
  const readLength = (): number => {
    const first = readByte();
    if (first < 0x80) return first;
    return hold("Connect X25519 PKCS8 length encoding rejected");
  };
  const readTagWithin = (expected: number, end: number): number => {
    if (offset >= end) {
      return hold("Connect X25519 PKCS8 private key is truncated");
    }
    const tag = readByte();
    if (tag !== expected) {
      return hold("Connect X25519 PKCS8 structure rejected");
    }
    const length = readLength();
    if (offset + length > end) {
      return hold("Connect X25519 PKCS8 private key is truncated");
    }
    return length;
  };
  if (offset >= pkcs8.byteLength) {
    return hold("Connect X25519 PKCS8 private key is truncated");
  }
  const outerTag = readByte();
  if (outerTag !== 0x30) {
    return hold("Connect X25519 PKCS8 structure rejected");
  }
  const outerLength = readLength();
  const outerEnd = offset + outerLength;
  if (outerEnd !== pkcs8.byteLength) {
    return hold("Connect X25519 PKCS8 trailing data rejected");
  }
  const versionLength = readTagWithin(0x02, outerEnd);
  if (versionLength !== 1 || readByte() !== 0x00) {
    return hold("Connect X25519 PKCS8 version rejected");
  }
  const algorithmLength = readTagWithin(0x30, outerEnd);
  const algorithmEnd = offset + algorithmLength;
  const oidLength = readTagWithin(0x06, algorithmEnd);
  if (
    oidLength !== 3 ||
    readByte() !== 0x2b ||
    readByte() !== 0x65 ||
    readByte() !== 0x6e
  ) {
    return hold("Connect X25519 PKCS8 OID rejected");
  }
  if (offset !== algorithmEnd) {
    return hold("Connect X25519 PKCS8 algorithm parameters rejected");
  }
  const privateContainerLength = readTagWithin(0x04, outerEnd);
  const privateContainerEnd = offset + privateContainerLength;
  if (privateContainerEnd !== outerEnd) {
    return hold("Connect X25519 PKCS8 private key framing rejected");
  }
  const curveKeyLength = readTagWithin(0x04, privateContainerEnd);
  if (curveKeyLength !== X25519_PRIVATE_BYTES) {
    return hold("Connect X25519 PKCS8 private key length rejected");
  }
  const privateStart = offset;
  offset += X25519_PRIVATE_BYTES;
  if (offset !== privateContainerEnd || offset !== outerEnd) {
    return hold("Connect X25519 PKCS8 trailing data rejected");
  }
  let allZero = true;
  for (let index = 0; index < X25519_PRIVATE_BYTES; index += 1) {
    if (pkcs8[privateStart + index] !== 0) {
      allZero = false;
      break;
    }
  }
  if (allZero) {
    return hold("Connect X25519 PKCS8 private key rejected");
  }
  const privateKey = pkcs8.slice(
    privateStart,
    privateStart + X25519_PRIVATE_BYTES,
  );
  return privateKey;
}

function encodeBoundedDeviceId(deviceId: string): Uint8Array {
  if (typeof deviceId !== "string" || !deviceId) {
    return hold("Connect identity device id rejected");
  }
  const encoded = new TextEncoder().encode(deviceId);
  if (encoded.byteLength === 0 || encoded.byteLength > MAX_DEVICE_ID_BYTES) {
    return hold("Connect identity device id rejected");
  }
  return encoded;
}

function buildIdentityAad(input: {
  version: number;
  schema: string;
  deviceId: string;
  publicKey: Uint8Array;
}): Uint8Array {
  const encoder = new TextEncoder();
  const schema = encoder.encode(input.schema);
  const deviceId = encodeBoundedDeviceId(input.deviceId);
  if (input.publicKey.byteLength !== X25519_PUBLIC_BYTES) {
    return hold("Connect identity public key length rejected");
  }
  if (schema.byteLength === 0 || schema.byteLength > 64) {
    return hold("Connect identity schema rejected");
  }
  const out = new Uint8Array(
    4 + 4 + schema.byteLength + 4 + deviceId.byteLength + X25519_PUBLIC_BYTES,
  );
  const view = new DataView(out.buffer);
  let offset = 0;
  view.setUint32(offset, input.version, false);
  offset += 4;
  view.setUint32(offset, schema.byteLength, false);
  offset += 4;
  out.set(schema, offset);
  offset += schema.byteLength;
  view.setUint32(offset, deviceId.byteLength, false);
  offset += 4;
  out.set(deviceId, offset);
  offset += deviceId.byteLength;
  out.set(input.publicKey, offset);
  return out;
}

/** Realm-independent wrapping-key shape check; decrypt authenticates the brand. */
function isWrappingKeyShape(key: unknown): boolean {
  if (typeof key !== "object" || key === null) return false;
  const candidate = key as {
    type?: unknown;
    extractable?: unknown;
    algorithm?: unknown;
    usages?: unknown;
  };
  if (candidate.type !== "secret" || candidate.extractable !== false) {
    return false;
  }
  const algorithm = candidate.algorithm;
  if (
    typeof algorithm !== "object" ||
    algorithm === null ||
    !("name" in algorithm) ||
    (algorithm as AesKeyAlgorithm).name !== "AES-GCM" ||
    (algorithm as AesKeyAlgorithm).length !== AES_GCM_KEY_BITS
  ) {
    return false;
  }
  if (!Array.isArray(candidate.usages)) return false;
  const usages = new Set(candidate.usages);
  return usages.has("decrypt") && usages.has("encrypt");
}

function isLegacyV1IdentityRecord(value: unknown): boolean {
  return isRecord(value) && value.version === 1 && "privateCryptoKey" in value;
}

function asArrayBuffer(value: unknown, expected: number): ArrayBuffer | null {
  if (value instanceof ArrayBuffer && value.byteLength === expected) {
    return value;
  }
  if (
    ArrayBuffer.isView(value) &&
    value.byteLength === expected &&
    value.buffer instanceof ArrayBuffer
  ) {
    return value.buffer.slice(
      value.byteOffset,
      value.byteOffset + value.byteLength,
    );
  }
  return null;
}

function structuralNormalizeRecord(
  value: unknown,
): PersistedConnectIdentity | null {
  if (value === undefined || value === null) return null;
  if (isLegacyV1IdentityRecord(value)) {
    return hold(
      "Connect browser identity requires explicit repair: opaque v1 custody cannot be used by the WASM byte ABI",
    );
  }
  if (!isRecord(value)) {
    return hold("Connect browser identity generation is stale or invalid");
  }
  const publicKey = asArrayBuffer(value.publicKey, X25519_PUBLIC_BYTES);
  const ciphertext = asArrayBuffer(
    value.ciphertext,
    WRAPPED_PRIVATE_CIPHERTEXT_BYTES,
  );
  const iv = asArrayBuffer(value.iv, AES_GCM_IV_BYTES);
  if (
    value.version !== CONNECT_IDENTITY_RECORD_VERSION ||
    value.schema !== CONNECT_IDENTITY_RECORD_SCHEMA ||
    typeof value.deviceId !== "string" ||
    !value.deviceId ||
    !publicKey ||
    !ciphertext ||
    !iv ||
    typeof value.hostGeneration !== "number" ||
    !Number.isSafeInteger(value.hostGeneration) ||
    value.hostGeneration <= 0 ||
    typeof value.createdAt !== "number" ||
    !Number.isFinite(value.createdAt) ||
    !isWrappingKeyShape(value.wrappingKey)
  ) {
    return hold("Connect browser identity generation is stale or invalid");
  }
  // Bound deviceId before any AAD allocation downstream.
  encodeBoundedDeviceId(value.deviceId);
  return {
    version: CONNECT_IDENTITY_RECORD_VERSION,
    schema: CONNECT_IDENTITY_RECORD_SCHEMA,
    deviceId: value.deviceId,
    publicKey,
    ciphertext,
    iv,
    wrappingKey: value.wrappingKey as CryptoKey,
    hostGeneration: value.hostGeneration,
    createdAt: value.createdAt,
  };
}

async function authenticatePersistedRecord(
  record: PersistedConnectIdentity,
  crypto: ConnectIdentityCrypto,
): Promise<PersistedConnectIdentity> {
  const probe = await unwrapPrivateKeyBytes(record, crypto);
  wipeBytes(probe);
  return record;
}

async function unwrapPrivateKeyBytes(
  record: PersistedConnectIdentity,
  crypto: ConnectIdentityCrypto,
): Promise<Uint8Array> {
  if (!isWrappingKeyShape(record.wrappingKey)) {
    return hold("Connect identity wrapping key is invalid");
  }
  const publicKey = asBytes(record.publicKey);
  const aad = buildIdentityAad({
    version: record.version,
    schema: record.schema,
    deviceId: record.deviceId,
    publicKey,
  });
  let plaintext: ArrayBuffer;
  try {
    plaintext = await crypto.subtle.decrypt(
      {
        name: "AES-GCM",
        iv: new Uint8Array(record.iv),
        additionalData: aad,
        tagLength: 128,
      },
      record.wrappingKey,
      record.ciphertext,
    );
  } catch {
    return hold("Connect identity ciphertext failed authentication");
  }
  const privateKey = new Uint8Array(plaintext);
  if (privateKey.byteLength !== X25519_PRIVATE_BYTES) {
    wipeBytes(privateKey);
    return hold("Connect identity private key length rejected");
  }
  return privateKey;
}

function toDeviceIdentity(
  record: PersistedConnectIdentity,
  viewGeneration: number,
): ConnectDeviceIdentity {
  return {
    deviceId: record.deviceId,
    publicKey: asBytes(record.publicKey),
    hostGeneration: viewGeneration,
  };
}

function toCustody(
  record: PersistedConnectIdentity,
  viewGeneration: number,
  crypto: ConnectIdentityCrypto,
): ConnectDeviceCustody {
  const identity = toDeviceIdentity(record, viewGeneration);
  return {
    ...identity,
    unwrapPrivateKey: () => unwrapPrivateKeyBytes(record, crypto),
  };
}

async function withIdentityLock<T>(
  locks: LockManager | null | undefined,
  run: () => Promise<T>,
): Promise<T> {
  const manager =
    locks === null
      ? null
      : (locks ??
        (typeof globalThis.navigator !== "undefined"
          ? globalThis.navigator.locks
          : undefined));
  if (!manager || typeof manager.request !== "function") {
    // Explicit unsupported frontier: storage putIfAbsent remains authoritative
    // inside one origin tab; cross-tab create races are not mutually excluded.
    return run();
  }
  return manager.request(CONNECT_IDENTITY_LOCK_NAME, run);
}

async function createWrappedIdentity(
  crypto: ConnectIdentityCrypto,
  storage: ConnectIdentityStorage,
  hostGeneration: number,
  now: () => number,
): Promise<ConnectDeviceCustody> {
  let keyPair: CryptoKeyPair;
  try {
    keyPair = (await crypto.subtle.generateKey({ name: "X25519" }, true, [
      "deriveBits",
    ])) as CryptoKeyPair;
  } catch {
    return hold("This browser cannot create the X25519 Connect identity");
  }

  let pkcs8: Uint8Array | null = null;
  let privateKey: Uint8Array | null = null;
  let publicKey: Uint8Array | null = null;
  try {
    try {
      pkcs8 = new Uint8Array(
        await crypto.subtle.exportKey("pkcs8", keyPair.privateKey),
      );
      privateKey = parseX25519Pkcs8PrivateKey(pkcs8);
      publicKey = asBytes(
        await crypto.subtle.exportKey("raw", keyPair.publicKey),
      );
    } catch (error) {
      if (error instanceof ConnectBrowserIdentityHoldError) throw error;
      return hold("Connect X25519 public identity export is unavailable");
    }
    if (publicKey.byteLength !== X25519_PUBLIC_BYTES) {
      return hold("Connect X25519 public identity export is unavailable");
    }

    let wrappingKey: CryptoKey;
    try {
      wrappingKey = await crypto.subtle.generateKey(
        { name: "AES-GCM", length: AES_GCM_KEY_BITS },
        false,
        ["encrypt", "decrypt"],
      );
    } catch {
      return hold("Connect identity wrapping key could not be created");
    }
    if (!isWrappingKeyShape(wrappingKey)) {
      return hold("Connect identity wrapping key is invalid");
    }

    const deviceId = makeDeviceId(crypto);
    encodeBoundedDeviceId(deviceId);
    const iv = new Uint8Array(AES_GCM_IV_BYTES);
    crypto.getRandomValues(iv);
    const aad = buildIdentityAad({
      version: CONNECT_IDENTITY_RECORD_VERSION,
      schema: CONNECT_IDENTITY_RECORD_SCHEMA,
      deviceId,
      publicKey,
    });
    let ciphertext: ArrayBuffer;
    try {
      ciphertext = await crypto.subtle.encrypt(
        {
          name: "AES-GCM",
          iv,
          additionalData: aad,
          tagLength: 128,
        },
        wrappingKey,
        privateKey,
      );
    } catch {
      return hold("Connect identity private key could not be wrapped");
    }
    if (
      !(ciphertext instanceof ArrayBuffer) ||
      ciphertext.byteLength !== WRAPPED_PRIVATE_CIPHERTEXT_BYTES
    ) {
      return hold("Connect identity private key could not be wrapped");
    }

    wipeBytes(privateKey);
    privateKey = null;
    wipeBytes(pkcs8);
    pkcs8 = null;

    const record: PersistedConnectIdentity = {
      version: CONNECT_IDENTITY_RECORD_VERSION,
      schema: CONNECT_IDENTITY_RECORD_SCHEMA,
      deviceId,
      publicKey: copyBytes(publicKey),
      ciphertext,
      iv: copyBytes(iv),
      wrappingKey,
      hostGeneration,
      createdAt: now(),
    };

    let committed: PersistedConnectIdentity;
    try {
      committed = await storage.putIfAbsent(record);
    } catch (error) {
      if (error instanceof ConnectBrowserIdentityHoldError) throw error;
      return hold("Connect browser identity could not be persisted");
    }
    // Authenticate the durable winner — a raced existing row may be corrupt.
    const authenticated = await authenticatePersistedRecord(committed, crypto);
    return toCustody(authenticated, hostGeneration, crypto);
  } finally {
    wipeBytes(privateKey);
    wipeBytes(pkcs8);
  }
}

export async function loadOrCreateConnectIdentity(
  hostGeneration: number,
  options: ConnectIdentityOptions = {},
): Promise<ConnectDeviceCustody> {
  const crypto = options.crypto ?? defaultCrypto();
  const storage = options.storage ?? createIndexedDbIdentityStorage();
  const now = options.now ?? Date.now;
  return withIdentityLock(options.locks, async () => {
    let record: PersistedConnectIdentity | null;
    try {
      record = structuralNormalizeRecord(await storage.load());
    } catch (error) {
      if (error instanceof ConnectBrowserIdentityHoldError) throw error;
      return hold("Connect browser identity storage could not be opened");
    }
    if (!record) {
      return createWrappedIdentity(crypto, storage, hostGeneration, now);
    }
    // Host process generation is ephemeral view metadata only. A valid wrapped
    // device identity is not rewritten or rejected when generations differ.
    const authenticated = await authenticatePersistedRecord(record, crypto);
    return toCustody(authenticated, hostGeneration, crypto);
  });
}

/** Build a per-handshake unwrap factory; temporary private bytes are caller-wiped. */
export function createConnectHandshakeMaterialFactory(
  custody: ConnectDeviceCustody,
): ConnectHandshakeMaterialFactory {
  return async () => {
    const privateKey = await custody.unwrapPrivateKey();
    return {
      privateKey,
      localPublic: custody.publicKey.slice(),
    };
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
  if (response.status === 401)
    throw new ConnectPairingRequiredError();
  if (!response.ok) return hold("Connect pairing status was rejected");
}

function publishConnectSelection(
  host: RuntimeHost,
  marker: ConnectHostPublication,
  transport?: ConnectBrowserTransport,
): void {
  const published: Record<string, unknown> = {
    transport: "connect" as const,
    endpoint: marker.endpoint,
    generation: marker.generation,
    protocolMajor: marker.protocolMajor,
    protocolMinor: marker.protocolMinor,
    ...(marker.hostPublicId ? { hostPublicId: marker.hostPublicId, hostPublicKey: marker.hostPublicKey } : {}),
  };
  if (transport) {
    published.connectTransport = transport;
  }
  host[CONNECT_STORE_CONFIGURATION_KEY] = published;
}

/**
 * Build the browser transport only after the same-origin pair cookie is
 * proven valid, custody authenticates, and the WASM protocol identity matches.
 * Connect selection is published before any await so authentication/storage/
 * crypto failures remain fail-closed Connect HOLDs rather than legacy `/api/ws`.
 */
export async function bootstrapConnect(
  options: ConnectBootstrapOptions = {},
): Promise<ConnectBootstrapHandle | null> {
  const candidateHost = options.host ?? globalThis;
  if (!isRecord(candidateHost)) return null;
  const host: RuntimeHost = candidateHost;

  // Explicit Connect selection must win the store race before any await so
  // failures stay fail-closed Connect, never legacy. Capture a validated
  // publication first: CONNECT_STORE_CONFIGURATION_KEY is a supported source,
  // and publishConnectSelected would otherwise destroy it.
  if (!hasExplicitConnectSelection(host)) return null;
  const marker = readConnectHostPublication(host);
  publishConnectSelected(host);
  if (!marker) {
    return hold("Connect host publication is invalid");
  }
  publishConnectSelection(host, marker);

  try {
    // Public host metadata is first-trust material. This deliberately happens
    // before pairing fetches or crypto/socket initialization. Older unit
    // fixtures without marker metadata still exercise bootstrap compatibility;
    // the production cache-first entry rejects them before mounting a UI.
    if (hasCompleteConnectHostBinding(marker)) {
      const location = options.location ?? {
        protocol: globalThis.location.protocol,
        host: globalThis.location.host,
      };
      await assertHostTrust(
        {
          origin: connectOrigin(location),
          hostPublicId: marker.hostPublicId,
          hostPublicKey: marker.hostPublicKey,
        },
        { storage: options.hostTrustStorage },
      );
    }
    await assertPaired(options.fetch ?? globalThis.fetch.bind(globalThis));
    const custody = await loadOrCreateConnectIdentity(
      marker.generation,
      options,
    );
    const transportOptions = options.transportOptions ?? {};
    const cryptoLoader =
      options.cryptoLoader ?? transportOptions.cryptoLoader;
    await resolveConnectCrypto(cryptoLoader);
    const transport = new ConnectBrowserTransport({
      firstPairing: transportOptions.firstPairing ?? true,
      localPublic: custody.publicKey,
      handshakeMaterialFactory: createConnectHandshakeMaterialFactory(custody),
      expectedRemote: marker.hostPublicKey
        ? Uint8Array.from(marker.hostPublicKey.match(/../g)!, (byte) => parseInt(byte, 16))
        : transportOptions.expectedRemote,
      hostPublicId: marker.hostPublicId
        ? Uint8Array.from(marker.hostPublicId.replace(/-/g, "").match(/../g)!, (byte) => parseInt(byte, 16))
        : transportOptions.hostPublicId,
      // Authenticate as a device. Canonical enrollment maps this peer's key to
      // a host-owned DeviceId; the opaque custody ID is never cast to that type.
      devicePublicId: transportOptions.devicePublicId ?? connectDevicePublicId(custody.deviceId),
      purpose: transportOptions.purpose,
      role: transportOptions.role,
      openedAtUnix: transportOptions.openedAtUnix,
      directReachable: transportOptions.directReachable,
      clientId: transportOptions.clientId,
      capabilities: transportOptions.capabilities ?? NATIVE_BROWSER_CAPABILITIES,
      capabilityGrant: transportOptions.capabilityGrant,
      limits: transportOptions.limits,
      location: options.location ?? transportOptions.location,
      cryptoLoader,
      socketFactory: transportOptions.socketFactory,
      onState: transportOptions.onState,
      onEnvelope: transportOptions.onEnvelope,
      onCapabilityGrant: transportOptions.onCapabilityGrant,
      onClientId: transportOptions.onClientId,
    });
    // Projection request/resume adapters remain an explicit later-wave boundary.
    publishConnectSelection(host, marker, transport);
    return {
      marker,
      identity: {
        deviceId: custody.deviceId,
        publicKey: custody.publicKey.slice(),
        hostGeneration: custody.hostGeneration,
      },
      transport,
      suspend: () => transport.suspend(),
      stop: () => transport.stop(),
    };
  } catch (error) {
    if (error instanceof ConnectBrowserIdentityHoldError) throw error;
    throw error;
  }
}

const CROSS_ORIGIN_PAIR_PATH = "/api/connect/cross-origin-pair";
const CROSS_ORIGIN_TICKET_MAX_TTL_MS = 60_000;
const CROSS_ORIGIN_CLOCK_SKEW_MS = 5_000;
const DEFAULT_CROSS_ORIGIN_PAIR_DEADLINE_MS = 15_000;

function publicKeyHex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

function hostIdBytes(hostPublicId: string): Uint8Array {
  return Uint8Array.from(
    hostPublicId.replace(/-/g, "").match(/../g)!,
    (byte) => parseInt(byte, 16),
  );
}

function hostKeyBytes(hostPublicKey: string): Uint8Array {
  return Uint8Array.from(hostPublicKey.match(/../g)!, (byte) =>
    parseInt(byte, 16),
  );
}

function isUuidV7Strict(value: unknown): value is string {
  return (
    typeof value === "string" &&
    /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(
      value,
    )
  );
}

/**
 * Resolve configured fleet hosts from the page Connect marker plus optional
 * inert fleet meta. Does not dial; arbitrary URL routes never use this result
 * to invent a connection target.
 */
export function resolveConfiguredFleetHosts(input?: {
  marker?: ConnectHostPublication | null;
  fleetJson?: unknown;
  location?: { protocol: string; host: string };
  documentSource?: Pick<Document, "querySelector">;
}): ReturnType<typeof parseConnectFleetDescriptors> {
  const location = input?.location ?? {
    protocol: globalThis.location.protocol,
    host: globalThis.location.host,
  };
  const marker = input?.marker ?? readConnectHostPublication();
  if (!hasCompleteConnectHostBinding(marker)) {
    return {
      hosts: [],
      heldAdditions: true,
      holdReason: "missing_page_host",
    };
  }
  const fleetJson =
    input?.fleetJson !== undefined
      ? input.fleetJson
      : readConnectFleetMetaJson(input?.documentSource ?? document);
  return parseConnectFleetDescriptors({
    pageHost: {
      hostPublicId: marker.hostPublicId,
      hostPublicKey: marker.hostPublicKey,
      origin: connectOrigin(location),
      generation: marker.generation,
      protocolMajor: marker.protocolMajor,
      protocolMinor: marker.protocolMinor,
      label: "This device",
    },
    fleetJson,
  });
}

/**
 * Bootstrap a pinned cross-origin host B without retargeting page-host A
 * markers or publishing global store transport ownership for B.
 *
 * Without a grant: known-pin DMCX1 resume (no pairing-status cookie fetch).
 * With a one-time grant: credentialless POST to B's cross-origin-pair, then
 * ticket admission. Grant/ticket are never persisted or placed in the URL.
 */
export async function bootstrapCrossOriginConnect(
  options: ConnectCrossOriginBootstrapOptions,
): Promise<ConnectBootstrapHandle> {
  const descriptor = options.descriptor;
  if (descriptor.isPageHost) {
    return hold("Cross-origin Connect bootstrap rejected page host descriptor");
  }
  const endpoint = buildConnectCrossOriginEndpoint(descriptor.origin);
  if (!endpoint) {
    return hold("Connect cross-origin endpoint rejected");
  }

  // Pin trust to B.origin + B.hostPublicId before any fetch or socket.
  await assertHostTrust(
    {
      origin: descriptor.origin,
      hostPublicId: descriptor.hostPublicId,
      hostPublicKey: descriptor.hostPublicKey,
    },
    { storage: options.hostTrustStorage },
  );

  const custody = await loadOrCreateConnectIdentity(descriptor.generation, options);
  const fetcher = options.fetch ?? globalThis.fetch.bind(globalThis);
  const now = options.now ?? Date.now;
  let attachTicket: string | null = null;
  // Production browser Connect always uses Noise XX (for_browser_fleet).
  const firstPairing = true;

  const grant = options.grant;
  if (grant && typeof grant.grant === "string" && grant.grant.trim()) {
    const grantText = grant.grant.trim();
    // Clear caller-visible grant promptly after capture into this frame.
    (grant as { grant: string }).grant = "";
    const pairUrl = `${descriptor.origin}${CROSS_ORIGIN_PAIR_PATH}`;
    const controller = new AbortController();
    const deadline = options.pairDeadlineMs ?? DEFAULT_CROSS_ORIGIN_PAIR_DEADLINE_MS;
    const timer = globalThis.setTimeout(() => controller.abort(), deadline);
    let response: Response;
    try {
      response = await withResponseAbort(fetcher(pairUrl, {
        method: "POST",
        credentials: "omit",
        cache: "no-store",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          grant: grantText,
          browserInstallId: custody.deviceId,
          ...(grant.label ? { label: grant.label } : {}),
          publicKey: publicKeyHex(custody.publicKey),
        }),
        signal: controller.signal,
      }), controller.signal);
      if (!response.ok) {
        return hold("Connect cross-origin pair was rejected");
      }
      const text = await readBoundedResponseText(
        response,
        4_096,
        controller.signal,
        "Connect cross-origin pair body rejected",
      );
      let body: unknown;
      try {
        body = JSON.parse(text);
      } catch {
        return hold("Connect cross-origin pair body rejected");
      }
      if (!isRecord(body)) return hold("Connect cross-origin pair body rejected");
      const returnedHost = protocolUuid(body.hostPublicId);
      if (returnedHost !== descriptor.hostPublicId) {
        return hold("Connect cross-origin pair returned a foreign host");
      }
      if (!isUuidV7Strict(body.clientId)) {
        return hold("Connect cross-origin pair client identity rejected");
      }
      if (
        typeof body.attachTicket !== "string" ||
        inboundTicketRejected(body.attachTicket)
      ) {
        return hold("Connect cross-origin pair ticket rejected");
      }
      if (
        typeof body.expiresAtEpochMs !== "number" ||
        !Number.isSafeInteger(body.expiresAtEpochMs)
      ) {
        return hold("Connect cross-origin pair expiry rejected");
      }
      const skewBound =
        now() + CROSS_ORIGIN_TICKET_MAX_TTL_MS + CROSS_ORIGIN_CLOCK_SKEW_MS;
      if (
        body.expiresAtEpochMs > skewBound ||
        body.expiresAtEpochMs < now() - CROSS_ORIGIN_CLOCK_SKEW_MS
      ) {
        return hold("Connect cross-origin pair expiry rejected");
      }
      attachTicket = body.attachTicket;
    } catch (error) {
      if (error instanceof ConnectBrowserIdentityHoldError) throw error;
      if (error instanceof BoundedResponseError) {
        return hold(error.message);
      }
      return hold("Connect cross-origin pair request failed");
    } finally {
      globalThis.clearTimeout(timer);
    }
  }

  const transportOptions = options.transportOptions ?? {};
  const cryptoLoader = options.cryptoLoader ?? transportOptions.cryptoLoader;
  await resolveConnectCrypto(cryptoLoader);

  const transport = new ConnectBrowserTransport({
    firstPairing,
    localPublic: custody.publicKey,
    handshakeMaterialFactory: createConnectHandshakeMaterialFactory(custody),
    expectedRemote: hostKeyBytes(descriptor.hostPublicKey),
    hostPublicId: hostIdBytes(descriptor.hostPublicId),
    devicePublicId:
      transportOptions.devicePublicId ?? connectDevicePublicId(custody.deviceId),
    purpose: transportOptions.purpose,
    role: transportOptions.role,
    openedAtUnix: transportOptions.openedAtUnix,
    directReachable: transportOptions.directReachable ?? true,
    clientId: transportOptions.clientId,
    capabilities: transportOptions.capabilities ?? NATIVE_BROWSER_CAPABILITIES,
    capabilityGrant: transportOptions.capabilityGrant,
    limits: transportOptions.limits,
    explicitTarget: {
      origin: descriptor.origin,
      endpoint,
    },
    crossOriginTicket: attachTicket,
    cryptoLoader,
    socketFactory: transportOptions.socketFactory,
    onState: transportOptions.onState,
    onEnvelope: transportOptions.onEnvelope,
    onCapabilityGrant: transportOptions.onCapabilityGrant,
    onClientId: transportOptions.onClientId,
  });

  const marker: ConnectHostPublication = {
    transport: "connect",
    endpoint: "/api/connect",
    generation: descriptor.generation,
    protocolMajor: descriptor.protocolMajor,
    protocolMinor: descriptor.protocolMinor,
    hostPublicId: descriptor.hostPublicId,
    hostPublicKey: descriptor.hostPublicKey,
  };

  return {
    marker,
    identity: {
      deviceId: custody.deviceId,
      publicKey: custody.publicKey.slice(),
      hostGeneration: custody.hostGeneration,
    },
    transport,
    suspend: () => transport.suspend(),
    stop: () => transport.stop(),
  };
}

function inboundTicketRejected(ticket: string): boolean {
  if (!ticket || ticket.length > 1_024) return true;
  try {
    return new TextEncoder().encode(ticket).byteLength > 1_024;
  } catch {
    return true;
  }
}
