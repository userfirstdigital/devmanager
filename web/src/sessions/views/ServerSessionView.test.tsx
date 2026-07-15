// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import type { SemanticEvent, WebSessionSummary } from "../../api/types";
import { ServerSessionView } from "./ServerSessionView";

const session: WebSessionSummary = {
  sessionId: "web",
  stableSessionKey: "server:web",
  kind: "server",
  status: "Running",
  projectId: "portal",
  commandId: "web",
  tabId: "web",
  dimensions: { cols: 100, rows: 30, cell_width: 8, cell_height: 18 },
  lastActivityEpochMs: Date.UTC(2026, 6, 14),
  attention: "none",
  attentionCount: 0,
  adapterHealth: "healthy",
  rawRequired: false,
  oldestSequence: 1,
  latestSequence: 1,
};

const output = {
  stableSessionKey: "server:web",
  sequence: 1,
  occurredAtEpochMs: Date.UTC(2026, 6, 14),
  source: "server",
  kind: "output",
  stream: "stdout",
  text: "VITE ready in 120 ms",
} as SemanticEvent;

afterEach(cleanup);

describe("native server view", () => {
  it("keeps controls compact and exposes streaming output immediately", () => {
    render(
      <ServerSessionView
        session={session}
        command={{ id: "web", label: "Web", port: 5173, status: "Running" }}
        port={{ port: 5173, inUse: true, pid: 42, processName: "node" }}
        events={[output]}
        density="calm"
        actionsDisabled={false}
        onStart={() => {}}
        onStop={() => {}}
        onRestart={() => {}}
      />,
    );

    expect(screen.getByText("VITE ready in 120 ms").isConnected).toBe(true);
    expect(screen.getByText("5173").isConnected).toBe(true);
    expect(screen.getByRole("button", { name: /restart/i }).isConnected).toBe(true);
    expect(screen.queryByRole("button", { name: /^output$/i })).toBeNull();
  });
});
