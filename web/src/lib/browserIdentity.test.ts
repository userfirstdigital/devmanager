import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  buildPairingRequest,
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

  it("sends the pairing secret only in a POST body and retains stable browser identity", () => {
    const request = buildPairingRequest("PAIR1234");
    expect(request.method).toBe("POST");
    expect(request.credentials).toBe("include");
    expect(JSON.parse(request.body as string)).toEqual({
      t: "PAIR1234", browserInstallId: "browser-install-uuid",
    });
    expect(buildWebSocketUrl({ protocol: "https:", host: "example.test" })).toBe(
      "wss://example.test/api/ws?browserInstallId=browser-install-uuid",
    );
  });
});
