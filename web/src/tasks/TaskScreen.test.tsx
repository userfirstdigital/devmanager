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
import {
  PAIRED_OWNER_ACTIONS,
  type CapabilityGrant,
} from "../connect/permissions";
import { TaskScreen } from "./TaskScreen";

vi.mock("./views/AiSessionView", () => ({
  AiSessionView: ({
    composer,
    onQuestionChoice,
    questionChoicesDisabled,
  }: {
    composer: React.ReactNode;
    onQuestionChoice?(choice: string): void;
    questionChoicesDisabled?: boolean;
  }) => (
    <div aria-label="Native AI conversation">
      <button
        type="button"
        disabled={questionChoicesDisabled || !onQuestionChoice}
        onClick={() => onQuestionChoice?.("Continue")}
      >
        Continue
      </button>
      {composer}
    </div>
  ),
}));

vi.mock("./views/RawTerminalView", () => ({
  RawTerminalView: ({ interactionLabel }: { interactionLabel?: string }) => (
    <div aria-label="Raw provider interaction">
      {interactionLabel ?? "Raw terminal"}
    </div>
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

function workspace(
  kind: WebAiKind,
  id = "ai-a",
  overrides: Partial<WebWorkspaceSnapshot["sessions"][number]> = {},
): WebWorkspaceSnapshot {
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
        ...overrides,
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
      <TaskScreen
        route={{ name: "task", taskId: "tab:ai-a" }}
        workspace={workspace("codex")}
        status={{ kind: "open" }}
        onNavigate={() => {}}
        grant={ownerGrant()}
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
      expect(
        screen.getByLabelText("Raw provider interaction").textContent,
      ).toMatch(/Codex.*\/model/i),
    );
  });

  it("exits a Claude provider interaction when returning to native", async () => {
    const user = userEvent.setup();
    const submitComposer = vi.fn().mockResolvedValue({
      mutationId: "mutation-claude",
      stableSessionKey: "tab:ai-a",
      acceptedSequence: 1,
      leaseGeneration: 1,
    });
    const sendInput = vi.fn();
    useStore.setState({
      drafts: { "tab:ai-a": "/model" },
      submitComposer,
      sendInput,
    });
    render(
      <TaskScreen
        route={{ name: "task", taskId: "tab:ai-a" }}
        workspace={workspace("claude")}
        status={{ kind: "open" }}
        onNavigate={() => {}}
        grant={ownerGrant()}
      />,
    );

    await user.click(screen.getByRole("button", { name: /send message/i }));
    await screen.findByLabelText("Raw provider interaction");
    await user.click(
      screen.getByRole("button", { name: /return to native conversation/i }),
    );

    expect(sendInput).toHaveBeenCalledWith("pty-ai-a", "\u{1b}", "bytes");
    expect(screen.getByLabelText("Native AI conversation")).toBeTruthy();
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
      <TaskScreen
        route={{ name: "task", taskId: "tab:ai-a" }}
        workspace={workspace("codex", "ai-a")}
        status={{ kind: "open" }}
        onNavigate={() => {}}
        grant={ownerGrant()}
      />,
    );
    await user.click(screen.getByRole("button", { name: /send message/i }));

    rerender(
      <TaskScreen
        route={{ name: "task", taskId: "tab:ai-b" }}
        workspace={workspace("codex", "ai-b")}
        status={{ kind: "open" }}
        onNavigate={() => {}}
        grant={ownerGrant("tab:ai-b")}
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

describe("native AI session interactions", () => {
  it("submits question choices through the composer path and disables while pending", async () => {
    const user = userEvent.setup();
    const submitComposer = vi.fn().mockResolvedValue({
      mutationId: "mutation-choice",
      stableSessionKey: "tab:ai-a",
      acceptedSequence: 1,
      leaseGeneration: 1,
    });
    useStore.setState({
      submitComposer,
      pendingMutations: {
        "tab:ai-a": {
          mutationId: "pending-a",
          stableSessionKey: "tab:ai-a",
          text: "busy",
          attachments: [],
        },
      },
    });
    const { rerender } = render(
      <TaskScreen
        route={{ name: "task", taskId: "tab:ai-a" }}
        workspace={workspace("claude", "ai-a", { attention: "needsInput" })}
        status={{ kind: "open" }}
        onNavigate={() => {}}
        grant={ownerGrant()}
      />,
    );
    expect(
      (screen.getByRole("button", { name: "Continue" }) as HTMLButtonElement)
        .disabled,
    ).toBe(true);

    useStore.setState({ pendingMutations: {} });
    rerender(
      <TaskScreen
        route={{ name: "task", taskId: "tab:ai-a" }}
        workspace={workspace("claude", "ai-a", { attention: "needsInput" })}
        status={{ kind: "open" }}
        onNavigate={() => {}}
        grant={ownerGrant()}
      />,
    );
    await user.click(screen.getByRole("button", { name: "Continue" }));
    expect(submitComposer).toHaveBeenCalledWith("tab:ai-a", "Continue", []);
  });

  it("keeps historical question choices disabled unless attention is needsInput", () => {
    render(
      <TaskScreen
        route={{ name: "task", taskId: "tab:ai-a" }}
        workspace={workspace("claude", "ai-a", { attention: "none" })}
        status={{ kind: "open" }}
        onNavigate={() => {}}
        grant={ownerGrant()}
      />,
    );
    expect(
      (screen.getByRole("button", { name: "Continue" }) as HTMLButtonElement)
        .disabled,
    ).toBe(true);
  });

  it("preserves an unsent composer draft when a quick choice is submitted", async () => {
    const user = userEvent.setup();
    const accepted = deferred<ComposerAccepted>();
    const submitComposer = vi.fn((stableSessionKey: string, text: string) => {
      useStore.setState((state) => ({
        drafts: { ...state.drafts, [stableSessionKey]: text },
        pendingMutations: {
          ...state.pendingMutations,
          [stableSessionKey]: {
            mutationId: "mutation-choice",
            stableSessionKey,
            text,
            attachments: [],
          },
        },
      }));
      return accepted.promise;
    });
    useStore.setState({
      drafts: { "tab:ai-a": "unsent draft notes" },
      submitComposer,
    });

    render(
      <TaskScreen
        route={{ name: "task", taskId: "tab:ai-a" }}
        workspace={workspace("claude", "ai-a", { attention: "needsInput" })}
        status={{ kind: "open" }}
        onNavigate={() => {}}
        grant={ownerGrant()}
      />,
    );

    expect(
      (screen.getByRole("textbox", { name: /message/i }) as HTMLTextAreaElement)
        .value,
    ).toBe("unsent draft notes");

    await user.click(screen.getByRole("button", { name: "Continue" }));
    expect(submitComposer).toHaveBeenCalledWith("tab:ai-a", "Continue", []);
    expect(
      (screen.getByRole("textbox", { name: /message/i }) as HTMLTextAreaElement)
        .value,
    ).toBe("unsent draft notes");
    expect(useStore.getState().drafts["tab:ai-a"]).toBe("unsent draft notes");

    await act(async () => {
      accepted.resolve({
        mutationId: "mutation-choice",
        stableSessionKey: "tab:ai-a",
        acceptedSequence: 1,
        leaseGeneration: 1,
      });
      await accepted.promise;
    });

    expect(
      (screen.getByRole("textbox", { name: /message/i }) as HTMLTextAreaElement)
        .value,
    ).toBe("unsent draft notes");
    expect(useStore.getState().drafts["tab:ai-a"]).toBe("unsent draft notes");
  });

  it("keeps the composer editable during a transient disconnect", () => {
    render(
      <TaskScreen
        route={{ name: "task", taskId: "tab:ai-a" }}
        workspace={workspace("claude")}
        status={{ kind: "connecting" }}
        onNavigate={() => {}}
        grant={ownerGrant()}
      />,
    );

    const textarea = screen.getByRole("textbox", {
      name: /message/i,
    }) as HTMLTextAreaElement;
    expect(textarea.disabled).toBe(false);
    expect(screen.getByText(/reconnecting/i).isConnected).toBe(true);
    expect(
      (screen.getByRole("button", { name: /send/i }) as HTMLButtonElement)
        .disabled,
    ).toBe(true);
  });

  it("shows Stop from Thinking activity, not merely because the PTY is live", () => {
    const interruptSession = vi.fn();
    useStore.setState({ interruptSession });
    const { rerender } = render(
      <TaskScreen
        route={{ name: "task", taskId: "tab:ai-a" }}
        workspace={workspace("claude")}
        status={{ kind: "open" }}
        onNavigate={() => {}}
        grant={ownerGrant()}
      />,
    );
    expect(
      screen.getByRole("button", { name: /send message/i }).isConnected,
    ).toBe(true);

    rerender(
      <TaskScreen
        route={{ name: "task", taskId: "tab:ai-a" }}
        workspace={workspace("claude", "ai-a", { aiActivity: "Thinking" })}
        status={{ kind: "open" }}
        onNavigate={() => {}}
        grant={ownerGrant()}
      />,
    );
    expect(screen.getByRole("button", { name: /stop/i }).isConnected).toBe(
      true,
    );
  });
});

const watcherGrant: CapabilityGrant = {
  role: "watcher",
  taskId: "tab:ai-a",
  actions: ["readTask", "readPresence"],
};

function ownerGrant(taskId = "tab:ai-a"): CapabilityGrant {
  return {
    role: "owner",
    taskId,
    actions: PAIRED_OWNER_ACTIONS,
  };
}

const collaboratorGrant: CapabilityGrant = {
  role: "collaborator",
  taskId: "tab:ai-a",
  actions: ["readTask", "readPresence", "mutateTask", "sendPrompt"],
};

describe("connect permission and controller gating", () => {
  it("lets watchers observe but not send, answer, or use owner controls", () => {
    useStore.setState({
      writerLease: {
        ownerClientInstanceId: "host-owner",
        generation: 4,
        expiresAtEpochMs: 10_000,
        youAreOwner: false,
      },
    });
    render(
      <TaskScreen
        route={{ name: "task", taskId: "tab:ai-a" }}
        workspace={workspace("claude", "ai-a", { attention: "needsInput" })}
        status={{ kind: "open" }}
        onNavigate={() => {}}
        grant={watcherGrant}
      />,
    );

    expect(screen.getByTestId("guest-action-disabled").textContent).toMatch(
      /view only/i,
    );
    expect(screen.queryByRole("button", { name: /send message/i })).toBeNull();
    expect(
      (screen.getByRole("button", { name: "Continue" }) as HTMLButtonElement)
        .disabled,
    ).toBe(true);
    expect(screen.queryByTestId("connect-owner-badge")).toBeNull();
    expect(screen.getByTestId("connect-visible-controller").textContent).toMatch(
      /host-owner/,
    );
  });

  it("keeps collaborator send enabled and hides owner-only badge", async () => {
    const user = userEvent.setup();
    const submitComposer = vi.fn().mockResolvedValue({
      mutationId: "mutation-collab",
      stableSessionKey: "tab:ai-a",
      acceptedSequence: 1,
      leaseGeneration: 1,
    });
    useStore.setState({
      drafts: { "tab:ai-a": "hello from collaborator" },
      submitComposer,
      writerLease: {
        ownerClientInstanceId: "host-owner",
        generation: 4,
        expiresAtEpochMs: 10_000,
        youAreOwner: false,
      },
    });
    render(
      <TaskScreen
        route={{ name: "task", taskId: "tab:ai-a" }}
        workspace={workspace("claude")}
        status={{ kind: "open" }}
        onNavigate={() => {}}
        grant={collaboratorGrant}
      />,
    );

    expect(screen.queryByTestId("guest-action-disabled")).toBeNull();
    expect(screen.queryByTestId("connect-owner-badge")).toBeNull();
    await user.click(screen.getByRole("button", { name: /send message/i }));
    expect(submitComposer).toHaveBeenCalledWith(
      "tab:ai-a",
      "hello from collaborator",
      [],
    );
  });

  it("shows a denied notice and does not submit when sendPrompt is missing", async () => {
    const user = userEvent.setup();
    const submitComposer = vi.fn();
    useStore.setState({
      drafts: { "tab:ai-a": "blocked text" },
      submitComposer,
    });
    render(
      <TaskScreen
        route={{ name: "task", taskId: "tab:ai-a" }}
        workspace={workspace("claude")}
        status={{ kind: "open" }}
        onNavigate={() => {}}
        grant={{
          role: "collaborator",
          taskId: "tab:ai-a",
          actions: ["readTask", "readPresence", "mutateTask"],
        }}
      />,
    );

    expect(screen.getByTestId("guest-action-disabled").textContent).toMatch(
      /not permitted/i,
    );
    await user.click(screen.getByRole("button", { name: /send message/i }));
    expect(submitComposer).not.toHaveBeenCalled();
  });

  it("shows owner badge and controller for an authoritative owner", () => {
    useStore.setState({
      writerLease: {
        ownerClientInstanceId: "paired-owner",
        generation: 9,
        expiresAtEpochMs: 10_000,
        youAreOwner: true,
      },
    });
    render(
      <TaskScreen
        route={{ name: "task", taskId: "tab:ai-a" }}
        workspace={workspace("claude")}
        status={{ kind: "open" }}
        onNavigate={() => {}}
        grant={ownerGrant()}
      />,
    );

    expect(screen.getByTestId("connect-owner-badge").textContent).toMatch(
      /owner/i,
    );
    expect(screen.getByTestId("connect-visible-controller").textContent).toMatch(
      /paired-owner/,
    );
  });
});
