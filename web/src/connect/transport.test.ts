import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { buildWebSocketUrl } from "../lib/browserIdentity";
import {
  MAX_INBOUND_BINARY_BYTES,
  MAX_INBOUND_TEXT_BYTES,
  MAX_PENDING_OUTBOUND_BYTES,
  MAX_PENDING_OUTBOUND_ITEMS,
  allowsRawTerminal,
  classifyInboundFrame,
  inboundTextByteLength,
  isRawTerminalWriterFrame,
  selectConnectRoute,
} from "./transport";

const location = { protocol: "http:", host: "example.test" };

describe("selectConnectRoute", () => {
  beforeEach(() => {
    vi.stubGlobal("localStorage", {
      getItem: vi.fn(() => "browser-install-uuid"),
      setItem: vi.fn(),
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("prefers the existing host WebSocket when direct is available", () => {
    const route = selectConnectRoute({
      preferDirect: true,
      directAvailable: true,
      location,
    });

    expect(route).toEqual({
      kind: "direct",
      url: buildWebSocketUrl(location),
      reason: "preferredDirect",
    });
    expect(allowsRawTerminal(route)).toBe(true);
  });

  it("falls back to an explicit relay using the same host protocol URL", () => {
    const hostUrl = buildWebSocketUrl(location);
    const route = selectConnectRoute({
      preferDirect: true,
      directAvailable: false,
      relayUrl: hostUrl,
      location,
    });

    expect(route).toEqual({
      kind: "relay",
      url: hostUrl,
      reason: "directUnavailable",
    });
    expect(allowsRawTerminal(route)).toBe(false);
  });

  it("ignores an undocumented relay URL and stays on the host contract", () => {
    const route = selectConnectRoute({
      preferDirect: false,
      directAvailable: true,
      relayUrl: "wss://relay.example/undocumented",
      location,
    });

    expect(route.kind).toBe("relay");
    expect(route.url).toBe(buildWebSocketUrl(location));
    expect(route.url).not.toContain("relay.example");
    expect(route.reason).toBe("explicitFallback");
    expect(allowsRawTerminal(route)).toBe(false);
  });
});

describe("inbound frame bounds", () => {
  it("accepts a well-formed text frame under the host outbound cap", () => {
    expect(classifyInboundFrame({ channel: "text", byteLength: 128 })).toBe(
      "ok",
    );
    expect(
      classifyInboundFrame({
        channel: "binary",
        byteLength: 1024,
        frameType: 0x01,
      }),
    ).toBe("ok");
  });

  it("rejects oversized inbound text and binary without inspecting payload text", () => {
    expect(
      classifyInboundFrame({
        channel: "text",
        byteLength: MAX_INBOUND_TEXT_BYTES + 1,
      }),
    ).toBe("oversized");
    expect(
      classifyInboundFrame({
        channel: "binary",
        byteLength: MAX_INBOUND_BINARY_BYTES + 1,
        frameType: 0x01,
      }),
    ).toBe("oversized");
    expect(inboundTextByteLength("hello")).toBe(5);
  });

  it("rejects malformed binary that is too short or unknown", () => {
    expect(
      classifyInboundFrame({
        channel: "binary",
        byteLength: 4,
        frameType: 0x01,
      }),
    ).toBe("malformed");
    expect(
      classifyInboundFrame({
        channel: "binary",
        byteLength: 32,
        frameType: 0x99,
      }),
    ).toBe("malformed");
  });
});

describe("pending queue bounds and raw-terminal classification", () => {
  it("matches the live client pending-work budget", () => {
    expect(MAX_PENDING_OUTBOUND_ITEMS).toBe(256);
    expect(MAX_PENDING_OUTBOUND_BYTES).toBe(8 * 1_024 * 1_024);
  });

  it("treats only PTY writer frames as raw terminal traffic", () => {
    expect(
      isRawTerminalWriterFrame({ type: "input", sessionId: "pty-a" }),
    ).toBe(true);
    expect(
      isRawTerminalWriterFrame({ type: "pasteImage", sessionId: "pty-a" }),
    ).toBe(true);
    expect(
      isRawTerminalWriterFrame({ type: "resize", sessionId: "pty-a" }),
    ).toBe(true);
    expect(
      isRawTerminalWriterFrame({
        type: "interruptSession",
        stableSessionKey: "tab:a",
      }),
    ).toBe(false);
  });
});
