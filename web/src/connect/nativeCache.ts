/**
 * Presentation-only durable cache for native Connect host sessions.
 * Separate IndexedDB namespace from identity custody. Never stores fences as
 * action authority, private keys, raw terminal bytes, or provider secrets.
 */

import { protocolUuid } from "./hostOutput";
import {
  decodeSemanticJournalPage,
  isNativeUnitTaskCommand,
  NativeProtocolError,
  type NativeUuid,
  type SemanticJournalFact,
  type SnapshotListItem,
} from "./nativeProtocol";

export const NATIVE_CACHE_DB_NAME = "devmanager.connect.native" as const;
export const NATIVE_CACHE_DB_VERSION = 1 as const;
export const NATIVE_CACHE_SCHEMA_VERSION = 1 as const;
/** Read-cache TTL for metadata/history only — never silently TTL-delete drafts/outbox. */
export const NATIVE_CACHE_TTL_MS = 7 * 24 * 60 * 60 * 1000;

/** Live/metadata task projection bound (deliberately large). */
export const MAX_METADATA_TASKS_PER_HOST = 4_096;
/** @deprecated Use MAX_METADATA_TASKS_PER_HOST — kept as alias for older imports. */
export const MAX_CACHED_TASKS_PER_HOST = MAX_METADATA_TASKS_PER_HOST;
/** Bounded conversation history cache count (separate from metadata). */
export const MAX_HISTORY_CONVERSATIONS_PER_HOST = 64;
export const MAX_CACHED_FACTS_PER_TASK = 4_096;
export const MAX_CACHED_FACTS_TOTAL_BYTES = 8 * 1024 * 1024;
export const MAX_OUTBOX_ITEMS = 64;
export const MAX_OUTBOX_TOTAL_BYTES = 4 * 1024 * 1024;
export const MAX_IDB_PREFIX_SCAN = 8_192;

const PROJECTION_STORE = "projections";
const DRAFT_STORE = "drafts";
const OUTBOX_STORE = "outbox";
const META_STORE = "meta";

export class NativeCacheError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "NativeCacheError";
  }
}

export type NativeOutboxStatus =
  | "pending"
  | "in_flight"
  | "uncertain"
  | "blocked_client_mismatch";

export interface NativeCachedTaskMeta {
  taskId: NativeUuid;
  revision: number;
  actionEpoch: number;
  title: string | null;
  lifecycle: string | null;
  projectId: NativeUuid | null;
  environmentId: NativeUuid | null;
  createdAtMs: number | null;
  connectivity: string | null;
  attention: string | null;
  activity: string | null;
  primaryAgentId: NativeUuid | null;
  updatedAtMs: number;
}

export interface NativeCachedConversation {
  taskId: NativeUuid;
  afterSequence: number;
  throughSequence: number;
  highWater: number;
  oldestSequence: number;
  cursorRolledOver: boolean;
  nextSequence: number | null;
  facts: SemanticJournalFact[];
  updatedAtMs: number;
}

export interface NativeCachedDraft {
  taskId: NativeUuid;
  text: string;
  updatedAtMs: number;
}

export interface NativeOutboxRecord {
  hostPublicId: NativeUuid;
  clientId: NativeUuid;
  commandId: NativeUuid;
  taskId: NativeUuid;
  /** Exact CommandEnvelope object persisted before send. */
  commandPayload: unknown;
  text: string;
  issuedAtMs: number;
  status: NativeOutboxStatus;
  updatedAtMs: number;
}

export interface NativeHostCacheSnapshot {
  hostPublicId: NativeUuid;
  tasks: NativeCachedTaskMeta[];
  conversations: NativeCachedConversation[];
  drafts: NativeCachedDraft[];
  outbox: NativeOutboxRecord[];
}

export interface NativeCacheStore {
  loadHost(hostPublicId: NativeUuid): Promise<NativeHostCacheSnapshot>;
  putTasks(
    hostPublicId: NativeUuid,
    tasks: NativeCachedTaskMeta[],
  ): Promise<void>;
  putConversation(
    hostPublicId: NativeUuid,
    conversation: NativeCachedConversation,
  ): Promise<void>;
  putDraft(hostPublicId: NativeUuid, draft: NativeCachedDraft): Promise<void>;
  clearDraft(hostPublicId: NativeUuid, taskId: NativeUuid): Promise<void>;
  putOutbox(record: NativeOutboxRecord): Promise<void>;
  settleOutbox(
    hostPublicId: NativeUuid,
    commandId: NativeUuid,
    acceptedText?: string,
  ): Promise<void>;
  updateOutboxStatus(
    hostPublicId: NativeUuid,
    commandId: NativeUuid,
    status: NativeOutboxStatus,
  ): Promise<void>;
  clearHost(hostPublicId: NativeUuid): Promise<void>;
}

function rejected(message: string): never {
  throw new NativeCacheError(message);
}

function requireHostId(value: unknown): NativeUuid {
  const id = protocolUuid(value);
  if (!id) rejected("invalid hostPublicId");
  return id;
}

function requireTaskId(value: unknown): NativeUuid {
  const id = protocolUuid(value);
  if (!id) rejected("invalid taskId");
  return id;
}

function estimateJsonBytes(value: unknown): number {
  return new TextEncoder().encode(jsonText(value)).byteLength;
}

/** JSON-only cache records must never retain caller-owned object references. */
function jsonText(value: unknown): string {
  const serialized = JSON.stringify(value);
  if (serialized === undefined) rejected("cache value is not JSON");
  return serialized;
}

function cloneJson<T>(value: T): T {
  return JSON.parse(jsonText(value)) as T;
}

function cloneConversation(
  conversation: NativeCachedConversation,
): NativeCachedConversation {
  return {
    ...conversation,
    facts: cloneJson(conversation.facts),
  };
}

function cloneOutbox(record: NativeOutboxRecord): NativeOutboxRecord {
  return {
    ...record,
    commandPayload: cloneJson(record.commandPayload),
  };
}

function requireOutboxStatus(value: unknown): NativeOutboxStatus {
  const allowed: NativeOutboxStatus[] = [
    "pending",
    "in_flight",
    "uncertain",
    "blocked_client_mismatch",
  ];
  if (!allowed.includes(value as NativeOutboxStatus)) {
    rejected("outbox status rejected");
  }
  return value as NativeOutboxStatus;
}

function withinTtl(updatedAtMs: number, nowMs: number): boolean {
  return nowMs - updatedAtMs <= NATIVE_CACHE_TTL_MS && updatedAtMs <= nowMs + 60_000;
}

function projectionKey(hostPublicId: string, taskId: string): string {
  return `${hostPublicId}:${taskId}`;
}

function draftKey(hostPublicId: string, taskId: string): string {
  return `${hostPublicId}:draft:${taskId}`;
}

function outboxKey(hostPublicId: string, commandId: string): string {
  return `${hostPublicId}:cmd:${commandId}`;
}

function hostKeyRange(host: string): IDBKeyRange {
  return IDBKeyRange.bound(host + ":", host + ":\uffff", false, false);
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function exactKeys(value: Record<string, unknown>, keys: string[]): boolean {
  const actual = Object.keys(value);
  return actual.length === keys.length && keys.every((key) => key in value);
}

/**
 * Strict outbox command envelope — SubmitProviderInput SendNow/terminal, or
 * phone-supported metadata lifecycle/rename. No arbitrary payloads.
 */
export function validateOutboxCommandPayload(
  payload: unknown,
  expected: {
    hostPublicId: NativeUuid;
    clientId: NativeUuid;
    commandId: NativeUuid;
    taskId: NativeUuid;
    text: string;
  },
): unknown {
  if (!isPlainObject(payload)) rejected("outbox payload rejected");
  if (
    !exactKeys(payload, [
      "command_id",
      "client_id",
      "task_id",
      "issued_at_ms",
      "expected_task_revision",
      "command",
    ])
  ) {
    rejected("outbox payload shape rejected");
  }
  if (protocolUuid(payload.command_id) !== expected.commandId) {
    rejected("outbox command_id mismatch");
  }
  if (protocolUuid(payload.client_id) !== expected.clientId) {
    rejected("outbox client_id mismatch");
  }
  if (protocolUuid(payload.task_id) !== expected.taskId) {
    rejected("outbox task_id mismatch");
  }
  if (
    typeof payload.issued_at_ms !== "number" ||
    !Number.isSafeInteger(payload.issued_at_ms)
  ) {
    rejected("outbox issued_at_ms rejected");
  }
  if (
    typeof payload.expected_task_revision !== "number" ||
    !Number.isSafeInteger(payload.expected_task_revision) ||
    payload.expected_task_revision <= 0
  ) {
    rejected("outbox expected_task_revision rejected");
  }

  if (isNativeUnitTaskCommand(payload.command)) {
    if (expected.text !== "") rejected("metadata outbox text must be empty");
    return payload;
  }

  const command = payload.command;
  if (!isPlainObject(command) || Object.keys(command).length !== 1) {
    rejected("outbox command variant rejected");
  }

  if ("rename_task" in command) {
    const rename = command.rename_task;
    if (!isPlainObject(rename) || !exactKeys(rename, ["title"])) {
      rejected("rename_task shape rejected");
    }
    if (typeof rename.title !== "string" || rename.title.trim().length === 0) {
      rejected("rename_task title rejected");
    }
    if (rename.title !== expected.text) rejected("rename_task text mismatch");
    return payload;
  }

  if (!exactKeys(command, ["submit_provider_input"])) {
    rejected("outbox command variant rejected");
  }
  const submit = command.submit_provider_input;
  if (
    !isPlainObject(submit) ||
    !exactKeys(submit, [
      "agent_session_id",
      "runtime_generation",
      "turn_id",
      "action_epoch",
      "question_id",
      "approval_id",
      "action",
    ])
  ) {
    rejected("submit_provider_input shape rejected");
  }
  if (submit.question_id !== null || submit.approval_id !== null) {
    rejected("outbox blockers must be null");
  }
  if (!protocolUuid(submit.agent_session_id)) rejected("agent_session_id rejected");
  if (!protocolUuid(submit.turn_id)) rejected("turn_id rejected");
  if (
    typeof submit.runtime_generation !== "number" ||
    !Number.isSafeInteger(submit.runtime_generation)
  ) {
    rejected("runtime_generation rejected");
  }
  if (
    typeof submit.action_epoch !== "number" ||
    !Number.isSafeInteger(submit.action_epoch)
  ) {
    rejected("action_epoch rejected");
  }
  const action = submit.action;
  if (isPlainObject(action) && exactKeys(action, ["terminal_input"])) {
    const terminal = action.terminal_input;
    if (!isPlainObject(terminal) || !exactKeys(terminal, ["text"]) ||
        typeof terminal.text !== "string" || terminal.text !== expected.text ||
        !["\r", "\u001b", "\u001b[A", "\u001b[B", "\u0003"].includes(terminal.text)) {
      rejected("terminal control shape rejected");
    }
    return payload;
  }
  if (!isPlainObject(action) || !exactKeys(action, ["send_now"])) {
    rejected("send_now action rejected");
  }
  const sendNow = action.send_now;
  if (!isPlainObject(sendNow) || !exactKeys(sendNow, ["text", "wait"])) {
    rejected("send_now shape rejected");
  }
  if (typeof sendNow.text !== "string" || sendNow.text !== expected.text) {
    rejected("send_now text mismatch");
  }
  if (typeof sendNow.wait !== "boolean") rejected("send_now wait rejected");
  if ("images" in sendNow || "image" in sendNow) {
    rejected("images disallowed in outbox");
  }
  void expected.hostPublicId;
  return payload;
}

function validateConversation(
  conversation: NativeCachedConversation,
): NativeCachedConversation {
  const taskId = requireTaskId(conversation.taskId);
  if (!Array.isArray(conversation.facts)) rejected("conversation facts rejected");
  if (conversation.facts.length > MAX_CACHED_FACTS_PER_TASK) {
    rejected("conversation facts exceed bound");
  }
  if (
    typeof conversation.afterSequence !== "number" ||
    typeof conversation.throughSequence !== "number" ||
    typeof conversation.highWater !== "number" ||
    typeof conversation.oldestSequence !== "number" ||
    typeof conversation.cursorRolledOver !== "boolean" ||
    !(
      conversation.nextSequence === null ||
      typeof conversation.nextSequence === "number"
    )
  ) {
    rejected("conversation cursors rejected");
  }
  if (conversation.afterSequence > conversation.throughSequence) {
    rejected("conversation after/through ordering rejected");
  }
  if (conversation.throughSequence > conversation.highWater) {
    rejected("conversation through/high_water ordering rejected");
  }
  // Re-validate each fact through the protocol page decoder (single-fact final pages).
  let previous = 0;
  for (const fact of conversation.facts) {
    try {
      decodeSemanticJournalPage({
        after_sequence: previous,
        through_sequence: fact.sequence,
        high_water: fact.sequence,
        oldest_sequence: fact.sequence,
        cursor_rolled_over: false,
        encoded_bytes: 32,
        next_sequence: null,
        facts: [
          {
            id: fact.id,
            sequence: fact.sequence,
            occurred_at_ms: fact.occurredAtMs,
            provider: fact.provider,
            schema_version: fact.schemaVersion,
            kind: fact.kind,
            visibility: fact.visibility,
            privacy_class: fact.privacyClass,
            redacted: fact.redacted,
            payload: fact.payload,
          },
        ],
      });
    } catch (error) {
      if (error instanceof NativeProtocolError) {
        rejected(`conversation fact validation failed: ${error.message}`);
      }
      throw error;
    }
    if (fact.sequence <= previous) rejected("conversation fact sequence rejected");
    previous = fact.sequence;
  }
  return { ...conversation, taskId };
}

function validateOutbox(record: NativeOutboxRecord): NativeOutboxRecord {
  const hostPublicId = requireHostId(record.hostPublicId);
  const clientId = protocolUuid(record.clientId);
  const commandId = protocolUuid(record.commandId);
  const taskId = requireTaskId(record.taskId);
  if (!clientId || !commandId) rejected("outbox identity rejected");
  if (typeof record.text !== "string") rejected("outbox text rejected");
  if (
    typeof record.issuedAtMs !== "number" ||
    !Number.isSafeInteger(record.issuedAtMs)
  ) {
    rejected("outbox issuedAtMs rejected");
  }
  if (
    typeof record.updatedAtMs !== "number" ||
    !Number.isSafeInteger(record.updatedAtMs)
  ) {
    rejected("outbox updatedAtMs rejected");
  }
  const status = requireOutboxStatus(record.status);
  const commandPayload = validateOutboxCommandPayload(record.commandPayload, {
    hostPublicId,
    clientId,
    commandId,
    taskId,
    text: record.text,
  });
  const normalized = {
    ...record,
    hostPublicId,
    clientId,
    commandId,
    taskId,
    commandPayload,
    status,
  };
  const bytes = estimateJsonBytes(normalized);
  if (bytes > MAX_OUTBOX_TOTAL_BYTES) rejected("outbox record too large");
  return cloneOutbox(normalized);
}

function openRequest<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () =>
      reject(request.error ?? new NativeCacheError("IndexedDB request failed"));
  });
}

function transactionDone(transaction: IDBTransaction): Promise<void> {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve();
    transaction.onerror = () =>
      reject(transaction.error ?? new NativeCacheError("IndexedDB transaction failed"));
    transaction.onabort = () =>
      reject(transaction.error ?? new NativeCacheError("IndexedDB transaction aborted"));
  });
}

/** Collect host-prefix keys via cursor (bounded) — no unbounded getAllKeys. */
function collectPrefixedKeys(
  store: IDBObjectStore,
  host: string,
  max: number,
): Promise<IDBValidKey[]> {
  return new Promise((resolve, reject) => {
    const keys: IDBValidKey[] = [];
    const request = store.openCursor(hostKeyRange(host));
    request.onerror = () =>
      reject(request.error ?? new NativeCacheError("IndexedDB cursor failed"));
    request.onsuccess = () => {
      const cursor = request.result;
      if (!cursor) {
        resolve(keys);
        return;
      }
      if (keys.length >= max) {
        reject(new NativeCacheError("IndexedDB prefix scan bound exceeded"));
        return;
      }
      keys.push(cursor.key);
      cursor.continue();
    };
  });
}

async function readPrefixedEntries<T>(
  store: IDBObjectStore,
  host: string,
): Promise<Array<{ key: IDBValidKey; value: T }>> {
  const keys = await collectPrefixedKeys(store, host, MAX_IDB_PREFIX_SCAN);
  const entries: Array<{ key: IDBValidKey; value: T }> = [];
  for (const key of keys) {
    const value = await openRequest<T | undefined>(store.get(key));
    if (value !== undefined) entries.push({ key, value });
  }
  return entries;
}

function historyVictimKeys(
  entries: readonly { key: IDBValidKey; value: NativeCachedConversation }[],
  incomingKey: string,
  incoming: NativeCachedConversation,
): IDBValidKey[] {
  const incomingBytes = estimateJsonBytes(incoming);
  if (incomingBytes > MAX_CACHED_FACTS_TOTAL_BYTES) {
    rejected("cached conversation budget exceeded");
  }
  const retained = entries
    .filter((entry) => entry.key !== incomingKey)
    .slice()
    .sort((left, right) => left.value.updatedAtMs - right.value.updatedAtMs);
  let totalBytes = incomingBytes;
  for (const entry of retained) totalBytes += estimateJsonBytes(entry.value);

  const victims: IDBValidKey[] = [];
  while (
    (retained.length + 1 > MAX_HISTORY_CONVERSATIONS_PER_HOST ||
      totalBytes > MAX_CACHED_FACTS_TOTAL_BYTES) &&
    retained.length > 0
  ) {
    const victim = retained.shift();
    if (!victim) break;
    victims.push(victim.key);
    totalBytes -= estimateJsonBytes(victim.value);
  }
  if (
    retained.length + 1 > MAX_HISTORY_CONVERSATIONS_PER_HOST ||
    totalBytes > MAX_CACHED_FACTS_TOTAL_BYTES
  ) {
    rejected("cached conversation budget exceeded");
  }
  return victims;
}

function assertOutboxCapacity(
  entries: readonly NativeOutboxRecord[],
  incoming: NativeOutboxRecord,
): void {
  const otherRecords = entries.filter(
    (entry) => entry.commandId !== incoming.commandId,
  );
  const bytes =
    estimateJsonBytes(incoming) +
    otherRecords.reduce((total, entry) => total + estimateJsonBytes(entry), 0);
  if (
    otherRecords.length + 1 > MAX_OUTBOX_ITEMS ||
    bytes > MAX_OUTBOX_TOTAL_BYTES
  ) {
    rejected("outbox capacity exceeded");
  }
}

/** In-memory adapter for unit tests and offline fixtures. */
export function createMemoryNativeCacheStore(
  now: () => number = () => Date.now(),
): NativeCacheStore {
  const tasks = new Map<string, NativeCachedTaskMeta[]>();
  const conversations = new Map<string, NativeCachedConversation>();
  const drafts = new Map<string, NativeCachedDraft>();
  const outbox = new Map<string, NativeOutboxRecord>();
  let writeChain: Promise<void> = Promise.resolve();

  const serialize = async <T>(work: () => Promise<T> | T): Promise<T> => {
    const run = writeChain.then(work, work);
    writeChain = run.then(
      () => undefined,
      () => undefined,
    );
    return run;
  };

  return {
    async loadHost(hostPublicId) {
      const host = requireHostId(hostPublicId);
      const nowMs = now();
      // TTL applies only to read-cache metadata/history — never drafts/outbox.
      const taskList = (tasks.get(host) ?? []).filter((task) =>
        withinTtl(task.updatedAtMs, nowMs),
      );
      const conversationList: NativeCachedConversation[] = [];
      for (const [key, value] of conversations) {
        if (!key.startsWith(`${host}:`)) continue;
        if (!withinTtl(value.updatedAtMs, nowMs)) {
          conversations.delete(key);
          continue;
        }
        conversationList.push(cloneConversation(value));
      }
      const draftList: NativeCachedDraft[] = [];
      for (const [key, value] of drafts) {
        if (!key.startsWith(`${host}:`)) continue;
        draftList.push({ ...value });
      }
      const outboxList: NativeOutboxRecord[] = [];
      for (const [key, value] of outbox) {
        if (!key.startsWith(`${host}:`)) continue;
        outboxList.push(cloneOutbox(value));
      }
      return {
        hostPublicId: host,
        tasks: taskList,
        conversations: conversationList,
        drafts: draftList,
        outbox: outboxList,
      };
    },

    async putTasks(hostPublicId, nextTasks) {
      await serialize(() => {
        const host = requireHostId(hostPublicId);
        if (nextTasks.length > MAX_METADATA_TASKS_PER_HOST) {
          rejected("cached task list exceeds bound");
        }
        const nowMs = now();
        tasks.set(
          host,
          nextTasks.map((task) => ({
          ...task,
            taskId: requireTaskId(task.taskId),
            updatedAtMs: task.updatedAtMs ?? nowMs,
          })),
        );
      });
    },

    async putConversation(hostPublicId, conversation) {
      await serialize(() => {
        const host = requireHostId(hostPublicId);
        const validated = validateConversation(conversation);
        const key = projectionKey(host, validated.taskId);
        const incoming = cloneConversation({
          ...validated,
          updatedAtMs: validated.updatedAtMs ?? now(),
        });
        const existing: Array<{ key: string; value: NativeCachedConversation }> = [];
        for (const [existingKey, storedConversation] of conversations) {
          if (!existingKey.startsWith(`${host}:`)) continue;
          existing.push({ key: existingKey, value: storedConversation });
        }
        // Plan before mutation: an oversized incoming history must not evict
        // an existing read cache merely to fail afterward.
        const victims = historyVictimKeys(existing, key, incoming);
        conversations.delete(key);
        for (const victim of victims) conversations.delete(String(victim));
        conversations.set(key, incoming);
      });
    },

    async putDraft(hostPublicId, draft) {
      await serialize(() => {
        const host = requireHostId(hostPublicId);
        const taskId = requireTaskId(draft.taskId);
        if (typeof draft.text !== "string") rejected("draft text rejected");
        drafts.set(draftKey(host, taskId), {
          taskId,
          text: draft.text,
          updatedAtMs: draft.updatedAtMs ?? now(),
        });
      });
    },

    async clearDraft(hostPublicId, taskId) {
      await serialize(() => {
        drafts.delete(draftKey(requireHostId(hostPublicId), requireTaskId(taskId)));
      });
    },

    async putOutbox(record) {
      await serialize(() => {
        const validated = validateOutbox(record);
        const host = validated.hostPublicId;
        let count = 0;
        let bytes = 0;
        for (const [key, existing] of outbox) {
          if (!key.startsWith(`${host}:`)) continue;
          if (existing.commandId === validated.commandId) continue;
          count += 1;
          bytes += estimateJsonBytes(existing);
        }
        bytes += estimateJsonBytes(validated);
        if (count + 1 > MAX_OUTBOX_ITEMS || bytes > MAX_OUTBOX_TOTAL_BYTES) {
          rejected("outbox capacity exceeded");
        }
        outbox.set(outboxKey(host, validated.commandId), {
          ...validated,
          updatedAtMs: validated.updatedAtMs ?? now(),
        });
      });
    },

    async settleOutbox(hostPublicId, commandId, acceptedText) {
      await serialize(() => {
        const host = requireHostId(hostPublicId);
        const id = protocolUuid(commandId) ?? rejected("commandId");
        if (acceptedText !== undefined && typeof acceptedText !== "string") {
          rejected("accepted draft text rejected");
        }
        const existing = outbox.get(outboxKey(host, id));
        outbox.delete(outboxKey(host, id));
        if (acceptedText !== undefined && existing?.text === acceptedText) {
          const key = draftKey(host, existing.taskId);
          if (drafts.get(key)?.text === acceptedText) drafts.delete(key);
        }
      });
    },

    async updateOutboxStatus(hostPublicId, commandId, status) {
      await serialize(() => {
        const key = outboxKey(
          requireHostId(hostPublicId),
          protocolUuid(commandId) ?? rejected("commandId"),
        );
        const existing = outbox.get(key);
        if (!existing) rejected("outbox record missing");
        outbox.set(key, { ...existing, status, updatedAtMs: now() });
      });
    },

    async clearHost(hostPublicId) {
      await serialize(() => {
        const host = requireHostId(hostPublicId);
        tasks.delete(host);
        for (const key of [...conversations.keys()]) {
          if (key.startsWith(`${host}:`)) conversations.delete(key);
        }
        for (const key of [...drafts.keys()]) {
          if (key.startsWith(`${host}:`)) drafts.delete(key);
        }
        for (const key of [...outbox.keys()]) {
          if (key.startsWith(`${host}:`)) outbox.delete(key);
        }
      });
    },
  };
}

/**
 * IndexedDB persistence. Durable commit completes first; memory mirror follows.
 * Inject an IDBFactory seam for tests — package.json has no fake-indexeddb.
 */
export function createIndexedDbNativeCacheStore(
  indexedDb: IDBFactory = globalThis.indexedDB,
  now: () => number = () => Date.now(),
): NativeCacheStore {
  if (!indexedDb) {
    return {
      async loadHost() {
        rejected("IndexedDB unavailable");
      },
      async putTasks() {
        rejected("IndexedDB unavailable");
      },
      async putConversation() {
        rejected("IndexedDB unavailable");
      },
      async putDraft() {
        rejected("IndexedDB unavailable");
      },
      async clearDraft() {
        rejected("IndexedDB unavailable");
      },
      async putOutbox() {
        rejected("IndexedDB unavailable");
      },
      async settleOutbox() {
        rejected("IndexedDB unavailable");
      },
      async updateOutboxStatus() {
        rejected("IndexedDB unavailable");
      },
      async clearHost() {
        rejected("IndexedDB unavailable");
      },
    };
  }

  let databasePromise: Promise<IDBDatabase> | null = null;
  let writeChain: Promise<void> = Promise.resolve();
  const memory = createMemoryNativeCacheStore(now);

  const database = (): Promise<IDBDatabase> => {
    databasePromise ??= new Promise((resolve, reject) => {
      const request = indexedDb.open(NATIVE_CACHE_DB_NAME, NATIVE_CACHE_DB_VERSION);
      request.onupgradeneeded = () => {
        const db = request.result;
        if (!db.objectStoreNames.contains(PROJECTION_STORE)) {
          db.createObjectStore(PROJECTION_STORE);
        }
        if (!db.objectStoreNames.contains(DRAFT_STORE)) {
          db.createObjectStore(DRAFT_STORE);
        }
        if (!db.objectStoreNames.contains(OUTBOX_STORE)) {
          db.createObjectStore(OUTBOX_STORE);
        }
        if (!db.objectStoreNames.contains(META_STORE)) {
          db.createObjectStore(META_STORE);
        }
      };
      request.onsuccess = () => resolve(request.result);
      request.onerror = () =>
        reject(request.error ?? new NativeCacheError("IndexedDB open failed"));
      request.onblocked = () =>
        reject(new NativeCacheError("IndexedDB native cache blocked"));
    });
    return databasePromise;
  };

  const serialize = async <T>(work: () => Promise<T>): Promise<T> => {
    const run = writeChain.then(work, work);
    writeChain = run.then(
      () => undefined,
      () => undefined,
    );
    return run;
  };

  return {
    async loadHost(hostPublicId) {
      return serialize(async () => {
        const host = requireHostId(hostPublicId);
        const existing = await memory.loadHost(host);
        if (
          existing.tasks.length > 0 ||
          existing.conversations.length > 0 ||
          existing.drafts.length > 0 ||
          existing.outbox.length > 0
        ) {
          return existing;
        }
        const db = await database();
        const tx = db.transaction(
          [PROJECTION_STORE, DRAFT_STORE, OUTBOX_STORE, META_STORE],
          "readonly",
        );
        const done = transactionDone(tx);
        const taskRaw = await openRequest<NativeCachedTaskMeta[] | undefined>(
          tx.objectStore(META_STORE).get(`tasks:${host}`),
        );
        const projectionKeys = await collectPrefixedKeys(
          tx.objectStore(PROJECTION_STORE),
          host,
          MAX_IDB_PREFIX_SCAN,
        );
        const draftKeys = await collectPrefixedKeys(
          tx.objectStore(DRAFT_STORE),
          host,
          MAX_IDB_PREFIX_SCAN,
        );
        const outboxKeys = await collectPrefixedKeys(
          tx.objectStore(OUTBOX_STORE),
          host,
          MAX_IDB_PREFIX_SCAN,
        );
        const projectionStore = tx.objectStore(PROJECTION_STORE);
        const draftStore = tx.objectStore(DRAFT_STORE);
        const outboxStore = tx.objectStore(OUTBOX_STORE);
        const conversationsRaw: unknown[] = [];
        const draftsRaw: unknown[] = [];
        const outboxRaw: unknown[] = [];
        for (const key of projectionKeys) {
          conversationsRaw.push(await openRequest(projectionStore.get(key)));
        }
        for (const key of draftKeys) {
          draftsRaw.push(await openRequest(draftStore.get(key)));
        }
        for (const key of outboxKeys) {
          outboxRaw.push(await openRequest(outboxStore.get(key)));
        }
        await done;
        // Mirror only after the IDB transaction completes — never await non-IDB
        // work while the transaction is open (avoids auto-commit divergence).
        for (const value of conversationsRaw) {
          if (!value) continue;
          try {
            await memory.putConversation(host, value as NativeCachedConversation);
          } catch {
            // Quarantine individual corrupt conversation records.
          }
        }
        for (const value of draftsRaw) {
          if (!value) continue;
          try {
            await memory.putDraft(host, value as NativeCachedDraft);
          } catch {
            // Quarantine individual corrupt drafts.
          }
        }
        for (const value of outboxRaw) {
          if (!value) continue;
          try {
            await memory.putOutbox(value as NativeOutboxRecord);
          } catch {
            // Quarantine individual corrupt outbox records.
          }
        }
        if (Array.isArray(taskRaw) && taskRaw.length > 0) {
          await memory.putTasks(host, taskRaw);
        }
        return memory.loadHost(host);
      });
    },

    async putTasks(hostPublicId, nextTasks) {
      await serialize(async () => {
        const host = requireHostId(hostPublicId);
        if (nextTasks.length > MAX_METADATA_TASKS_PER_HOST) {
          rejected("cached task list exceeds bound");
        }
        const db = await database();
        const tx = db.transaction(META_STORE, "readwrite");
        const done = transactionDone(tx);
        tx.objectStore(META_STORE).put(nextTasks, `tasks:${host}`);
        await done;
        // Durable commit first, then mirror.
        await memory.putTasks(hostPublicId, nextTasks);
      });
    },

    async putConversation(hostPublicId, conversation) {
      await serialize(async () => {
        const host = requireHostId(hostPublicId);
        const validated = cloneConversation(validateConversation(conversation));
        const key = projectionKey(host, validated.taskId);
        const db = await database();
        const tx = db.transaction(PROJECTION_STORE, "readwrite");
        const done = transactionDone(tx);
        const store = tx.objectStore(PROJECTION_STORE);
        // The reads below are all IDB requests. Do not await validation, cache
        // mirroring, or any other non-IDB work before this transaction commits.
        const existing = await readPrefixedEntries<NativeCachedConversation>(
          store,
          host,
        );
        const checked = existing.map((entry) => ({
          key: entry.key,
          value: validateConversation(entry.value),
        }));
        for (const victim of historyVictimKeys(checked, key, validated)) {
          store.delete(victim);
        }
        store.put(validated, key);
        await done;
        await memory.putConversation(host, validated);
      });
    },

    async putDraft(hostPublicId, draft) {
      await serialize(async () => {
        const host = requireHostId(hostPublicId);
        const taskId = requireTaskId(draft.taskId);
        if (typeof draft.text !== "string") rejected("draft text rejected");
        const record = {
          taskId,
          text: draft.text,
          updatedAtMs: draft.updatedAtMs ?? now(),
        };
        const db = await database();
        const tx = db.transaction(DRAFT_STORE, "readwrite");
        const done = transactionDone(tx);
        tx.objectStore(DRAFT_STORE).put(record, draftKey(host, taskId));
        await done;
        await memory.putDraft(hostPublicId, record);
      });
    },

    async clearDraft(hostPublicId, taskId) {
      await serialize(async () => {
        const host = requireHostId(hostPublicId);
        const id = requireTaskId(taskId);
        const db = await database();
        const tx = db.transaction(DRAFT_STORE, "readwrite");
        const done = transactionDone(tx);
        tx.objectStore(DRAFT_STORE).delete(draftKey(host, id));
        await done;
        await memory.clearDraft(host, id);
      });
    },

    async putOutbox(record) {
      await serialize(async () => {
        const validated = validateOutbox(record);
        const db = await database();
        const tx = db.transaction(OUTBOX_STORE, "readwrite");
        const done = transactionDone(tx);
        const store = tx.objectStore(OUTBOX_STORE);
        // Capacity is checked from durable rows, not the process-local mirror.
        const existing = await readPrefixedEntries<NativeOutboxRecord>(
          store,
          validated.hostPublicId,
        );
        assertOutboxCapacity(
          existing.map((entry) => validateOutbox(entry.value)),
          validated,
        );
        store.put(validated, outboxKey(validated.hostPublicId, validated.commandId));
        await done;
        await memory.putOutbox(validated);
      });
    },

    async settleOutbox(hostPublicId, commandId, acceptedText) {
      await serialize(async () => {
        const host = requireHostId(hostPublicId);
        const id = protocolUuid(commandId) ?? rejected("commandId");
        if (acceptedText !== undefined && typeof acceptedText !== "string") {
          rejected("accepted draft text rejected");
        }
        const db = await database();
        const tx = db.transaction([OUTBOX_STORE, DRAFT_STORE], "readwrite");
        const done = transactionDone(tx);
        const outboxStore = tx.objectStore(OUTBOX_STORE);
        const existing = await openRequest<NativeOutboxRecord | undefined>(
          outboxStore.get(outboxKey(host, id)),
        );
        outboxStore.delete(outboxKey(host, id));
        if (acceptedText !== undefined && existing?.text === acceptedText) {
          const draftStore = tx.objectStore(DRAFT_STORE);
          const currentDraft = await openRequest<NativeCachedDraft | undefined>(
            draftStore.get(draftKey(host, existing.taskId)),
          );
          if (currentDraft?.text === acceptedText) {
            draftStore.delete(draftKey(host, existing.taskId));
          }
        }
        await done;
        await memory.settleOutbox(host, id, acceptedText);
      });
    },

    async updateOutboxStatus(hostPublicId, commandId, status) {
      await serialize(async () => {
        const host = requireHostId(hostPublicId);
        const id = protocolUuid(commandId) ?? rejected("commandId");
        const nextStatus = requireOutboxStatus(status);
        const db = await database();
        const tx = db.transaction(OUTBOX_STORE, "readwrite");
        const done = transactionDone(tx);
        const store = tx.objectStore(OUTBOX_STORE);
        const existing = await openRequest<NativeOutboxRecord | undefined>(
          store.get(outboxKey(host, id)),
        );
        if (!existing) rejected("outbox record missing");
        const updated = validateOutbox({
          ...existing,
          status: nextStatus,
          updatedAtMs: now(),
        });
        store.put(updated, outboxKey(host, id));
        await done;
        // A fresh store has no mirror entry; reinsert the committed durable row.
        await memory.putOutbox(updated);
      });
    },

    async clearHost(hostPublicId) {
      await serialize(async () => {
        const host = requireHostId(hostPublicId);
        const db = await database();
        const tx = db.transaction(
          [PROJECTION_STORE, DRAFT_STORE, OUTBOX_STORE, META_STORE],
          "readwrite",
        );
        const done = transactionDone(tx);
        const deletePrefixed = (storeName: string) => {
          const store = tx.objectStore(storeName);
          const request = store.openCursor(hostKeyRange(host));
          request.onsuccess = () => {
            const cursor = request.result;
            if (!cursor) return;
            cursor.delete();
            cursor.continue();
          };
        };
        deletePrefixed(PROJECTION_STORE);
        deletePrefixed(DRAFT_STORE);
        deletePrefixed(OUTBOX_STORE);
        tx.objectStore(META_STORE).delete(`tasks:${host}`);
        await done;
        await memory.clearHost(host);
      });
    },
  };
}

export function tasksFromSnapshotItems(
  items: SnapshotListItem[],
  updatedAtMs: number,
): NativeCachedTaskMeta[] {
  return items.map((item) => ({
    taskId: item.taskId,
    revision: item.revision,
    actionEpoch: item.actionEpoch,
    title: item.title,
    lifecycle: item.lifecycle,
    projectId: item.projectId,
    environmentId: item.environmentId,
    createdAtMs: item.createdAtMs,
    connectivity: item.connectivity,
    attention: item.attention,
    activity: item.activity,
    primaryAgentId: item.primaryAgentId,
    updatedAtMs,
  }));
}
