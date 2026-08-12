import { buildWebSocketUrl } from "../lib/browserIdentity";

export type ConnectTransportKind = "direct" | "relay";

export type ConnectRouteReason =
  | "preferredDirect"
  | "directUnavailable"
  | "explicitFallback";

export interface ConnectLocationLike {
  protocol: string;
  host: string;
}

export interface ConnectRoute {
  kind: ConnectTransportKind;
  url: string;
  reason: ConnectRouteReason;
}

export interface ConnectRouteSelection {
  preferDirect?: boolean;
  directAvailable?: boolean;
  /** Same-origin host `/api/ws` only; any other value is ignored. */
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
  const documentedRelay =
    typeof selection.relayUrl === "string" &&
    selection.relayUrl === hostUrl
      ? selection.relayUrl
      : hostUrl;
  return {
    kind: "relay",
    url: documentedRelay,
    reason: directAvailable ? "explicitFallback" : "directUnavailable",
  };
}

export function allowsRawTerminal(route: ConnectRoute): boolean {
  return route.kind === "direct";
}

export function isRawTerminalWriterFrame(frame: {
  type: string;
}): boolean {
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
