import { buildWebSocketUrl } from "../lib/browserIdentity";
import {
  CONNECT_BROWSER_E2E_HOLD,
  ConnectCryptoHoldError,
  resolveConnectCrypto,
  type ConnectCryptoLoader,
  type ConnectCryptoRuntime,
  type ConnectWasmHandshake,
  type ConnectWasmTransport,
} from "./crypto";

export { CONNECT_BROWSER_E2E_HOLD } from "./crypto";

export type ConnectTransportKind = "direct" | "relay";

export type ConnectRouteReason =
  | "preferredDirect"
  | "directUnavailable"
  | "explicitFallback"
  | "advertisedRelayAbsent"
  | "advertisedRelayInvalid";

export interface ConnectLocationLike {
  protocol: string;
  host: string;
}
export type ConnectRoute =
  | {
      kind: ConnectTransportKind;
      url: string;
      reason: Exclude<
        ConnectRouteReason,
        "advertisedRelayAbsent" | "advertisedRelayInvalid"
      >;
    }
  | {
      kind: "noRoute";
      reason: "advertisedRelayAbsent" | "advertisedRelayInvalid";
    };

export interface ConnectRouteSelection {
  preferDirect?: boolean;
  directAvailable?: boolean;
  /** Host-authenticated relay advertisement; never a user-entered fallback. */
  relayUrl?: string | null;
  location: ConnectLocationLike;
}

export type InboundChannel = "text" | "binary";
export type InboundFrameClass = "ok" | "oversized" | "malformed";

/** Host `WEB_OUTBOUND_MAX_BYTES` in `src/remote/web/bridge.rs`. */
export const MAX_INBOUND_TEXT_BYTES = 4 * 1_024 * 1_024;
export const MAX_INBOUND_BINARY_BYTES = 4 * 1_024 * 1_024;
export const MAX_PENDING_OUTBOUND_ITEMS = 256;
export const MAX_PENDING_OUTBOUND_BYTES = 8 * 1_024 * 1_024;
export const MIN_SESSION_OUTPUT_FRAME_BYTES = 1 + 4 + 8;
export const SESSION_OUTPUT_FRAME_TYPE = 0x01;

const inboundEncoder = new TextEncoder();
const MAX_ADVERTISED_RELAY_URL_BYTES = 2_048;
const PAIRING_QUERY_KEYS = new Set([
  "t",
  "token",
  "pairing",
  "pairingToken",
  "pairing_token",
]);

/** Validate a host-advertised WebSocket relay without accepting pairing data. */
export function parseAdvertisedRelayUrl(raw: unknown): string | null {
  if (typeof raw !== "string") return null;
  const trimmed = raw.trim();
  if (!trimmed || trimmed.length > MAX_ADVERTISED_RELAY_URL_BYTES) return null;
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    return null;
  }
  if (parsed.protocol !== "ws:" && parsed.protocol !== "wss:") return null;
  if (!parsed.hostname || parsed.username || parsed.password || parsed.hash) {
    return null;
  }
  for (const key of parsed.searchParams.keys()) {
    if (PAIRING_QUERY_KEYS.has(key)) return null;
  }
  return trimmed;
}

function sharesDirectOrigin(
  advertised: string,
  location: ConnectLocationLike,
): boolean {
  try {
    const advertisedUrl = new URL(advertised);
    const directUrl = new URL(buildWebSocketUrl(location));
    return (
      advertisedUrl.protocol === directUrl.protocol &&
      advertisedUrl.host === directUrl.host
    );
  } catch {
    return false;
  }
}

export function selectConnectRoute(
  selection: ConnectRouteSelection,
): ConnectRoute {
  const preferDirect = selection.preferDirect !== false;
  const directAvailable = selection.directAvailable !== false;
  const hostUrl = buildWebSocketUrl(selection.location);
  if (preferDirect && directAvailable) {
    return {
      kind: "direct",
      url: hostUrl,
      reason: "preferredDirect",
    };
  }
  if (
    selection.relayUrl === undefined ||
    selection.relayUrl === null ||
    selection.relayUrl.trim() === ""
  ) {
    return { kind: "noRoute", reason: "advertisedRelayAbsent" };
  }
  const advertised = parseAdvertisedRelayUrl(selection.relayUrl);
  if (!advertised || sharesDirectOrigin(advertised, selection.location)) {
    return { kind: "noRoute", reason: "advertisedRelayInvalid" };
  }
  return {
    kind: "relay",
    url: advertised,
    reason: directAvailable ? "explicitFallback" : "directUnavailable",
  };
}

export function allowsRawTerminal(route: ConnectRoute): boolean {
  return route.kind === "direct";
}

export function isRawTerminalWriterFrame(
  frame: { type: string } & Record<string, unknown>,
): boolean {
  return (
    frame.type === "input" ||
    frame.type === "pasteImage" ||
    frame.type === "resize"
  );
}

export function inboundTextByteLength(data: string): number {
  return inboundEncoder.encode(data).byteLength;
}

export function classifyInboundFrame(input: {
  channel: InboundChannel;
  byteLength: number;
  frameType?: number;
}): InboundFrameClass {
  if (input.byteLength < 0 || !Number.isFinite(input.byteLength)) {
    return "malformed";
  }
  if (input.channel === "text") {
    return input.byteLength > MAX_INBOUND_TEXT_BYTES ? "oversized" : "ok";
  }
  if (input.byteLength > MAX_INBOUND_BINARY_BYTES) return "oversized";
  if (
    input.byteLength < MIN_SESSION_OUTPUT_FRAME_BYTES ||
    input.frameType !== SESSION_OUTPUT_FRAME_TYPE
  ) {
    return "malformed";
  }
  return "ok";
}
/**
 * Browser-side Connect transport boundary.
 *
 * The native Connect protocol uses Noise XX + ChaChaPoly + BLAKE2s and its
 * sealed frame format is intentionally not approximated with browser JSON.
 * The Connect client below is held until the audited Rust/WASM module loads;
 * callers must surface that state and cannot silently fall back to the
 * plaintext/legacy `/api/ws` route for Connect application traffic.
 */

export class ConnectBrowserTransportError extends ConnectCryptoHoldError {

  constructor(message = "Connect browser E2E transport is not available in this build") {
    super(message);
    this.name = "ConnectBrowserTransportError";
  }
}

export type ConnectBrowserTransportState =
  | { kind: "held"; code: typeof CONNECT_BROWSER_E2E_HOLD; reason: string }
  | { kind: "ready" };

/** The browser-visible endpoint. The state machine opens it only after the
 * exact Rust/WASM Noise implementation has loaded successfully. */
export function buildConnectWebSocketUrl(
  locationLike: Pick<Location, "protocol" | "host"> = window.location,
): string {
  const scheme = locationLike.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${locationLike.host}/api/connect`;
}

export function connectBrowserTransportState(): ConnectBrowserTransportState {
  return {
    kind: "held",
    code: CONNECT_BROWSER_E2E_HOLD,
    reason:
      "Connect requires the Rust/WASM Noise XX/ChaChaPoly/BLAKE2s leaf; Connect will not downgrade to /api/ws.",
  };
}

/**
 * Native-compatible frame codec used by the eventual audited Noise client.
 * Keeping this codec now prevents a second, incompatible browser framing
 * format from being invented later. It performs no encryption itself.
 */
export function encodeConnectSealedFrame(frame: {
  version: number;
  sequence: bigint;
  nonce: Uint8Array;
  ciphertext: Uint8Array;
  tag: Uint8Array;
}): Uint8Array {
  if (frame.nonce.byteLength !== 16 || frame.tag.byteLength !== 32) {
    throw new ConnectBrowserTransportError("invalid Connect sealed-frame nonce/tag");
  }
  if (!Number.isInteger(frame.version) || frame.version !== 1) {
    throw new ConnectBrowserTransportError("unsupported Connect sealed-frame version");
  }
  if (frame.sequence <= 0n || frame.sequence > 0xffff_ffff_ffff_ffffn) {
    throw new ConnectBrowserTransportError("invalid Connect sealed-frame sequence");
  }
  const output = new Uint8Array(1 + 8 + 16 + frame.ciphertext.byteLength + 32);
  const view = new DataView(output.buffer);
  output[0] = frame.version;
  view.setBigUint64(1, frame.sequence, false);
  output.set(frame.nonce, 9);
  output.set(frame.ciphertext, 25);
  output.set(frame.tag, 25 + frame.ciphertext.byteLength);
  return output;
}

export function decodeConnectSealedFrame(input: ArrayBuffer | Uint8Array): {
  version: number;
  sequence: bigint;
  nonce: Uint8Array;
  ciphertext: Uint8Array;
  tag: Uint8Array;
} {
  const bytes = input instanceof Uint8Array ? input : new Uint8Array(input);
  if (bytes.byteLength < 1 + 8 + 16 + 32) {
    throw new ConnectBrowserTransportError("truncated Connect sealed frame");
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const version = view.getUint8(0);
  const sequence = view.getBigUint64(1, false);
  if (version !== 1 || sequence === 0n) {
    throw new ConnectBrowserTransportError("invalid Connect sealed frame header");
  }
  return {
    version,
    sequence,
    nonce: bytes.slice(9, 25),
    ciphertext: bytes.slice(25, -32),
    tag: bytes.slice(-32),
  };
}

/** The first direct Connect websocket frame binds the crypto prologue. */
export const CONNECT_GREETING_MAGIC = "DMCN1" as const;
export const CONNECT_GREETING_BYTES = 5 + 16 + 16 + 16;

export interface ConnectGreeting {
  hostPublicId: Uint8Array;
  routeId: Uint8Array;
  sessionId: Uint8Array;
}

export function parseConnectGreeting(
  input: ArrayBuffer | Uint8Array,
): ConnectGreeting | null {
  const bytes = input instanceof Uint8Array ? input : new Uint8Array(input);
  if (bytes.byteLength !== CONNECT_GREETING_BYTES) return null;
  const decoder = new TextDecoder();
  if (decoder.decode(bytes.slice(0, 5)) !== CONNECT_GREETING_MAGIC) return null;
  const hostPublicId = bytes.slice(5, 21);
  const routeId = bytes.slice(21, 37);
  const sessionId = bytes.slice(37, 53);
  if (
    hostPublicId.every((value) => value === 0) ||
    routeId.every((value) => value === 0) ||
    sessionId.every((value) => value === 0)
  ) {
    return null;
  }
  return { hostPublicId, routeId, sessionId };
}

export type ConnectConnectionState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "connecting" }
  | { kind: "handshaking" }
  | { kind: "ready" }
  | { kind: "resyncing" }
  | { kind: "reconnecting" }
  | { kind: "held"; code: typeof CONNECT_BROWSER_E2E_HOLD; reason: string }
  | { kind: "closed"; reason: string };

export interface ConnectEnvelopeLimits {
  max_physical_frame_bytes: number;
  max_reassembled_message_bytes: number;
  max_page_items: number;
  max_page_encoded_bytes: number;
  max_chunk_bytes: number;
  max_cumulative_bytes: number;
}

export interface ConnectEnvelopeJson {
  protocolMajor: number;
  protocolMinor: number;
  connectionId: string;
  sessionId: string;
  channelId: string;
  channel: "critical" | "durable" | "ephemeral";
  sequence: number;
  requestId: string | null;
  operationId: string | null;
  limits: ConnectEnvelopeLimits;
  compression: "none";
  privacyClass: "local_only" | "managed_metadata" | "raw_content";
  payloadKind: number;
  payloadVersion: number;
  payloadBase64: string;
}

export interface DecodedConnectEnvelope extends ConnectEnvelopeJson {
  payload: unknown;
}

/** A typed application payload to be sent over the authenticated channel. */
export interface ConnectPayloadRequest {
  payloadKind: number;
  payload: unknown;
  requestId?: string | null;
  operationId?: string | null;
  privacyClass?: ConnectEnvelopeJson["privacyClass"];
  payloadVersion?: number;
}

export interface ConnectRequestOptions {
  requestId?: string;
  operationId?: string | null;
  privacyClass?: ConnectEnvelopeJson["privacyClass"];
  payloadVersion?: number;
}

type ConnectSocket = {
  readonly readyState: number;
  binaryType: BinaryType;
  onopen: ((event: unknown) => void) | null;
  onmessage: ((event: { data: unknown }) => void) | null;
  onclose: ((event: { code?: number; reason?: string }) => void) | null;
  onerror: ((event: unknown) => void) | null;
  send(data: Uint8Array): void;
  close(): void;
};

type ConnectSocketFactory = (url: string) => ConnectSocket;

interface PendingConnectRequest {
  operationId: string | null;
  resolve(envelope: DecodedConnectEnvelope): void;
  reject(error: Error): void;
}

export interface ConnectBrowserTransportOptions {
  firstPairing: boolean;
  privateKey: Uint8Array;
  localPublic: Uint8Array;
  expectedRemote?: Uint8Array;
  hostPublicId?: Uint8Array;
  devicePublicId?: Uint8Array;
  purpose?: 1 | 2;
  role?: "initiator" | "responder";
  openedAtUnix?: bigint;
  directReachable?: boolean;
  clientId?: string | null;
  capabilities?: number;
  capabilityGrant?: unknown;
  limits?: ConnectEnvelopeLimits;
  location?: ConnectLocationLike;
  cryptoLoader?: ConnectCryptoLoader;
  socketFactory?: ConnectSocketFactory;
  onState?(state: ConnectConnectionState): void;
  onEnvelope?(envelope: DecodedConnectEnvelope): void;
  onCapabilityGrant?(grant: unknown): void;
  onClientId?(clientId: string): void;
}

const DEFAULT_CONNECT_LIMITS: ConnectEnvelopeLimits = {
  max_physical_frame_bytes: 1 * 1024 * 1024,
  max_reassembled_message_bytes: 16 * 1024 * 1024,
  max_page_items: 1_000,
  max_page_encoded_bytes: 512 * 1024,
  max_chunk_bytes: 256 * 1024,
  max_cumulative_bytes: 16 * 1024 * 1024,
};

const CONNECT_RECONNECT_MIN_MS = 1_000;
const CONNECT_RECONNECT_MAX_MS = 10_000;
const CONNECT_HELLO_KIND = 1;
const CONNECT_CAPABILITIES_KIND = 2;
const CONNECT_QUERY_KIND = 5;
const CONNECT_COMMAND_KIND = 6;
const CONNECT_RESYNC_KIND = 15;
const CONNECT_ERROR_KIND = 16;
const CONNECT_QUERY_REPLY_KIND = 18;

const CRITICAL_PAYLOAD_KINDS = new Set([
  CONNECT_HELLO_KIND,
  CONNECT_CAPABILITIES_KIND,
  CONNECT_QUERY_KIND,
  CONNECT_COMMAND_KIND,
  CONNECT_RESYNC_KIND,
  CONNECT_ERROR_KIND,
  18,
  7,
  8,
]);
const EPHEMERAL_PAYLOAD_KINDS = new Set([9, 10, 11]);

function channelForPayloadKind(
  payloadKind: number,
): ConnectEnvelopeJson["channel"] {
  if (CRITICAL_PAYLOAD_KINDS.has(payloadKind)) return "critical";
  if (EPHEMERAL_PAYLOAD_KINDS.has(payloadKind)) return "ephemeral";
  return "durable";
}

function isUuidV7(value: unknown): value is string {
  if (typeof value !== "string") return false;
  return /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(
    value,
  );
}

function sameLimits(
  left: ConnectEnvelopeLimits,
  right: ConnectEnvelopeLimits,
): boolean {
  return (
    left.max_physical_frame_bytes === right.max_physical_frame_bytes &&
    left.max_reassembled_message_bytes === right.max_reassembled_message_bytes &&
    left.max_page_items === right.max_page_items &&
    left.max_page_encoded_bytes === right.max_page_encoded_bytes &&
    left.max_chunk_bytes === right.max_chunk_bytes &&
    left.max_cumulative_bytes === right.max_cumulative_bytes
  );
}

function isConnectLimits(value: unknown): value is ConnectEnvelopeLimits {
  if (typeof value !== "object" || value === null) return false;
  const record = value as Record<string, unknown>;
  const keys = [
    "max_physical_frame_bytes",
    "max_reassembled_message_bytes",
    "max_page_items",
    "max_page_encoded_bytes",
    "max_chunk_bytes",
    "max_cumulative_bytes",
  ];
  return (
    Object.keys(record).length === keys.length &&
    keys.every(
      (key) =>
        typeof record[key] === "number" &&
        Number.isSafeInteger(record[key]) &&
        (record[key] as number) > 0,
    )
  );
}

function hasExactKeys(
  record: Record<string, unknown>,
  keys: readonly string[],
): boolean {
  return (
    Object.keys(record).length === keys.length &&
    keys.every((key) => Object.prototype.hasOwnProperty.call(record, key))
  );
}

/**
 * Resync is completed only by the host's exact bounded snapshot contract.
 * This deliberately validates the wire shape here instead of treating any
 * QueryReply as an authoritative stream reset.
 */
function isBoundedResyncSnapshot(
  payload: unknown,
  requestId: string | null,
  limits: ConnectEnvelopeLimits,
): boolean {
  if (requestId === null || typeof payload !== "object" || payload === null) {
    return false;
  }
  const reply = payload as Record<string, unknown>;
  if (!hasExactKeys(reply, ["request_id", "outcome"]) || reply.request_id !== requestId) {
    return false;
  }
  if (typeof reply.outcome !== "object" || reply.outcome === null) return false;
  const outcome = reply.outcome as Record<string, unknown>;
  if (!hasExactKeys(outcome, ["ok"])) return false;
  if (typeof outcome.ok !== "object" || outcome.ok === null) return false;
  const ok = outcome.ok as Record<string, unknown>;
  if (!hasExactKeys(ok, ["snapshot_page"])) return false;
  if (typeof ok.snapshot_page !== "object" || ok.snapshot_page === null) {
    return false;
  }
  const snapshotResult = ok.snapshot_page as Record<string, unknown>;
  if (!hasExactKeys(snapshotResult, ["page"])) return false;
  if (typeof snapshotResult.page !== "object" || snapshotResult.page === null) {
    return false;
  }
  const page = snapshotResult.page as Record<string, unknown>;
  if (
    !hasExactKeys(page, [
      "snapshot_id",
      "through_sequence",
      "section",
      "after_item",
      "items",
      "encoded_bytes",
      "next_cursor",
    ]) ||
    !isUuidV7(page.snapshot_id) ||
    page.section !== "tasks" ||
    typeof page.through_sequence !== "number" ||
    !Number.isSafeInteger(page.through_sequence) ||
    page.through_sequence < 0 ||
    page.after_item !== null ||
    !Array.isArray(page.items) ||
    page.items.length > limits.max_page_items ||
    typeof page.encoded_bytes !== "number" ||
    !Number.isSafeInteger(page.encoded_bytes) ||
    page.encoded_bytes <= 0 ||
    page.encoded_bytes > limits.max_page_encoded_bytes
  ) {
    return false;
  }
  const nextCursor = page.next_cursor;
  if (nextCursor === null) return true;
  if (Array.isArray(nextCursor)) {
    return nextCursor.length <= limits.max_cumulative_bytes;
  }
  return (
    typeof nextCursor === "string" &&
    new TextEncoder().encode(nextCursor).byteLength <= limits.max_cumulative_bytes
  );
}

function base64Encode(bytes: Uint8Array): string {
  let binary = "";
  for (let offset = 0; offset < bytes.byteLength; offset += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
  }
  return btoa(binary);
}

function base64Decode(value: string): Uint8Array {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

function secureRandomBytes(length: number): Uint8Array {
  const bytes = new Uint8Array(length);
  if (typeof globalThis.crypto?.getRandomValues !== "function") {
    throw new ConnectBrowserTransportError("Connect browser randomness unavailable");
  }
  globalThis.crypto.getRandomValues(bytes);
  return bytes;
}

function uuidV7(): string {
  const bytes = secureRandomBytes(16);
  const timestamp = BigInt(Date.now());
  for (let index = 0; index < 6; index += 1) {
    bytes[index] = Number((timestamp >> BigInt((5 - index) * 8)) & 0xffn);
  }
  bytes[6] = (bytes[6] & 0x0f) | 0x70;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (value) => value.toString(16).padStart(2, "0"));
  return `${hex.slice(0, 4).join("")}-${hex.slice(4, 6).join("")}-${hex
    .slice(6, 8)
    .join("")}-${hex.slice(8, 10).join("")}-${hex.slice(10).join("")}`;
}

/** Generate a protocol-valid request/operation identity for a Connect call. */
export function createConnectRequestId(): string {
  return uuidV7();
}

function uuidFromBytes(bytes: Uint8Array): string {
  if (
    bytes.byteLength !== 16 ||
    (bytes[6] & 0xf0) !== 0x70 ||
    (bytes[8] & 0xc0) !== 0x80
  ) {
    throw new ConnectBrowserTransportError("Connect greeting identifier rejected");
  }
  const hex = Array.from(bytes, (value) => value.toString(16).padStart(2, "0"));
  return `${hex.slice(0, 4).join("")}-${hex.slice(4, 6).join("")}-${hex
    .slice(6, 8)
    .join("")}-${hex.slice(8, 10).join("")}-${hex.slice(10).join("")}`;
}

function locationForConnect(locationLike?: ConnectLocationLike): ConnectLocationLike {
  if (locationLike) return locationLike;
  const current = globalThis.location;
  if (!current?.protocol || !current.host) {
    throw new ConnectBrowserTransportError("Connect browser origin unavailable");
  }
  return { protocol: current.protocol, host: current.host };
}

/**
 * One state machine for the browser Connect route. The browser only handles
 * websocket events and byte transport; Noise, sealed-frame authentication,
 * and MessagePack are Rust/WASM operations.
 */
export class ConnectBrowserTransport {
  private stateValue: ConnectConnectionState = { kind: "idle" };
  private socket: ConnectSocket | null = null;
  private runtime: ConnectCryptoRuntime | null = null;
  private handshake: ConnectWasmHandshake | null = null;
  private transport: ConnectWasmTransport | null = null;
  private greeting: ConnectGreeting | null = null;
  private connectionId = "";
  private channelId = "";
  private outboundSequence = 1;
  private inboundSequence = 0;
  private negotiatedLimits: ConnectEnvelopeLimits | null = null;
  private helloAccepted = false;
  private resyncInFlight = false;
  private resyncRequestId: string | null = null;
  private stopped = false;
  private reconnectTimer: ReturnType<typeof globalThis.setTimeout> | null = null;
  private reconnectDelayMs = CONNECT_RECONNECT_MIN_MS;
  private connectionEpoch = 0;
  private readonly envelopeListeners = new Set<
    (envelope: DecodedConnectEnvelope) => void
  >();
  private readonly pendingRequests = new Map<string, PendingConnectRequest>();
  private readonly stateListeners = new Set<
    (state: ConnectConnectionState) => void
  >();

  constructor(private readonly options: ConnectBrowserTransportOptions) {}

  state(): ConnectConnectionState {
    return this.stateValue;
  }

  subscribe(listener: (state: ConnectConnectionState) => void): () => void {
    this.stateListeners.add(listener);
    listener(this.stateValue);
    return () => this.stateListeners.delete(listener);
  }

  /** Subscribe to authenticated application envelopes without replacing the
   * option callback owned by the transport creator. */
  subscribeEnvelope(
    listener: (envelope: DecodedConnectEnvelope) => void,
  ): () => void {
    this.envelopeListeners.add(listener);
    return () => this.envelopeListeners.delete(listener);
  }

  async start(): Promise<void> {
    if (this.stopped || this.stateValue.kind === "loading") return;
    if (
      this.stateValue.kind === "connecting" ||
      this.stateValue.kind === "handshaking" ||
      this.stateValue.kind === "ready" ||
      this.stateValue.kind === "resyncing"
    ) {
      return;
    }
    if (this.reconnectTimer !== null) {
      globalThis.clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    const epoch = ++this.connectionEpoch;
    this.setState({ kind: "loading" });
    try {
      this.runtime = await resolveConnectCrypto(this.options.cryptoLoader);
    } catch (error) {
      const reason =
        error instanceof ConnectCryptoHoldError
          ? error.message
          : "Connect Rust/WASM crypto is unavailable";
      this.setState({ kind: "held", code: CONNECT_BROWSER_E2E_HOLD, reason });
      return;
    }
    if (this.stopped || epoch !== this.connectionEpoch) return;
    this.openSocket(epoch);
  }

  stop(): void {
    this.stopped = true;
    ++this.connectionEpoch;
    if (this.reconnectTimer !== null) {
      globalThis.clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    const socket = this.socket;
    this.socket = null;
    try {
      socket?.close();
    } catch {
      // Browser close is best effort; state is already terminal.
    }
    this.handshake = null;
    this.transport = null;
    this.rejectPendingRequests("Connect transport stopped");
    this.helloAccepted = false;
    this.resyncInFlight = false;
    this.resyncRequestId = null;
    this.setState({ kind: "closed", reason: "Connect transport stopped" });
  }

  /** Explicit replay/resync request; no legacy websocket downgrade exists. */
  requestResync(reason: "gap" | "replay_unavailable" = "gap"): boolean {
    if (this.stateValue.kind !== "ready" && this.stateValue.kind !== "resyncing") {
      return false;
    }
    if (this.resyncInFlight) return true;
    this.setState({ kind: "resyncing" });
    const requestId = uuidV7();
    this.resyncRequestId = requestId;
    const sent = this.sendEnvelope(CONNECT_RESYNC_KIND, {
      channel_sequence: this.inboundSequence,
      newest_sequence: this.inboundSequence,
      reason,
    }, { requestId });
    this.resyncInFlight = sent;
    if (!sent) this.resyncRequestId = null;
    return sent;
  }

  sendPayload(
    payloadKind: number,
    payload: unknown,
    options: ConnectRequestOptions = {},
  ): boolean {
    if (this.stateValue.kind !== "ready") return false;
    return this.sendEnvelope(payloadKind, payload, options);
  }

  /**
   * Send a request-lane payload and resolve it only when the authenticated
   * response carries the same request/operation correlation. This is the
   * transport-neutral primitive used by a Connect projection client; legacy
   * WebAction values must be translated to typed domain payloads by the host
   * adapter before calling it.
   */
  request(
    payloadKind: number,
    payload: unknown,
    options: ConnectRequestOptions = {},
  ): Promise<DecodedConnectEnvelope> {
    if (this.stopped) {
      return Promise.reject(new ConnectBrowserTransportError("Connect transport stopped"));
    }
    if (this.stateValue.kind !== "ready") {
      return Promise.reject(new ConnectBrowserTransportError("Connect transport is not ready"));
    }
    const requestId = options.requestId ?? uuidV7();
    if (!isUuidV7(requestId)) {
      return Promise.reject(new ConnectBrowserTransportError("invalid Connect request identity"));
    }
    const operationId = options.operationId ?? null;
    if (operationId !== null && !isUuidV7(operationId)) {
      return Promise.reject(new ConnectBrowserTransportError("invalid Connect operation identity"));
    }
    if (
      typeof payload === "object" &&
      payload !== null &&
      "request_id" in payload &&
      (payload as { request_id?: unknown }).request_id !== requestId
    ) {
      return Promise.reject(
        new ConnectBrowserTransportError("Connect request correlation mismatch"),
      );
    }
    return new Promise<DecodedConnectEnvelope>((resolve, reject) => {
      this.pendingRequests.set(requestId, { operationId, resolve, reject });
      const sent = this.sendEnvelope(payloadKind, payload, {
        requestId,
        operationId,
        privacyClass: options.privacyClass,
        payloadVersion: options.payloadVersion,
      });
      if (!sent) {
        this.pendingRequests.delete(requestId);
        reject(new ConnectBrowserTransportError("Connect request could not be sent"));
      }
    });
  }

  private openSocket(epoch: number): void {
    const locationLike = locationForConnect(this.options.location);
    const url = buildConnectWebSocketUrl(locationLike);
    const factory =
      this.options.socketFactory ??
      ((address: string) => new WebSocket(address) as unknown as ConnectSocket);
    let socket: ConnectSocket;
    try {
      // Same-origin `/api/connect` carries the paired cookie and browser
      // Origin automatically. Pairing data is never placed in the URL.
      socket = factory(url);
    } catch {
      this.setState({ kind: "closed", reason: "Connect socket construction failed" });
      return;
    }
    this.socket = socket;
    this.greeting = null;
    this.handshake = null;
    this.transport = null;
    this.connectionId = uuidV7();
    this.channelId = uuidV7();
    this.outboundSequence = 1;
    // A new Noise channel has a new connection/channel identity and a fresh
    // receive sequence. Carrying the old channel cursor across the handshake
    // would make the first frame look like a replay and would request resync
    // against a channel that no longer exists.
    this.inboundSequence = 0;
    this.negotiatedLimits = null;
    this.helloAccepted = false;
    this.resyncInFlight = false;
    this.resyncRequestId = null;
    socket.binaryType = "arraybuffer";
    this.setState({ kind: "connecting" });
    socket.onopen = () => {
      if (epoch !== this.connectionEpoch || this.socket !== socket) return;
      // The server greeting is the first frame; handshake starts only after
      // DMCN1 binds route/session IDs into the Rust prologue.
    };
    socket.onmessage = (event) => {
      if (epoch !== this.connectionEpoch || this.socket !== socket) return;
      void this.handleMessage(event.data);
    };
    socket.onclose = () => {
      if (epoch !== this.connectionEpoch || this.socket !== socket) return;
      this.socket = null;
      this.handshake = null;
      this.transport = null;
      this.helloAccepted = false;
      this.negotiatedLimits = null;
      this.resyncInFlight = false;
      this.resyncRequestId = null;
      this.rejectPendingRequests("Connect socket closed");
      if (!this.stopped) this.scheduleReconnect();
    };
    socket.onerror = () => {
      // onclose owns reconnect and state transitions; browser errors are opaque.
    };
  }

  private async handleMessage(data: unknown): Promise<void> {
    if (typeof data === "string" || data instanceof Blob) {
      this.protocolFailure();
      return;
    }
    const bytes =
      data instanceof ArrayBuffer
        ? new Uint8Array(data)
        : data instanceof Uint8Array
          ? data
          : null;
    if (!bytes) {
      this.protocolFailure();
      return;
    }
    try {
      if (!this.greeting) {
        this.beginHandshake(bytes);
        return;
      }
      const stateKind = this.stateValue.kind;
      if (stateKind === "handshaking" && this.handshake) {
        this.advanceHandshake(bytes);
        return;
      }
      if (
        stateKind === "handshaking" ||
        stateKind === "ready" ||
        stateKind === "resyncing"
      ) {
        if (stateKind === "handshaking" && !this.transport) {
          this.protocolFailure();
          return;
        }
        this.handleSealedEnvelope(bytes);
        return;
      }
      this.protocolFailure();
    } catch {
      this.protocolFailure();
    }
  }

  private beginHandshake(bytes: Uint8Array): void {
    const greeting = parseConnectGreeting(bytes);
    const runtime = this.runtime;
    if (!greeting || !runtime) throw new Error("greeting rejected");
    if (
      this.options.hostPublicId &&
      base64Encode(this.options.hostPublicId) !== base64Encode(greeting.hostPublicId)
    ) {
      throw new Error("host binding rejected");
    }
    this.greeting = greeting;
    const hostPublicId = this.options.hostPublicId ?? greeting.hostPublicId;
    this.handshake = new runtime.WasmConnectHandshake(
      runtime.connect_noise_pattern(this.options.firstPairing),
      this.options.firstPairing,
      this.options.role === "responder" ? 1 : 0,
      this.options.privateKey,
      this.options.localPublic,
      this.options.expectedRemote,
      hostPublicId,
      this.options.devicePublicId,
      greeting.routeId,
      greeting.sessionId,
      this.options.purpose ?? 1,
      this.options.openedAtUnix ?? BigInt(Math.floor(Date.now() / 1000)),
      this.options.directReachable ?? true,
    );
    this.setState({ kind: "handshaking" });
    if (this.options.role !== "responder") this.sendHandshakeMessage();
  }

  private advanceHandshake(bytes: Uint8Array): void {
    const handshake = this.handshake;
    if (!handshake) throw new Error("handshake missing");
    handshake.read_message(bytes);
    if (!handshake.is_finished()) {
      this.sendHandshakeMessage();
      return;
    }
    this.completeHandshake();
  }

  private sendHandshakeMessage(): void {
    const socket = this.socket;
    const handshake = this.handshake;
    if (!socket || !handshake) throw new Error("handshake unavailable");
    socket.send(handshake.write_message());
    if (handshake.is_finished()) this.completeHandshake();
  }

  private completeHandshake(): void {
    const handshake = this.handshake;
    if (!handshake || !handshake.is_finished()) return;
    this.transport = handshake.finish();
    this.handshake = null;
    this.reconnectDelayMs = CONNECT_RECONNECT_MIN_MS;
    // Noise completion only authenticates the channel. The application is not
    // ready until the host's typed Hello response has been validated.
    this.helloAccepted = false;
    this.setState({ kind: "handshaking" });
    this.sendHello();
  }

  private sendHello(): void {
    const payload: Record<string, unknown> = {
      capabilities: this.options.capabilities ?? 0,
      limits: this.options.limits ?? DEFAULT_CONNECT_LIMITS,
      privacy_class: "local_only",
    };
    if (this.options.clientId) payload.client_id = this.options.clientId;
    if (this.options.capabilityGrant !== undefined) {
      payload.capability_grant = this.options.capabilityGrant;
    }
    this.sendEnvelope(CONNECT_HELLO_KIND, payload);
  }

  private sendEnvelope(
    payloadKind: number,
    payload: unknown,
    options: ConnectRequestOptions = {},
  ): boolean {
    const socket = this.socket;
    const runtime = this.runtime;
    const transport = this.transport;
    const greeting = this.greeting;
    if (!socket || !runtime || !transport || !greeting) return false;
    try {
      const limits = this.negotiatedLimits ?? this.options.limits ?? DEFAULT_CONNECT_LIMITS;
      const payloadBytes = runtime.encode_connect_payload_json(JSON.stringify(payload));
      const sequence = this.outboundSequence;
      if (!Number.isSafeInteger(sequence) || sequence <= 0) return false;
      const privacyClass = options.privacyClass ?? "local_only";
      const payloadVersion = options.payloadVersion ?? 1;
      if (
        !Number.isInteger(payloadKind) ||
        payloadKind <= 0 ||
        payloadKind > 0xffff ||
        !Number.isInteger(payloadVersion) ||
        payloadVersion <= 0 ||
        payloadVersion > 0xffff ||
        !["local_only", "managed_metadata", "raw_content"].includes(privacyClass)
      ) {
        throw new ConnectBrowserTransportError("Connect payload metadata rejected");
      }
      const envelope: ConnectEnvelopeJson = {
        protocolMajor: 1,
        protocolMinor: 0,
        connectionId: this.connectionId,
        sessionId: uuidFromBytes(greeting.sessionId),
        channelId: this.channelId,
        channel: channelForPayloadKind(payloadKind),
        sequence,
        requestId: options.requestId ?? null,
        operationId: options.operationId ?? null,
        limits,
        compression: "none",
        privacyClass,
        payloadKind,
        payloadVersion,
        payloadBase64: base64Encode(payloadBytes),
      };
      if (!isUuidV7(envelope.connectionId) || !isUuidV7(envelope.sessionId) || !isUuidV7(envelope.channelId)) {
        throw new ConnectBrowserTransportError("Connect channel identity rejected");
      }
      if (envelope.requestId !== null && !isUuidV7(envelope.requestId)) {
        throw new ConnectBrowserTransportError("Connect request identity rejected");
      }
      if (envelope.operationId !== null && !isUuidV7(envelope.operationId)) {
        throw new ConnectBrowserTransportError("Connect operation identity rejected");
      }
      const plaintext = runtime.encode_connect_envelope_json(
        JSON.stringify(envelope),
      );
      const sealed = transport.seal(BigInt(sequence), secureRandomBytes(16), plaintext);
      socket.send(sealed);
      this.outboundSequence += 1;
      return true;
    } catch {
      this.protocolFailure();
      return false;
    }
  }

  private handleSealedEnvelope(bytes: Uint8Array): void {
    const runtime = this.runtime;
    const transport = this.transport;
    const greeting = this.greeting;
    if (!runtime || !transport || !greeting) throw new Error("transport missing");
    const sealed = decodeConnectSealedFrame(bytes);
    const envelopeBytes = transport.open(bytes);
    let parsed: unknown;
    try {
      parsed = JSON.parse(runtime.decode_connect_envelope_json(envelopeBytes));
    } catch {
      throw new Error("Connect envelope metadata rejected");
    }
    if (typeof parsed !== "object" || parsed === null) {
      throw new Error("Connect envelope metadata rejected");
    }
    // The structural checks below are the runtime guard for this cast. The
    // Rust/WASM decoder already bounds the bytes; the browser still rejects
    // missing/foreign metadata before dispatching the payload.
    const envelope = parsed as ConnectEnvelopeJson;
    const expectedSessionId = uuidFromBytes(greeting.sessionId);
    if (
      envelope.protocolMajor !== 1 ||
      envelope.protocolMinor !== 0 ||
      envelope.connectionId !== this.connectionId ||
      envelope.sessionId !== expectedSessionId ||
      envelope.channelId !== this.channelId ||
      !Number.isSafeInteger(envelope.sequence) ||
      envelope.sequence <= 0 ||
      BigInt(envelope.sequence) !== sealed.sequence ||
      !Number.isInteger(envelope.payloadKind) ||
      envelope.payloadKind <= 0 ||
      envelope.payloadKind > 0xffff ||
      !Number.isInteger(envelope.payloadVersion) ||
      envelope.payloadVersion <= 0 ||
      envelope.payloadVersion > 0xffff ||
      envelope.compression !== "none" ||
      (envelope.requestId !== null && !isUuidV7(envelope.requestId)) ||
      (envelope.operationId !== null && !isUuidV7(envelope.operationId)) ||
      typeof envelope.payloadBase64 !== "string" ||
      !isConnectLimits(envelope.limits) ||
      !["local_only", "managed_metadata", "raw_content"].includes(
        envelope.privacyClass,
      )
    ) {
      throw new Error("Connect envelope metadata rejected");
    }
    const limits = envelope.limits;
    const channel = channelForPayloadKind(envelope.payloadKind);
    if (envelope.channel !== channel) {
      throw new Error("Connect envelope channel rejected");
    }
    if (
      envelope.privacyClass === "raw_content" &&
      ![10, 11, 14].includes(envelope.payloadKind)
    ) {
      throw new Error("Connect privacy class rejected");
    }
    if (this.helloAccepted && this.negotiatedLimits && !sameLimits(limits, this.negotiatedLimits)) {
      throw new Error("Connect negotiated limits changed");
    }
    if (envelope.sequence <= this.inboundSequence) return;
    const isResyncSnapshotFrame =
      this.stateValue.kind === "resyncing" &&
      this.resyncInFlight &&
      envelope.payloadKind === CONNECT_QUERY_REPLY_KIND &&
      envelope.requestId !== null &&
      envelope.requestId === this.resyncRequestId &&
      envelope.operationId === null &&
      envelope.payloadVersion === 1 &&
      envelope.privacyClass === "local_only";
    if (this.stateValue.kind === "resyncing" && !isResyncSnapshotFrame) {
      // Resync is an authoritative snapshot boundary. Do not deliver a frame
      // from the old stream, even when it happens to be the next sequence.
      // A query reply from another request is never a resync completion.
      this.requestResync("gap");
      return;
    }
    if (
      envelope.sequence > this.inboundSequence + 1 &&
      !isResyncSnapshotFrame
    ) {
      // Do not decode, advance, or deliver a frame beyond the first missing
      // sequence. The authenticated bounded snapshot response is the only
      // legal way to close this gap, and completion is handled explicitly
      // below.
      this.requestResync("gap");
      return;
    }
    let payload: unknown;
    try {
      payload = JSON.parse(
        runtime.decode_connect_payload_json(base64Decode(envelope.payloadBase64)),
      ) as unknown;
    } catch {
      throw new Error("Connect payload rejected");
    }

    if (!this.helloAccepted) {
      if (
        this.stateValue.kind !== "handshaking" ||
        envelope.payloadKind !== CONNECT_HELLO_KIND ||
        envelope.requestId !== null
      ) {
        throw new Error("Connect Hello response rejected");
      }
      if (typeof payload !== "object" || payload === null) {
        throw new Error("Connect Hello payload rejected");
      }
      const hello = payload as Record<string, unknown>;
      if (
        !isConnectLimits(hello.limits)
      ) {
        throw new Error("Connect negotiated Hello limits rejected");
      }
      const negotiated = hello.limits;
      if (!sameLimits(negotiated, limits)) {
        throw new Error("Connect Hello limits do not match envelope");
      }
      this.negotiatedLimits = negotiated;
      this.helloAccepted = true;
      this.inboundSequence = envelope.sequence;
      this.setState({ kind: "ready" });
    } else if (isResyncSnapshotFrame) {
      if (!isBoundedResyncSnapshot(payload, envelope.requestId, limits)) {
        throw new Error("Connect resync snapshot rejected");
      }
      // The snapshot is authoritative for the missing interval. Its
      // through_sequence belongs to the host snapshot journal, not this
      // Connect channel, so only the authenticated envelope sequence advances
      // the channel cursor.
      this.inboundSequence = envelope.sequence;
      this.resyncInFlight = false;
      this.resyncRequestId = null;
      this.setState({ kind: "ready" });
    } else {
      if (envelope.sequence !== this.inboundSequence + 1) {
        throw new Error("Connect sequence gap remained unresolved");
      }
      this.inboundSequence = envelope.sequence;
    }
    if (
      envelope.payloadKind === CONNECT_HELLO_KIND ||
      envelope.payloadKind === CONNECT_CAPABILITIES_KIND
    ) {
      if (typeof payload === "object" && payload !== null) {
        const record = payload as Record<string, unknown>;
        if (typeof record.client_id === "string") this.options.onClientId?.(record.client_id);
        if ("capability_grant" in record) this.options.onCapabilityGrant?.(record.capability_grant);
      }
    }
    const decoded = { ...envelope, payload } as DecodedConnectEnvelope;
    const pending = envelope.requestId
      ? this.pendingRequests.get(envelope.requestId)
      : undefined;
    if (pending) {
      if (pending.operationId !== (envelope.operationId ?? null)) {
        throw new Error("Connect request correlation rejected");
      }
      this.pendingRequests.delete(envelope.requestId as string);
      pending.resolve(decoded);
    }
    this.options.onEnvelope?.(decoded);
    for (const listener of this.envelopeListeners) listener(decoded);
  }

  private scheduleReconnect(): void {
    if (this.stopped || this.reconnectTimer !== null) return;
    this.setState({ kind: "reconnecting" });
    const delay = this.reconnectDelayMs;
    this.reconnectDelayMs = Math.min(
      this.reconnectDelayMs * 2,
      CONNECT_RECONNECT_MAX_MS,
    );
    this.reconnectTimer = globalThis.setTimeout(() => {
      this.reconnectTimer = null;
      void this.start();
    }, delay);
  }

  private protocolFailure(): void {
    const socket = this.socket;
    this.socket = null;
    this.handshake = null;
    this.transport = null;
    this.helloAccepted = false;
    this.negotiatedLimits = null;
    this.resyncInFlight = false;
    this.resyncRequestId = null;
    this.rejectPendingRequests("Connect protocol rejected");
    try {
      socket?.close();
    } catch {
      // Best effort close; no downgrade is attempted.
    }
    this.setState({ kind: "closed", reason: "Connect protocol rejected" });
  }

  private rejectPendingRequests(reason: string): void {
    if (this.pendingRequests.size === 0) return;
    const error = new ConnectBrowserTransportError(reason);
    for (const [requestId, pending] of this.pendingRequests) {
      this.pendingRequests.delete(requestId);
      pending.reject(error);
    }
  }

  private setState(state: ConnectConnectionState): void {
    this.stateValue = state;
    this.options.onState?.(state);
    for (const listener of this.stateListeners) listener(state);
  }
}

