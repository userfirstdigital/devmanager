import { describe, expect, it } from "vitest";
import { safeRoute } from "./notificationRoute";

describe("safeRoute", () => {
  const origin = "https://devmanager.local";

  it("falls back when push data contains a malformed URL", () => {
    expect(safeRoute("http://[", origin)).toBe("/tasks");
  });

  it("keeps only same-origin route components", () => {
    expect(safeRoute("/tasks?filter=active#latest", origin)).toBe(
      "/tasks?filter=active#latest",
    );
    expect(safeRoute("/tasks/tab%3Atab-1", origin)).toBe("/tasks/tab%3Atab-1");
    expect(safeRoute("/tasks/server%3Adev%2Fweb", origin)).toBe(
      "/tasks/server%3Adev%2Fweb",
    );
    expect(safeRoute("https://example.com/escape", origin)).toBe("/tasks");
    expect(safeRoute("/api/push", origin)).toBe("/tasks");
    expect(safeRoute("/settings", origin)).toBe("/tasks");
  });
});
