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
