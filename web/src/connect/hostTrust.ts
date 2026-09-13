import { protocolUuid } from "./hostOutput";

export const CONNECT_HOST_TRUST_DB_NAME = "devmanager.connect.host-trust" as const;
export const CONNECT_HOST_TRUST_DB_VERSION = 1 as const;
export const CONNECT_HOST_TRUST_MAX_PER_ORIGIN = 16;

const HOST_TRUST_STORE = "hosts";
const HOST_TRUST_ORIGIN_INDEX = "origin";
const HOST_KEY_BYTES_HEX = /^[0-9a-f]{64}$/;

export class HostTrustHoldError extends Error {
  readonly code = "browser-connect-host-trust-held" as const;

  constructor(message: string) {
    super(message);
    this.name = "HostTrustHoldError";
  }
}

/** Public host binding only; no raw private or session material is persisted. */
export interface HostTrustRecord {
  origin: string;
  hostPublicId: string;
  hostPublicKey: string;
}

export interface HostTrustStorage {
  /** Atomically returns the first durable record for this (origin, host) key. */
  pin(record: HostTrustRecord): Promise<HostTrustRecord>;
}

export interface HostTrustOptions {
  storage?: HostTrustStorage;
  indexedDb?: IDBFactory;
}

function hold(message: string): never {
  throw new HostTrustHoldError(message);
}

function normalizedRecord(candidate: HostTrustRecord): HostTrustRecord {
  let origin: string;
  try {
    const url = new URL(candidate.origin);
    if ((url.protocol !== "https:" && url.protocol !== "http:") || url.origin !== candidate.origin) {
      return hold("Connect host origin is invalid");
    }
    origin = url.origin;
  } catch {
    return hold("Connect host origin is invalid");
  }
  const hostPublicId = protocolUuid(candidate.hostPublicId);
  if (!hostPublicId) return hold("Connect host identifier is invalid");
  const hostPublicKey = candidate.hostPublicKey.toLowerCase();
  if (!HOST_KEY_BYTES_HEX.test(hostPublicKey) || /^0{64}$/.test(hostPublicKey)) {
    return hold("Connect host public key is invalid");
  }
  return { origin, hostPublicId, hostPublicKey };
}

function sameBinding(left: HostTrustRecord, right: HostTrustRecord): boolean {
  return (
    left.origin === right.origin &&
    left.hostPublicId === right.hostPublicId &&
    left.hostPublicKey === right.hostPublicKey
  );
}

function storageFailure(): never {
  return hold("Connect host trust storage is unavailable");
}

/**
 * Pins a host key before any Connect network request. A changed first-winner
 * record is a HOLD, never a key rotation or in-memory fallback.
 */
export async function assertHostTrust(
  candidate: HostTrustRecord,
  options: HostTrustOptions = {},
): Promise<HostTrustRecord> {
  const record = normalizedRecord(candidate);
  const storage = options.storage ?? createIndexedDbHostTrustStorage(options.indexedDb);
  let winner: HostTrustRecord;
  try {
    winner = normalizedRecord(await storage.pin(record));
  } catch (error) {
    if (error instanceof HostTrustHoldError) throw error;
    return storageFailure();
  }
  if (!sameBinding(winner, record)) {
    return hold("Connect host key changed; explicit trust repair is required");
  }
  return winner;
}

function requestResult<T>(request: IDBRequest<T>): Promise<T> {
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

function recordKey(record: HostTrustRecord): [string, string] {
  return [record.origin, record.hostPublicId];
}

function structuralRecord(value: unknown): HostTrustRecord | null {
  if (!value || typeof value !== "object") return null;
  const raw = value as Partial<HostTrustRecord>;
  if (
    typeof raw.origin !== "string" ||
    typeof raw.hostPublicId !== "string" ||
    typeof raw.hostPublicKey !== "string"
  ) return null;
  try {
    return normalizedRecord(raw as HostTrustRecord);
  } catch {
    return null;
  }
}

/** IndexedDB is the only production store; tests may inject an explicit fake. */
export function createIndexedDbHostTrustStorage(
  indexedDb: IDBFactory = globalThis.indexedDB,
): HostTrustStorage {
  if (!indexedDb) return storageFailure();
  let databasePromise: Promise<IDBDatabase> | null = null;
  const database = (): Promise<IDBDatabase> => {
    databasePromise ??= new Promise((resolve, reject) => {
      const request = indexedDb.open(CONNECT_HOST_TRUST_DB_NAME, CONNECT_HOST_TRUST_DB_VERSION);
      request.onupgradeneeded = () => {
        const store = request.result.objectStoreNames.contains(HOST_TRUST_STORE)
          ? request.transaction!.objectStore(HOST_TRUST_STORE)
          : request.result.createObjectStore(HOST_TRUST_STORE, { keyPath: ["origin", "hostPublicId"] });
        if (!store.indexNames.contains(HOST_TRUST_ORIGIN_INDEX)) {
          store.createIndex(HOST_TRUST_ORIGIN_INDEX, "origin", { unique: false });
        }
      };
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error ?? new Error("IndexedDB host trust open failed"));
      request.onblocked = () => reject(new Error("IndexedDB host trust store blocked"));
    });
    return databasePromise;
  };
  return {
    async pin(candidate) {
      const record = normalizedRecord(candidate);
      const db = await database();
      const tx = db.transaction(HOST_TRUST_STORE, "readwrite");
      const store = tx.objectStore(HOST_TRUST_STORE);
      const existing = structuralRecord(await requestResult(store.get(recordKey(record))));
      if (existing) {
        await transactionDone(tx);
        return existing;
      }
      const index = store.index(HOST_TRUST_ORIGIN_INDEX);
      const existingForOrigin = await requestResult(index.count(IDBKeyRange.only(record.origin)));
      if (existingForOrigin >= CONNECT_HOST_TRUST_MAX_PER_ORIGIN) {
        try { tx.abort(); } catch { /* transaction is terminal */ }
        return hold("Connect host trust capacity reached; explicit repair is required");
      }
      await requestResult(store.add(record));
      await transactionDone(tx);
      return record;
    },
  };
}
