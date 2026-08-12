import { buildWebSocketUrl } from "../lib/browserIdentity";

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
 * This build has no audited WebCrypto/WASM implementation of that exact
 * stack, so the browser path is an explicit typed HOLD. Callers must surface
 * this state and cannot silently fall back to the plaintext/legacy `/api/ws`
 * route for Connect application traffic.
 */

export const CONNECT_BROWSER_E2E_HOLD = "browser-e2e-transport-held" as const;

export class ConnectBrowserTransportError extends Error {
  readonly code = CONNECT_BROWSER_E2E_HOLD;

  constructor(message = "Connect browser E2E transport is not available in this build") {
    super(message);
    this.name = "ConnectBrowserTransportError";
  }
}

export type ConnectBrowserTransportState =
  | { kind: "held"; code: typeof CONNECT_BROWSER_E2E_HOLD; reason: string }
  | { kind: "ready" };

/** The browser-visible endpoint; opening it is deliberately refused until the
 * exact Noise implementation is supplied. */
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
      "Noise XX/ChaChaPoly/BLAKE2s browser implementation is pending; Connect will not downgrade to /api/ws.",
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

