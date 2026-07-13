// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  clearOtherRuntimes,
  loadDraft,
  pruneDrafts,
  removeDraft,
  saveDraft,
} from "./draftStore";

describe("runtime-scoped draft storage", () => {
  beforeEach(() => localStorage.clear());

  it("keeps a draft only for the current host runtime", () => {
    saveDraft("runtime-a", "tab:x", "hello");

    expect(loadDraft("runtime-a", "tab:x")).toBe("hello");
    expect(loadDraft("runtime-b", "tab:x")).toBeNull();

    clearOtherRuntimes("runtime-b");
    expect(loadDraft("runtime-a", "tab:x")).toBeNull();
  });

  it("expires old drafts and prunes sessions that no longer exist", () => {
    const now = Date.UTC(2026, 6, 13);
    saveDraft("runtime-a", "tab:old", "old", now - 8 * 24 * 60 * 60 * 1000);
    saveDraft("runtime-a", "tab:keep", "keep", now);
    saveDraft("runtime-a", "tab:gone", "gone", now);

    expect(loadDraft("runtime-a", "tab:old", now)).toBeNull();
    pruneDrafts("runtime-a", new Set(["tab:keep"]), now);

    expect(loadDraft("runtime-a", "tab:keep", now)).toBe("keep");
    expect(loadDraft("runtime-a", "tab:gone", now)).toBeNull();
  });

  it("bounds each persisted draft to 32 KiB and removes acknowledged text", () => {
    saveDraft("runtime-a", "tab:x", `hello-${"🙂".repeat(20_000)}`);
    const saved = loadDraft("runtime-a", "tab:x");

    expect(new TextEncoder().encode(saved ?? "").byteLength).toBeLessThanOrEqual(32 * 1024);
    expect(saved).toMatch(/^hello-/);

    removeDraft("runtime-a", "tab:x");
    expect(loadDraft("runtime-a", "tab:x")).toBeNull();
  });

  it("fails safely when browser storage is unavailable", () => {
    const getItem = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new DOMException("blocked");
    });

    expect(loadDraft("runtime-a", "tab:x")).toBeNull();
    getItem.mockRestore();
  });
});
