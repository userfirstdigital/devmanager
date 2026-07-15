// @vitest-environment jsdom

import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  WEB_PROTOCOL_VERSION,
  type ComposerAccepted,
  type WebAiKind,
  type WebWorkspaceSnapshot,
} from "../api/types";
import { EMPTY_WRITER_LEASE } from "../api/types";
import { useStore } from "../store";
import { SessionScreen } from "./SessionScreen";

vi.mock("./views/AiSessionView", () => ({
  AiSessionView: ({ composer }: { composer: React.ReactNode }) => (
    <div aria-label="Native AI conversation">{composer}</div>
  ),
}));

vi.mock("./views/RawTerminalView", () => ({
  RawTerminalView: ({ interactionLabel }: { interactionLabel?: string }) => (
    <div aria-label="Raw provider interaction">{interactionLabel ?? "Raw terminal"}</div>
  ),
}));

vi.mock("../settings/densityPreference", () => ({
  useDensityPreference: () => ["compact"],
}));

vi.mock("../settings/inputPreference", () => ({
  useReturnBehavior: () => ["newline"],
  useTerminalPreference: () => ["native"],
}));

function deferred<T>() {
  let resolve: (value: T) => void = () => {};
  const promise = new Promise<T>((resolvePromise) => {
    resolve = resolvePromise;
  });
  return { promise, resolve };
}

function workspace(kind: WebAiKind, id = "ai-a"): WebWorkspaceSnapshot {
  return {
    webProtocolVersion: WEB_PROTOCOL_VERSION,
    runtimeInstanceId: "runtime-a",
    revision: 1,
    serverId: "server-a",
    projects: [
      {
        id: "project-a",
        name: "DevManager",
        color: null,
        folders: [],
      },
    ],
    sshConnections: [],
    tabs: [
      {
        id,
        kind,
        projectId: "project-a",
        commandId: null,
        sessionId: `pty-${id}`,
        connectionId: null,
        label: `${kind} work`,
      },
    ],
    sessions: [
      {
        sessionId: `pty-${id}`,
        stableSessionKey: `tab:${id}`,
        kind,
        status: "Running",
        projectId: "project-a",
        commandId: null,
        tabId: id,
        dimensions: { cols: 80, rows: 24, cell_width: 10, cell_height: 20 },
        lastActivityEpochMs: 1,
        attention: "none",
        attentionCount: 0,
        adapterHealth: "healthy",
        rawRequired: false,
        oldestSequence: 0,
        latestSequence: 0,
      },
    ],
    portStatuses: [],
    writerLease: EMPTY_WRITER_LEASE,
  };
}

beforeEach(() => {
  localStorage.clear();
  useStore.setState({
    drafts: {},
    pendingMutations: {},
    composerSafety: {},
    writerLease: { ...EMPTY_WRITER_LEASE },
  });
  vi.spyOn(globalThis, "fetch").mockResolvedValue({
    ok: true,
    status: 200,
    json: async () => ({ provider: "codex", commands: [] }),
  } as Response);
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("session slash command integration", () => {
  it("opens the real provider interaction only after /model is acknowledged", async () => {
    const user = userEvent.setup();
    const accepted = deferred<ComposerAccepted>();
    const submitComposer = vi.fn(() => accepted.promise);
    useStore.setState({
      drafts: { "tab:ai-a": "/model" },
      submitComposer,
    });
    render(
      <SessionScreen
        route={{ name: "session", kind: "tab", id: "ai-a" }}
        workspace={workspace("codex")}
        status={{ kind: "open" }}
        onNavigate={() => {}}
      />,
    );

    await user.click(screen.getByRole("button", { name: /send message/i }));
    expect(screen.getByLabelText("Native AI conversation")).toBeTruthy();
    expect(screen.queryByLabelText("Raw provider interaction")).toBeNull();

    await act(async () => {
      accepted.resolve({
        mutationId: "mutation-a",
        stableSessionKey: "tab:ai-a",
        acceptedSequence: 1,
        leaseGeneration: 1,
      });
      await accepted.promise;
    });
    await waitFor(() =>
      expect(screen.getByLabelText("Raw provider interaction").textContent).toMatch(
        /Codex.*\/model/i,
      ),
    );
  });

  it("keeps inline commands native and ignores an old acknowledgement after scope change", async () => {
    const user = userEvent.setup();
    const oldAccepted = deferred<ComposerAccepted>();
    const submitComposer = vi
      .fn()
      .mockReturnValueOnce(oldAccepted.promise)
      .mockResolvedValue({
        mutationId: "mutation-b",
        stableSessionKey: "tab:ai-b",
        acceptedSequence: 2,
        leaseGeneration: 1,
      });
    useStore.setState({
      drafts: { "tab:ai-a": "/model", "tab:ai-b": "/compact" },
      submitComposer,
    });
    const { rerender } = render(
      <SessionScreen
        route={{ name: "session", kind: "tab", id: "ai-a" }}
        workspace={workspace("codex", "ai-a")}
        status={{ kind: "open" }}
        onNavigate={() => {}}
      />,
    );
    await user.click(screen.getByRole("button", { name: /send message/i }));

    rerender(
      <SessionScreen
        route={{ name: "session", kind: "tab", id: "ai-b" }}
        workspace={workspace("codex", "ai-b")}
        status={{ kind: "open" }}
        onNavigate={() => {}}
      />,
    );
    await act(async () => {
      oldAccepted.resolve({
        mutationId: "mutation-a",
        stableSessionKey: "tab:ai-a",
        acceptedSequence: 1,
        leaseGeneration: 1,
      });
      await oldAccepted.promise;
    });
    expect(screen.queryByLabelText("Raw provider interaction")).toBeNull();

    await user.click(screen.getByRole("button", { name: /send message/i }));
    await waitFor(() => expect(submitComposer).toHaveBeenCalledTimes(2));
    expect(screen.getByLabelText("Native AI conversation")).toBeTruthy();
    expect(screen.queryByLabelText("Raw provider interaction")).toBeNull();
  });
});
