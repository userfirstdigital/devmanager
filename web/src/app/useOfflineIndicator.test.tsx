// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useOfflineIndicator } from "./useOfflineIndicator";

describe("sustained offline indicator", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("keeps connecting and brief closed transitions silent", () => {
    const { result, rerender } = renderHook<boolean, { kind: "connecting" | "closed" }>(
      ({ kind }: { kind: "connecting" | "closed" }) =>
        useOfflineIndicator(
          kind === "connecting"
            ? { kind: "connecting" }
            : { kind: "closed", reason: "network changed" },
        ),
      { initialProps: { kind: "connecting" } },
    );

    expect(result.current).toBe(false);
    rerender({ kind: "closed" });
    act(() => vi.advanceTimersByTime(6_999));
    expect(result.current).toBe(false);
  });

  it("appears at seven seconds and clears immediately when the socket opens", () => {
    const { result, rerender } = renderHook(
      ({ open }: { open: boolean }) =>
        useOfflineIndicator(
          open ? { kind: "open" } : { kind: "closed", reason: "offline" },
        ),
      { initialProps: { open: false } },
    );

    act(() => vi.advanceTimersByTime(7_000));
    expect(result.current).toBe(true);
    rerender({ open: true });
    expect(result.current).toBe(false);
  });
});
