import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  buildPairingUrl,
  buildWebSocketUrl,
  getBrowserInstallId,
} from "./browserIdentity";

function makeStorage() {
  const values = new Map<string, string>();
  return {
    getItem(key: string) {
      return values.get(key) ?? null;
    },
    setItem(key: string, value: string) {
      values.set(key, value);
    },
    removeItem(key: string) {
      values.delete(key);
    },
    clear() {
      values.clear();
    },
  };
}

describe("browserIdentity", () => {
  beforeEach(() => {
    vi.stubGlobal("localStorage", makeStorage());
    vi.stubGlobal("crypto", {
      randomUUID: vi.fn(() => "browser-install-uuid"),
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("reuses one browser install id from localStorage", () => {
    const first = getBrowserInstallId();
    const second = getBrowserInstallId();

    expect(first).toBe("browser-install-uuid");
    expect(second).toBe("browser-install-uuid");
  });

  it("includes browser install id in pairing and websocket urls", () => {
    expect(buildPairingUrl("PAIR1234")).toBe(
      "/pair?t=PAIR1234&browserInstallId=browser-install-uuid",
    );
    expect(buildWebSocketUrl({ protocol: "https:", host: "example.test" })).toBe(
      "wss://example.test/api/ws?browserInstallId=browser-install-uuid",
    );
  });
});
