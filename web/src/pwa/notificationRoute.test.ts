import { describe, expect, it } from "vitest";
import { safeRoute } from "./notificationRoute";

describe("safeRoute", () => {
  const origin = "https://devmanager.local";

  it("falls back when push data contains a malformed URL", () => {
    expect(safeRoute("http://[", origin)).toBe("/sessions");
  });

  it("keeps only same-origin route components", () => {
    expect(safeRoute("/sessions?filter=active#latest", origin)).toBe(
      "/sessions?filter=active#latest",
    );
    expect(safeRoute("/session/tab/tab-1", origin)).toBe(
      "/session/tab/tab-1",
    );
    expect(safeRoute("/session/server/dev%2Fweb", origin)).toBe(
      "/session/server/dev%2Fweb",
    );
    expect(safeRoute("https://example.com/escape", origin)).toBe("/sessions");
    expect(safeRoute("/api/push", origin)).toBe("/sessions");
    expect(safeRoute("/settings", origin)).toBe("/sessions");
  });
});
