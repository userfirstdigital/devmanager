// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import { AppShell } from "./AppShell";

afterEach(cleanup);

describe("native app shell connection feedback", () => {
  it("keeps a cached workspace readable while clearly showing automatic reconnect", () => {
    render(
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
    expect(screen.getByRole("status").textContent).toMatch(/reconnecting automatically/i);
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
