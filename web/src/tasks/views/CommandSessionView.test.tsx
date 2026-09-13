// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { CommandSessionView } from "./CommandSessionView";

const outputEvent = {
  stableSessionKey: "tab:ssh",
  sequence: 1,
  occurredAtEpochMs: Date.UTC(2026, 6, 14),
  source: "ssh",
  kind: "output",
  stream: "stdout",
  text: "Welcome to the server",
} as const;

afterEach(cleanup);

describe("command session lifecycle controls", () => {
  it("renders command output as native text without an accordion", () => {
    render(
      <CommandSessionView
        events={[outputEvent]}
        density="calm"
        connected
        actionsDisabled={false}
        composer={<div>Composer</div>}
      />,
    );

    expect(screen.getByText("Welcome to the server").isConnected).toBe(true);
    expect(screen.queryByRole("button", { name: /^output$/i })).toBeNull();
  });

  it("offers restart and disconnect for a connected SSH session", () => {
    const onRestart = vi.fn();
    const onDisconnect = vi.fn();
    render(
      <CommandSessionView
        events={[]}
        density="calm"
        connected
        actionsDisabled={false}
        composer={<div>Composer</div>}
        onRestart={onRestart}
        onDisconnect={onDisconnect}
      />,
    );

    fireEvent.click(screen.getByLabelText(/session actions/i));
    fireEvent.click(screen.getByRole("menuitem", { name: /restart/i }));
    fireEvent.click(screen.getByRole("menuitem", { name: /disconnect/i }));
    expect(onRestart).toHaveBeenCalledTimes(1);
    expect(onDisconnect).toHaveBeenCalledTimes(1);
  });

  it("keeps lifecycle mutations disabled during reconnect", () => {
    render(
      <CommandSessionView
        events={[]}
        density="calm"
        connected
        actionsDisabled
        composer={<div>Composer</div>}
        onRestart={() => {}}
        onDisconnect={() => {}}
      />,
    );
    fireEvent.click(screen.getByLabelText(/session actions/i));
    expect(
      (screen.getByRole("menuitem", { name: /restart/i }) as HTMLButtonElement)
        .disabled,
    ).toBe(true);
    expect(
      (screen.getByRole("menuitem", { name: /disconnect/i }) as HTMLButtonElement)
        .disabled,
    ).toBe(true);
  });
});
