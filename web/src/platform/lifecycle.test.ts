// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";

import { bindAppLifecycle } from "./lifecycle";

describe("bindAppLifecycle", () => {
  let visibility: string;
  let unbind: (() => void) | null = null;

  afterEach(() => {
    unbind?.();
    unbind = null;
    vi.restoreAllMocks();
  });

  function stubVisibility(state: "visible" | "hidden") {
    visibility = state;
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => visibility,
    });
  }

  it("pageshow sets visibility from the real document.visibilityState", () => {
    const setVisibility = vi.fn();
    const foreground = vi.fn();
    const suspend = vi.fn();
    stubVisibility("visible");
    unbind = bindAppLifecycle({ setVisibility, foreground, suspend });
    window.dispatchEvent(new Event("pageshow"));
    expect(setVisibility).toHaveBeenCalledWith(true);
    expect(foreground).toHaveBeenCalled();
  });

  it("pageshow while hidden does not claim visible writer authority", () => {
    const setVisibility = vi.fn();
    const foreground = vi.fn();
    const suspend = vi.fn();
    stubVisibility("hidden");
    unbind = bindAppLifecycle({ setVisibility, foreground, suspend });
    window.dispatchEvent(new Event("pageshow"));
    expect(setVisibility).toHaveBeenCalledWith(false);
    expect(foreground).not.toHaveBeenCalled();
  });

  it("hidden focus and online do not call foreground", () => {
    const setVisibility = vi.fn();
    const foreground = vi.fn();
    const suspend = vi.fn();
    stubVisibility("hidden");
    unbind = bindAppLifecycle({ setVisibility, foreground, suspend });
    window.dispatchEvent(new Event("focus"));
    window.dispatchEvent(new Event("online"));
    expect(foreground).not.toHaveBeenCalled();
    expect(setVisibility).not.toHaveBeenCalledWith(true);
  });

  it("pagehide sets visibility false then suspends", () => {
    const setVisibility = vi.fn();
    const foreground = vi.fn();
    const suspend = vi.fn();
    stubVisibility("visible");
    unbind = bindAppLifecycle({ setVisibility, foreground, suspend });
    window.dispatchEvent(new Event("pagehide"));
    expect(setVisibility).toHaveBeenCalledWith(false);
    expect(suspend).toHaveBeenCalledTimes(1);
    expect(setVisibility.mock.invocationCallOrder[0]!).toBeLessThan(
      suspend.mock.invocationCallOrder[0]!,
    );
  });

  it("visible focus still foregrounds", () => {
    const setVisibility = vi.fn();
    const foreground = vi.fn();
    const suspend = vi.fn();
    stubVisibility("visible");
    unbind = bindAppLifecycle({ setVisibility, foreground, suspend });
    window.dispatchEvent(new Event("focus"));
    expect(foreground).toHaveBeenCalledTimes(1);
  });
});
