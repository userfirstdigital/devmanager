// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import type { SemanticEvent } from "../../api/types";
import { buildLogRows, LogTimeline } from "./LogTimeline";

function event(
  sequence: number,
  kind: SemanticEvent["kind"],
  detail: Record<string, unknown> = {},
): SemanticEvent {
  return {
    ...detail,
    stableSessionKey: "server:web",
    sequence,
    occurredAtEpochMs: Date.UTC(2026, 6, 14, 12, 0, sequence),
    source: "server",
    kind,
  } as SemanticEvent;
}

afterEach(cleanup);

describe("continuous native logs", () => {
  it("coalesces adjacent streams and separates restarted runs", () => {
    const rows = buildLogRows([
      event(1, "status", { state: "Running", detail: null }),
      event(2, "output", { stream: "stdout", text: "ready\n" }),
      event(3, "output", { stream: "stdout", text: "request\n" }),
      event(4, "status", { state: "Starting", detail: "Restarting" }),
      event(5, "output", { stream: "stderr", text: "warning\n" }),
    ]);

    expect(rows).toMatchObject([
      { kind: "output", stream: "stdout", text: "ready\nrequest\n" },
      { kind: "runBoundary", label: "New run" },
      { kind: "output", stream: "stderr", text: "warning\n" },
    ]);
  });

  it("shows output directly without an output accordion", () => {
    render(
      <LogTimeline
        events={[
          event(1, "command", {
            command_id: "command-1",
            text: "npm run dev",
            exit_code: null,
          }),
          event(2, "output", { stream: "stdout", text: "Local: http://localhost:5173" }),
        ]}
        emptyTitle="No output"
        emptyDetail="Waiting"
      />,
    );

    expect(screen.getByText("npm run dev").isConnected).toBe(true);
    expect(screen.getByText(/Local: http:\/\/localhost:5173/).isConnected).toBe(true);
    expect(screen.queryByRole("button", { name: /^output$/i })).toBeNull();
  });

  it("offers New output after the reader scrolls away from the bottom", () => {
    const first = event(1, "output", { stream: "stdout", text: "first" });
    const { container, rerender } = render(
      <LogTimeline events={[first]} emptyTitle="No output" emptyDetail="Waiting" />,
    );
    const scroller = container.querySelector(".dm-log-scroll") as HTMLDivElement;
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 1000 },
      clientHeight: { configurable: true, value: 300 },
    });
    scroller.scrollTop = 100;
    fireEvent.scroll(scroller);

    rerender(
      <LogTimeline
        events={[first, event(2, "output", { stream: "stdout", text: "second" })]}
        emptyTitle="No output"
        emptyDetail="Waiting"
      />,
    );

    expect(screen.getByRole("button", { name: /new output/i }).isConnected).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: /new output/i }));
    expect(screen.queryByRole("button", { name: /new output/i })).toBeNull();
  });
});
