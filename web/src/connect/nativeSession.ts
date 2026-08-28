/**
 * One retained NativeHostSession per immutable hostPublicId.
 * No React, no global state, no legacy RemoteAction/PTY mapping.
 */

import type {
  ConnectConnectionState,
  ConnectPayloadRequest,
  DecodedConnectEnvelope,
} from "./transport";
import { createConnectRequestId } from "./transport";
import {
  decodeHostOutput,
  HOST_CRITICAL_OUTPUT,
  HOST_DURABLE_OUTPUT,
  protocolUuid,
} from "./hostOutput";
import {
  createMemoryNativeCacheStore,
  tasksFromSnapshotItems,
  type NativeCacheStore,
  type NativeCachedConversation,
  type NativeCachedDraft,
  type NativeCachedTaskMeta,
  type NativeOutboxRecord,
  MAX_CACHED_FACTS_PER_TASK,
  MAX_HISTORY_CONVERSATIONS_PER_HOST,
  MAX_METADATA_TASKS_PER_HOST,
} from "./nativeCache";
import {
  CAPABILITY_EVENT_REPLAY,
  CAPABILITY_PAGED_SNAPSHOTS,
  CAPABILITY_PROVIDER_INPUT,
  CAPABILITY_SEMANTIC_CONVERSATION,
  CAPABILITY_TASK_COCKPIT,
  NATIVE_COMMAND_KIND,
  NATIVE_COMMAND_RECEIPT_KIND,
  NATIVE_CONVERSATION_DIRTY_KIND,
  NATIVE_TERMINAL_KEYS,
  NativeProtocolError,
  assertCapabilities,
  buildBeginCloseTaskCommand,
  buildContinueEventReplayQuery,
  buildCommandReceiptStatusQuery,
  buildDeleteTaskCommand,
  buildGlobalOpenEventReplayQuery,
  buildOpenConversationSubscriptionQuery,
  buildOpenTasksSnapshotPageQuery,
  buildProviderInputStateQuery,
  buildReleaseConversationSubscriptionQuery,
  buildReleaseEventReplayQuery,
  buildReleaseSnapshotQuery,
  buildRenameTaskCommand,
  buildReopenTaskCommand,
  buildResumeTasksSnapshotPageQuery,
  buildSettleTaskCommand,
  buildSubmitProviderInputSendNow,
  buildSubmitProviderTerminalKey,
  isProviderSendNowCommand,
  buildTaskCockpitConversationQuery,
  buildTaskCockpitTerminalQuery,
  buildTaskSnapshotQuery,
  connectBinaryMarker,
  copyResumeCursorBytes,
  decodeCommandReceipt,
  decodeCommandReceiptStatusQueryResult,
  decodeConversationDirtyEnvelope,
  decodeConversationSubscriptionResult,
  decodeEventReplayPageResult,
  decodeProviderInputStateQueryResult,
  decodeQueryReply,
  decodeSnapshotPageQueryResult,
  decodeTaskCockpitConversationResult,
  decodeTaskCockpitTerminalResult,
  decodeTaskSnapshotItem,
  decodeTaskSnapshotQueryResult,
  firstTurnIdFromCommandId,
  requiredCapabilitiesForQuery,
  type NativeAuthority,
  type NativeTerminalSnapshot,
  type NativeTerminalKey,
  type NativeUuid,
  type SemanticJournalFact,
  type SemanticJournalPage,
} from "./nativeProtocol";

export const MAX_CONCURRENT_TASK_REFRESHES = 4;
export const MAX_WATCHED_TASKS = 64;
export const MAX_HANDOFF_BUFFER_EVENTS = 2_048;
export const MAX_SNAPSHOT_PAGES = 64;
export const MAX_SNAPSHOT_CUMULATIVE_ITEMS = MAX_METADATA_TASKS_PER_HOST;
export const MAX_EVENT_REPLAY_PAGES = 256;
export const MAX_CONVERSATION_PAGES = 256;
export const MAX_EARLY_DIRTY_NOTICES = 64;

/**
 * Injected Connect transport port. Inspect public methods on ConnectBrowserTransport;
 * suspend/wake remain optional when absent.
 */
export interface NativeConnectTransport {
  start(): Promise<void>;
  stop(): void;
  subscribe(listener: (state: ConnectConnectionState) => void): () => void;
  subscribeEnvelope(
    listener: (envelope: DecodedConnectEnvelope) => void,
  ): () => void;
  request(
    payloadKind: number,
    payload: unknown,
    options?: {
      requestId?: string;
      operationId?: string | null;
      privacyClass?: "local_only" | "managed_metadata" | "raw_content";
      payloadVersion?: number;
    },
  ): Promise<DecodedConnectEnvelope>;
  suspend?(): void;
  wake?(input?: { hiddenDurationMs?: number }): unknown;
  requestResync?(reason?: "gap" | "replay_unavailable"): boolean;
}

export type NativeConnectionStatus =
  | "idle"
  | "connecting"
  | "transport_ready"
  | "syncing"
  | "ready"
  | "degraded"
  | "misbound"
  | "stopped";

export type NativeSyncStatus =
  | "cold"
  | "hydrating"
  | "syncing_snapshot"
  | "syncing_replay"
  | "live"
  | "error";

export type NativeSendFailure =
  | "not_ready"
  | "no_agent"
  | "blockers"
  | "storage_failure"
  | "client_mismatch"
  | "transport_uncertain"
  | "reconciliation_required"
  | "rejected"
  | "invalid_lifecycle";

export type NativeTaskMutation =
  | { kind: "settle" }
  | { kind: "reopen" }
  | { kind: "begin_close" }
  | { kind: "delete" }
  | { kind: "rename"; title: string };

export interface NativeHostSessionView {
  hostPublicId: NativeUuid;
  connectionStatus: NativeConnectionStatus;
  syncStatus: NativeSyncStatus;
  clientId: NativeUuid | null;
  capabilities: number;
  leaseEpoch: number | null;
  tasks: ReadonlyMap<NativeUuid, NativeCachedTaskMeta>;
  conversations: ReadonlyMap<NativeUuid, NativeCachedConversation>;
  drafts: ReadonlyMap<NativeUuid, NativeCachedDraft>;
  outbox: ReadonlyMap<NativeUuid, NativeOutboxRecord>;
  lastError: string | null;
  replayThrough: number;
}

export interface NativeHostSessionOptions {
  hostPublicId: NativeUuid;
  transport: NativeConnectTransport;
  cache?: NativeCacheStore;
  now?: () => number;
  createRequestId?: () => string;
  createCommandId?: () => string;
}

type SessionLease = {
  epoch: number;
  clientId: NativeUuid;
};

type ConversationWatch = {
  refCount: number;
  subscriptionId: NativeUuid | null;
  queryInFlight: boolean;
  dirty: boolean;
  pendingDirtyHighWater: number | null;
  earlyDirtyNotices: Array<{
    subscriptionId: NativeUuid;
    highWater: number;
  }>;
  leaseEpoch: number;
};

function cursorKey(cursor: Uint8Array | { $connectBinary: string } | null): string {
  if (cursor === null || cursor === undefined) return "";
  if (cursor instanceof Uint8Array) {
    return connectBinaryMarker(cursor).$connectBinary;
  }
  return cursor.$connectBinary;
}

export class NativeHostSession {
  readonly hostPublicId: NativeUuid;
  private readonly transport: NativeConnectTransport;
  private readonly cache: NativeCacheStore;
  private readonly now: () => number;
  private readonly createRequestId: () => string;
  private readonly createCommandId: () => string;

  private connectionStatus: NativeConnectionStatus = "idle";
  private syncStatus: NativeSyncStatus = "cold";
  private capabilities = 0;
  private lease: SessionLease | null = null;
  private lastError: string | null = null;
  private stopped = false;
  private transportReady = false;
  private syncGeneration = 0;
  // Never reuse an epoch after disconnect: old async work must not acquire the
  // authority of a later connection that happens to have the same ClientId.
  private leaseCounter = 0;

  private readonly tasks = new Map<NativeUuid, NativeCachedTaskMeta>();
  private readonly conversations = new Map<NativeUuid, NativeCachedConversation>();
  private readonly drafts = new Map<NativeUuid, NativeCachedDraft>();
  private readonly outbox = new Map<NativeUuid, NativeOutboxRecord>();
  private readonly outboxOperations = new Set<NativeUuid>();
  /** One in-flight mutation/send gate per task — never overlap across command kinds. */
  private readonly taskCommandGates = new Set<NativeUuid>();
  private readonly watches = new Map<NativeUuid, ConversationWatch>();
  private readonly viewListeners = new Set<(view: NativeHostSessionView) => void>();

  private unsubState: (() => void) | null = null;
  private unsubEnvelope: (() => void) | null = null;
  private replaySubscriptionId: NativeUuid | null = null;
  private replayThrough = 0;
  private handoffBuffer: DecodedConnectEnvelope[] = [];
  private handoffActive = false;
  private handoffOverflow = false;
  private refreshQueue: NativeUuid[] = [];
  private refreshInFlight = 0;

  constructor(options: NativeHostSessionOptions) {
    const host = protocolUuid(options.hostPublicId);
    if (!host) throw new NativeProtocolError("invalid hostPublicId");
    this.hostPublicId = host;
    this.transport = options.transport;
    this.cache = options.cache ?? createMemoryNativeCacheStore();
    this.now = options.now ?? (() => Date.now());
    this.createRequestId = options.createRequestId ?? createConnectRequestId;
    this.createCommandId = options.createCommandId ?? createConnectRequestId;
  }

  subscribe(listener: (view: NativeHostSessionView) => void): () => void {
    this.viewListeners.add(listener);
    listener(this.view());
    return () => this.viewListeners.delete(listener);
  }

  view(): NativeHostSessionView {
    return {
      hostPublicId: this.hostPublicId,
      connectionStatus: this.connectionStatus,
      syncStatus: this.syncStatus,
      clientId: this.lease?.clientId ?? null,
      capabilities: this.capabilities,
      leaseEpoch: this.lease?.epoch ?? null,
      tasks: new Map(this.tasks),
      conversations: new Map(this.conversations),
      drafts: new Map(this.drafts),
      outbox: new Map(this.outbox),
      lastError: this.lastError,
      replayThrough: this.replayThrough,
    };
  }

  async hydrate(): Promise<void> {
    this.syncStatus = "hydrating";
    this.emit();
    const snap = await this.cache.loadHost(this.hostPublicId);
    if (snap.hostPublicId !== this.hostPublicId) {
      this.failMisbound("cache host collision");
      return;
    }
    this.tasks.clear();
    for (const task of snap.tasks.slice(0, MAX_METADATA_TASKS_PER_HOST)) {
      this.tasks.set(task.taskId, task);
    }
    this.conversations.clear();
    for (const conversation of snap.conversations.slice(
      0,
      MAX_HISTORY_CONVERSATIONS_PER_HOST,
    )) {
      this.conversations.set(conversation.taskId, conversation);
    }
    for (const draft of snap.drafts) {
      // The offline composer may already have accepted typing while IDB was
      // opening. Cache hydration must never replace that newer local draft.
      if (!this.drafts.has(draft.taskId)) this.drafts.set(draft.taskId, draft);
    }
    this.outbox.clear();
    for (const item of snap.outbox) {
      // Reload in_flight → uncertain; never auto-resubmit.
      const status =
        item.status === "in_flight" ? ("uncertain" as const) : item.status;
      const record = { ...item, status, updatedAtMs: this.now() };
      this.outbox.set(record.commandId, record);
      if (status !== item.status) {
        try {
          await this.cache.updateOutboxStatus(
            this.hostPublicId,
            record.commandId,
            status,
          );
        } catch {
          // Keep in-memory uncertain view; root receipt lookup reconciles later.
        }
      }
    }
    this.syncStatus = "cold";
    this.emit();
  }

  async start(): Promise<void> {
    if (this.stopped) return;
    this.invalidateLease("start");
    this.connectionStatus = "connecting";
    this.emit();
    this.unsubState?.();
    this.unsubEnvelope?.();
    this.unsubState = this.transport.subscribe((state) => {
      void this.onTransportState(state);
    });
    this.unsubEnvelope = this.transport.subscribeEnvelope((envelope) => {
      void this.onEnvelope(envelope);
    });
    await this.transport.start();
  }

  stop(): void {
    this.stopped = true;
    this.invalidateLease("stop");
    this.clearHandoff();
    this.unsubState?.();
    this.unsubEnvelope?.();
    this.unsubState = null;
    this.unsubEnvelope = null;
    this.transport.stop();
    this.connectionStatus = "stopped";
    this.emit();
  }

  suspend(): void {
    this.transport.suspend?.();
  }

  wake(input?: { hiddenDurationMs?: number }): void {
    this.transport.wake?.(input);
    const lease = this.lease;
    if (lease && this.connectionStatus === "ready") {
      void this.reconcileOutbox(lease);
      for (const [taskId, watch] of this.watches) {
        if (watch.dirty && watch.pendingDirtyHighWater !== null) {
          void this.drainConversation(taskId, watch.pendingDirtyHighWater).catch(() => undefined);
        }
      }
    }
  }

  /**
   * Owner transport was replaced for re-pair. Invalidate the lease, preserve
   * every outbox row as uncertain (never clear), and wait for the next Hello.
   */
  fenceTransportReplacement(): void {
    if (this.stopped) return;
    this.invalidateLease("transport_replaced");
    this.clearHandoff();
    this.transportReady = false;
    this.connectionStatus = "connecting";
    this.lastError = null;
    for (const [commandId, record] of this.outbox) {
      if (record.status === "uncertain") continue;
      this.outbox.set(commandId, {
        ...record,
        status: "uncertain",
        updatedAtMs: this.now(),
      });
      void this.cache
        .updateOutboxStatus(this.hostPublicId, commandId, "uncertain")
        .catch(() => undefined);
    }
    this.emit();
  }

  async watchTask(taskId: NativeUuid): Promise<void> {
    const id = this.requireTaskId(taskId);
    const existing = this.watches.get(id);
    if (existing) {
      existing.refCount += 1;
      return;
    }
    if (this.watches.size >= MAX_WATCHED_TASKS) {
      throw new NativeProtocolError("watched task limit exceeded");
    }
    const watch: ConversationWatch = {
      refCount: 1,
      subscriptionId: null,
      queryInFlight: false,
      dirty: false,
      pendingDirtyHighWater: null,
      earlyDirtyNotices: [],
      leaseEpoch: this.lease?.epoch ?? -1,
    };
    this.watches.set(id, watch);
    if (this.lease && this.connectionStatus === "ready") {
      await this.openConversationWatch(id);
    }
  }

  async unwatchTask(taskId: NativeUuid): Promise<void> {
    const id = this.requireTaskId(taskId);
    const watch = this.watches.get(id);
    if (!watch) return;
    watch.refCount -= 1;
    if (watch.refCount > 0) return;
    this.watches.delete(id);
    const subscriptionId = watch.subscriptionId;
    const lease = this.lease;
    if (subscriptionId && lease) {
      try {
        if (!this.leaseMatches(lease)) return;
        await this.query(
          buildReleaseConversationSubscriptionQuery({
            ...this.authorityFor(lease, this.createRequestId()),
            taskId: id,
            subscriptionId,
          }),
        );
      } catch {
        // Retain bounded cache view after release failure.
      }
    }
  }

  async readTerminal(taskId: NativeUuid): Promise<NativeTerminalSnapshot> {
    const lease = this.lease;
    if (!lease || this.connectionStatus !== "ready") {
      throw new NativeProtocolError("Terminal host is unavailable");
    }
    assertCapabilities(this.capabilities, CAPABILITY_TASK_COCKPIT);
    const id = this.requireTaskId(taskId);
    const requestId = this.createRequestId();
    const response = await this.query(buildTaskCockpitTerminalQuery({
      ...this.authorityFor(lease, requestId), taskId: id,
    }));
    if (!this.leaseMatches(lease) || this.connectionStatus !== "ready") {
      throw new NativeProtocolError("Terminal connection changed");
    }
    return decodeTaskCockpitTerminalResult(decodeQueryReply(response.payload, requestId), id);
  }

  async sendText(
    taskId: NativeUuid,
    text: string,
  ): Promise<{ ok: true; commandId: NativeUuid } | { ok: false; reason: NativeSendFailure }> {
    return this.submitProviderInput(taskId, text);
  }

  async sendTerminalKey(taskId: NativeUuid, key: NativeTerminalKey): Promise<
    { ok: true; commandId: NativeUuid } | { ok: false; reason: NativeSendFailure }
  > {
    return this.submitProviderInput(taskId, "", key);
  }

  /**
   * Canonical metadata mutation (Done/restore/rename/archive/delete).
   * Reads TaskSnapshot first, freezes lease/client, persists exact command ID
   * + payload before wire send. Never clears composer drafts on acceptance.
   */
  async mutateTask(
    taskId: NativeUuid,
    mutation: NativeTaskMutation,
  ): Promise<{ ok: true; commandId: NativeUuid } | { ok: false; reason: NativeSendFailure }> {
    const id = this.requireTaskId(taskId);
    if (this.taskCommandGates.has(id)) {
      return { ok: false, reason: "reconciliation_required" };
    }
    this.taskCommandGates.add(id);
    try {
      return await this.mutateTaskOnce(id, mutation);
    } finally {
      this.taskCommandGates.delete(id);
    }
  }

  async settleTask(taskId: NativeUuid) {
    return this.mutateTask(taskId, { kind: "settle" });
  }

  async reopenTask(taskId: NativeUuid) {
    return this.mutateTask(taskId, { kind: "reopen" });
  }

  async beginCloseTask(taskId: NativeUuid) {
    return this.mutateTask(taskId, { kind: "begin_close" });
  }

  async deleteTask(taskId: NativeUuid) {
    return this.mutateTask(taskId, { kind: "delete" });
  }

  async renameTask(taskId: NativeUuid, title: string) {
    return this.mutateTask(taskId, { kind: "rename", title });
  }

  private async mutateTaskOnce(
    taskId: NativeUuid,
    mutation: NativeTaskMutation,
  ): Promise<{ ok: true; commandId: NativeUuid } | { ok: false; reason: NativeSendFailure }> {
    const lease = this.lease;
    if (!lease || this.connectionStatus !== "ready") {
      return { ok: false, reason: "not_ready" };
    }
    const capturedClientId = lease.clientId;
    const capturedEpoch = lease.epoch;
    if ([...this.outbox.values()].some((record) => record.taskId === taskId)) {
      return { ok: false, reason: "reconciliation_required" };
    }

    let snapshot;
    try {
      snapshot = await this.readTaskSnapshot(taskId, {
        epoch: capturedEpoch,
        clientId: capturedClientId,
      });
    } catch {
      return { ok: false, reason: "not_ready" };
    }
    if (!this.leaseMatches({ epoch: capturedEpoch, clientId: capturedClientId })) {
      return { ok: false, reason: "not_ready" };
    }
    if (!lifecycleAllowsMutation(snapshot.lifecycle, mutation)) {
      return { ok: false, reason: "invalid_lifecycle" };
    }

    const commandId = this.createCommandId();
    const issuedAtMs = this.now();
    const commandRequestId = this.createRequestId();
    const authority = {
      hostPublicId: this.hostPublicId,
      clientId: capturedClientId,
      requestId: commandRequestId,
    };
    const base = {
      authority,
      commandId,
      taskId,
      issuedAtMs,
      expectedTaskRevision: snapshot.revision,
    };
    let request: ConnectPayloadRequest;
    let outboxText = "";
    try {
      switch (mutation.kind) {
        case "settle":
          request = buildSettleTaskCommand(base);
          break;
        case "reopen":
          request = buildReopenTaskCommand(base);
          break;
        case "begin_close":
          request = buildBeginCloseTaskCommand(base);
          break;
        case "delete":
          request = buildDeleteTaskCommand(base);
          break;
        case "rename":
          outboxText = mutation.title;
          request = buildRenameTaskCommand({ ...base, title: mutation.title });
          break;
      }
    } catch {
      return { ok: false, reason: "not_ready" };
    }

    if (!this.leaseMatches({ epoch: capturedEpoch, clientId: capturedClientId })) {
      return { ok: false, reason: "not_ready" };
    }

    const outbox: NativeOutboxRecord = {
      hostPublicId: this.hostPublicId,
      clientId: capturedClientId,
      commandId,
      taskId,
      commandPayload: request.payload,
      text: outboxText,
      issuedAtMs,
      status: "pending",
      updatedAtMs: this.now(),
    };
    try {
      await this.cache.putOutbox(outbox);
    } catch {
      return { ok: false, reason: "storage_failure" };
    }
    if (!this.leaseMatches({ epoch: capturedEpoch, clientId: capturedClientId })) {
      this.outbox.set(commandId, outbox);
      this.emit();
      return { ok: false, reason: "client_mismatch" };
    }
    this.outbox.set(commandId, outbox);
    this.emit();
    const result = await this.dispatchOutbox(outbox, request, {
      epoch: capturedEpoch,
      clientId: capturedClientId,
    });
    if (result.ok) {
      await this.refreshTaskSnapshot(taskId);
    }
    return result;
  }

  private async readTaskSnapshot(
    taskId: NativeUuid,
    lease: SessionLease,
  ): Promise<{ revision: number; lifecycle: string | null }> {
    const requestId = this.createRequestId();
    const envelope = await this.query(
      buildTaskSnapshotQuery({
        ...this.authorityFor(lease, requestId),
        taskId,
      }),
    );
    if (!this.leaseMatches(lease)) {
      throw new NativeProtocolError("stale lease during task snapshot");
    }
    const item = decodeTaskSnapshotQueryResult(
      decodeQueryReply(envelope.payload, requestId),
      taskId,
    );
    return { revision: item.revision, lifecycle: item.lifecycle };
  }

  private async submitProviderInput(taskId: NativeUuid, text: string, terminalKey?: NativeTerminalKey): Promise<
    { ok: true; commandId: NativeUuid } | { ok: false; reason: NativeSendFailure }
  > {
    const id = this.requireTaskId(taskId);
    if (this.taskCommandGates.has(id)) return { ok: false, reason: "reconciliation_required" };
    this.taskCommandGates.add(id);
    try { return await this.submitProviderInputOnce(id, text, terminalKey); }
    finally { this.taskCommandGates.delete(id); }
  }

  private async submitProviderInputOnce(taskId: NativeUuid, text: string, terminalKey?: NativeTerminalKey): Promise<
    { ok: true; commandId: NativeUuid } | { ok: false; reason: NativeSendFailure }
  > {
    const lease = this.lease;
    if (!lease || this.connectionStatus !== "ready") {
      return { ok: false, reason: "not_ready" };
    }
    // Capture one clientId before the first await; never rebuild from a later lease.
    const capturedClientId = lease.clientId;
    const capturedEpoch = lease.epoch;
    const id = this.requireTaskId(taskId);
    const requestId = this.createRequestId();
    if ([...this.outbox.values()].some((record) => record.taskId === id)) {
      return { ok: false, reason: "reconciliation_required" };
    }
    let state;
    try {
      const replyEnvelope = await this.query(
        buildProviderInputStateQuery({
          ...this.authorityFor(lease, requestId),
          taskId: id,
        }),
      );
      if (!this.leaseMatches({ epoch: capturedEpoch, clientId: capturedClientId })) {
        return { ok: false, reason: "not_ready" };
      }
      const decoded = decodeQueryReply(replyEnvelope.payload, requestId);
      state = decodeProviderInputStateQueryResult(
        decoded,
        this.authorityFor(lease, requestId),
        id,
      );
    } catch {
      return { ok: false, reason: "not_ready" };
    }
    if (!this.leaseMatches({ epoch: capturedEpoch, clientId: capturedClientId })) {
      return { ok: false, reason: "not_ready" };
    }
    if (!state.fence) return { ok: false, reason: "no_agent" };
    if (
      state.fence.openQuestion ||
      state.fence.openApproval ||
      state.fence.pendingWaitCommandIds.length > 0
    ) {
      return { ok: false, reason: "blockers" };
    }

    const commandId = this.createCommandId();
    const issuedAtMs = this.now();
    const commandRequestId = this.createRequestId();
    let request: ConnectPayloadRequest;
    try {
      const input = {
        authority: {
          hostPublicId: this.hostPublicId,
          clientId: capturedClientId,
          requestId: commandRequestId,
        },
        commandId,
        text,
        issuedAtMs,
        fence: state.fence,
      };
      request = terminalKey === undefined
        ? buildSubmitProviderInputSendNow(input)
        : buildSubmitProviderTerminalKey({ ...input, key: terminalKey });
    } catch (error) {
      if (error instanceof NativeProtocolError) {
        if (/blocker/.test(error.message)) return { ok: false, reason: "blockers" };
        if (/non-open|no-agent|incomplete/.test(error.message)) {
          return { ok: false, reason: "no_agent" };
        }
      }
      return { ok: false, reason: "not_ready" };
    }

    if (!this.leaseMatches({ epoch: capturedEpoch, clientId: capturedClientId })) {
      return { ok: false, reason: "not_ready" };
    }

    const outbox: NativeOutboxRecord = {
      hostPublicId: this.hostPublicId,
      clientId: capturedClientId,
      commandId,
      taskId: id,
      commandPayload: request.payload,
      // Only fixed control keys are durable here, never a terminal password or
      // arbitrary raw PTY input. Reconciliation uses the original command ID.
      text: terminalKey === undefined ? text : NATIVE_TERMINAL_KEYS[terminalKey],
      issuedAtMs,
      status: "pending",
      updatedAtMs: this.now(),
    };
    try {
      await this.cache.putOutbox(outbox);
    } catch {
      return { ok: false, reason: "storage_failure" };
    }
    if (!this.leaseMatches({ epoch: capturedEpoch, clientId: capturedClientId })) {
      // Persisted under the captured client; do not wire-dispatch under a new lease.
      this.outbox.set(commandId, outbox);
      this.emit();
      return { ok: false, reason: "client_mismatch" };
    }
    this.outbox.set(commandId, outbox);
    this.emit();
    return this.dispatchOutbox(outbox, request, {
      epoch: capturedEpoch,
      clientId: capturedClientId,
    });
  }

  async retryOutbox(commandId: NativeUuid): Promise<
    { ok: true; commandId: NativeUuid } | { ok: false; reason: NativeSendFailure }
  > {
    const id = protocolUuid(commandId);
    if (!id) return { ok: false, reason: "not_ready" };
    if (this.outboxOperations.has(id)) return { ok: false, reason: "reconciliation_required" };
    this.outboxOperations.add(id);
    try { return await this.recoverOutbox(id); }
    finally { this.outboxOperations.delete(id); }
  }

  private async recoverOutbox(id: NativeUuid): Promise<
    { ok: true; commandId: NativeUuid } | { ok: false; reason: NativeSendFailure }
  > {
    const record = this.outbox.get(id);
    if (!record) return { ok: false, reason: "not_ready" };
    if (record.status === "blocked_client_mismatch") {
      return { ok: false, reason: "client_mismatch" };
    }
    const lease = this.lease;
    if (!lease || this.connectionStatus !== "ready") return { ok: false, reason: "not_ready" };
    if (record.clientId !== lease.clientId) {
      const blocked = { ...record, status: "blocked_client_mismatch" as const };
      try {
        await this.cache.updateOutboxStatus(
          this.hostPublicId,
          id,
          "blocked_client_mismatch",
        );
      } catch {
        return { ok: false, reason: "storage_failure" };
      }
      this.outbox.set(id, blocked);
      this.emit();
      return { ok: false, reason: "client_mismatch" };
    }
    // Reconcile every retained command before any retry. Only an authenticated
    // explicit null receipt permits reusing the exact original command bytes.
    try {
      const requestId = this.createRequestId();
      const reply = await this.query(buildCommandReceiptStatusQuery({
        ...this.authorityFor(lease, requestId), taskId: record.taskId,
        commandPayload: record.commandPayload,
      }));
      if (!this.leaseMatches(lease)) return { ok: false, reason: "not_ready" };
      const receipt = decodeCommandReceiptStatusQueryResult(decodeQueryReply(reply.payload, requestId), id);
      if (receipt) return await this.settleReceipt(record, receipt.kind === "accepted");
      await this.cache.updateOutboxStatus(this.hostPublicId, id, "pending");
      if (!this.leaseMatches(lease)) return { ok: false, reason: "not_ready" };
      this.outbox.set(id, { ...record, status: "pending" });
    } catch {
      return { ok: false, reason: "reconciliation_required" };
    }
    const request: ConnectPayloadRequest = {
      payloadKind: NATIVE_COMMAND_KIND,
      payload: record.commandPayload,
      requestId: this.createRequestId(),
      operationId: null,
      privacyClass: "local_only",
      payloadVersion: 1,
    };
    return this.dispatchOutboxOnce({ ...record, status: "pending" }, request, lease);
  }

  async cancelUnsent(commandId: NativeUuid): Promise<boolean> {
    const id = protocolUuid(commandId);
    if (!id) return false;
    const record = this.outbox.get(id);
    // Only pending is cancellable — never pretend in_flight/uncertain were undelivered.
    if (!record || record.status !== "pending" || this.outboxOperations.has(id)) return false;
    try {
      await this.cache.settleOutbox(this.hostPublicId, id);
    } catch {
      return false;
    }
    this.outbox.delete(id);
    this.emit();
    return true;
  }

  setDraft(taskId: NativeUuid, text: string): Promise<void> {
    const id = this.requireTaskId(taskId);
    const draft = { taskId: id, text, updatedAtMs: this.now() };
    this.drafts.set(id, draft);
    this.emit();
    return this.cache.putDraft(this.hostPublicId, draft);
  }

  private async dispatchOutbox(
    record: NativeOutboxRecord,
    request: ConnectPayloadRequest,
    lease: SessionLease,
  ): Promise<{ ok: true; commandId: NativeUuid } | { ok: false; reason: NativeSendFailure }> {
    if (this.outboxOperations.has(record.commandId)) return { ok: false, reason: "reconciliation_required" };
    this.outboxOperations.add(record.commandId);
    try { return await this.dispatchOutboxOnce(record, request, lease); }
    finally { this.outboxOperations.delete(record.commandId); }
  }

  private async dispatchOutboxOnce(
    record: NativeOutboxRecord,
    request: ConnectPayloadRequest,
    lease: SessionLease,
  ): Promise<{ ok: true; commandId: NativeUuid } | { ok: false; reason: NativeSendFailure }> {
    if (!this.leaseMatches(lease) || record.clientId !== lease.clientId) {
      return { ok: false, reason: "client_mismatch" };
    }
    if (record.status !== "pending") {
      if (record.status === "uncertain" || record.status === "in_flight") {
        return { ok: false, reason: "reconciliation_required" };
      }
      return { ok: false, reason: "not_ready" };
    }
    try {
      await this.cache.updateOutboxStatus(
        this.hostPublicId,
        record.commandId,
        "in_flight",
      );
    } catch {
      return { ok: false, reason: "storage_failure" };
    }
    const inFlight = { ...record, status: "in_flight" as const, updatedAtMs: this.now() };
    this.outbox.set(record.commandId, inFlight);
    this.emit();

    if (!this.leaseMatches(lease)) {
      // Already in_flight on the wire path boundary — mark uncertain, never erase.
      try {
        await this.cache.updateOutboxStatus(
          this.hostPublicId,
          record.commandId,
          "uncertain",
        );
        this.outbox.set(record.commandId, {
          ...inFlight,
          status: "uncertain",
          updatedAtMs: this.now(),
        });
        this.emit();
      } catch {
        this.outbox.set(record.commandId, {
          ...inFlight,
          status: "uncertain",
          updatedAtMs: this.now(),
        });
        this.emit();
      }
      return { ok: false, reason: "transport_uncertain" };
    }

    try {
      const envelope = await this.transport.request(
        request.payloadKind,
        request.payload,
        {
          requestId: request.requestId ?? undefined,
          privacyClass: "local_only",
          payloadVersion: 1,
        },
      );
      if (!this.leaseMatches(lease)) {
        await this.markUncertain(record.commandId, inFlight);
        return { ok: false, reason: "transport_uncertain" };
      }
      if (envelope.payloadKind !== NATIVE_COMMAND_RECEIPT_KIND) {
        await this.markUncertain(record.commandId, inFlight);
        return { ok: false, reason: "transport_uncertain" };
      }
      const receipt = decodeCommandReceipt(envelope.payload, record.commandId);
      return await this.settleReceipt(inFlight, receipt.kind === "accepted");
    } catch {
      const reconciled = await this.reconcileDispatchedReceipt(inFlight, lease);
      if (reconciled) return reconciled;
      await this.markUncertain(record.commandId, inFlight);
      return { ok: false, reason: "transport_uncertain" };
    }
  }

  /** One receipt read after a transport error; it never auto-resends a command. */
  private async reconcileDispatchedReceipt(
    record: NativeOutboxRecord,
    lease: SessionLease,
  ): Promise<
    | { ok: true; commandId: NativeUuid }
    | { ok: false; reason: NativeSendFailure }
    | null
  > {
    if (!this.leaseMatches(lease) || this.connectionStatus !== "ready") return null;
    try {
      const requestId = this.createRequestId();
      const reply = await this.query(buildCommandReceiptStatusQuery({
        ...this.authorityFor(lease, requestId),
        taskId: record.taskId,
        commandPayload: record.commandPayload,
      }));
      if (!this.leaseMatches(lease) || this.connectionStatus !== "ready") return null;
      const receipt = decodeCommandReceiptStatusQueryResult(
        decodeQueryReply(reply.payload, requestId),
        record.commandId,
      );
      if (!receipt) return null;
      return await this.settleReceipt(record, receipt.kind === "accepted");
    } catch {
      return null;
    }
  }

  private async settleReceipt(record: NativeOutboxRecord, accepted: boolean): Promise<
    { ok: true; commandId: NativeUuid } | { ok: false; reason: NativeSendFailure }
  > {
    const clearDraft =
      accepted && isProviderSendNowCommand(record.commandPayload);
    try {
      await this.cache.settleOutbox(
        this.hostPublicId,
        record.commandId,
        clearDraft ? record.text : undefined,
      );
    } catch {
      await this.markUncertain(record.commandId, record);
      return { ok: false, reason: "storage_failure" };
    }
    this.outbox.delete(record.commandId);
    if (
      clearDraft &&
      this.drafts.get(record.taskId)?.text === record.text
    ) {
      this.drafts.delete(record.taskId);
    }
    this.emit();
    return accepted ? { ok: true, commandId: record.commandId } : { ok: false, reason: "rejected" };
  }

  private async markUncertain(
    commandId: NativeUuid,
    record: NativeOutboxRecord,
  ): Promise<void> {
    try {
      await this.cache.updateOutboxStatus(this.hostPublicId, commandId, "uncertain");
      this.outbox.set(commandId, {
        ...record,
        status: "uncertain",
        updatedAtMs: this.now(),
      });
    } catch {
      // Never erase after wire uncertainty.
      this.outbox.set(commandId, {
        ...record,
        status: "uncertain",
        updatedAtMs: this.now(),
      });
    }
    this.emit();
  }

  private async onTransportState(state: ConnectConnectionState): Promise<void> {
    if (this.stopped) return;
    if (state.kind === "ready") {
      // Transport ready precedes Hello but does not authorize native client work.
      this.transportReady = true;
      if (!this.lease) {
        this.connectionStatus = "transport_ready";
        this.emit();
      }
      return;
    }
    if (state.kind === "connecting" || state.kind === "handshaking" || state.kind === "loading") {
      this.connectionStatus = "connecting";
      this.emit();
      return;
    }
    if (
      state.kind === "closed" ||
      state.kind === "held" ||
      state.kind === "reconnecting"
    ) {
      this.transportReady = false;
      this.invalidateLease("disconnect");
      this.clearHandoff();
      this.connectionStatus = "degraded";
      if (state.kind === "held" || state.kind === "closed") {
        this.lastError = state.reason;
      }
      this.emit();
    }
  }

  private async onEnvelope(envelope: DecodedConnectEnvelope): Promise<void> {
    if (this.stopped) return;
    if (envelope.payloadKind === 1) {
      await this.onHello(envelope);
      return;
    }
    if (envelope.payloadKind === NATIVE_CONVERSATION_DIRTY_KIND) {
      this.onConversationDirty(envelope);
      return;
    }
    if (
      envelope.payloadKind === HOST_DURABLE_OUTPUT ||
      envelope.payloadKind === HOST_CRITICAL_OUTPUT
    ) {
      if (this.handoffActive) {
        if (this.handoffBuffer.length >= MAX_HANDOFF_BUFFER_EVENTS) {
          this.handoffOverflow = true;
          this.clearHandoff();
          void this.forceResync("handoff overflow");
          return;
        }
        this.handoffBuffer.push(envelope);
        return;
      }
      await this.applyHostOutput(envelope);
      return;
    }
  }

  private async onHello(envelope: DecodedConnectEnvelope): Promise<void> {
    if (!this.transportReady) {
      this.failMisbound("hello before transport ready");
      return;
    }
    const payload =
      envelope.payload !== null && typeof envelope.payload === "object"
        ? (envelope.payload as Record<string, unknown>)
        : null;
    if (!payload) {
      this.failMisbound("hello payload rejected");
      return;
    }
    const clientId = protocolUuid(payload.client_id);
    if (!clientId) {
      this.failMisbound("hello client_id rejected");
      return;
    }
    if (typeof payload.capabilities !== "number") {
      this.failMisbound("hello capabilities rejected");
      return;
    }

    // Overlapping Hello cancels prior handoff and invalidates the previous lease.
    this.clearHandoff();
    this.syncGeneration += 1;
    const generation = this.syncGeneration;
    const epoch = ++this.leaseCounter;
    this.lease = { epoch, clientId };
    this.capabilities = payload.capabilities;
    this.connectionStatus = "syncing";

    for (const item of this.outbox.values()) {
      if (item.clientId !== clientId) {
        try {
          await this.cache.updateOutboxStatus(
            this.hostPublicId,
            item.commandId,
            "blocked_client_mismatch",
          );
          this.outbox.set(item.commandId, {
            ...item,
            status: "blocked_client_mismatch",
            updatedAtMs: this.now(),
          });
        } catch {
          this.outbox.set(item.commandId, {
            ...item,
            status: "blocked_client_mismatch",
            updatedAtMs: this.now(),
          });
        }
      }
    }
    this.emit();
    await this.syncFromHost(generation);
  }

  private onConversationDirty(envelope: DecodedConnectEnvelope): void {
    try {
      // The native carrier is validated against the negotiated capability set
      // and its physical lane before interpreting the semantic notice.
      decodeHostOutput(envelope, this.capabilities);
      const notice = decodeConversationDirtyEnvelope({
        payloadKind: envelope.payloadKind,
        payload: envelope.payload,
        requestId: envelope.requestId,
        operationId: envelope.operationId,
        privacyClass: envelope.privacyClass,
      });
      const lease = this.lease;
      if (!lease) return;
      const watch = this.watches.get(notice.taskId);
      if (!watch || watch.leaseEpoch !== lease.epoch) return;

      if (!watch.subscriptionId) {
        // Buffer only for this task's early open — never evict other tasks' notices.
        if (watch.earlyDirtyNotices.length < MAX_EARLY_DIRTY_NOTICES) {
          watch.earlyDirtyNotices.push({
            subscriptionId: notice.subscriptionId,
            highWater: notice.highWater,
          });
        }
        return;
      }
      if (watch.subscriptionId !== notice.subscriptionId) return;
      if (watch.queryInFlight) {
        watch.dirty = true;
        watch.pendingDirtyHighWater = Math.max(
          watch.pendingDirtyHighWater ?? 0,
          notice.highWater,
        );
        return;
      }
      void this.drainConversation(notice.taskId, notice.highWater).catch(() => {
        if (this.leaseMatches(lease)) {
          watch.dirty = true;
          watch.pendingDirtyHighWater = Math.max(watch.pendingDirtyHighWater ?? 0, notice.highWater);
        }
      });
    } catch {
      // Malformed dirty carriers do not poison the session.
    }
  }

  private async applyHostOutput(envelope: DecodedConnectEnvelope): Promise<void> {
    const lease = this.lease;
    if (!lease) return;
    let decoded: unknown;
    try {
      decoded = decodeHostOutput(envelope, this.capabilities);
    } catch {
      return;
    }
    const root =
      decoded !== null && typeof decoded === "object"
        ? (decoded as Record<string, unknown>)
        : null;
    const message = root && recordOf(root.message);
    if (!message) return;

    if (envelope.payloadKind === HOST_CRITICAL_OUTPUT && "resync_required" in message) {
      const notice = recordOf(message.resync_required);
      if (!notice || protocolUuid(notice.subscription_id) !== this.replaySubscriptionId) return;
      await this.forceResync("host resync_required");
      return;
    }
    if (envelope.payloadKind !== HOST_DURABLE_OUTPUT || !("durable_event" in message)) {
      return;
    }
    const body = recordOf(message.durable_event);
    if (!body) return;
    if (
      this.replaySubscriptionId &&
      protocolUuid(body.subscription_id) !== this.replaySubscriptionId
    ) {
      return;
    }
    const event = recordOf(body.event);
    if (!event) return;
    const sequence = event.sequence;
    if (typeof sequence !== "number") return;
    // Snapshot/replay handoff can overlap live deliveries. They are already
    // reflected in the frozen snapshot or replay page, not a sequence gap.
    if (sequence <= this.replayThrough) return;
    if (sequence !== this.replayThrough + 1) {
      await this.forceResync("durable sequence gap");
      return;
    }
    this.replayThrough = sequence;
    const taskId =
      event.task_id === null || event.task_id === undefined
        ? null
        : protocolUuid(event.task_id);
    if (taskId) {
      this.scheduleTaskRefresh(taskId);
    } else {
      // Global durable facts: refresh via fresh task list on next bounded sync if needed.
      void this.refreshTaskListIfLive();
    }
    this.emit();
  }

  private async forceResync(reason: string): Promise<void> {
    if (!this.lease) return;
    this.lastError = reason;
    this.syncGeneration += 1;
    const generation = this.syncGeneration;
    this.clearHandoff();
    this.transport.requestResync?.("gap");
    await this.syncFromHost(generation);
  }

  private async syncFromHost(generation: number): Promise<void> {
    const lease = this.lease;
    if (!lease) return;
    this.connectionStatus = "syncing";
    this.syncStatus = "syncing_snapshot";
    this.emit();

    // Enable handoff before snapshot so live durable frames are not silently dropped.
    this.handoffActive = true;
    this.handoffBuffer = [];
    this.handoffOverflow = false;

    try {
      assertCapabilities(this.capabilities, CAPABILITY_PAGED_SNAPSHOTS);
      const { items, throughSequence } = await this.loadAllTaskPages(lease, generation);
      if (!this.syncStillValid(generation, lease)) {
        return;
      }
      // Atomic install only after complete pagination; replayThrough = frozen HWM.
      const metas = tasksFromSnapshotItems(items, this.now());
      this.tasks.clear();
      for (const meta of metas.slice(0, MAX_METADATA_TASKS_PER_HOST)) {
        this.tasks.set(meta.taskId, meta);
      }
      this.replayThrough = throughSequence;
      try {
        await this.cache.putTasks(this.hostPublicId, [...this.tasks.values()]);
      } catch {
        // Live metadata remains; history cache write may fail independently.
      }

      if (!this.syncStillValid(generation, lease)) {
        return;
      }
      this.syncStatus = "syncing_replay";
      this.emit();
      assertCapabilities(this.capabilities, CAPABILITY_EVENT_REPLAY);
      await this.openGlobalEventReplay(lease, generation, throughSequence);
      if (!this.syncStillValid(generation, lease)) {
        return;
      }

      if (this.handoffOverflow) {
        throw new NativeProtocolError("handoff overflow requires fresh sync");
      }
      const buffered = [...this.handoffBuffer];
      this.handoffActive = false;
      this.handoffBuffer = [];
      for (const bufferedEnvelope of buffered) {
        if (!this.syncStillValid(generation, lease)) return;
        await this.applyHostOutput(bufferedEnvelope);
      }

      if (!this.syncStillValid(generation, lease)) return;
      this.syncStatus = "live";
      this.connectionStatus = "ready";
      this.lastError = null;
      this.emit();
      // Recovery is host/client-scoped and retains the original command IDs.
      // It runs independently of conversation hydration, bounded by the outbox.
      void this.reconcileOutbox(lease);
      // History hydration must not hold the inbox/composer behind every open
      // pane. Bound concurrent watchers and keep each failure independent.
      const tasks = [...this.watches.keys()];
      for (let index = 0; index < tasks.length; index += MAX_CONCURRENT_TASK_REFRESHES) {
        if (!this.syncStillValid(generation, lease)) return;
        await Promise.all(tasks.slice(index, index + MAX_CONCURRENT_TASK_REFRESHES).map(async (taskId) => {
          try { await this.openConversationWatch(taskId); }
          catch (error) {
            if (!this.syncStillValid(generation, lease)) return;
            this.lastError = error instanceof Error ? error.message : "Conversation refresh failed";
            this.emit();
          }
        }));
      }
    } catch (error) {
      if (!this.syncStillValid(generation, lease)) return;
      this.clearHandoff();
      this.syncStatus = "error";
      this.connectionStatus = "degraded";
      this.lastError =
        error instanceof Error ? error.message : "native sync failed";
      this.emit();
    }
  }

  private async loadAllTaskPages(
    lease: SessionLease,
    generation: number,
  ): Promise<{ items: ReturnType<typeof decodeSnapshotPageQueryResult>["items"]; throughSequence: number }> {
    const requestId = this.createRequestId();
    const open = buildOpenTasksSnapshotPageQuery(
      this.authorityFor(lease, requestId),
    );
    assertCapabilities(
      this.capabilities,
      requiredCapabilitiesForQuery(
        (open.payload as { query: Record<string, unknown> }).query,
      ),
    );
    const first = decodeSnapshotPageQueryResult(
      decodeQueryReply((await this.query(open)).payload, requestId),
    );
    if (!this.syncStillValid(generation, lease)) {
      throw new NativeProtocolError("stale lease during snapshot");
    }
    const snapshotId = first.snapshotId;
    const throughSequence = first.throughSequence;
    const collected = [...first.items];
    let nextCursor = first.nextCursor;
    const seenCursors = new Set<string>();
    let pages = 1;

    try {
      while (nextCursor !== null && nextCursor !== undefined) {
        if (!this.syncStillValid(generation, lease)) {
          throw new NativeProtocolError("stale lease during snapshot");
        }
        if (pages >= MAX_SNAPSHOT_PAGES) {
          throw new NativeProtocolError("snapshot page limit exceeded");
        }
        if (collected.length > MAX_SNAPSHOT_CUMULATIVE_ITEMS) {
          throw new NativeProtocolError("snapshot cumulative item limit exceeded");
        }
        const bytes =
          nextCursor instanceof Uint8Array
            ? copyResumeCursorBytes(nextCursor)
            : copyResumeCursorBytes(nextCursor as { $connectBinary: string });
        const key = cursorKey(connectBinaryMarker(bytes));
        if (seenCursors.has(key)) {
          throw new NativeProtocolError("snapshot cursor cycle");
        }
        seenCursors.add(key);
        const resumeId = this.createRequestId();
        const page = decodeSnapshotPageQueryResult(
          decodeQueryReply(
            (
              await this.query(
                buildResumeTasksSnapshotPageQuery({
                  ...this.authorityFor(lease, resumeId),
                  snapshotId,
                  resumeCursor: bytes,
                }),
              )
            ).payload,
            resumeId,
          ),
        );
        if (
          page.snapshotId !== snapshotId ||
          page.throughSequence !== throughSequence
        ) {
          throw new NativeProtocolError("snapshot page boundary mismatch");
        }
        collected.push(...page.items);
        nextCursor = page.nextCursor;
        pages += 1;
      }
      if (collected.length > MAX_SNAPSHOT_CUMULATIVE_ITEMS) {
        throw new NativeProtocolError("snapshot cumulative item limit exceeded");
      }
      return { items: collected, throughSequence };
    } finally {
      try {
        if (this.leaseMatches(lease)) {
          await this.query(
            buildReleaseSnapshotQuery({
              ...this.authorityFor(lease, this.createRequestId()),
              snapshotId,
            }),
          );
        }
      } catch {
        // Best-effort release on success, failure, and cancel.
      }
    }
  }

  private async openGlobalEventReplay(
    lease: SessionLease,
    generation: number,
    afterSequence: number,
  ): Promise<void> {
    if (this.replaySubscriptionId && this.leaseMatches(lease)) {
      try {
        await this.query(
          buildReleaseEventReplayQuery({
            ...this.authorityFor(lease, this.createRequestId()),
            subscriptionId: this.replaySubscriptionId,
          }),
        );
      } catch {
        // Best-effort prior release.
      }
      this.replaySubscriptionId = null;
    }
    const requestId = this.createRequestId();
    const open = buildGlobalOpenEventReplayQuery({
      ...this.authorityFor(lease, requestId),
      afterSequence,
    });
    const page = decodeEventReplayPageResult(
      decodeQueryReply((await this.query(open)).payload, requestId),
    );
    if (!this.syncStillValid(generation, lease)) return;
    if (page.afterSequence !== afterSequence) {
      throw new NativeProtocolError("event replay initial cursor mismatch");
    }
    this.replaySubscriptionId = page.subscriptionId;
    this.replayThrough = page.throughSequence;
    for (const taskId of page.affectedTaskIds) {
      this.scheduleTaskRefresh(taskId);
    }
    let cursor = page.nextCursor;
    let expectedAfterSequence = page.lastSequence;
    const pinnedThroughSequence = page.throughSequence;
    const seenCursors = new Set<string>();
    let pages = 1;
    while (cursor) {
      if (!this.syncStillValid(generation, lease)) return;
      if (pages >= MAX_EVENT_REPLAY_PAGES) {
        throw new NativeProtocolError("event replay page limit exceeded");
      }
      const key = cursorKey(cursor);
      if (seenCursors.has(key)) {
        throw new NativeProtocolError("event replay cursor cycle");
      }
      seenCursors.add(key);
      const continueId = this.createRequestId();
      const nextEnvelope = await this.query(
        buildContinueEventReplayQuery({
          ...this.authorityFor(lease, continueId),
          subscriptionId: page.subscriptionId,
          resumeCursor: cursor,
        }),
      );
      if (!this.syncStillValid(generation, lease)) return;
      const next = decodeEventReplayPageResult(
        decodeQueryReply(nextEnvelope.payload, continueId),
      );
      if (next.subscriptionId !== page.subscriptionId) {
        throw new NativeProtocolError("event replay subscription mismatch");
      }
      if (next.afterSequence !== expectedAfterSequence) {
        throw new NativeProtocolError("event replay continuation cursor mismatch");
      }
      if (next.throughSequence !== pinnedThroughSequence) {
        throw new NativeProtocolError("event replay through_sequence drift");
      }
      this.replayThrough = pinnedThroughSequence;
      for (const taskId of next.affectedTaskIds) {
        this.scheduleTaskRefresh(taskId);
      }
      expectedAfterSequence = next.lastSequence;
      cursor = next.nextCursor;
      pages += 1;
    }
  }

  private async openConversationWatch(taskId: NativeUuid): Promise<void> {
    const watch = this.watches.get(taskId);
    const lease = this.lease;
    if (!watch || !lease) return;
    if (watch.queryInFlight) {
      watch.dirty = true;
      return;
    }
    assertCapabilities(
      this.capabilities,
      CAPABILITY_TASK_COCKPIT | CAPABILITY_SEMANTIC_CONVERSATION,
    );
    watch.queryInFlight = true;
    watch.leaseEpoch = lease.epoch;
    const cached = this.conversations.get(taskId);
    const after = cached?.nextSequence ?? cached?.throughSequence ?? 0;
    try {
      const requestId = this.createRequestId();
      const opened = decodeConversationSubscriptionResult(
        decodeQueryReply(
          (
            await this.query(
              buildOpenConversationSubscriptionQuery({
                ...this.authorityFor(lease, requestId),
                taskId,
                afterSequence: after,
              }),
            )
          ).payload,
          requestId,
        ),
      );
      // Delayed open after unwatch: release returned subscription; no merge/cache.
      if (!this.watches.has(taskId) || this.watches.get(taskId) !== watch) {
        if (this.leaseMatches(lease)) {
          try {
            await this.query(
              buildReleaseConversationSubscriptionQuery({
                ...this.authorityFor(lease, this.createRequestId()),
                taskId,
                subscriptionId: opened.subscriptionId,
              }),
            );
          } catch {
            // Best-effort release of orphaned open.
          }
        }
        return;
      }
      if (!this.leaseMatches(lease) || watch.leaseEpoch !== lease.epoch) return;

      watch.subscriptionId = opened.subscriptionId;
      for (const early of watch.earlyDirtyNotices) {
        if (early.subscriptionId !== opened.subscriptionId) continue;
        watch.dirty = true;
        watch.pendingDirtyHighWater = Math.max(
          watch.pendingDirtyHighWater ?? 0,
          early.highWater,
        );
      }
      watch.earlyDirtyNotices = [];
      await this.mergeConversationPage(
        taskId,
        opened.page,
        opened.page.cursorRolledOver,
      );

      // OPEN ONCE — further pages use task_cockpit.conversation {after_sequence}.
      let next = opened.page.nextSequence;
      let pages = 1;
      const seenAfter = new Set<number>([after]);
      while (next !== null) {
        if (!this.watches.has(taskId) || this.watches.get(taskId) !== watch) return;
        if (!this.leaseMatches(lease) || watch.leaseEpoch !== lease.epoch) return;
        if (pages >= MAX_CONVERSATION_PAGES) {
          throw new NativeProtocolError("conversation page limit exceeded");
        }
        if (seenAfter.has(next)) {
          throw new NativeProtocolError("conversation after_sequence cycle");
        }
        seenAfter.add(next);
        const pageRequestId = this.createRequestId();
        const page = decodeTaskCockpitConversationResult(
          decodeQueryReply(
            (
              await this.query(
                buildTaskCockpitConversationQuery({
                  ...this.authorityFor(lease, pageRequestId),
                  taskId,
                  afterSequence: next,
                }),
              )
            ).payload,
            pageRequestId,
          ),
        );
        if (!this.watches.has(taskId) || this.watches.get(taskId) !== watch) return;
        if (!this.leaseMatches(lease) || watch.leaseEpoch !== lease.epoch) return;
        await this.mergeConversationPage(taskId, page, page.cursorRolledOver);
        next = page.nextSequence;
        pages += 1;
      }
    } finally {
      watch.queryInFlight = false;
      if (
        this.watches.get(taskId) === watch &&
        this.leaseMatches(lease) &&
        watch.leaseEpoch === lease.epoch &&
        watch.dirty
      ) {
        watch.dirty = false;
        const high = watch.pendingDirtyHighWater;
        watch.pendingDirtyHighWater = null;
        if (high !== null) await this.drainConversation(taskId, high);
      }
    }
  }

  private async drainConversation(
    taskId: NativeUuid,
    highWater: number,
  ): Promise<void> {
    const watch = this.watches.get(taskId);
    const lease = this.lease;
    if (!watch || !lease) return;
    if (watch.leaseEpoch !== lease.epoch) return;
    if (watch.queryInFlight) {
      watch.dirty = true;
      watch.pendingDirtyHighWater = Math.max(
        watch.pendingDirtyHighWater ?? 0,
        highWater,
      );
      return;
    }
    watch.queryInFlight = true;
    try {
      const seenAfter = new Set<number>();
      for (let pages = 0; pages < MAX_CONVERSATION_PAGES; pages += 1) {
        if (!this.watches.has(taskId) || this.watches.get(taskId) !== watch) return;
        if (!this.leaseMatches(lease) || watch.leaseEpoch !== lease.epoch) return;
        const current = this.conversations.get(taskId);
        const after =
          current?.nextSequence ??
          (current && highWater > current.highWater
            ? current.throughSequence
            : null);
        if (after === null || after === undefined) return;
        if (seenAfter.has(after)) {
          throw new NativeProtocolError("conversation drain cycle");
        }
        seenAfter.add(after);
        const requestId = this.createRequestId();
        const page = decodeTaskCockpitConversationResult(
          decodeQueryReply(
            (
              await this.query(
                buildTaskCockpitConversationQuery({
                  ...this.authorityFor(lease, requestId),
                  taskId,
                  afterSequence: after,
                }),
              )
            ).payload,
            requestId,
          ),
        );
        if (!this.watches.has(taskId) || this.watches.get(taskId) !== watch) return;
        if (!this.leaseMatches(lease) || watch.leaseEpoch !== lease.epoch) return;
        await this.mergeConversationPage(taskId, page, page.cursorRolledOver);
        if (page.nextSequence === null) {
          if (page.highWater >= highWater) return;
          continue;
        }
      }
    } finally {
      watch.queryInFlight = false;
      if (
        this.watches.get(taskId) === watch &&
        this.leaseMatches(lease) &&
        watch.leaseEpoch === lease.epoch &&
        watch.dirty
      ) {
        watch.dirty = false;
        const high = watch.pendingDirtyHighWater ?? highWater;
        watch.pendingDirtyHighWater = null;
        await this.drainConversation(taskId, high);
      }
    }
  }

  private async mergeConversationPage(
    taskId: NativeUuid,
    page: SemanticJournalPage,
    reset: boolean,
  ): Promise<void> {
    const existing = this.conversations.get(taskId);
    let facts: SemanticJournalFact[] =
      reset || page.cursorRolledOver || !existing ? [] : [...existing.facts];
    const positions = new Map(facts.map((fact, index) => [fact.id, index]));
    for (const fact of page.facts) {
      const position = positions.get(fact.id);
      if (position !== undefined) {
        if (fact.sequence > facts[position].sequence) facts[position] = fact;
      } else {
        positions.set(fact.id, facts.length);
        facts.push(fact);
      }
    }
    facts.sort((left, right) => left.sequence - right.sequence);
    if (facts.length > MAX_CACHED_FACTS_PER_TASK) {
      facts = facts.slice(facts.length - MAX_CACHED_FACTS_PER_TASK);
    }
    const merged: NativeCachedConversation = {
      taskId,
      afterSequence: page.afterSequence,
      throughSequence: page.throughSequence,
      highWater: page.highWater,
      oldestSequence: page.oldestSequence,
      cursorRolledOver: page.cursorRolledOver,
      nextSequence: page.nextSequence,
      facts,
      updatedAtMs: this.now(),
    };
    // Live view always updates; history cache may evict under quota.
    this.conversations.set(taskId, merged);
    try {
      await this.cache.putConversation(this.hostPublicId, merged);
    } catch {
      // Quota/history failure must not block live conversation projection.
    }
    this.emit();
  }

  private scheduleTaskRefresh(taskId: NativeUuid): void {
    if (!this.tasks.has(taskId) && this.tasks.size >= MAX_METADATA_TASKS_PER_HOST) {
      return;
    }
    if (!this.refreshQueue.includes(taskId)) this.refreshQueue.push(taskId);
    void this.pumpRefreshQueue();
  }

  private async pumpRefreshQueue(): Promise<void> {
    while (
      this.refreshInFlight < MAX_CONCURRENT_TASK_REFRESHES &&
      this.refreshQueue.length > 0
    ) {
      const taskId = this.refreshQueue.shift();
      if (!taskId) return;
      this.refreshInFlight += 1;
      void this.refreshTaskSnapshot(taskId).finally(() => {
        this.refreshInFlight -= 1;
        void this.pumpRefreshQueue();
      });
    }
  }

  private async refreshTaskSnapshot(taskId: NativeUuid): Promise<void> {
    const lease = this.lease;
    if (!lease) return;
    const requestId = this.createRequestId();
    try {
      const envelope = await this.query(
        buildTaskSnapshotQuery({
          ...this.authorityFor(lease, requestId),
          taskId,
        }),
      );
      if (!this.leaseMatches(lease)) return;
      const reply = decodeQueryReply(envelope.payload, requestId);
      if (reply.outcome.kind !== "ok") return;
      const body = reply.outcome.result.task_snapshot as { snapshot?: unknown };
      const item = decodeTaskSnapshotItem(body, { expectedTaskId: taskId });
      const current = this.tasks.get(taskId);
      if (current && current.revision > item.revision) return;
      const meta: NativeCachedTaskMeta = {
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
        updatedAtMs: this.now(),
      };
      this.tasks.set(taskId, meta);
      try {
        await this.cache.putTasks(this.hostPublicId, [...this.tasks.values()]);
      } catch {
        // Live metadata retained.
      }
      this.emit();
    } catch {
      // Soft coalesced refresh failure.
    }
  }

  private async refreshTaskListIfLive(): Promise<void> {
    if (this.connectionStatus !== "ready" || !this.lease) return;
    // A global event may introduce a previously unknown task. Refresh the
    // bounded snapshot/replay pair instead of only querying existing rows.
    this.syncGeneration += 1;
    await this.syncFromHost(this.syncGeneration);
  }

  private async reconcileOutbox(lease: SessionLease): Promise<void> {
    for (const record of [...this.outbox.values()]) {
      if (!this.leaseMatches(lease) || this.connectionStatus !== "ready") return;
      if (record.clientId === lease.clientId && record.status !== "blocked_client_mismatch") {
        await this.retryOutbox(record.commandId);
      }
    }
  }

  private async query(
    request: ConnectPayloadRequest,
  ): Promise<DecodedConnectEnvelope> {
    return this.transport.request(request.payloadKind, request.payload, {
      requestId: request.requestId ?? undefined,
      operationId: request.operationId,
      privacyClass: request.privacyClass,
      payloadVersion: request.payloadVersion,
    });
  }

  private authorityFor(lease: SessionLease, requestId: NativeUuid): NativeAuthority {
    return {
      hostPublicId: this.hostPublicId,
      clientId: lease.clientId,
      requestId,
    };
  }

  private leaseMatches(lease: SessionLease): boolean {
    return (
      this.lease !== null &&
      this.lease.epoch === lease.epoch &&
      this.lease.clientId === lease.clientId
    );
  }

  private syncStillValid(generation: number, lease: SessionLease): boolean {
    return this.syncGeneration === generation && this.leaseMatches(lease);
  }

  private requireTaskId(taskId: NativeUuid): NativeUuid {
    const id = protocolUuid(taskId);
    if (!id) throw new NativeProtocolError("invalid taskId");
    return id;
  }

  private invalidateLease(_reason: string): void {
    this.syncGeneration += 1;
    this.lease = null;
    this.capabilities = 0;
    this.replaySubscriptionId = null;
    this.refreshQueue = [];
    for (const [taskId, watch] of this.watches) {
      this.watches.set(taskId, {
        refCount: watch.refCount, subscriptionId: null, queryInFlight: false,
        dirty: false, pendingDirtyHighWater: null, earlyDirtyNotices: [], leaseEpoch: -1,
      });
    }
  }

  private clearHandoff(): void {
    this.handoffActive = false;
    this.handoffBuffer = [];
  }

  private failMisbound(reason: string): void {
    this.invalidateLease("misbound");
    this.clearHandoff();
    this.connectionStatus = "misbound";
    this.syncStatus = "error";
    this.lastError = reason;
    this.emit();
  }

  private emit(): void {
    const snapshot = this.view();
    for (const listener of this.viewListeners) listener(snapshot);
  }
}

function recordOf(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

export function peekFirstTurnId(commandId: NativeUuid): NativeUuid {
  return firstTurnIdFromCommandId(commandId);
}

export function requiredSessionCapabilities(): bigint {
  return (
    CAPABILITY_PAGED_SNAPSHOTS |
    CAPABILITY_EVENT_REPLAY |
    CAPABILITY_SEMANTIC_CONVERSATION |
    CAPABILITY_PROVIDER_INPUT |
    CAPABILITY_TASK_COCKPIT
  );
}

function lifecycleAllowsMutation(
  lifecycle: string | null,
  mutation: NativeTaskMutation,
): boolean {
  switch (mutation.kind) {
    case "settle":
      return lifecycle === "open" || lifecycle === "settled";
    case "reopen":
      return (
        lifecycle === "settled" ||
        lifecycle === "closing" ||
        lifecycle === "archived"
      );
    case "begin_close":
      return (
        lifecycle === "open" ||
        lifecycle === "settled" ||
        lifecycle === "closing"
      );
    case "delete":
      return lifecycle === "archived";
    case "rename":
      return lifecycle === "open" || lifecycle === "settled" || lifecycle === "closing";
  }
}
