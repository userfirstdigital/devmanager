// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { act } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { AppShell } from "./AppShell";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("native app shell connection feedback", () => {
  it("keeps cached content readable and shows only sustained offline state", () => {
    vi.useFakeTimers();
    const { rerender } = render(
      <AppShell
        route={{ name: "sessions" }}
        status={{ kind: "closed", reason: "network changed" }}
        attentionCount={0}
        lastError={null}
        onDismissError={() => {}}
        onNavigate={() => {}}
      >
        <p>Cached session list</p>
      </AppShell>,
    );

    expect(screen.getByText("Cached session list").isConnected).toBe(true);
    expect(screen.queryByRole("status")).toBeNull();
    act(() => vi.advanceTimersByTime(6_999));
    expect(screen.queryByRole("status")).toBeNull();
    act(() => vi.advanceTimersByTime(1));
    expect(screen.getByRole("status").textContent).toMatch(/offline.*reconnecting/i);
    expect(screen.queryByRole("button", { name: /resume|reconnect/i })).toBeNull();

    rerender(
      <AppShell
        route={{ name: "sessions" }}
        status={{ kind: "open" }}
        attentionCount={0}
        lastError={null}
        onDismissError={() => {}}
        onNavigate={() => {}}
      >
        <p>Cached session list</p>
      </AppShell>,
    );
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("surfaces and dismisses action failures", async () => {
    const user = userEvent.setup();
    const onDismissError = vi.fn();
    render(
      <AppShell
        route={{ name: "sessions" }}
        status={{ kind: "open" }}
        attentionCount={0}
        lastError="Server could not be restarted."
        onDismissError={onDismissError}
        onNavigate={() => {}}
      >
        <p>Sessions</p>
      </AppShell>,
    );

    expect(screen.getByRole("alert").textContent).toContain(
      "Server could not be restarted.",
    );
    await user.click(screen.getByRole("button", { name: /dismiss/i }));
    expect(onDismissError).toHaveBeenCalledTimes(1);
  });
});
