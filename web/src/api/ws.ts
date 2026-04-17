import type {
  RemoteAction,
  RemoteActionResult,
  TerminalScreenSnapshot,
  WsInbound,
  WsOutbound,
} from "./types";
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
}

interface PendingRequest {
  resolve(result: RemoteActionResult): void;
  reject(error: Error): void;
}

const BINARY_FRAME_SESSION_OUTPUT = 0x01;

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
  // JS numbers lose u64 precision at 2^53, but sequence numbers never get
  // anywhere near that range in practice — fine to downcast via Number().
  const seqHigh = view.getUint32(idEnd, false);
  const seqLow = view.getUint32(idEnd + 4, false);
  const chunkSeq = seqHigh * 0x1_0000_0000 + seqLow;
  const bytes = new Uint8Array(buffer, idEnd + 8);
  return { sessionId, chunkSeq, bytes };
}

/**
 * Minimal WebSocket client with automatic reconnect and cookie-auth
 * awareness. A missing/invalid remembered auth cookie results in a 401 during
 * the upgrade handshake; browsers surface that as a generic close event, so
 * we pre-check via `/api/me` before opening the WS and report `unauthorized`
 * up to the caller if that probe fails.
 */
export class WsClient {
  private ws: WebSocket | null = null;
  private stopped = false;
  private reconnectDelayMs = 1000;
  private reconnectTimer: number | null = null;
  private pingTimer: number | null = null;
  private nextRequestId = 1;
  private pendingRequests = new Map<number, PendingRequest>();

  constructor(private readonly cb: WsClientCallbacks) {}

  async start(): Promise<void> {
    if (this.stopped) return;
    this.cb.onStatus({ kind: "connecting" });

    // Authentication probe — cheap, one HTTP round trip, tells us whether
    // the remembered cookie is present AND signed by the current host.
    // Without this step an invalid cookie would just look like "websocket
    // closed" in Chrome devtools, which is hard to distinguish from a
    // network blip.
    try {
      const meResp = await fetch("/api/me", { credentials: "include" });
      if (meResp.status === 401) {
        this.cb.onStatus({ kind: "unauthorized" });
        return;
      }
      if (!meResp.ok) {
        this.scheduleReconnect(`me probe ${meResp.status}`);
        return;
      }
    } catch (error) {
      this.scheduleReconnect(`me probe error: ${error}`);
      return;
    }

    const url = buildWebSocketUrl(location);
    let socket: WebSocket;
    try {
      socket = new WebSocket(url);
    } catch (error) {
      this.scheduleReconnect(`construct failed: ${error}`);
      return;
    }
    socket.binaryType = "arraybuffer";
    this.ws = socket;

    socket.onopen = () => {
      this.reconnectDelayMs = 1000;
      this.cb.onStatus({ kind: "open" });
      this.startHeartbeat();
    };

    socket.onmessage = (event) => {
      if (typeof event.data === "string") {
        let parsed: WsOutbound;
        try {
          parsed = JSON.parse(event.data) as WsOutbound;
        } catch {
          return;
        }
        if (parsed.type === "response") {
          this.resolvePendingRequest(parsed.id, parsed.result);
          return;
        }
        this.cb.onMessage(parsed);
      } else if (event.data instanceof ArrayBuffer) {
        const frame = decodeSessionOutputFrame(event.data);
        if (frame) this.cb.onSessionOutput(frame);
      }
    };

    socket.onclose = (event) => {
      this.stopHeartbeat();
      const reason = event.reason || `code ${event.code}`;
      this.rejectPendingRequests(`websocket closed: ${reason}`);
      this.ws = null;
      this.cb.onStatus({
        kind: "closed",
        reason,
      });
      if (!this.stopped) {
        this.scheduleReconnect(reason);
      }
    };

    socket.onerror = () => {
      // onclose will fire immediately after and handle reconnect; the error
      // event itself carries no useful detail in browsers.
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

  request(action: RemoteAction): Promise<RemoteActionResult> {
    const id = this.nextRequestId++;
    return new Promise((resolve, reject) => {
      this.pendingRequests.set(id, { resolve, reject });
      if (!this.send({ type: "request", id, action })) {
        this.pendingRequests.delete(id);
        reject(new Error("WebSocket is not connected."));
      }
    });
  }

  stop(): void {
    this.stopped = true;
    this.stopHeartbeat();
    this.rejectPendingRequests("websocket stopped");
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.ws) {
      try {
        this.ws.close();
      } catch {}
      this.ws = null;
    }
  }

  private scheduleReconnect(_reason: string): void {
    if (this.stopped) return;
    if (this.reconnectTimer !== null) return;
    const delay = this.reconnectDelayMs;
    this.reconnectTimer = window.setTimeout(() => {
      this.reconnectTimer = null;
      this.start().catch(() => this.scheduleReconnect("retry failed"));
    }, delay);
    this.reconnectDelayMs = Math.min(this.reconnectDelayMs * 2, 10_000);
  }

  private startHeartbeat(): void {
    this.stopHeartbeat();
    this.pingTimer = window.setInterval(() => {
      this.send({ type: "ping" });
    }, 20_000);
  }

  private stopHeartbeat(): void {
    if (this.pingTimer !== null) {
      clearInterval(this.pingTimer);
      this.pingTimer = null;
    }
  }

  private resolvePendingRequest(id: number, result: RemoteActionResult): void {
    const pending = this.pendingRequests.get(id);
    if (!pending) return;
    this.pendingRequests.delete(id);
    pending.resolve(result);
  }

  private rejectPendingRequests(reason: string): void {
    if (this.pendingRequests.size === 0) return;
    const error = new Error(reason);
    for (const [id, pending] of this.pendingRequests) {
      this.pendingRequests.delete(id);
      pending.reject(error);
    }
  }
}
