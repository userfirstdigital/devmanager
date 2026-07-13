// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  clearOtherRuntimes,
  hasExactDraftHandoff,
  loadDraft,
  pruneDrafts,
  removeDraft,
  saveDraft,
  stageDraftHandoff,
} from "./draftStore";

describe("runtime-scoped draft storage", () => {
  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
  });

  afterEach(() => vi.restoreAllMocks());

  it("stages and consumes an exact runtime-scoped compatibility handoff", () => {
    const exactDraft = "  exact draft\n";
    saveDraft("runtime-a", "tab:x", exactDraft);

    expect(
      stageDraftHandoff("runtime-a", { "tab:x": exactDraft }),
    ).toBe(true);
    expect(
      hasExactDraftHandoff("runtime-a", { "tab:x": exactDraft }),
    ).toBe(true);
    expect(loadDraft("runtime-a", "tab:x")).toBe(exactDraft);
    expect(
      hasExactDraftHandoff("runtime-a", { "tab:x": exactDraft }),
    ).toBe(false);
    expect(loadDraft("runtime-a", "tab:x")).toBe(exactDraft);
  });

  it("rejects a handoff that cannot preserve every exact byte", () => {
    const oversized = "x".repeat(32 * 1024 + 1);

    expect(stageDraftHandoff("runtime-a", { "tab:x": oversized })).toBe(
      false,
    );
    expect(
      hasExactDraftHandoff("runtime-a", { "tab:x": oversized }),
    ).toBe(false);
  });

  it("clears a stale handoff when the exact draft set becomes empty", () => {
    expect(
      stageDraftHandoff("runtime-a", { "tab:x": "stale draft" }),
    ).toBe(true);

    expect(stageDraftHandoff("runtime-a", {})).toBe(true);
    expect(loadDraft("runtime-a", "tab:x")).toBeNull();
  });

  it("fails closed when session handoff storage rejects the write", () => {
    const setItem = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementation(() => {
        throw new DOMException("quota exceeded");
      });

    expect(stageDraftHandoff("runtime-a", { "tab:x": "keep me" })).toBe(
      false,
    );
    setItem.mockRestore();
  });

  it("prunes and removes handoff-only drafts with the session lifecycle", () => {
    expect(
      stageDraftHandoff("runtime-a", {
        "tab:keep": "keep",
        "tab:gone": "gone",
      }),
    ).toBe(true);

    pruneDrafts("runtime-a", new Set(["tab:keep"]));
    expect(
      hasExactDraftHandoff("runtime-a", { "tab:keep": "keep" }),
    ).toBe(true);

    removeDraft("runtime-a", "tab:keep");
    expect(
      hasExactDraftHandoff("runtime-a", { "tab:keep": "keep" }),
    ).toBe(false);
  });

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
