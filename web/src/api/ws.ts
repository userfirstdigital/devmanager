import type {
  ComposerAccepted,
  ComposerRejected,
  ComposerSubmission,
  RemoteAction,
  RemoteActionResult,
  ResumeContext,
  TerminalScreenSnapshot,
  WebWriterLeaseState,
  WsInbound,
  WsOutbound,
} from "./types";
import { EMPTY_WRITER_LEASE } from "./types";
import { buildWebSocketUrl } from "../lib/browserIdentity";

export type WsStatus =
  | { kind: "idle" }
  | { kind: "connecting" }
  | { kind: "open" }
  | { kind: "closed"; reason?: string }
  | { kind: "unauthorized" };

/** Decoded session-output binary frame from the host. */
export interface SessionOutputFrame {
  sessionId: string;
  chunkSeq: number;
  bytes: Uint8Array;
}

export interface SessionBootstrapFrame {
  sessionId: string;
  bytes: Uint8Array;
  screen: TerminalScreenSnapshot;
}

export interface WsClientCallbacks {
  onStatus(status: WsStatus): void;
  onMessage(message: WsOutbound): void;
  onSessionOutput(frame: SessionOutputFrame): void;
  getResumeContext?(): ResumeContext;
}

interface PendingRequest {
  action: RemoteAction;
  sent: boolean;
  resolve(result: RemoteActionResult): void;
  reject(error: Error): void;
}

interface PendingComposer {
  fingerprint: string;
  submission: ComposerSubmission;
  promise: Promise<ComposerAccepted>;
  resolve(result: ComposerAccepted): void;
  reject(error: Error): void;
  inFlight: boolean;
  lastSentAt: number;
}

export class ComposerRejectedError extends Error {
  readonly mutationId: string;
  readonly code: ComposerRejected["code"];
  readonly writerLease: WebWriterLeaseState;

  constructor(rejected: ComposerRejected) {
    super(rejected.message);
    this.name = "ComposerRejectedError";
    this.mutationId = rejected.mutationId;
    this.code = rejected.code;
    this.writerLease = rejected.writerLease;
  }
}

const BINARY_FRAME_SESSION_OUTPUT = 0x01;
const STALE_SOCKET_MS = 30_000;
const LEASE_HEARTBEAT_MS = 4_000;
const COMPOSER_RETRY_MIN_MS = 250;
const COMPOSER_RETRY_MAX_MS = 1_000;
const COMPOSER_ACK_TIMEOUT_MS = 5_000;
const CLIENT_INSTANCE_ID_KEY = "devmanager.clientInstanceId";
let fallbackClientInstanceId: string | null = null;

const TRANSIENT_COMPOSER_REJECTIONS: ReadonlySet<ComposerRejected["code"]> =
  new Set([
    "leaseBusy",
    "staleGeneration",
    "nativeControllerActive",
    "mutationInFlight",
  ]);

export function isTransientComposerRejection(
  code: ComposerRejected["code"],
): boolean {
  return TRANSIENT_COMPOSER_REJECTIONS.has(code);
}

function createClientInstanceId(): string {
  if (typeof globalThis.crypto?.randomUUID === "function") {
    return globalThis.crypto.randomUUID();
  }
  return `tab-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

export function getClientInstanceId(): string {
  const storage = globalThis.sessionStorage;
  if (!storage) {
    fallbackClientInstanceId ??= createClientInstanceId();
    return fallbackClientInstanceId;
  }
  try {
    const existing = storage.getItem(CLIENT_INSTANCE_ID_KEY)?.trim();
    if (existing) return existing;
    const created = createClientInstanceId();
    storage.setItem(CLIENT_INSTANCE_ID_KEY, created);
    return created;
  } catch {
    fallbackClientInstanceId ??= createClientInstanceId();
    return fallbackClientInstanceId;
  }
}

function currentRoute(): string {
  const locationLike = globalThis.location as
    | (Location & { pathname?: string; search?: string; hash?: string })
    | undefined;
  const pathname = locationLike?.pathname || "/sessions";
  return `${pathname}${locationLike?.search ?? ""}${locationLike?.hash ?? ""}`;
}

function currentVisibility(): boolean {
  return typeof document === "undefined" || document.visibilityState !== "hidden";
}

function defaultResumeContext(): ResumeContext {
  const visible = currentVisibility();
  return {
    seenRuntimeInstanceId: null,
    seenRevision: null,
    route: currentRoute(),
    desiredSessionKey: null,
    semanticAfterSequence: null,
    visible,
    wantsWriterLease: visible,
  };
}

function composerFingerprint(submission: ComposerSubmission): string {
  return JSON.stringify([
    submission.stableSessionKey,
    submission.text,
    submission.attachments,
  ]);
}

/**
 * Decode the binary session-output frame emitted by the Rust bridge.
 * Layout (see `src/remote/web/bridge.rs::encode_session_output_frame`):
 *
 *   [0]            frame type (0x01)
 *   [1..5)         big-endian u32 session_id utf-8 length
 *   [5..5+N)       session_id utf-8
 *   [5+N..13+N)    big-endian u64 chunk_seq
 *   [13+N..)       raw PTY bytes
 */
export function decodeSessionOutputFrame(
  buffer: ArrayBuffer,
): SessionOutputFrame | null {
  const view = new DataView(buffer);
  if (view.byteLength < 1 + 4 + 8) return null;
  if (view.getUint8(0) !== BINARY_FRAME_SESSION_OUTPUT) return null;
  const idLen = view.getUint32(1, false);
  const idStart = 5;
  const idEnd = idStart + idLen;
  if (idEnd + 8 > view.byteLength) return null;
  const decoder = new TextDecoder();
  const sessionId = decoder.decode(new Uint8Array(buffer, idStart, idLen));
  const seqHigh = view.getUint32(idEnd, false);
  const seqLow = view.getUint32(idEnd + 4, false);
  const chunkSeq = seqHigh * 0x1_0000_0000 + seqLow;
  const bytes = new Uint8Array(buffer, idEnd + 8);
  return { sessionId, chunkSeq, bytes };
}

/**
 * WebSocket client for the host-authoritative protocol. Every socket open and
 * every explicit wake performs one atomic Resume; it never reconstructs
 * session focus, subscriptions, or controller ownership with legacy frames.
 */
export class WsClient {
  private ws: WebSocket | null = null;
  private stopped = false;
  private starting = false;
  private reconnectDelayMs = 1000;
  private reconnectTimer: number | null = null;
  private heartbeatTimer: number | null = null;
  private composerRetryTimer: number | null = null;
  private composerRetryDelayMs = COMPOSER_RETRY_MIN_MS;
  private lastWriterLeaseRequestAt = Number.NEGATIVE_INFINITY;
  private lastFrameAt = 0;
  private nextRequestId = 1;
  private connectionEpoch = 0;
  private visible = currentVisibility();
  private writerLease: WebWriterLeaseState = { ...EMPTY_WRITER_LEASE };
  private readonly clientInstanceId = getClientInstanceId();
  private readonly pendingRequests = new Map<number, PendingRequest>();
  private readonly pendingComposers = new Map<string, PendingComposer>();

  constructor(private readonly cb: WsClientCallbacks) {}

  async start(): Promise<void> {
    if (this.stopped || this.starting) return;
    if (this.ws?.readyState === WebSocket.OPEN) return;
    if (this.ws?.readyState === WebSocket.CONNECTING) return;

    const epoch = ++this.connectionEpoch;
    this.starting = true;
    this.cb.onStatus({ kind: "connecting" });

    try {
      const meResp = await fetch("/api/me", { credentials: "include" });
      if (!this.isCurrentEpoch(epoch)) return;
      if (meResp.status === 401) {
        this.cb.onStatus({ kind: "unauthorized" });
        return;
      }
      if (!meResp.ok) {
        this.scheduleReconnect(`me probe ${meResp.status}`);
        return;
      }
    } catch (error) {
      if (this.isCurrentEpoch(epoch)) {
        this.scheduleReconnect(`me probe error: ${error}`);
      }
      return;
    } finally {
      if (this.connectionEpoch === epoch) this.starting = false;
    }

    if (!this.isCurrentEpoch(epoch)) return;
    let socket: WebSocket;
    try {
      socket = new WebSocket(buildWebSocketUrl(location));
    } catch (error) {
      this.scheduleReconnect(`construct failed: ${error}`);
      return;
    }
    socket.binaryType = "arraybuffer";
    this.ws = socket;

    socket.onopen = () => {
      if (!this.isCurrentSocket(socket, epoch)) {
        try {
          socket.close();
        } catch {}
        return;
      }
      this.lastFrameAt = Date.now();
      this.reconnectDelayMs = 1000;
      this.cb.onStatus({ kind: "open" });
      this.startHeartbeat();
      this.resume();
      this.scheduleComposerRetry(true);
    };

    socket.onmessage = (event) => {
      if (!this.isCurrentSocket(socket, epoch)) return;
      this.lastFrameAt = Date.now();
      if (typeof event.data === "string") {
        let parsed: WsOutbound;
        try {
          parsed = JSON.parse(event.data) as WsOutbound;
        } catch {
          return;
        }
        this.observeWriterLease(parsed);
        if (parsed.type === "response") {
          this.resolvePendingRequest(parsed.id, parsed.result);
          return;
        }
        if (parsed.type === "composerAccepted") {
          this.resolvePendingComposer(parsed);
        } else if (parsed.type === "composerRejected") {
          this.handleComposerRejected(parsed);
        }
        this.cb.onMessage(parsed);
        this.continueComposerRetriesAfter(parsed);
      } else if (event.data instanceof ArrayBuffer) {
        const frame = decodeSessionOutputFrame(event.data);
        if (frame) this.cb.onSessionOutput(frame);
      }
    };

    socket.onclose = (event) => {
      if (!this.isCurrentSocket(socket, epoch)) return;
      this.stopHeartbeat();
      const reason = event.reason || `code ${event.code}`;
      this.handleRequestDisconnect(`websocket closed: ${reason}`);
      this.markPendingComposersForRetry();
      this.ws = null;
      this.writerLease = { ...EMPTY_WRITER_LEASE };
      this.cb.onStatus({ kind: "closed", reason });
      if (!this.stopped) this.scheduleReconnect(reason);
      this.scheduleComposerRetry(true);
    };

    socket.onerror = () => {
      // Browser error events are opaque; onclose owns status and reconnect.
    };
  }

  send(message: WsInbound): boolean {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return false;
    try {
      this.ws.send(JSON.stringify(message));
      return true;
    } catch {
      return false;
    }
  }

  resume(): boolean {
    const context = this.cb.getResumeContext?.() ?? defaultResumeContext();
    this.visible = context.visible;
    return this.send({
      type: "resume",
      ...context,
      clientInstanceId: this.clientInstanceId,
    });
  }

  request(action: RemoteAction): Promise<RemoteActionResult> {
    const id = this.nextRequestId++;
    return new Promise((resolve, reject) => {
      const pending: PendingRequest = {
        action,
        sent: false,
        resolve,
        reject,
      };
      this.pendingRequests.set(id, pending);
      if (!this.trySendRequest(id, pending)) {
        this.ensureWriterLease();
        this.scheduleComposerRetry();
      }
    });
  }

  submitComposer(submission: ComposerSubmission): Promise<ComposerAccepted> {
    const fingerprint = composerFingerprint(submission);
    const existing = this.pendingComposers.get(submission.mutationId);
    if (existing) {
      if (existing.fingerprint === fingerprint) return existing.promise;
      return Promise.reject(
        new Error("This mutation ID is already pending with different content."),
      );
    }

    let resolvePending: (result: ComposerAccepted) => void = () => {};
    let rejectPending: (error: Error) => void = () => {};
    const promise = new Promise<ComposerAccepted>((resolve, reject) => {
      resolvePending = resolve;
      rejectPending = reject;
    });
    this.pendingComposers.set(submission.mutationId, {
      fingerprint,
      submission,
      promise,
      resolve: resolvePending,
      reject: rejectPending,
      inFlight: false,
      lastSentAt: 0,
    });
    this.composerRetryDelayMs = COMPOSER_RETRY_MIN_MS;
    const pending = this.pendingComposers.get(submission.mutationId);
    if (pending && !this.trySendComposer(pending)) {
      if (!this.ws || this.ws.readyState !== WebSocket.OPEN) this.wake();
    }
    this.scheduleComposerRetry();
    return promise;
  }

  /**
   * Ask the host for foreground ownership before a non-idempotent action.
   * The action itself is still sent once and is never replayed automatically.
   */
  ensureWriterLease(): void {
    if (this.stopped || !this.visible || this.writerLease.youAreOwner) return;
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      this.wake();
      return;
    }
    this.requestWriterLease();
  }

  cancelComposer(mutationId: string, reason = "composer mutation superseded"): void {
    const pending = this.pendingComposers.get(mutationId);
    if (!pending) return;
    this.pendingComposers.delete(mutationId);
    pending.reject(new Error(reason));
    if (!this.hasPendingRetryWork()) this.cancelComposerRetry();
  }

  stop(): void {
    this.stopped = true;
    this.starting = false;
    ++this.connectionEpoch;
    this.stopHeartbeat();
    this.cancelComposerRetry();
    this.rejectPendingRequests("websocket stopped");
    this.rejectPendingComposers("websocket stopped");
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    const socket = this.ws;
    this.ws = null;
    if (socket) {
      try {
        socket.close();
      } catch {}
    }
    this.writerLease = { ...EMPTY_WRITER_LEASE };
  }

  /**
   * Foreground/focus recovery bypasses backoff. A healthy socket sends one
   * Resume immediately; a stale/closed socket sends its one Resume on open.
   */
  wake(): void {
    if (this.stopped) return;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.reconnectDelayMs = 1000;

    if (this.ws?.readyState === WebSocket.OPEN) {
      if (Date.now() - this.lastFrameAt <= STALE_SOCKET_MS) {
        this.resume();
        return;
      }
      const stale = this.ws;
      this.handleRequestDisconnect("stale websocket replaced");
      this.markPendingComposersForRetry();
      this.ws = null;
      this.writerLease = { ...EMPTY_WRITER_LEASE };
      ++this.connectionEpoch;
      this.stopHeartbeat();
      try {
        stale.close();
      } catch {}
      void this.start();
      return;
    }

    if (this.ws?.readyState === WebSocket.CONNECTING || this.starting) return;
    void this.start();
  }

  setVisibility(visible: boolean): void {
    this.visible = visible;
    if (visible) {
      this.wake();
      return;
    }
    this.send({
      type: "setVisibility",
      clientInstanceId: this.clientInstanceId,
      visible: false,
    });
  }

  resetRuntime(reason = "host runtime changed"): void {
    this.cancelComposerRetry();
    this.rejectPendingRequests(reason);
    this.rejectPendingComposers(reason);
    this.writerLease = { ...EMPTY_WRITER_LEASE };
  }

  leaseState(): WebWriterLeaseState {
    return this.writerLease;
  }

  private isCurrentEpoch(epoch: number): boolean {
    return !this.stopped && this.connectionEpoch === epoch;
  }

  private isCurrentSocket(socket: WebSocket, epoch: number): boolean {
    return this.isCurrentEpoch(epoch) && this.ws === socket;
  }

  private scheduleReconnect(_reason: string): void {
    if (this.stopped || this.reconnectTimer !== null) return;
    const delay = this.reconnectDelayMs;
    this.reconnectTimer = globalThis.setTimeout(() => {
      this.reconnectTimer = null;
      this.start().catch(() => this.scheduleReconnect("retry failed"));
    }, delay);
    this.reconnectDelayMs = Math.min(this.reconnectDelayMs * 2, 10_000);
  }

  private startHeartbeat(): void {
    this.stopHeartbeat();
    this.heartbeatTimer = globalThis.setInterval(() => {
      if (this.writerLease.youAreOwner) {
        this.send({
          type: "writerLeaseHeartbeat",
          clientInstanceId: this.clientInstanceId,
          expectedLeaseGeneration: this.writerLease.generation,
          visible: this.visible,
        });
      } else {
        this.send({ type: "ping" });
      }
    }, LEASE_HEARTBEAT_MS);
  }

  private stopHeartbeat(): void {
    if (this.heartbeatTimer !== null) {
      clearInterval(this.heartbeatTimer);
      this.heartbeatTimer = null;
    }
  }

  private requestWriterLease(): boolean {
    if (
      !this.visible ||
      !this.ws ||
      this.ws.readyState !== WebSocket.OPEN ||
      Date.now() - this.lastWriterLeaseRequestAt < COMPOSER_RETRY_MIN_MS
    ) {
      return false;
    }
    const sent = this.send({
      type: "acquireWriterLease",
      clientInstanceId: this.clientInstanceId,
      visible: true,
    });
    if (sent) this.lastWriterLeaseRequestAt = Date.now();
    return sent;
  }

  private trySendComposer(pending: PendingComposer): boolean {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return false;
    if (!this.writerLease.youAreOwner) {
      this.requestWriterLease();
      return false;
    }
    const sent = this.send({
      type: "composerSubmit",
      ...pending.submission,
      expectedLeaseGeneration: this.writerLease.generation,
    });
    if (sent) {
      pending.inFlight = true;
      pending.lastSentAt = Date.now();
    }
    return sent;
  }

  private trySendRequest(id: number, pending: PendingRequest): boolean {
    if (pending.sent) return true;
    if (
      !this.ws ||
      this.ws.readyState !== WebSocket.OPEN ||
      !this.writerLease.youAreOwner
    ) {
      return false;
    }
    const sent = this.send({
      type: "request",
      id,
      action: pending.action,
      expectedLeaseGeneration: this.writerLease.generation,
    });
    if (sent) pending.sent = true;
    return sent;
  }

  private hasUnsentRequests(): boolean {
    for (const pending of this.pendingRequests.values()) {
      if (!pending.sent) return true;
    }
    return false;
  }

  private hasPendingRetryWork(): boolean {
    return this.pendingComposers.size > 0 || this.hasUnsentRequests();
  }

  private flushStagedRequests(): void {
    for (const [id, pending] of this.pendingRequests) {
      if (!pending.sent) this.trySendRequest(id, pending);
    }
  }

  private scheduleComposerRetry(resetDelay = false): void {
    if (this.stopped || !this.hasPendingRetryWork()) return;
    if (resetDelay) {
      this.composerRetryDelayMs = COMPOSER_RETRY_MIN_MS;
      if (this.composerRetryTimer !== null) {
        clearTimeout(this.composerRetryTimer);
        this.composerRetryTimer = null;
      }
    }
    if (this.composerRetryTimer !== null) return;
    const delay = this.composerRetryDelayMs;
    this.composerRetryTimer = globalThis.setTimeout(() => {
      this.composerRetryTimer = null;
      this.composerRetryDelayMs = Math.min(
        delay * 2,
        COMPOSER_RETRY_MAX_MS,
      );
      this.driveComposerRetries();
    }, delay);
  }

  private cancelComposerRetry(): void {
    if (this.composerRetryTimer !== null) {
      clearTimeout(this.composerRetryTimer);
      this.composerRetryTimer = null;
    }
    this.composerRetryDelayMs = COMPOSER_RETRY_MIN_MS;
  }

  private driveComposerRetries(): void {
    if (this.stopped || !this.hasPendingRetryWork()) {
      this.cancelComposerRetry();
      return;
    }
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      this.scheduleComposerRetry();
      return;
    }
    if (!this.writerLease.youAreOwner) {
      this.requestWriterLease();
      this.scheduleComposerRetry();
      return;
    }

    this.flushStagedRequests();
    const now = Date.now();
    for (const pending of this.pendingComposers.values()) {
      if (
        pending.inFlight &&
        now - pending.lastSentAt < COMPOSER_ACK_TIMEOUT_MS
      ) {
        continue;
      }
      pending.inFlight = false;
      this.trySendComposer(pending);
    }
    if (this.hasPendingRetryWork()) this.scheduleComposerRetry();
    else this.cancelComposerRetry();
  }

  private markPendingComposersForRetry(): void {
    for (const pending of this.pendingComposers.values()) {
      pending.inFlight = false;
      pending.lastSentAt = 0;
    }
  }

  private observeWriterLease(message: WsOutbound): void {
    switch (message.type) {
      case "snapshot":
        this.writerLease = message.workspace.writerLease;
        break;
      case "delta":
        this.writerLease = message.delta.writerLease;
        break;
      case "resumeState":
      case "writerLeaseState":
      case "composerRejected":
        this.writerLease = message.writerLease;
        break;
      default:
        break;
    }
  }

  private resolvePendingRequest(id: number, result: RemoteActionResult): void {
    const pending = this.pendingRequests.get(id);
    if (!pending) return;
    this.pendingRequests.delete(id);
    pending.resolve(result);
    if (!this.hasPendingRetryWork()) this.cancelComposerRetry();
  }

  private handleRequestDisconnect(reason: string): void {
    const error = new Error(reason);
    for (const [id, pending] of this.pendingRequests) {
      if (!pending.sent) continue;
      this.pendingRequests.delete(id);
      pending.reject(error);
    }
  }

  private rejectPendingRequests(reason: string): void {
    if (this.pendingRequests.size === 0) return;
    const error = new Error(reason);
    for (const [id, pending] of this.pendingRequests) {
      this.pendingRequests.delete(id);
      pending.reject(error);
    }
    if (!this.hasPendingRetryWork()) this.cancelComposerRetry();
  }

  private resolvePendingComposer(accepted: ComposerAccepted): void {
    const pending = this.pendingComposers.get(accepted.mutationId);
    if (!pending) return;
    this.pendingComposers.delete(accepted.mutationId);
    pending.resolve(accepted);
    if (!this.hasPendingRetryWork()) this.cancelComposerRetry();
  }

  private handleComposerRejected(rejected: ComposerRejected): void {
    const pending = this.pendingComposers.get(rejected.mutationId);
    if (!pending) return;
    if (isTransientComposerRejection(rejected.code)) {
      pending.inFlight = false;
      pending.lastSentAt = 0;
      this.scheduleComposerRetry(true);
      return;
    }
    this.pendingComposers.delete(rejected.mutationId);
    pending.reject(new ComposerRejectedError(rejected));
    if (!this.hasPendingRetryWork()) this.cancelComposerRetry();
  }

  private continueComposerRetriesAfter(message: WsOutbound): void {
    if (!this.hasPendingRetryWork()) return;
    switch (message.type) {
      case "snapshot":
      case "delta":
      case "resumeState":
      case "writerLeaseState":
        if (this.writerLease.youAreOwner) this.driveComposerRetries();
        else this.scheduleComposerRetry();
        break;
      case "composerRejected":
        // A rejection itself always observes the bounded retry backoff. A
        // later authoritative ownership frame may drive immediately.
        break;
      default:
        break;
    }
  }

  private rejectPendingComposers(reason: string): void {
    if (this.pendingComposers.size === 0) return;
    const error = new Error(reason);
    for (const [mutationId, pending] of this.pendingComposers) {
      this.pendingComposers.delete(mutationId);
      pending.reject(error);
    }
    if (!this.hasPendingRetryWork()) this.cancelComposerRetry();
  }
}
