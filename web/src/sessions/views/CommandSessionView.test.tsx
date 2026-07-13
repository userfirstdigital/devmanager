// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { CommandSessionView } from "./CommandSessionView";

afterEach(cleanup);

describe("command session lifecycle controls", () => {
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
