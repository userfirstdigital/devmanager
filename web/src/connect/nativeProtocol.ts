/**
 * Bounded typed browser adapter for the canonical native Connect protocol.
 * Encode/decode foundation only: no socket, UI, storage, or polling.
 */

import type { ConnectPayloadRequest } from "./transport";
import { capabilityBits, protocolUuid } from "./hostOutput";

export const NATIVE_QUERY_KIND = 5;
export const NATIVE_COMMAND_KIND = 6;
export const NATIVE_COMMAND_RECEIPT_KIND = 7;
export const NATIVE_QUERY_REPLY_KIND = 18;
/** Root carrier for conversation dirty notices (ephemeral). */
export const NATIVE_CONVERSATION_DIRTY_KIND = 22;

export const CAPABILITY_PAGED_SNAPSHOTS = 1n << 0n;
export const CAPABILITY_EVENT_REPLAY = 1n << 1n;
export const CAPABILITY_SEMANTIC_CONVERSATION = 1n << 5n;
export const CAPABILITY_PROVIDER_INPUT = 1n << 14n;
export const CAPABILITY_TASK_COCKPIT = 1n << 17n;
/** The actual browser Hello must advertise the canonical session it mounts. */
export const NATIVE_BROWSER_CAPABILITIES = Number(
  CAPABILITY_PAGED_SNAPSHOTS |
    CAPABILITY_EVENT_REPLAY |
    CAPABILITY_SEMANTIC_CONVERSATION |
    CAPABILITY_PROVIDER_INPUT |
    CAPABILITY_TASK_COCKPIT |
    (1n << 7n), // BrowserProjection
);

export const MAX_PROVIDER_INPUT_TEXT_BYTES = 64 * 1024;
/** Verified `MAX_PROVIDER_SESSION_ID_BYTES` in `src/domain/agent.rs`. */
export const MAX_PROVIDER_SESSION_ID_BYTES = 256;
/** Host-filtered pending wait id collection bound. */
export const MAX_PENDING_WAIT_COMMAND_IDS = 128;
/** Verified `MAX_CONNECT_RESUME_CURSOR_BYTES` / `MAX_CONNECT_CURSOR_BYTES`. */
export const MAX_RESUME_CURSOR_BYTES = 64 * 1024;
/** Max padded base64 chars for a 64KiB decoded cursor (bound before atob). */
export const MAX_RESUME_CURSOR_BASE64_CHARS =
  Math.ceil(MAX_RESUME_CURSOR_BYTES / 3) * 4;
/** Host cockpit conversation page bounds. */
export const MAX_SEMANTIC_PAGE_FACTS = 128;
export const MAX_SEMANTIC_PAGE_ENCODED_BYTES = 256 * 1024;
export const MAX_SEMANTIC_TEXT_BYTES = 64 * 1024;
export const MAX_SNAPSHOT_PAGE_ITEMS = 1_000;

export class NativeProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "NativeProtocolError";
  }
}

export type NativeUuid = string;
/** Canonical Rust ProviderKind wire names. UI labels may still say Claude Code. */
export type NativeProviderKind = "claude" | "codex";
export type NativeProviderModel =
  | "provider_default"
  | "codex_sol"
  | "codex_terra"
  | "codex_luna"
  | "claude_opus"
  | "claude_sonnet"
  | "claude_haiku";
export type NativeReasoningEffort =
  | "provider_default"
  | "low"
  | "medium"
  | "high"
  | "extra_high"
  | "max"
  | "ultra";
export type NativeAccessMode = "full_access" | "workspace_write" | "read_only";
export interface NativeProviderLaunchOptions {
  model: NativeProviderModel;
  reasoningEffort: NativeReasoningEffort;
  access: NativeAccessMode;
}

export type AgentLifecycle = "open" | "closing" | "closed";

export interface NativeAuthority {
  hostPublicId: NativeUuid;
  clientId: NativeUuid;
  requestId: NativeUuid;
}

/**
 * Fence from a fresh correlated ProviderInputState result only.
 * hostPublicId/clientId are caller-bound ownership claims, not wire security.
 */
export interface ProviderInputFence {
  hostPublicId: NativeUuid;
  clientId: NativeUuid;
  taskId: NativeUuid;
  taskRevision: number;
  actionEpoch: number;
  agentSessionId: NativeUuid;
  runtimeGeneration: number;
  agentLifecycle: AgentLifecycle;
  providerKind: string | null;
  providerSessionId: string | null;
  currentTurn: NativeUuid | null;
  openQuestion: NativeUuid | null;
  openApproval: NativeUuid | null;
  pendingWaitCommandIds: NativeUuid[];
}

export interface ProviderInputStateView {
  taskId: NativeUuid;
  taskRevision: number;
  actionEpoch: number;
  agentSessionId: NativeUuid | null;
  resourceId: NativeUuid | null;
  runtimeGeneration: number | null;
  agentLifecycle: AgentLifecycle | null;
  providerKind: string | null;
  providerSessionId: string | null;
  currentTurn: NativeUuid | null;
  openQuestion: NativeUuid | null;
  openApproval: NativeUuid | null;
  pendingWaitCommandIds: NativeUuid[];
  /** Null when no agent is bound — thin metadata only; not SendNow authority. */
  fence: ProviderInputFence | null;
}

export interface NativeConfigProjectView {
  configId: string;
  label: string;
  rootConfigured: boolean;
  workspaceId: NativeUuid | null;
  folders: Array<{ configId: string; label: string; serverCount: number }>;
}

export interface NativeConfigSnapshotView {
  revision: number;
  projects: NativeConfigProjectView[];
  providers: Array<{ provider: "claude" | "codex"; commandConfigured: boolean }>;
}

export interface TaskSnapshotItemView {
  taskId: NativeUuid;
  revision: number;
  actionEpoch: number;
  primaryAgentId: NativeUuid | null;
  connectivity: string | null;
  attention: string | null;
  activity: string | null;
  reviewReadiness: string | null;
  title: string | null;
  lifecycle: string | null;
  projectId: NativeUuid | null;
  environmentId: NativeUuid | null;
  createdAtMs: number | null;
}

export type SemanticJournalPayload =
  | { kind: "user_message"; text: string }
  | { kind: "assistant_text"; text: string }
  | { kind: "reasoning_summary"; text: string }
  | { kind: "tool_call"; tool_name: string; call_id: string }
  | { kind: "tool_result"; call_id: string; status: string }
  | { kind: "approval_request"; request_id: string; summary: string }
  | { kind: "approval_result"; request_id: string; decision: string }
  | { kind: "question"; question_id: string; prompt: string; options: string[] }
  | { kind: "plan_step"; step_id: string; title: string; status: string }
  | { kind: "usage_observation"; remaining_percent: number | null }
  | { kind: "error"; code: string; message: string }
  | { kind: "turn_state"; state: string }
  | { kind: "session_state"; state: string }
  | { kind: "artifact_reference"; label: string }
  | {
      kind: "unknown";
      provider: string;
      source_type: string;
      schema_version: number;
      diagnostic_ref: string;
    };

export interface SemanticJournalFact {
  id: NativeUuid;
  sequence: number;
  occurredAtMs: number | null;
  provider: string;
  schemaVersion: number;
  kind: string;
  visibility: string;
  privacyClass: "local_only" | "shareable";
  redacted: boolean;
  payload: SemanticJournalPayload;
}

export interface SemanticJournalPage {
  afterSequence: number;
  throughSequence: number;
  highWater: number;
  oldestSequence: number;
  cursorRolledOver: boolean;
  encodedBytes: number;
  /** Null marks the final page (`Option<u64>` on the wire). */
  nextSequence: number | null;
  facts: SemanticJournalFact[];
}

export type QueryReplyOutcome =
  | { kind: "ok"; result: Record<string, unknown> }
  | { kind: "err"; error: QueryErrorView };

export interface QueryErrorView {
  code:
    | "not_found"
    | "unauthorized"
    | "invalid_request"
    | "conflict"
    | "unsupported_capability"
    | "replay_unavailable"
    | "unavailable";
  oldestSequence?: number;
  newestSequence?: number;
  reason?: string;
}

export interface DecodedQueryReply {
  requestId: NativeUuid;
  outcome: QueryReplyOutcome;
}

export interface SnapshotPageView {
  snapshotId: NativeUuid;
  throughSequence: number;
  section: string;
  afterItem: unknown;
  items: SnapshotListItem[];
  encodedBytes: number;
  nextCursor: unknown;
}

export interface SnapshotListItem {
  kind: "task";
  taskId: NativeUuid;
  revision: number;
  actionEpoch: number;
  primaryAgentId: NativeUuid | null;
  title: string | null;
  lifecycle: string | null;
  projectId: NativeUuid | null;
  environmentId: NativeUuid | null;
  createdAtMs: number | null;
  connectivity: string | null;
  attention: string | null;
  activity: string | null;
}

export type CommandReceiptView =
  | {
      kind: "accepted";
      commandId: NativeUuid;
      operationId: NativeUuid;
      taskRevision: number | null;
      eventIds: NativeUuid[];
    }
  | {
      kind: "rejected";
      commandId: NativeUuid;
      code: string;
      currentRevision: number | null;
    };

export interface EventReplayPageView {
  subscriptionId: NativeUuid;
  afterSequence: number;
  throughSequence: number;
  /** Last durable sequence carried by this page; the next page must bind it. */
  lastSequence: number;
  nextCursor: Uint8Array | null;
  eventCount: number;
  /** Bounded task ids referenced by events when present; never invented. */
  affectedTaskIds: NativeUuid[];
}

export interface ConversationSubscriptionView {
  subscriptionId: NativeUuid;
  page: SemanticJournalPage;
}

export interface ConversationDirtyNotice {
  subscriptionId: NativeUuid;
  taskId: NativeUuid;
  highWater: number;
  requiredCapabilities: number;
}

export interface ConnectBinaryMarker {
  $connectBinary: string;
}

function rejected(message: string): never {
  throw new NativeProtocolError(message);
}

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function requireUuid(value: unknown, label: string): NativeUuid {
  const id = protocolUuid(value);
  if (!id) rejected(`invalid ${label}`);
  return id;
}

function optionalUuid(value: unknown, label: string): NativeUuid | null {
  if (value === null) return null;
  if (value === undefined) rejected(`missing ${label}`);
  return requireUuid(value, label);
}

function requireSafeUnsigned(value: unknown, label: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    rejected(`invalid ${label}`);
  }
  return value;
}

function requireSafePositive(value: unknown, label: string): number {
  const n = requireSafeUnsigned(value, label);
  if (n === 0) rejected(`invalid ${label}`);
  return n;
}

function requireSafeI64(value: unknown, label: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value)) {
    rejected(`invalid ${label}`);
  }
  return value;
}

function utf8ByteLength(text: string): number {
  return new TextEncoder().encode(text).byteLength;
}

function boundedText(value: unknown, label: string, maxBytes: number): string {
  if (typeof value !== "string") rejected(`invalid ${label}`);
  if (utf8ByteLength(value) > maxBytes) rejected(`${label} exceeds bound`);
  return value;
}

function requireAuthority(input: NativeAuthority): NativeAuthority {
  return {
    hostPublicId: requireUuid(input.hostPublicId, "hostPublicId"),
    clientId: requireUuid(input.clientId, "clientId"),
    requestId: requireUuid(input.requestId, "requestId"),
  };
}

function requireExactKeys(
  value: Record<string, unknown>,
  keys: readonly string[],
  label: string,
): void {
  const actual = Object.keys(value);
  if (actual.length !== keys.length || !keys.every((key) => key in value)) {
    rejected(`${label} missing or unexpected fields`);
  }
}

function queryPayloadRequest(
  authority: NativeAuthority,
  taskId: NativeUuid | null,
  query: Record<string, unknown>,
): ConnectPayloadRequest {
  return {
    payloadKind: NATIVE_QUERY_KIND,
    payload: {
      request_id: authority.requestId,
      client_id: authority.clientId,
      task_id: taskId,
      query,
    },
    requestId: authority.requestId,
    operationId: null,
    privacyClass: "local_only",
    payloadVersion: 1,
  };
}

function bytesToBase64(bytes: Uint8Array): string {
  let binary = "";
  for (let offset = 0; offset < bytes.byteLength; offset += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
  }
  return btoa(binary);
}

function base64ToBytes(value: string): Uint8Array {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

/** JSON ABI marker for MessagePack BIN; root WASM encoder expands it. */
export function connectBinaryMarker(bytes: Uint8Array): ConnectBinaryMarker {
  if (bytes.byteLength === 0 || bytes.byteLength > MAX_RESUME_CURSOR_BYTES) {
    rejected("resume cursor byte length rejected");
  }
  return { $connectBinary: bytesToBase64(bytes) };
}

/** Return a copy of caller resume-cursor bytes from a marker or raw bytes. */
export function copyResumeCursorBytes(
  value: ConnectBinaryMarker | Uint8Array | readonly number[],
): Uint8Array {
  if (value instanceof Uint8Array) {
    if (value.byteLength === 0 || value.byteLength > MAX_RESUME_CURSOR_BYTES) {
      rejected("resume cursor byte length rejected");
    }
    return value.slice();
  }
  // The WASM MessagePack decoder represents BIN values as bounded JSON byte
  // arrays. Markers are only the browser-to-WASM encoding form.
  if (Array.isArray(value)) {
    if (value.length === 0 || value.length > MAX_RESUME_CURSOR_BYTES) {
      rejected("resume cursor byte length rejected");
    }
    const bytes = new Uint8Array(value.length);
    for (let index = 0; index < value.length; index += 1) {
      const byte = value[index];
      if (!Number.isInteger(byte) || byte < 0 || byte > 255) {
        rejected("resume cursor byte rejected");
      }
      bytes[index] = byte;
    }
    return bytes;
  }
  const map = record(value) ?? rejected("resume cursor marker rejected");
  if (!("$connectBinary" in map) || Object.keys(map).length !== 1) {
    rejected("resume cursor marker rejected");
  }
  const encoded = map.$connectBinary;
  if (typeof encoded !== "string" || encoded.length === 0) {
    rejected("resume cursor marker rejected");
  }
  // Bound the base64 string before atob allocates decoded output.
  if (encoded.length > MAX_RESUME_CURSOR_BASE64_CHARS) {
    rejected("resume cursor base64 length rejected");
  }
  if (!/^[A-Za-z0-9+/]+={0,2}$/.test(encoded) || encoded.length % 4 !== 0) {
    rejected("resume cursor marker rejected");
  }
  let bytes: Uint8Array;
  try {
    bytes = base64ToBytes(encoded);
  } catch {
    return rejected("resume cursor marker rejected");
  }
  if (bytes.byteLength === 0 || bytes.byteLength > MAX_RESUME_CURSOR_BYTES) {
    rejected("resume cursor byte length rejected");
  }
  // Canonical padded base64 round-trip (reject whitespace / non-canonical forms).
  if (bytesToBase64(bytes) !== encoded) {
    rejected("resume cursor base64 is not canonical");
  }
  return bytes.slice();
}

function encodeResumeCursor(bytes: Uint8Array): ConnectBinaryMarker {
  return connectBinaryMarker(bytes);
}

export function requiredCapabilitiesForQuery(
  query: Record<string, unknown>,
): bigint {
  const keys = Object.keys(query);
  if (keys.length !== 1) rejected("query must contain exactly one variant");
  const variant = keys[0]!;
  switch (variant) {
    case "command_receipt_status":
    case "task_snapshot":
      return 0n;
    case "snapshot_page":
    case "release_snapshot":
      return CAPABILITY_PAGED_SNAPSHOTS;
    case "open_event_replay":
    case "continue_event_replay":
    case "release_event_replay":
      // Domain event subscription — EventReplay only (not SemanticConversation).
      return CAPABILITY_EVENT_REPLAY;
    case "task_cockpit": {
      const cockpit = query.task_cockpit;
      if (cockpit === "terminal") return CAPABILITY_TASK_COCKPIT;
      if (cockpit === "config_snapshot") return CAPABILITY_TASK_COCKPIT;
      if (cockpit === "provider_input_state") {
        return CAPABILITY_TASK_COCKPIT | CAPABILITY_PROVIDER_INPUT;
      }
      const body = record(cockpit) ?? rejected("unknown task_cockpit query");
      const cockpitKeys = Object.keys(body);
      if (cockpitKeys.length !== 1) rejected("unknown task_cockpit query");
      if (cockpitKeys[0] === "conversation") {
        return CAPABILITY_TASK_COCKPIT | CAPABILITY_SEMANTIC_CONVERSATION;
      }
      if (
        cockpitKeys[0] === "open_conversation_subscription" ||
        cockpitKeys[0] === "release_conversation_subscription"
      ) {
        return CAPABILITY_TASK_COCKPIT | CAPABILITY_SEMANTIC_CONVERSATION;
      }
      rejected("unknown task_cockpit query");
    }
    default:
      rejected(`unknown query variant ${variant}`);
  }
}

/**
 * Capability intersection for the phone-supported command subset only.
 * Unit lifecycle variants are bare strings on the wire (`"settle_task"`), not
 * objects — matching Rust externally-tagged unit enum serde.
 */
export function requiredCapabilitiesForCommand(command: unknown): bigint {
  if (typeof command === "string") {
    switch (command) {
      case "settle_task":
      case "reopen_task":
      case "begin_close_task":
      case "delete_task":
        return 0n;
      default:
        rejected(`unknown command variant ${command}`);
    }
  }
  const body = record(command) ?? rejected("command must be a variant");
  const keys = Object.keys(body);
  if (keys.length !== 1) rejected("command must contain exactly one variant");
  if (keys[0] === "submit_provider_input") return CAPABILITY_PROVIDER_INPUT;
  if (keys[0] === "start_provider_session") return CAPABILITY_PROVIDER_INPUT;
  if (keys[0] === "create_task_v2") return 0n;
  if (keys[0] === "rename_task") return 0n;
  rejected(`unknown command variant ${keys[0]}`);
}

/** Phone-supported unit lifecycle command strings (Rust unit enum variants). */
export const NATIVE_UNIT_TASK_COMMANDS = [
  "settle_task",
  "reopen_task",
  "begin_close_task",
  "delete_task",
] as const;

export type NativeUnitTaskCommand = (typeof NATIVE_UNIT_TASK_COMMANDS)[number];

export function isNativeUnitTaskCommand(
  value: unknown,
): value is NativeUnitTaskCommand {
  return (
    typeof value === "string" &&
    (NATIVE_UNIT_TASK_COMMANDS as readonly string[]).includes(value)
  );
}

/**
 * Validate the nested `command` field of a persisted CommandEnvelope.
 * Unit variants must remain strings; object-shaped unit forms are rejected.
 */
export function assertSupportedCommandVariant(command: unknown): void {
  if (typeof command === "string") {
    if (!isNativeUnitTaskCommand(command)) {
      rejected("command variant rejected");
    }
    return;
  }
  const body = record(command) ?? rejected("command variant rejected");
  const keys = Object.keys(body);
  if (keys.length !== 1) rejected("command variant rejected");
  const key = keys[0]!;
  if (
    key === "submit_provider_input" ||
    key === "start_provider_session" ||
    key === "rename_task" ||
    key === "create_task_v2"
  ) return;
  if (isNativeUnitTaskCommand(key)) {
    rejected("unit command variant must be a string");
  }
  rejected("command variant rejected");
}

export function assertCapabilities(granted: unknown, required: bigint): void {
  // capabilityBits is a type predicate (boolean), not the bit value. Zero granted
  // bits are valid when required is 0 — never use granted's truthiness.
  if (!capabilityBits(granted)) rejected("invalid capability bits");
  const bits = BigInt(granted);
  if (required === 0n) return;
  if ((bits & required) !== required) rejected("unsupported capability");
}

export function buildTaskSnapshotQuery(
  input: NativeAuthority & { taskId: NativeUuid },
): ConnectPayloadRequest {
  const authority = requireAuthority(input);
  const taskId = requireUuid(input.taskId, "taskId");
  return queryPayloadRequest(authority, taskId, { task_snapshot: {} });
}

/**
 * Build the read-only durable receipt lookup from the exact persisted command
 * envelope. The nested envelope is never rebuilt and its command_id is never
 * replaced; the host validates it against the authenticated client again.
 */
export function buildCommandReceiptStatusQuery(
  input: NativeAuthority & {
    taskId: NativeUuid | null;
    commandPayload: unknown;
  },
): ConnectPayloadRequest {
  const authority = requireAuthority(input);
  const taskId =
    input.taskId === null
      ? null
      : requireUuid(input.taskId, "taskId");
  const command = record(input.commandPayload) ?? rejected("command envelope rejected");
  requireExactKeys(
    command,
    [
      "command_id",
      "client_id",
      "task_id",
      "issued_at_ms",
      "expected_task_revision",
      "command",
    ],
    "command envelope",
  );
  const commandId = requireUuid(command.command_id, "command_id");
  const clientId = requireUuid(command.client_id, "client_id");
  if (clientId !== authority.clientId) rejected("command clientId mismatch");
  const commandTaskId =
    command.task_id === null
      ? null
      : requireUuid(command.task_id, "command.task_id");
  if (commandTaskId !== taskId) rejected("command taskId mismatch");
  requireSafeI64(command.issued_at_ms, "issued_at_ms");
  if (command.expected_task_revision !== null) {
    requireSafeUnsigned(command.expected_task_revision, "expected_task_revision");
  }
  assertSupportedCommandVariant(command.command);
  requiredCapabilitiesForCommand(command.command);
  // Validate the ID even though it is not otherwise used: this keeps the
  // builder's proof that the persisted command identity is the one queried.
  void commandId;
  return queryPayloadRequest(authority, taskId, {
    command_receipt_status: { command },
  });
}

export function buildTaskCockpitConversationQuery(
  input: NativeAuthority & { taskId: NativeUuid; afterSequence: number },
): ConnectPayloadRequest {
  const authority = requireAuthority(input);
  const taskId = requireUuid(input.taskId, "taskId");
  const afterSequence = requireSafeUnsigned(input.afterSequence, "afterSequence");
  return queryPayloadRequest(authority, taskId, {
    task_cockpit: { conversation: { after_sequence: afterSequence } },
  });
}

export function buildTaskCockpitTerminalQuery(
  input: NativeAuthority & { taskId: NativeUuid },
): ConnectPayloadRequest {
  const authority = requireAuthority(input);
  const taskId = requireUuid(input.taskId, "taskId");
  return queryPayloadRequest(authority, taskId, { task_cockpit: "terminal" });
}

export function buildTaskCockpitConfigSnapshotQuery(
  input: NativeAuthority,
): ConnectPayloadRequest {
  const authority = requireAuthority(input);
  return queryPayloadRequest(authority, null, { task_cockpit: "config_snapshot" });
}

export function buildProviderInputStateQuery(
  input: NativeAuthority & { taskId: NativeUuid },
): ConnectPayloadRequest {
  const authority = requireAuthority(input);
  const taskId = requireUuid(input.taskId, "taskId");
  return queryPayloadRequest(authority, taskId, {
    task_cockpit: "provider_input_state",
  });
}

export function buildOpenEventReplayQuery(
  input: NativeAuthority & { taskId: NativeUuid; afterSequence: number },
): ConnectPayloadRequest {
  const authority = requireAuthority(input);
  const taskId = requireUuid(input.taskId, "taskId");
  const afterSequence = requireSafeUnsigned(input.afterSequence, "afterSequence");
  return queryPayloadRequest(authority, taskId, {
    open_event_replay: { after_sequence: afterSequence },
  });
}

export function buildOpenTasksSnapshotPageQuery(
  input: NativeAuthority & { taskId?: NativeUuid | null },
): ConnectPayloadRequest {
  const authority = requireAuthority(input);
  const taskId =
    input.taskId === undefined || input.taskId === null
      ? null
      : requireUuid(input.taskId, "taskId");
  return queryPayloadRequest(authority, taskId, {
    snapshot_page: {
      section: "tasks",
      snapshot_id: null,
      resume_cursor: null,
    },
  });
}

export function buildResumeTasksSnapshotPageQuery(
  input: NativeAuthority & {
    taskId?: NativeUuid | null;
    snapshotId: NativeUuid;
    resumeCursor: Uint8Array;
  },
): ConnectPayloadRequest {
  const authority = requireAuthority(input);
  const taskId =
    input.taskId === undefined || input.taskId === null
      ? null
      : requireUuid(input.taskId, "taskId");
  const snapshotId = requireUuid(input.snapshotId, "snapshotId");
  if (!(input.resumeCursor instanceof Uint8Array)) {
    rejected("resume cursor must be Uint8Array");
  }
  const resumeCursor = encodeResumeCursor(input.resumeCursor);
  return queryPayloadRequest(authority, taskId, {
    snapshot_page: {
      section: "tasks",
      snapshot_id: snapshotId,
      resume_cursor: resumeCursor,
    },
  });
}

export function buildReleaseSnapshotQuery(
  input: NativeAuthority & { snapshotId: NativeUuid },
): ConnectPayloadRequest {
  const authority = requireAuthority(input);
  const snapshotId = requireUuid(input.snapshotId, "snapshotId");
  return queryPayloadRequest(authority, null, {
    release_snapshot: { snapshot_id: snapshotId },
  });
}

/** Global OpenEventReplay — task_id absent on the envelope. */
export function buildGlobalOpenEventReplayQuery(
  input: NativeAuthority & { afterSequence: number },
): ConnectPayloadRequest {
  const authority = requireAuthority(input);
  const afterSequence = requireSafeUnsigned(input.afterSequence, "afterSequence");
  return queryPayloadRequest(authority, null, {
    open_event_replay: { after_sequence: afterSequence },
  });
}

export function buildContinueEventReplayQuery(
  input: NativeAuthority & {
    subscriptionId: NativeUuid;
    resumeCursor: Uint8Array;
  },
): ConnectPayloadRequest {
  const authority = requireAuthority(input);
  const subscriptionId = requireUuid(input.subscriptionId, "subscriptionId");
  if (!(input.resumeCursor instanceof Uint8Array)) {
    rejected("resume cursor must be Uint8Array");
  }
  return queryPayloadRequest(authority, null, {
    continue_event_replay: {
      subscription_id: subscriptionId,
      resume_cursor: encodeResumeCursor(input.resumeCursor),
    },
  });
}

export function buildReleaseEventReplayQuery(
  input: NativeAuthority & { subscriptionId: NativeUuid },
): ConnectPayloadRequest {
  const authority = requireAuthority(input);
  const subscriptionId = requireUuid(input.subscriptionId, "subscriptionId");
  return queryPayloadRequest(authority, null, {
    release_event_replay: { subscription_id: subscriptionId },
  });
}

export function buildOpenConversationSubscriptionQuery(
  input: NativeAuthority & { taskId: NativeUuid; afterSequence: number },
): ConnectPayloadRequest {
  const authority = requireAuthority(input);
  const taskId = requireUuid(input.taskId, "taskId");
  const afterSequence = requireSafeUnsigned(input.afterSequence, "afterSequence");
  return queryPayloadRequest(authority, taskId, {
    task_cockpit: {
      open_conversation_subscription: { after_sequence: afterSequence },
    },
  });
}

export function buildReleaseConversationSubscriptionQuery(
  input: NativeAuthority & { taskId: NativeUuid; subscriptionId: NativeUuid },
): ConnectPayloadRequest {
  const authority = requireAuthority(input);
  const taskId = requireUuid(input.taskId, "taskId");
  const subscriptionId = requireUuid(input.subscriptionId, "subscriptionId");
  return queryPayloadRequest(authority, taskId, {
    task_cockpit: {
      release_conversation_subscription: { subscription_id: subscriptionId },
    },
  });
}

export function firstTurnIdFromCommandId(commandId: NativeUuid): NativeUuid {
  return requireUuid(commandId, "commandId");
}

/**
 * Build Command::SubmitProviderInput SendNow from a fresh ProviderInputFence.
 * Images are disallowed: phone cannot supply absolute host paths.
 */
export const NATIVE_TERMINAL_KEYS = {
  enter: "\r", escape: "\u001b", up: "\u001b[A", down: "\u001b[B", interrupt: "\u0003",
} as const;
export type NativeTerminalKey = keyof typeof NATIVE_TERMINAL_KEYS;

export function isTerminalInputCommand(payload: unknown): boolean {
  const command = record(
    typeof record(payload)?.command === "object"
      ? record(payload)?.command
      : null,
  );
  const submit = record(command?.submit_provider_input);
  return record(submit?.action)?.terminal_input !== undefined;
}

/** Only accepted SendNow text may clear a matching composer draft. */
export function isProviderSendNowCommand(payload: unknown): boolean {
  const command = record(
    typeof record(payload)?.command === "object"
      ? record(payload)?.command
      : null,
  );
  const submit = record(command?.submit_provider_input);
  return record(submit?.action)?.send_now !== undefined;
}

export function isMetadataTaskCommand(payload: unknown): boolean {
  const command = record(payload)?.command;
  if (isNativeUnitTaskCommand(command)) return true;
  const body = record(command);
  return body !== null && Object.keys(body).length === 1 && "rename_task" in body;
}

export function isTaskCreateV2Command(payload: unknown): boolean {
  const command = record(payload)?.command;
  const body = record(command);
  return body !== null && Object.keys(body).length === 1 && "create_task_v2" in body;
}

/** Max UTF-8 bytes for RenameTask titles on the phone path. */
export const MAX_TASK_TITLE_BYTES = 4096;

export interface NativeTaskCommandInput {
  authority: NativeAuthority;
  commandId: NativeUuid;
  taskId: NativeUuid;
  issuedAtMs: number;
  /** Exact current TaskSnapshot revision; never a guessed or stale value. */
  expectedTaskRevision: number;
}

export interface NativeCreateTaskV2Input {
  authority: NativeAuthority;
  commandId: NativeUuid;
  taskId: NativeUuid;
  environmentId: NativeUuid;
  projectId: NativeUuid;
  provider: NativeProviderKind;
  title: string;
  issuedAtMs: number;
  deferPrimaryProviderStart?: boolean;
}

export function buildCreateTaskV2Command(
  input: NativeCreateTaskV2Input,
): ConnectPayloadRequest {
  const authority = requireAuthority(input.authority);
  const commandId = requireUuid(input.commandId, "commandId");
  const taskId = requireUuid(input.taskId, "taskId");
  const environmentId = requireUuid(input.environmentId, "environmentId");
  const projectId = requireUuid(input.projectId, "projectId");
  const issuedAtMs = requireSafeI64(input.issuedAtMs, "issuedAtMs");
  const title = boundedText(input.title, "title", MAX_TASK_TITLE_BYTES).trim();
  if (!title) rejected("title must be non-empty");
  if (input.provider !== "claude" && input.provider !== "codex") {
    rejected("provider rejected");
  }
  const command = { create_task_v2: {
    id: taskId,
    environment_id: environmentId,
    title,
    description: null,
    project_id: projectId,
    workspace: { choice: "main", path: null, branch: null, external_confirmed: false },
    primary_provider: input.provider,
    defer_primary_provider_start: input.deferPrimaryProviderStart === true,
    assignment: "local_owner",
    created_at_ms: issuedAtMs,
    connectivity: "connected",
    attention: "none",
    activity: "idle",
    review_readiness: "not_ready",
  } };
  requiredCapabilitiesForCommand(command);
  return {
    payloadKind: NATIVE_COMMAND_KIND,
    payload: {
      command_id: commandId,
      client_id: authority.clientId,
      task_id: null,
      issued_at_ms: issuedAtMs,
      expected_task_revision: null,
      command,
    },
    requestId: authority.requestId,
    operationId: null,
    privacyClass: "local_only",
    payloadVersion: 1,
  };
}

export interface NativeStartProviderSessionInput extends NativeTaskCommandInput {
  agentSessionId: NativeUuid;
  resourceId: NativeUuid;
  provider: NativeProviderKind;
  actionEpoch: number;
  launchOptions: NativeProviderLaunchOptions;
}

export function buildStartProviderSessionCommand(
  input: NativeStartProviderSessionInput,
): ConnectPayloadRequest {
  const authority = requireAuthority(input.authority);
  const commandId = requireUuid(input.commandId, "commandId");
  const taskId = requireUuid(input.taskId, "taskId");
  const issuedAtMs = requireSafeI64(input.issuedAtMs, "issuedAtMs");
  const expectedTaskRevision = requireSafePositive(
    input.expectedTaskRevision,
    "expectedTaskRevision",
  );
  const actionEpoch = requireSafeUnsigned(input.actionEpoch, "actionEpoch");
  validateProviderLaunchOptions(input.provider, input.launchOptions);
  const command = {
    start_provider_session: {
      task_id: taskId,
      agent_session_id: requireUuid(input.agentSessionId, "agentSessionId"),
      resource_id: requireUuid(input.resourceId, "resourceId"),
      provider_kind: input.provider,
      mode: "new_conversation",
      launch_options: {
        model: input.launchOptions.model,
        reasoning_effort: input.launchOptions.reasoningEffort,
        access: input.launchOptions.access,
      },
      expected_task_revision: expectedTaskRevision,
      expected_action_epoch: actionEpoch,
    },
  };
  requiredCapabilitiesForCommand(command);
  return {
    payloadKind: NATIVE_COMMAND_KIND,
    payload: {
      command_id: commandId,
      client_id: authority.clientId,
      task_id: taskId,
      issued_at_ms: issuedAtMs,
      expected_task_revision: expectedTaskRevision,
      command,
    },
    requestId: authority.requestId,
    operationId: null,
    privacyClass: "local_only",
    payloadVersion: 1,
  };
}

function validateProviderLaunchOptions(
  provider: NativeProviderKind,
  options: NativeProviderLaunchOptions,
): void {
  const models: NativeProviderModel[] = provider === "codex"
    ? ["provider_default", "codex_sol", "codex_terra", "codex_luna"]
    : ["provider_default", "claude_opus", "claude_sonnet", "claude_haiku"];
  if (!models.includes(options.model)) {
    rejected("provider launch model rejected");
  }
  const efforts: NativeReasoningEffort[] = provider === "codex"
    ? ["provider_default", "low", "medium", "high", "extra_high", "max", "ultra"]
    : ["provider_default", "low", "medium", "high"];
  if (!efforts.includes(options.reasoningEffort)) {
    rejected("provider launch reasoning effort rejected");
  }
  if (!["full_access", "workspace_write", "read_only"].includes(options.access)) {
    rejected("provider launch access rejected");
  }
}

function buildTaskCommandEnvelope(
  input: NativeTaskCommandInput,
  command: string | Record<string, unknown>,
): ConnectPayloadRequest {
  const authority = requireAuthority(input.authority);
  const commandId = requireUuid(input.commandId, "commandId");
  const taskId = requireUuid(input.taskId, "taskId");
  const issuedAtMs = requireSafeI64(input.issuedAtMs, "issuedAtMs");
  const expectedTaskRevision = requireSafePositive(
    input.expectedTaskRevision,
    "expectedTaskRevision",
  );
  assertSupportedCommandVariant(command);
  return {
    payloadKind: NATIVE_COMMAND_KIND,
    payload: {
      command_id: commandId,
      client_id: authority.clientId,
      task_id: taskId,
      issued_at_ms: issuedAtMs,
      expected_task_revision: expectedTaskRevision,
      command,
    },
    requestId: authority.requestId,
    operationId: null,
    privacyClass: "local_only",
    payloadVersion: 1,
  };
}

export function buildSettleTaskCommand(
  input: NativeTaskCommandInput,
): ConnectPayloadRequest {
  return buildTaskCommandEnvelope(input, "settle_task");
}

export function buildReopenTaskCommand(
  input: NativeTaskCommandInput,
): ConnectPayloadRequest {
  return buildTaskCommandEnvelope(input, "reopen_task");
}

export function buildBeginCloseTaskCommand(
  input: NativeTaskCommandInput,
): ConnectPayloadRequest {
  return buildTaskCommandEnvelope(input, "begin_close_task");
}

export function buildDeleteTaskCommand(
  input: NativeTaskCommandInput,
): ConnectPayloadRequest {
  return buildTaskCommandEnvelope(input, "delete_task");
}

export function buildRenameTaskCommand(
  input: NativeTaskCommandInput & { title: string },
): ConnectPayloadRequest {
  const title = boundedText(input.title, "title", MAX_TASK_TITLE_BYTES);
  if (title.trim().length === 0) rejected("title must be non-empty");
  return buildTaskCommandEnvelope(input, { rename_task: { title } });
}

export interface NativeProviderSubmit {
  authority: NativeAuthority;
  commandId: NativeUuid;
  text: string;
  issuedAtMs: number;
  fence: ProviderInputFence;
  wait?: boolean;
}

export function buildSubmitProviderInputSendNow(input: NativeProviderSubmit): ConnectPayloadRequest {
  return buildProviderInput(input, "send_now");
}

export function buildSubmitProviderAnswerQuestion(
  input: NativeProviderSubmit,
): ConnectPayloadRequest {
  return buildProviderInput(input, "answer_question");
}

export function buildSubmitProviderTerminalKey(
  input: Omit<NativeProviderSubmit, "text" | "wait"> & { key: NativeTerminalKey },
): ConnectPayloadRequest {
  const text = NATIVE_TERMINAL_KEYS[input.key];
  if (typeof text !== "string") rejected("unknown terminal key");
  return buildProviderInput({ ...input, text }, "terminal_input");
}

function buildProviderInput(
  input: NativeProviderSubmit,
  actionKind: "send_now" | "terminal_input" | "answer_question",
): ConnectPayloadRequest {
  const authority = requireAuthority(input.authority);
  const commandId = requireUuid(input.commandId, "commandId");
  const issuedAtMs = requireSafeI64(input.issuedAtMs, "issuedAtMs");
  const text = boundedText(input.text, "text", MAX_PROVIDER_INPUT_TEXT_BYTES);
  if (text.length === 0) rejected("provider input text must be non-empty");

  const fence = input.fence;
  if (fence.hostPublicId !== authority.hostPublicId) {
    rejected("hostPublicId mismatch");
  }
  if (fence.clientId !== authority.clientId) {
    rejected("clientId mismatch");
  }
  if (fence.agentLifecycle !== "open") rejected("non-open agent rejects provider input");
  const answering = actionKind === "answer_question";
  if (answering) {
    if (fence.openQuestion === null) rejected("answer requires an open question");
    if (fence.currentTurn === null) rejected("answer requires a current turn");
    if (fence.openApproval !== null) rejected("approval blocker rejects question answer");
  } else {
    if (fence.openQuestion !== null) rejected("open question blocker rejects SendNow");
    if (fence.openApproval !== null) rejected("open approval blocker rejects SendNow");
    if (fence.pendingWaitCommandIds.length > 0) {
      rejected("pending wait blocker rejects SendNow");
    }
  }

  const taskId = requireUuid(fence.taskId, "fence.taskId");
  const expectedTaskRevision = requireSafePositive(
    fence.taskRevision,
    "fence.taskRevision",
  );
  const agentSessionId = requireUuid(fence.agentSessionId, "fence.agentSessionId");
  const runtimeGeneration = requireSafePositive(
    fence.runtimeGeneration,
    "fence.runtimeGeneration",
  );
  const actionEpoch = requireSafeUnsigned(fence.actionEpoch, "fence.actionEpoch");
  const turnId =
    fence.currentTurn === null
      ? firstTurnIdFromCommandId(commandId)
      : requireUuid(fence.currentTurn, "fence.currentTurn");

  return {
    payloadKind: NATIVE_COMMAND_KIND,
    payload: {
      command_id: commandId,
      client_id: authority.clientId,
      task_id: taskId,
      issued_at_ms: issuedAtMs,
      expected_task_revision: expectedTaskRevision,
      command: {
        submit_provider_input: {
          agent_session_id: agentSessionId,
          runtime_generation: runtimeGeneration,
          turn_id: turnId,
          action_epoch: actionEpoch,
          question_id: answering ? requireUuid(fence.openQuestion, "fence.openQuestion") : null,
          approval_id: null,
          action: actionKind === "terminal_input"
            ? { terminal_input: { text } }
            : answering
              ? { answer_question: {
                  question_id: requireUuid(fence.openQuestion, "fence.openQuestion"),
                  answer: text,
                } }
              : { send_now: { text, wait: input.wait === true } },
        },
      },
    },
    requestId: authority.requestId,
    operationId: null,
    privacyClass: "local_only",
    payloadVersion: 1,
  };
}

export function decodeQueryReply(
  payload: unknown,
  expectedRequestId: NativeUuid,
): DecodedQueryReply {
  const expected = requireUuid(expectedRequestId, "expectedRequestId");
  const root = record(payload) ?? rejected("QueryReply rejected");
  if (!("request_id" in root) || !("outcome" in root)) {
    rejected("QueryReply rejected");
  }
  const requestId = requireUuid(root.request_id, "request_id");
  if (requestId !== expected) rejected("QueryReply request correlation rejected");

  const outcomeMap = record(root.outcome) ?? rejected("QueryReply outcome rejected");
  const keys = Object.keys(outcomeMap);
  if (keys.length !== 1) rejected("QueryReply outcome rejected");
  if (keys[0] === "ok") {
    const result = record(outcomeMap.ok) ?? rejected("QueryReply ok rejected");
    return { requestId, outcome: { kind: "ok", result } };
  }
  if (keys[0] === "err") {
    return {
      requestId,
      outcome: { kind: "err", error: decodeQueryError(outcomeMap.err) },
    };
  }
  rejected("QueryReply outcome rejected");
}

function decodeQueryError(value: unknown): QueryErrorView {
  if (typeof value === "string") {
    switch (value) {
      case "not_found":
      case "unauthorized":
      case "invalid_request":
      case "conflict":
      case "unsupported_capability":
        return { code: value };
      default:
        rejected("QueryError rejected");
    }
  }
  const map = record(value) ?? rejected("QueryError rejected");
  const keys = Object.keys(map);
  if (keys.length !== 1) rejected("QueryError rejected");
  if (keys[0] === "replay_unavailable") {
    const body =
      record(map.replay_unavailable) ?? rejected("QueryError rejected");
    return {
      code: "replay_unavailable",
      oldestSequence: requireSafeUnsigned(body.oldest_sequence, "oldest_sequence"),
      newestSequence: requireSafeUnsigned(body.newest_sequence, "newest_sequence"),
    };
  }
  if (keys[0] === "unavailable") {
    const body = record(map.unavailable) ?? rejected("QueryError rejected");
    return {
      code: "unavailable",
      reason: boundedText(body.reason, "unavailable reason", MAX_SEMANTIC_TEXT_BYTES),
    };
  }
  rejected("QueryError rejected");
}

/**
 * Thin TaskSnapshotItem only — list/metadata, never SendNow authority.
 */
export function decodeTaskSnapshotItem(
  snapshot: unknown,
  options: { expectedTaskId?: NativeUuid } = {},
): TaskSnapshotItemView {
  const root = record(snapshot) ?? rejected("TaskSnapshotItem rejected");
  const itemRoot = record(root.snapshot) ?? root;
  if ("agents" in itemRoot || "provider_sessions" in itemRoot) {
    rejected("TaskSnapshotItem must remain thin list metadata");
  }
  const task = record(itemRoot.task) ?? rejected("TaskSnapshotItem.task rejected");
  const taskId = requireUuid(task.id, "task.id");
  if (options.expectedTaskId !== undefined && taskId !== options.expectedTaskId) {
    rejected("foreign task id rejected");
  }
  return {
    taskId,
    revision: requireSafePositive(task.revision, "task.revision"),
    actionEpoch: requireSafeUnsigned(task.action_epoch, "task.action_epoch"),
    primaryAgentId: optionalUuid(
      itemRoot.primary_agent_id === undefined ? null : itemRoot.primary_agent_id,
      "primary_agent_id",
    ),
    connectivity:
      typeof itemRoot.connectivity === "string" ? itemRoot.connectivity : null,
    attention: typeof itemRoot.attention === "string" ? itemRoot.attention : null,
    activity: typeof itemRoot.activity === "string" ? itemRoot.activity : null,
    reviewReadiness:
      typeof itemRoot.review_readiness === "string"
        ? itemRoot.review_readiness
        : null,
    title: typeof task.title === "string" ? task.title : null,
    lifecycle: typeof task.lifecycle === "string" ? task.lifecycle : null,
    projectId:
      task.project_id === undefined || task.project_id === null
        ? null
        : requireUuid(task.project_id, "project_id"),
    environmentId:
      task.environment_id === undefined || task.environment_id === null
        ? null
        : requireUuid(task.environment_id, "environment_id"),
    createdAtMs:
      task.created_at_ms === undefined || task.created_at_ms === null
        ? null
        : requireSafeI64(task.created_at_ms, "created_at_ms"),
  };
}

export function decodeTaskSnapshotQueryResult(
  reply: DecodedQueryReply,
  expectedTaskId?: NativeUuid,
): TaskSnapshotItemView {
  if (reply.outcome.kind !== "ok") rejected("task snapshot query error");
  const body =
    record(reply.outcome.result.task_snapshot) ??
    rejected("task_snapshot result missing");
  return decodeTaskSnapshotItem(body, { expectedTaskId });
}

function decodeAgentLifecycle(value: unknown): AgentLifecycle | null {
  if (value === null) return null;
  if (value === "open" || value === "closing" || value === "closed") return value;
  rejected("agent_lifecycle rejected");
}

const PROVIDER_INPUT_STATE_KEYS = [
  "task_id",
  "task_revision",
  "action_epoch",
  "agent_session_id",
  "resource_id",
  "runtime_generation",
  "agent_lifecycle",
  "provider_kind",
  "provider_session_id",
  "current_turn",
  "open_question",
  "open_approval",
  "pending_wait_command_ids",
] as const;

/**
 * Strict flat ProviderInputState projection. All fields required (null allowed).
 * pending_wait_command_ids is host-filtered; do not invent empty blockers.
 */
export function decodeProviderInputState(
  value: unknown,
  authority: NativeAuthority,
  expectedTaskId: NativeUuid,
): ProviderInputStateView {
  const auth = requireAuthority(authority);
  const expected = requireUuid(expectedTaskId, "expectedTaskId");
  const root = record(value) ?? rejected("ProviderInputState rejected");
  requireExactKeys(root, PROVIDER_INPUT_STATE_KEYS, "ProviderInputState");

  const taskId = requireUuid(root.task_id, "task_id");
  if (taskId !== expected) rejected("foreign task id rejected");

  const taskRevision = requireSafePositive(root.task_revision, "task_revision");
  const actionEpoch = requireSafeUnsigned(root.action_epoch, "action_epoch");
  const agentSessionId = optionalUuid(root.agent_session_id, "agent_session_id");
  const resourceId = optionalUuid(root.resource_id, "resource_id");
  const runtimeGeneration =
    root.runtime_generation === null
      ? null
      : requireSafeUnsigned(root.runtime_generation, "runtime_generation");
  const agentLifecycle = decodeAgentLifecycle(root.agent_lifecycle);

  let providerKind: string | null = null;
  if (root.provider_kind !== null) {
    providerKind = boundedText(root.provider_kind, "provider_kind", 64);
  }
  const providerSessionId = decodeProviderSessionId(root.provider_session_id);

  const currentTurn = optionalUuid(root.current_turn, "current_turn");
  const openQuestion = optionalUuid(root.open_question, "open_question");
  const openApproval = optionalUuid(root.open_approval, "open_approval");

  if (!Array.isArray(root.pending_wait_command_ids)) {
    rejected("pending_wait_command_ids rejected");
  }
  if (root.pending_wait_command_ids.length > MAX_PENDING_WAIT_COMMAND_IDS) {
    rejected("pending_wait_command_ids exceed bound");
  }
  const pendingWaitCommandIds: NativeUuid[] = [];
  const seenWaits = new Set<string>();
  for (let index = 0; index < root.pending_wait_command_ids.length; index += 1) {
    const id = requireUuid(
      root.pending_wait_command_ids[index],
      `pending_wait_command_ids[${index}]`,
    );
    if (seenWaits.has(id)) rejected("duplicate pending wait command id");
    seenWaits.add(id);
    pendingWaitCommandIds.push(id);
  }

  if (agentSessionId === null) {
    if (
      runtimeGeneration !== null ||
      resourceId !== null ||
      agentLifecycle !== null ||
      providerKind !== null ||
      providerSessionId !== null ||
      currentTurn !== null ||
      openQuestion !== null ||
      openApproval !== null ||
      pendingWaitCommandIds.length > 0
    ) {
      rejected("no-agent ProviderInputState must clear agent fence fields");
    }
    return {
      taskId,
      taskRevision,
      actionEpoch,
      agentSessionId: null,
      resourceId: null,
      runtimeGeneration: null,
      agentLifecycle: null,
      providerKind: null,
      providerSessionId: null,
      currentTurn: null,
      openQuestion: null,
      openApproval: null,
      pendingWaitCommandIds: [],
      fence: null,
    };
  }

  if (runtimeGeneration === null || agentLifecycle === null) {
    rejected("agent fence fields incomplete");
  }

  return {
    taskId,
    taskRevision,
    actionEpoch,
    agentSessionId,
    resourceId,
    runtimeGeneration,
    agentLifecycle,
    providerKind,
    providerSessionId,
    currentTurn,
    openQuestion,
    openApproval,
    pendingWaitCommandIds,
    fence: {
      hostPublicId: auth.hostPublicId,
      clientId: auth.clientId,
      taskId,
      taskRevision,
      actionEpoch,
      agentSessionId,
      runtimeGeneration,
      agentLifecycle,
      providerKind,
      providerSessionId,
      currentTurn,
      openQuestion,
      openApproval,
      pendingWaitCommandIds,
    },
  };
}

function decodeProviderSessionId(value: unknown): string | null {
  if (value === null) return null;
  if (typeof value !== "string" || value.length === 0) {
    rejected("provider_session_id rejected");
  }
  if (value.trim() !== value) rejected("provider_session_id rejected");
  if (utf8ByteLength(value) > MAX_PROVIDER_SESSION_ID_BYTES) {
    rejected("provider_session_id exceeds bound");
  }
  for (const character of value) {
    if (isUnsafeProviderSessionCharacter(character)) {
      rejected("provider_session_id rejected");
    }
  }
  return value;
}

function isUnsafeProviderSessionCharacter(character: string): boolean {
  const code = character.codePointAt(0);
  if (code === undefined) return true;
  // Unicode Cc (matches char::is_control) plus agent.rs format/bidi rejects.
  if ((code >= 0x00 && code <= 0x1f) || (code >= 0x7f && code <= 0x9f)) {
    return true;
  }
  return (
    code === 0x200b ||
    code === 0x200c ||
    code === 0x200d ||
    code === 0x200e ||
    code === 0x200f ||
    (code >= 0x202a && code <= 0x202e) ||
    code === 0x2060 ||
    (code >= 0x2061 && code <= 0x2064) ||
    (code >= 0x2066 && code <= 0x2069) ||
    code === 0xfeff
  );
}

export function decodeProviderInputStateQueryResult(
  reply: DecodedQueryReply,
  authority: NativeAuthority,
  expectedTaskId: NativeUuid,
): ProviderInputStateView {
  if (reply.outcome.kind !== "ok") rejected("provider input state query error");
  const cockpit =
    record(reply.outcome.result.task_cockpit) ??
    rejected("task_cockpit result missing");
  if (!("provider_input_state" in cockpit)) {
    rejected("provider_input_state result missing");
  }
  return decodeProviderInputState(
    cockpit.provider_input_state,
    authority,
    expectedTaskId,
  );
}

export function decodeSemanticJournalPage(value: unknown): SemanticJournalPage {
  const page = record(value) ?? rejected("SemanticJournalPage rejected");
  requireExactKeys(
    page,
    [
      "after_sequence",
      "through_sequence",
      "high_water",
      "oldest_sequence",
      "cursor_rolled_over",
      "encoded_bytes",
      "next_sequence",
      "facts",
    ],
    "SemanticJournalPage",
  );

  const afterSequence = requireSafeUnsigned(page.after_sequence, "after_sequence");
  const throughSequence = requireSafeUnsigned(
    page.through_sequence,
    "through_sequence",
  );
  const highWater = requireSafeUnsigned(page.high_water, "high_water");
  const oldestSequence = requireSafeUnsigned(
    page.oldest_sequence,
    "oldest_sequence",
  );
  if (typeof page.cursor_rolled_over !== "boolean") {
    rejected("cursor_rolled_over rejected");
  }
  const cursorRolledOver = page.cursor_rolled_over;
  const encodedBytes = requireSafeUnsigned(page.encoded_bytes, "encoded_bytes");
  if (encodedBytes > MAX_SEMANTIC_PAGE_ENCODED_BYTES) {
    rejected("SemanticJournalPage.encoded_bytes exceed bound");
  }

  // next_sequence: Option<u64> — null marks the final page.
  let nextSequence: number | null;
  if (page.next_sequence === null) {
    nextSequence = null;
  } else {
    nextSequence = requireSafeUnsigned(page.next_sequence, "next_sequence");
    if (nextSequence !== throughSequence) {
      rejected("next_sequence must equal through_sequence");
    }
  }

  // after <= through <= high always. Rollover resets after=0 (including empty).
  if (afterSequence > throughSequence || throughSequence > highWater) {
    rejected("sequence window ordering rejected");
  }

  if (!Array.isArray(page.facts)) rejected("SemanticJournalPage.facts rejected");
  if (page.facts.length > MAX_SEMANTIC_PAGE_FACTS) {
    rejected("SemanticJournalPage.facts exceed bound");
  }

  // oldest > high rejected except oldest==0 or empty page.
  if (
    oldestSequence > highWater &&
    oldestSequence !== 0 &&
    page.facts.length > 0
  ) {
    rejected("oldest_sequence above high_water");
  }

  const facts = page.facts.map((fact, index) => decodeSemanticFact(fact, index));
  const seenIds = new Set<string>();
  const seenSequences = new Set<number>();
  let previousSequence = afterSequence;
  for (let index = 0; index < facts.length; index += 1) {
    const fact = facts[index]!;
    if (fact.sequence <= afterSequence) {
      rejected("fact sequence must be after cursor");
    }
    if (fact.sequence <= previousSequence) {
      rejected("fact sequences must be strictly ascending");
    }
    if (fact.sequence > throughSequence) {
      rejected("fact sequence above through_sequence");
    }
    if (oldestSequence > 0 && fact.sequence < oldestSequence) {
      rejected("fact sequence below oldest_sequence");
    }
    if (seenIds.has(fact.id)) rejected("duplicate semantic fact id");
    if (seenSequences.has(fact.sequence)) rejected("duplicate semantic sequence");
    seenIds.add(fact.id);
    seenSequences.add(fact.sequence);
    previousSequence = fact.sequence;
  }

  if (nextSequence === null) {
    // Final page: host sets through_sequence = high_water (may be empty).
    if (throughSequence !== highWater) {
      rejected("final page through_sequence must equal high_water");
    }
  } else {
    // More pages remain: nonempty forward progress past the cursor.
    if (facts.length === 0 || throughSequence <= afterSequence) {
      rejected("intermediate page must make nonempty forward progress");
    }
    if (facts[facts.length - 1]!.sequence !== throughSequence) {
      rejected("intermediate through_sequence must equal last fact");
    }
  }

  return {
    afterSequence,
    throughSequence,
    highWater,
    oldestSequence,
    cursorRolledOver,
    encodedBytes,
    nextSequence,
    facts,
  };
}

function decodeSemanticFact(value: unknown, index: number): SemanticJournalFact {
  const fact = record(value) ?? rejected(`semantic fact ${index} rejected`);
  const id = requireUuid(fact.id, `fact[${index}].id`);
  const sequence = requireSafePositive(fact.sequence, `fact[${index}].sequence`);
  const occurredAtMs =
    fact.occurred_at_ms === undefined || fact.occurred_at_ms === null
      ? null
      : requireSafeI64(fact.occurred_at_ms, `fact[${index}].occurred_at_ms`);
  const provider = boundedText(fact.provider, `fact[${index}].provider`, 256);
  const schemaVersion = requireSafeUnsigned(
    fact.schema_version,
    `fact[${index}].schema_version`,
  );
  const kind = boundedText(fact.kind, `fact[${index}].kind`, 256);
  const visibility = boundedText(
    fact.visibility,
    `fact[${index}].visibility`,
    256,
  );
  const privacyClass = decodePrivacyClass(fact.privacy_class);
  if (typeof fact.redacted !== "boolean") {
    rejected(`fact[${index}].redacted rejected`);
  }
  return {
    id,
    sequence,
    occurredAtMs,
    provider,
    schemaVersion,
    kind,
    visibility,
    privacyClass,
    redacted: fact.redacted,
    payload: decodeSemanticPayload(fact.payload, index),
  };
}

function decodePrivacyClass(value: unknown): "local_only" | "shareable" {
  if (value === "local_only" || value === "shareable") return value;
  rejected("privacy_class rejected");
}

function decodeSemanticPayload(
  value: unknown,
  index: number,
): SemanticJournalPayload {
  const payload = record(value) ?? rejected(`fact[${index}].payload rejected`);
  const kind = payload.kind;
  if (typeof kind !== "string") rejected(`fact[${index}].payload.kind rejected`);
  switch (kind) {
    case "user_message":
    case "assistant_text":
    case "reasoning_summary":
      return {
        kind,
        text: boundedText(payload.text, `${kind}.text`, MAX_SEMANTIC_TEXT_BYTES),
      };
    case "tool_call":
      return {
        kind,
        tool_name: boundedText(payload.tool_name, "tool_name", 256),
        call_id: boundedText(payload.call_id, "call_id", 256),
      };
    case "tool_result":
      return {
        kind,
        call_id: boundedText(payload.call_id, "call_id", 256),
        // Host projections use this field for bounded command summaries and
        // unified diffs as well as short lifecycle labels.
        status: boundedText(payload.status, "status", MAX_SEMANTIC_TEXT_BYTES),
      };
    case "approval_request":
      return {
        kind,
        request_id: boundedText(payload.request_id, "request_id", 256),
        summary: boundedText(payload.summary, "summary", MAX_SEMANTIC_TEXT_BYTES),
      };
    case "approval_result":
      return {
        kind,
        request_id: boundedText(payload.request_id, "request_id", 256),
        decision: boundedText(payload.decision, "decision", 256),
      };
    case "question": {
      if (!Array.isArray(payload.options)) rejected("question.options rejected");
      if (payload.options.length > 64) rejected("question.options exceed bound");
      return {
        kind,
        question_id: boundedText(payload.question_id, "question_id", 256),
        prompt: boundedText(payload.prompt, "prompt", MAX_SEMANTIC_TEXT_BYTES),
        options: payload.options.map((option, optionIndex) =>
          boundedText(option, `option[${optionIndex}]`, MAX_SEMANTIC_TEXT_BYTES),
        ),
      };
    }
    case "plan_step":
      return {
        kind,
        step_id: boundedText(payload.step_id, "step_id", 256),
        title: boundedText(payload.title, "title", MAX_SEMANTIC_TEXT_BYTES),
        status: boundedText(payload.status, "status", 256),
      };
    case "usage_observation":
      if (payload.remaining_percent === null) {
        return { kind, remaining_percent: null };
      }
      if (
        typeof payload.remaining_percent !== "number" ||
        !Number.isSafeInteger(payload.remaining_percent) ||
        payload.remaining_percent < 0 ||
        payload.remaining_percent > 100
      ) {
        rejected("usage_observation.remaining_percent rejected");
      }
      return { kind, remaining_percent: payload.remaining_percent };
    case "error":
      return {
        kind,
        code: boundedText(payload.code, "code", 256),
        message: boundedText(payload.message, "message", MAX_SEMANTIC_TEXT_BYTES),
      };
    case "turn_state":
    case "session_state":
      return { kind, state: boundedText(payload.state, "state", 256) };
    case "artifact_reference":
      return {
        kind,
        label: boundedText(payload.label, "label", MAX_SEMANTIC_TEXT_BYTES),
      };
    case "unknown":
      return {
        kind,
        provider: boundedText(payload.provider, "unknown.provider", 256),
        source_type: boundedText(payload.source_type, "source_type", 256),
        schema_version: requireSafeUnsigned(
          payload.schema_version,
          "unknown.schema_version",
        ),
        diagnostic_ref: boundedText(payload.diagnostic_ref, "diagnostic_ref", 256),
      };
    default:
      rejected(`unsupported semantic payload kind ${kind}`);
  }
}

export function decodeTaskCockpitConversationResult(
  reply: DecodedQueryReply,
): SemanticJournalPage {
  if (reply.outcome.kind !== "ok") rejected("task cockpit query error");
  const cockpit =
    record(reply.outcome.result.task_cockpit) ??
    rejected("task_cockpit result missing");
  if (!("conversation" in cockpit)) rejected("conversation result missing");
  return decodeSemanticJournalPage(cockpit.conversation);
}

export function decodeTaskCockpitConfigSnapshotResult(
  reply: DecodedQueryReply,
): NativeConfigSnapshotView {
  if (reply.outcome.kind !== "ok") rejected("config snapshot query error");
  const cockpit = record(reply.outcome.result.task_cockpit)
    ?? rejected("task_cockpit result missing");
  const config = record(cockpit.config) ?? rejected("config result missing");
  if (!Array.isArray(config.projects) || config.projects.length > 1024) {
    rejected("config projects rejected");
  }
  if (!Array.isArray(config.providers) || config.providers.length > 32) {
    rejected("config providers rejected");
  }
  const projects = config.projects.map((rawProject, projectIndex) => {
    const project = record(rawProject) ?? rejected(`config project ${projectIndex} rejected`);
    if (!Array.isArray(project.folders) || project.folders.length > 1024) {
      rejected("config folders rejected");
    }
    const workspaceId = project.workspace_id === ""
      ? null
      : requireUuid(project.workspace_id, "workspace_id");
    return {
      configId: boundedText(project.config_id, "project.config_id", 256),
      label: boundedText(project.label, "project.label", 4096),
      rootConfigured: project.root_configured === true,
      workspaceId,
      folders: project.folders.map((rawFolder, folderIndex) => {
        const folder = record(rawFolder) ?? rejected(`config folder ${folderIndex} rejected`);
        return {
          configId: boundedText(folder.config_id, "folder.config_id", 256),
          label: boundedText(folder.label, "folder.label", 4096),
          serverCount: requireSafeUnsigned(folder.server_count, "folder.server_count"),
        };
      }),
    };
  });
  const providers = config.providers.map((rawProvider, providerIndex) => {
    const provider = record(rawProvider) ?? rejected(`config provider ${providerIndex} rejected`);
    if (provider.provider !== "claude" && provider.provider !== "codex") {
      rejected("config provider kind rejected");
    }
    if (typeof provider.command_configured !== "boolean") {
      rejected("config provider state rejected");
    }
    return {
      provider: provider.provider as "claude" | "codex",
      commandConfigured: provider.command_configured,
    };
  });
  return {
    revision: requireSafeUnsigned(config.revision, "config.revision"),
    projects,
    providers,
  };
}

export interface NativeTerminalSnapshot {
  taskId: NativeUuid;
  sequence: number;
  title: string | null;
  textLines: string[];
}

/** Read-only screen text from the canonical, runtime-fenced cockpit query. */
export function decodeTaskCockpitTerminalResult(
  reply: DecodedQueryReply,
  expectedTaskId: NativeUuid,
): NativeTerminalSnapshot {
  if (reply.outcome.kind !== "ok") {
    rejected(`terminal query: ${reply.outcome.error.code}`);
  }
  const cockpit = record(reply.outcome.result.task_cockpit)
    ?? rejected("task_cockpit result missing");
  const unavailable = record(cockpit.unavailable);
  if (unavailable) {
    const reason = boundedText(unavailable.reason, "terminal unavailable reason", 128);
    if (reason === "terminal_start_pending") {
      rejected("Terminal is starting on the host…");
    }
    if (reason === "terminal_not_started") {
      rejected("No terminal has started for this task yet.");
    }
    if (reason === "terminal_provider_setup_required") {
      rejected("The provider needs setup or trust confirmation on the host.");
    }
    rejected("No live terminal is available for this task.");
  }
  const denied = record(cockpit.denied);
  if (denied) rejected("Terminal access was denied by the host.");
  const terminal = record(cockpit.terminal)
    ?? rejected("The host returned no terminal state for this task.");
  const taskId = requireUuid(terminal.task_id, "terminal.task_id");
  if (taskId !== requireUuid(expectedTaskId, "expectedTaskId")) {
    rejected("terminal task mismatch");
  }
  if (!Array.isArray(terminal.text_lines) || terminal.text_lines.length > 4096) {
    rejected("terminal lines exceed limit");
  }
  let bytes = 0;
  const textLines = terminal.text_lines.map((line, index) => {
    const text = boundedText(line, `terminal line ${index}`, MAX_SEMANTIC_TEXT_BYTES);
    bytes += new TextEncoder().encode(text).byteLength;
    if (bytes > MAX_SEMANTIC_PAGE_ENCODED_BYTES) rejected("terminal text exceeds limit");
    return text;
  });
  return {
    taskId,
    sequence: requireSafeUnsigned(terminal.sequence, "terminal.sequence"),
    title: terminal.title == null ? null : boundedText(terminal.title, "terminal.title", 4096),
    textLines,
  };
}

export function decodeTasksSnapshotPage(value: unknown): SnapshotPageView {
  const page = record(value) ?? rejected("SnapshotPage rejected");
  const snapshotId = requireUuid(page.snapshot_id, "snapshot_id");
  const throughSequence = requireSafeUnsigned(
    page.through_sequence,
    "through_sequence",
  );
  if (page.section !== "tasks") rejected("SnapshotPage section rejected");
  if (!Array.isArray(page.items)) rejected("SnapshotPage.items rejected");
  if (page.items.length > MAX_SNAPSHOT_PAGE_ITEMS) {
    rejected("SnapshotPage.items exceed bound");
  }
  const encodedBytes = requireSafePositive(page.encoded_bytes, "encoded_bytes");
  const items: SnapshotListItem[] = [];
  for (const raw of page.items) {
    const entry = record(raw) ?? rejected("SnapshotItem rejected");
    const keys = Object.keys(entry);
    if (keys.length !== 1 || keys[0] !== "task") {
      rejected("SnapshotItem variant rejected");
    }
    const body = record(entry.task) ?? rejected("SnapshotItem.task rejected");
    const task = record(body.task) ?? rejected("TaskSnapshotItem.task rejected");
    items.push({
      kind: "task",
      taskId: requireUuid(task.id, "task.id"),
      revision: requireSafePositive(task.revision, "task.revision"),
      actionEpoch: requireSafeUnsigned(task.action_epoch, "task.action_epoch"),
      primaryAgentId: optionalUuid(
        body.primary_agent_id === undefined ? null : body.primary_agent_id,
        "primary_agent_id",
      ),
      title: typeof task.title === "string" ? task.title : null,
      lifecycle: typeof task.lifecycle === "string" ? task.lifecycle : null,
      projectId:
        task.project_id === undefined || task.project_id === null
          ? null
          : requireUuid(task.project_id, "project_id"),
      environmentId:
        task.environment_id === undefined || task.environment_id === null
          ? null
          : requireUuid(task.environment_id, "environment_id"),
      createdAtMs:
        task.created_at_ms === undefined || task.created_at_ms === null
          ? null
          : requireSafeI64(task.created_at_ms, "created_at_ms"),
      connectivity:
        typeof body.connectivity === "string" ? body.connectivity : null,
      attention: typeof body.attention === "string" ? body.attention : null,
      activity: typeof body.activity === "string" ? body.activity : null,
    });
  }
  return {
    snapshotId,
    throughSequence,
    section: "tasks",
    afterItem: page.after_item ?? null,
    items,
    encodedBytes,
    nextCursor:
      page.next_cursor === null || page.next_cursor === undefined
        ? null
        : copyResumeCursorBytes(
            page.next_cursor as ConnectBinaryMarker | Uint8Array | readonly number[],
          ),
  };
}

export function decodeSnapshotPageQueryResult(
  reply: DecodedQueryReply,
): SnapshotPageView {
  if (reply.outcome.kind !== "ok") rejected("snapshot page query error");
  const body =
    record(reply.outcome.result.snapshot_page) ??
    rejected("snapshot_page result missing");
  return decodeTasksSnapshotPage(body.page);
}

export function decodeCommandReceipt(
  payload: unknown,
  expectedCommandId?: NativeUuid,
): CommandReceiptView {
  const root = record(payload) ?? rejected("CommandReceipt rejected");
  const keys = Object.keys(root);
  if (keys.length !== 1) rejected("CommandReceipt rejected");
  if (keys[0] === "accepted") {
    const body = record(root.accepted) ?? rejected("accepted receipt rejected");
    const commandId = requireUuid(body.command_id, "command_id");
    if (expectedCommandId !== undefined && commandId !== expectedCommandId) {
      rejected("CommandReceipt command correlation rejected");
    }
    if (!Array.isArray(body.event_ids)) rejected("event_ids rejected");
    return {
      kind: "accepted",
      commandId,
      operationId: requireUuid(body.operation_id, "operation_id"),
      taskRevision:
        body.task_revision === null || body.task_revision === undefined
          ? null
          : requireSafeUnsigned(body.task_revision, "task_revision"),
      eventIds: body.event_ids.map((id, index) =>
        requireUuid(id, `event_ids[${index}]`),
      ),
    };
  }
  if (keys[0] === "rejected") {
    const body = record(root.rejected) ?? rejected("rejected receipt rejected");
    const commandId = requireUuid(body.command_id, "command_id");
    if (expectedCommandId !== undefined && commandId !== expectedCommandId) {
      rejected("CommandReceipt command correlation rejected");
    }
    if (typeof body.code !== "string") rejected("rejection code rejected");
    return {
      kind: "rejected",
      commandId,
      code: body.code,
      currentRevision:
        body.current_revision === null || body.current_revision === undefined
          ? null
          : requireSafeUnsigned(body.current_revision, "current_revision"),
    };
  }
  rejected("CommandReceipt rejected");
}

/**
 * Decode the read-only command receipt status result. `null` is the only
 * authoritative missing result; every query error or malformed response is a
 * hard recovery failure and must not trigger a command resubmission.
 */
export function decodeCommandReceiptStatusQueryResult(
  reply: DecodedQueryReply,
  expectedCommandId: NativeUuid,
): CommandReceiptView | null {
  const commandId = requireUuid(expectedCommandId, "expectedCommandId");
  if (reply.outcome.kind !== "ok") {
    if (reply.outcome.error.code === "conflict") {
      rejected("command receipt status conflict");
    }
    rejected("command receipt status query failed");
  }
  const body =
    record(reply.outcome.result.command_receipt_status) ??
    rejected("command_receipt_status result missing");
  requireExactKeys(body, ["receipt"], "command_receipt_status result");
  if (body.receipt === null) return null;
  return decodeCommandReceipt(body.receipt, commandId);
}

export function decodeEventReplayPageResult(
  reply: DecodedQueryReply,
): EventReplayPageView {
  if (reply.outcome.kind !== "ok") rejected("event replay query error");
  const body =
    record(reply.outcome.result.event_replay_page) ??
    rejected("event_replay_page result missing");
  requireExactKeys(body, ["subscription_id", "page"], "event_replay_page result");
  const subscriptionId = requireUuid(body.subscription_id, "subscription_id");
  const page = record(body.page) ?? rejected("event page rejected");
  requireExactKeys(
    page,
    ["after_sequence", "through_sequence", "events", "next_cursor"],
    "event replay page",
  );
  const afterSequence = requireSafeUnsigned(page.after_sequence, "after_sequence");
  const throughSequence = requireSafeUnsigned(
    page.through_sequence,
    "through_sequence",
  );
  if (throughSequence < afterSequence) {
    rejected("event replay sequence window rejected");
  }
  if (!Array.isArray(page.events)) rejected("event page events rejected");
  if (page.events.length > MAX_SNAPSHOT_PAGE_ITEMS) {
    rejected("event replay events exceed bound");
  }
  const affectedTaskIds: NativeUuid[] = [];
  const seenTasks = new Set<string>();
  const seenEventIds = new Set<string>();
  let lastSequence = afterSequence;
  for (let index = 0; index < page.events.length; index += 1) {
    const event = record(page.events[index]) ??
      rejected(`event replay event ${index} rejected`);
    requireExactKeys(
      event,
      ["id", "task_id", "sequence", "task_revision", "occurred_at_ms", "payload"],
      `event replay event ${index}`,
    );
    const eventId = requireUuid(event.id, `event replay event ${index}.id`);
    const sequence = requireSafePositive(
      event.sequence,
      `event replay event ${index}.sequence`,
    );
    if (sequence <= afterSequence || sequence > throughSequence) {
      rejected("event replay event sequence outside page window");
    }
    if (sequence <= lastSequence) {
      rejected("event replay event sequences must be strictly ascending");
    }
    if (seenEventIds.has(eventId)) {
      rejected("duplicate event replay event id");
    }
    seenEventIds.add(eventId);
    lastSequence = sequence;
    if (event.task_revision !== null) {
      requireSafeUnsigned(event.task_revision, `event replay event ${index}.task_revision`);
    }
    requireSafeI64(event.occurred_at_ms, `event replay event ${index}.occurred_at_ms`);
    if (!record(event.payload)) {
      rejected(`event replay event ${index}.payload rejected`);
    }
    const taskId = optionalUuid(event.task_id, `event replay event ${index}.task_id`);
    if (taskId !== null && !seenTasks.has(taskId)) {
      seenTasks.add(taskId);
      affectedTaskIds.push(taskId);
    }
  }
  let nextCursor: Uint8Array | null = null;
  if (page.next_cursor !== null && page.next_cursor !== undefined) {
    if (page.next_cursor instanceof Uint8Array) {
      nextCursor = copyResumeCursorBytes(page.next_cursor);
    } else {
      nextCursor = copyResumeCursorBytes(
        page.next_cursor as ConnectBinaryMarker | readonly number[],
      );
    }
  }
  if (nextCursor !== null && page.events.length === 0) {
    rejected("event replay intermediate page must make nonempty forward progress");
  }
  return {
    subscriptionId,
    afterSequence,
    throughSequence,
    lastSequence,
    nextCursor,
    eventCount: page.events.length,
    affectedTaskIds,
  };
}

export function decodeConversationSubscriptionResult(
  reply: DecodedQueryReply,
): ConversationSubscriptionView {
  if (reply.outcome.kind !== "ok") rejected("conversation subscription error");
  const cockpit =
    record(reply.outcome.result.task_cockpit) ??
    rejected("task_cockpit result missing");
  const body =
    record(cockpit.conversation_subscription) ??
    rejected("conversation_subscription result missing");
  return {
    subscriptionId: requireUuid(body.subscription_id, "subscription_id"),
    page: decodeSemanticJournalPage(body.page),
  };
}

export function decodeConversationSubscriptionReleased(
  reply: DecodedQueryReply,
): NativeUuid {
  if (reply.outcome.kind !== "ok") rejected("conversation release error");
  const cockpit =
    record(reply.outcome.result.task_cockpit) ??
    rejected("task_cockpit result missing");
  const body =
    record(cockpit.conversation_subscription_released) ??
    rejected("conversation_subscription_released missing");
  return requireUuid(body.subscription_id, "subscription_id");
}

/**
 * Decode Connect kind-22 conversation dirty carrier.
 * Shape: { required_capabilities: 131104, message: { conversation_dirty: {
 *   subscription_id, task_id, high_water
 * } } }
 */
export function decodeConversationDirtyEnvelope(input: {
  payloadKind: number;
  payload: unknown;
  requestId: string | null;
  operationId: string | null;
  privacyClass: string;
}): ConversationDirtyNotice {
  if (input.payloadKind !== NATIVE_CONVERSATION_DIRTY_KIND) {
    rejected("conversation dirty kind rejected");
  }
  if (input.requestId !== null || input.operationId !== null) {
    rejected("conversation dirty must omit request/operation ids");
  }
  if (input.privacyClass !== "local_only") {
    rejected("conversation dirty privacy rejected");
  }
  const wrapper = record(input.payload) ?? rejected("conversation dirty rejected");
  requireExactKeys(
    wrapper,
    ["required_capabilities", "message"],
    "conversation dirty wrapper",
  );
  if (!capabilityBits(wrapper.required_capabilities)) {
    rejected("required_capabilities rejected");
  }
  const expectedCapabilities =
    CAPABILITY_TASK_COCKPIT | CAPABILITY_SEMANTIC_CONVERSATION;
  if (Number(wrapper.required_capabilities) !== Number(expectedCapabilities)) {
    rejected("conversation dirty capability rejected");
  }
  const message = record(wrapper.message) ?? rejected("conversation dirty message");
  const keys = Object.keys(message);
  if (keys.length !== 1 || keys[0] !== "conversation_dirty") {
    rejected("conversation dirty message rejected");
  }
  const body =
    record(message.conversation_dirty) ?? rejected("conversation_dirty rejected");
  requireExactKeys(
    body,
    ["subscription_id", "task_id", "high_water"],
    "conversation_dirty",
  );
  return {
    subscriptionId: requireUuid(body.subscription_id, "subscription_id"),
    taskId: requireUuid(body.task_id, "task_id"),
    highWater: requireSafePositive(body.high_water, "high_water"),
    requiredCapabilities: wrapper.required_capabilities as number,
  };
}
