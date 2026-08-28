// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { NativeHostSession, NativeHostSessionView } from "./nativeSession";
import { NativeRemoteApp } from "./NativeRemoteApp";

const HOST_ID = "019c6e27-e55b-73d1-87d8-4e01f1f75043";
const CLIENT_ID = "019c6e27-e55b-73d1-87d8-4e01f1f75044";
const TASK_ID = "019c6e27-e55b-73d1-87d8-4e01f1f75045";
const SECOND_TASK_ID = "019c6e27-e55b-73d1-87d8-4e01f1f75046";

function task(
  taskId: string,
  title: string,
  lifecycle = "open",
  updatedAtMs = 10,
) {
  return {
    taskId,
    revision: 4,
    actionEpoch: 2,
    title,
    lifecycle,
    projectId: null,
    environmentId: null,
    createdAtMs: null,
    connectivity: "connected",
    attention: null,
    activity: "active",
    primaryAgentId: null,
    updatedAtMs,
  };
}

function makeView(
  overrides: Partial<NativeHostSessionView> = {},
): NativeHostSessionView {
  return {
    hostPublicId: HOST_ID,
    connectionStatus: "ready",
    syncStatus: "live",
    clientId: CLIENT_ID,
    capabilities: 0,
    leaseEpoch: 1,
    tasks: new Map([
      [
        TASK_ID,
        task(TASK_ID, "Investigate mobile sync"),
      ],
    ]),
    conversations: new Map([
      [
        TASK_ID,
        {
          taskId: TASK_ID,
          afterSequence: 0,
          throughSequence: 2,
          highWater: 2,
          oldestSequence: 1,
          cursorRolledOver: false,
          nextSequence: 3,
          facts: [
            {
              id: "fact-1",
              sequence: 1,
              occurredAtMs: 1,
              provider: "native",
              schemaVersion: 1,
              kind: "user_message",
              visibility: "local",
              privacyClass: "local_only",
              redacted: false,
              payload: { kind: "user_message", text: "Status?" },
            },
            {
              id: "fact-2",
              sequence: 2,
              occurredAtMs: 2,
              provider: "native",
              schemaVersion: 1,
              kind: "assistant_text",
              visibility: "local",
              privacyClass: "local_only",
              redacted: false,
              payload: { kind: "assistant_text", text: "Connected." },
            },
          ],
          updatedAtMs: 10,
        },
      ],
    ]),
    drafts: new Map(),
    outbox: new Map(),
    lastError: null,
    replayThrough: 2,
    ...overrides,
  };
}

function makeSession(
  view = makeView(),
  overrides: Partial<
    Pick<
      NativeHostSession,
      | "sendText"
      | "mutateTask"
      | "settleTask"
      | "reopenTask"
      | "beginCloseTask"
      | "deleteTask"
      | "renameTask"
    >
  > = {},
) {
  const mutateTask =
    overrides.mutateTask ??
    vi.fn(async () => ({ ok: true as const, commandId: "cmd-mutate" }));
  return Object.assign({
    view: vi.fn(() => view),
    subscribe: vi.fn(() => () => undefined),
    watchTask: vi.fn(async () => undefined),
    unwatchTask: vi.fn(async () => undefined),
    setDraft: vi.fn(async () => undefined),
    sendText: vi.fn(async () => ({ ok: true as const, commandId: "cmd-1" })),
    sendTerminalKey: vi.fn(async () => ({ ok: true as const, commandId: "cmd-key" })),
    readTerminal: vi.fn(async (taskId: string) => ({ taskId, sequence: 1, title: null, textLines: ["Codex ready"] })),
    mutateTask,
    settleTask: overrides.settleTask ?? vi.fn(async (taskId: string) => mutateTask(taskId, { kind: "settle" })),
    reopenTask: overrides.reopenTask ?? vi.fn(async (taskId: string) => mutateTask(taskId, { kind: "reopen" })),
    beginCloseTask:
      overrides.beginCloseTask ??
      vi.fn(async (taskId: string) => mutateTask(taskId, { kind: "begin_close" })),
    deleteTask: overrides.deleteTask ?? vi.fn(async (taskId: string) => mutateTask(taskId, { kind: "delete" })),
    renameTask:
      overrides.renameTask ??
      vi.fn(async (taskId: string, title: string) =>
        mutateTask(taskId, { kind: "rename", title }),
      ),
  }, overrides) as unknown as NativeHostSession;
}

describe("NativeRemoteApp", () => {
  beforeEach(() => {
    window.localStorage.clear();
    window.history.replaceState(null, "", "/tasks");
  });

  afterEach(() => {
    cleanup();
    window.localStorage.clear();
    window.history.replaceState(null, "", "/tasks");
  });

  it("offers rename for Done without reopening the task", async () => {
    const session = makeSession(makeView({
      tasks: new Map([[TASK_ID, task(TASK_ID, "Finished task", "settled")]]),
    }));
    render(<NativeRemoteApp hostPublicId={HOST_ID} session={session} />);
    fireEvent.click(screen.getByRole("button", { name: /finished task/i }));
    fireEvent.click(screen.getByRole("button", { name: "Task actions" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Rename" }));
    fireEvent.change(screen.getByLabelText("New task title"), {
      target: { value: "Renamed finished task" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save title" }));
    await waitFor(() => expect(session.mutateTask).toHaveBeenCalledWith(TASK_ID, {
      kind: "rename", title: "Renamed finished task",
    }));
    expect(session.reopenTask).not.toHaveBeenCalled();
    expect(session.view().tasks.get(TASK_ID)?.lifecycle).toBe("settled");
  });

  it("opens the owner terminal on demand without replacing the chat draft", async () => {
    const session = makeSession();
    render(<NativeRemoteApp hostPublicId={HOST_ID} session={session} />);
    fireEvent.click(screen.getByRole("button", { name: /investigate mobile sync/i }));
    fireEvent.change(screen.getByRole("textbox", { name: "Message" }), { target: { value: "unsent" } });
    expect(session.readTerminal).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Show terminal" }));
    expect(await screen.findByText("Codex ready")).not.toBeNull();
    expect(session.readTerminal).toHaveBeenCalledWith(TASK_ID);
    expect(screen.queryByRole("textbox", { name: "Message" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Terminal enter" }));
    expect(session.sendTerminalKey).toHaveBeenCalledWith(TASK_ID, "enter");
    fireEvent.click(screen.getByRole("button", { name: "Show conversation" }));
    expect((screen.getByRole("textbox", { name: "Message" }) as HTMLTextAreaElement).value).toBe("unsent");
    expect(session.sendText).not.toHaveBeenCalled();
  });

  it("opens a canonical watched task and renders its cached semantic conversation", async () => {
    const session = makeSession();

    render(
      <NativeRemoteApp
        hostPublicId={HOST_ID}
        hostLabel="Studio PC"
        session={session}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /investigate mobile sync/i }));

    expect(session.watchTask).toHaveBeenCalledWith(TASK_ID);
    expect(await screen.findByText("Connected.")).not.toBeNull();
    expect(screen.getByRole("textbox", { name: /message/i })).not.toBeNull();
  });

  it("shows the actual connection failure without hiding cached tasks", () => {
    const session = makeSession(makeView({
      connectionStatus: "degraded",
      syncStatus: "error",
      lastError: "Connect protocol rejected",
    }));
    render(<NativeRemoteApp hostPublicId={HOST_ID} session={session} />);
    expect(screen.getByText("Connect protocol rejected")).not.toBeNull();
    expect(screen.getByRole("button", { name: /investigate mobile sync/i })).not.toBeNull();
  });

  it("keeps the offline draft editable while disabling send", () => {
    const session = makeSession(
      makeView({ connectionStatus: "degraded", syncStatus: "syncing_replay" }),
    );

    render(
      <NativeRemoteApp
        hostPublicId={HOST_ID}
        session={session}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /investigate mobile sync/i }));
    const composer = screen.getByRole("textbox", { name: /message/i });
    fireEvent.change(composer, { target: { value: "Keep this offline." } });

    expect((composer as HTMLTextAreaElement).value).toBe("Keep this offline.");
    expect(session.setDraft).toHaveBeenCalledWith(TASK_ID, "Keep this offline.");
    expect((screen.getByRole("button", { name: /send/i }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.queryByText(/loading/i)).toBeNull();
  });

  it("clears a draft only after the selected task's send is accepted", async () => {
    const session = makeSession();
    render(
      <NativeRemoteApp
        hostPublicId={HOST_ID}
        session={session}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /investigate mobile sync/i }));
    const composer = screen.getByRole("textbox", { name: /message/i });
    fireEvent.change(composer, { target: { value: "Send this once." } });
    fireEvent.click(screen.getByRole("button", { name: /send/i }));

    await waitFor(() => expect((composer as HTMLTextAreaElement).value).toBe(""));
    expect(session.sendText).toHaveBeenCalledWith(TASK_ID, "Send this once.");
    expect(session.setDraft).toHaveBeenLastCalledWith(TASK_ID, "");
  });

  it("keeps Done compact at the bottom and archives behind a separate icon", () => {
    const session = makeSession(
      makeView({
        tasks: new Map([
          [TASK_ID, task(TASK_ID, "Open task", "open", 4)],
          [SECOND_TASK_ID, task(SECOND_TASK_ID, "Settled task", "settled", 3)],
          ["019c6e27-e55b-73d1-87d8-4e01f1f75047", task("019c6e27-e55b-73d1-87d8-4e01f1f75047", "Archived task", "archived", 2)],
          ["019c6e27-e55b-73d1-87d8-4e01f1f75048", task("019c6e27-e55b-73d1-87d8-4e01f1f75048", "Deleted task", "deleted", 1)],
          ["019c6e27-e55b-73d1-87d8-4e01f1f75049", task("019c6e27-e55b-73d1-87d8-4e01f1f75049", "Closing task", "closing", 5)],
        ]),
      }),
    );

    render(<NativeRemoteApp hostPublicId={HOST_ID} session={session} />);

    expect(screen.getByText("Open task")).not.toBeNull();
    expect(screen.getByText("Closing task")).not.toBeNull();
    expect(screen.getByText(/Archiving/i)).not.toBeNull();
    expect(screen.getByText("Done (1)")).not.toBeNull();
    expect((screen.getByText("Done (1)").closest("details") as HTMLDetailsElement).open).toBe(true);
    expect(screen.getByRole("region", { name: "Task inbox" }).contains(screen.getByText("Done (1)"))).toBe(true);
    expect(screen.queryByText("Archived task")).toBeNull();
    expect(screen.queryByText("Deleted task")).toBeNull();
    expect(screen.getByRole("button", { name: "Host status" })).not.toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Show archived tasks" }));
    expect(screen.getByRole("region", { name: "Archived tasks" })).not.toBeNull();
    expect(screen.getByText("Archived task")).not.toBeNull();
    expect(screen.queryByRole("region", { name: "Task inbox" })).toBeNull();
  });

  it("clears an accepted cached draft when its local draft version is initially absent", async () => {
    const session = makeSession(
      makeView({
        drafts: new Map([[TASK_ID, { taskId: TASK_ID, text: "Cached draft", updatedAtMs: 1 }]]),
      }),
    );
    render(<NativeRemoteApp hostPublicId={HOST_ID} session={session} />);

    fireEvent.click(screen.getByRole("button", { name: /investigate mobile sync/i }));
    const composer = screen.getByRole("textbox", { name: /message/i }) as HTMLTextAreaElement;
    expect(composer.value).toBe("Cached draft");
    fireEvent.click(screen.getByRole("button", { name: /send/i }));

    await waitFor(() => expect(composer.value).toBe(""));
    expect(session.sendText).toHaveBeenCalledWith(TASK_ID, "Cached draft");
    expect(session.setDraft).toHaveBeenLastCalledWith(TASK_ID, "");
  });

  it("preserves a newer draft after switching tasks while an earlier send is pending", async () => {
    let settleSend: (value: { ok: true; commandId: string }) => void = () => { throw new Error("send was not started"); };
    const sendText = vi.fn(
      () =>
        new Promise<{ ok: true; commandId: string }>((resolve) => {
          settleSend = resolve;
        }),
    );
    const session = makeSession(
      makeView({
        tasks: new Map([
          [TASK_ID, task(TASK_ID, "First task", "open", 2)],
          [SECOND_TASK_ID, task(SECOND_TASK_ID, "Second task", "open", 1)],
        ]),
      }),
      { sendText: sendText as unknown as NativeHostSession["sendText"] },
    );
    render(<NativeRemoteApp hostPublicId={HOST_ID} session={session} />);

    fireEvent.click(screen.getByRole("button", { name: /first task/i }));
    fireEvent.change(screen.getByRole("textbox", { name: /message/i }), {
      target: { value: "First send" },
    });
    fireEvent.click(screen.getByRole("button", { name: /send/i }));
    fireEvent.click(screen.getByRole("button", { name: /back to tasks/i }));
    fireEvent.click(screen.getByRole("button", { name: /second task/i }));
    const secondComposer = screen.getByRole("textbox", { name: /message/i }) as HTMLTextAreaElement;
    fireEvent.change(secondComposer, { target: { value: "Newer second draft" } });

    settleSend({ ok: true, commandId: "cmd-1" });

    await waitFor(() => expect(secondComposer.value).toBe("Newer second draft"));
    expect(session.setDraft).toHaveBeenCalledWith(SECOND_TASK_ID, "Newer second draft");
  });

  it("exposes task actions that mutate the frozen owner and never submit the composer", async () => {
    const mutateTask = vi.fn(async () => ({ ok: true as const, commandId: "cmd-m" }));
    const session = makeSession(makeView(), { mutateTask });
    render(<NativeRemoteApp hostPublicId={HOST_ID} session={session} />);
    fireEvent.click(screen.getByRole("button", { name: /investigate mobile sync/i }));
    fireEvent.change(screen.getByRole("textbox", { name: "Message" }), {
      target: { value: "do not send" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Task actions" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Done" }));
    await waitFor(() =>
      expect(mutateTask).toHaveBeenCalledWith(TASK_ID, { kind: "settle" }),
    );
    expect(session.sendText).not.toHaveBeenCalled();
    expect((screen.getByRole("textbox", { name: "Message" }) as HTMLTextAreaElement).value).toBe(
      "do not send",
    );
  });

  it("closes a deleted task only after the owning projection confirms deletion", async () => {
    const session = makeSession(makeView({
      tasks: new Map([[TASK_ID, task(TASK_ID, "Archived task", "archived")]]),
    }));
    let publish: (view: NativeHostSessionView) => void = () => {};
    vi.mocked(session.subscribe).mockImplementation((listener) => {
      publish = listener;
      return () => {};
    });
    render(<NativeRemoteApp hostPublicId={HOST_ID} session={session} />);
    fireEvent.click(screen.getByRole("button", { name: "Show archived tasks" }));
    fireEvent.click(screen.getByRole("button", { name: /^archived task/i }));
    fireEvent.change(screen.getByRole("textbox", { name: "Message" }), {
      target: { value: "Unsent draft" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Task actions" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Delete" }));
    fireEvent.click(screen.getByRole("button", { name: "Confirm delete" }));
    await waitFor(() => expect(session.mutateTask).toHaveBeenCalledWith(TASK_ID, { kind: "delete" }));
    expect(screen.getByRole("textbox", { name: "Message" })).not.toBeNull();
    expect((screen.getByRole("button", { name: "Send" }) as HTMLButtonElement).disabled).toBe(true);
    act(() => publish(makeView({
      tasks: new Map([[TASK_ID, task(TASK_ID, "Archived task", "deleted")]]),
    })));
    expect(screen.queryByRole("textbox", { name: "Message" })).toBeNull();
    expect(screen.queryByRole("button", { name: /^archived task/i })).toBeNull();
    expect(window.location.pathname).toBe("/tasks");
    expect(session.sendText).not.toHaveBeenCalled();
  });

  it("shows archiving progress only while the canonical lifecycle is closing", async () => {
    const session = makeSession();
    let publish: (view: NativeHostSessionView) => void = () => {};
    vi.mocked(session.subscribe).mockImplementation((listener) => {
      publish = listener;
      return () => {};
    });
    render(<NativeRemoteApp hostPublicId={HOST_ID} session={session} />);
    fireEvent.click(screen.getByRole("button", { name: /investigate mobile sync/i }));
    fireEvent.click(screen.getByRole("button", { name: "Task actions" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Archive" }));
    await waitFor(() => expect(session.mutateTask).toHaveBeenCalledWith(TASK_ID, { kind: "begin_close" }));
    act(() => publish(makeView({
      tasks: new Map([[TASK_ID, task(TASK_ID, "Investigate mobile sync", "closing")]]),
    })));
    expect(screen.getByText("Archiving in progress")).not.toBeNull();
    act(() => publish(makeView({
      tasks: new Map([[TASK_ID, task(TASK_ID, "Investigate mobile sync", "archived")]]),
    })));
    expect(screen.getByText("Archived · restore to write")).not.toBeNull();
    expect(screen.queryByText("Archiving in progress")).toBeNull();
    expect(screen.queryByText(/The host is still closing/i)).toBeNull();
  });

  it("keeps Done sendable without Restore while archived requires Restore; Done click settles only", async () => {
    const mutateTask = vi.fn(async () => ({ ok: true as const, commandId: "cmd-m" }));
    const session = makeSession(
      makeView({
        tasks: new Map([
          [TASK_ID, task(TASK_ID, "Settled task", "settled")],
        ]),
      }),
      { mutateTask },
    );
    render(<NativeRemoteApp hostPublicId={HOST_ID} session={session} />);
    fireEvent.click(screen.getByRole("button", { name: /settled task/i }));
    expect(screen.getByText(/send restores automatically/i)).not.toBeNull();
    fireEvent.change(screen.getByRole("textbox", { name: "Message" }), {
      target: { value: "resume from done" },
    });
    expect(
      (screen.getByRole("button", { name: /send/i }) as HTMLButtonElement).disabled,
    ).toBe(false);
    fireEvent.click(screen.getByRole("button", { name: /send/i }));
    await waitFor(() =>
      expect(session.sendText).toHaveBeenCalledWith(TASK_ID, "resume from done"),
    );
    expect(mutateTask).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Task actions" }));
    expect(screen.queryByRole("menuitem", { name: "Done" })).toBeNull();
    expect(screen.queryByRole("menuitem", { name: "Delete" })).toBeNull();
    fireEvent.click(screen.getByRole("menuitem", { name: "Restore" }));
    await waitFor(() =>
      expect(mutateTask).toHaveBeenCalledWith(TASK_ID, { kind: "reopen" }),
    );

    cleanup();
    window.history.replaceState(null, "", "/");
    window.localStorage.clear();
    const archived = makeSession(
      makeView({
        tasks: new Map([
          [TASK_ID, task(TASK_ID, "Archived task", "archived")],
        ]),
      }),
      { mutateTask: vi.fn(async () => ({ ok: true as const, commandId: "cmd-del" })) },
    );
    render(<NativeRemoteApp hostPublicId={HOST_ID} session={archived} />);
    fireEvent.click(screen.getByRole("button", { name: "Show archived tasks" }));
    fireEvent.click(screen.getByRole("button", { name: /^archived task/i }));
    fireEvent.change(screen.getByRole("textbox", { name: "Message" }), {
      target: { value: "blocked until restore" },
    });
    expect(
      (screen.getByRole("button", { name: /send/i }) as HTMLButtonElement).disabled,
    ).toBe(true);
    expect(archived.sendText).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Task actions" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Delete" }));
    expect(archived.mutateTask).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Confirm delete" }));
    await waitFor(() =>
      expect(archived.mutateTask).toHaveBeenCalledWith(TASK_ID, { kind: "delete" }),
    );
  });
});

describe("NativeRemoteApp multi-host fleet", () => {
  const HOST_B = "019c6e27-e55b-73d1-87d8-4e01f1f75050";
  const SAME_TASK = TASK_ID;

  function fleetSnapshot(
    hostPublicId: string,
    label: string,
    view: NativeHostSessionView,
    isPageHost: boolean,
  ) {
    return {
      descriptor: {
        hostPublicId,
        hostPublicKey: "ab".repeat(32),
        origin: isPageHost ? "http://127.0.0.1:8787" : "https://studio.example",
        label,
        generation: 1,
        protocolMajor: 1 as const,
        protocolMinor: 0,
        isPageHost,
      },
      view,
      hydrationKnown: true,
      pairingState: "ready" as const,
      transportAttached: view.connectionStatus === "ready",
      authenticated: view.connectionStatus === "ready",
      notice: null,
      cacheAvailable: true,
    };
  }

  beforeEach(() => {
    window.localStorage.clear();
    window.history.replaceState(null, "", "/tasks");
  });

  afterEach(() => {
    cleanup();
    window.localStorage.clear();
    window.history.replaceState(null, "", "/tasks");
  });

  it("lists the same task UUID from two hosts as independent rows and routes", async () => {
    const sessionA = makeSession(
      makeView({
        hostPublicId: HOST_ID,
        tasks: new Map([[SAME_TASK, task(SAME_TASK, "Page copy", "open", 2)]]),
      }),
    );
    const sessionB = makeSession(
      makeView({
        hostPublicId: HOST_B,
        connectionStatus: "degraded",
        syncStatus: "error",
        tasks: new Map([[SAME_TASK, task(SAME_TASK, "Studio copy", "open", 1)]]),
        conversations: new Map(),
      }),
    );
    render(
      <NativeRemoteApp
        hostPublicId={HOST_ID}
        session={sessionA}
        pageHostPublicId={HOST_ID}
        hostSessions={new Map([
          [HOST_ID, sessionA],
          [HOST_B, sessionB],
        ])}
        fleetEntries={[
          fleetSnapshot(HOST_ID, "Page", sessionA.view(), true),
          fleetSnapshot(HOST_B, "Studio", sessionB.view(), false),
        ]}
      />,
    );

    expect(screen.getByText("Page copy")).not.toBeNull();
    expect(screen.getByText("Studio copy")).not.toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /studio copy/i }));
    expect(window.location.pathname).toBe(
      `/tasks/${encodeURIComponent(HOST_B)}/${encodeURIComponent(SAME_TASK)}`,
    );
    expect(sessionB.watchTask).toHaveBeenCalledWith(SAME_TASK);
    expect(sessionA.watchTask).not.toHaveBeenCalled();
  });

  it("does not let a deferred send on A clear drafts while B is selected", async () => {
    let settleSend: (value: { ok: true; commandId: string }) => void = () => {
      throw new Error("send was not started");
    };
    const sessionA = makeSession(makeView(), {
      sendText: vi.fn(
        () =>
          new Promise<{ ok: true; commandId: string }>((resolve) => {
            settleSend = resolve;
          }),
      ) as unknown as NativeHostSession["sendText"],
    });
    const sessionB = makeSession(
      makeView({
        hostPublicId: HOST_B,
        tasks: new Map([
          [SECOND_TASK_ID, task(SECOND_TASK_ID, "Remote task", "open", 1)],
        ]),
        conversations: new Map(),
      }),
    );
    render(
      <NativeRemoteApp
        hostPublicId={HOST_ID}
        session={sessionA}
        pageHostPublicId={HOST_ID}
        hostSessions={new Map([
          [HOST_ID, sessionA],
          [HOST_B, sessionB],
        ])}
        fleetEntries={[
          fleetSnapshot(HOST_ID, "Page", sessionA.view(), true),
          fleetSnapshot(HOST_B, "Studio", sessionB.view(), false),
        ]}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /investigate mobile sync/i }));
    fireEvent.change(screen.getByRole("textbox", { name: /message/i }), {
      target: { value: "A pending" },
    });
    fireEvent.click(screen.getByRole("button", { name: /send/i }));
    fireEvent.click(screen.getByRole("button", { name: /back to tasks/i }));
    fireEvent.click(screen.getByRole("button", { name: /remote task/i }));
    const bComposer = screen.getByRole("textbox", { name: /message/i }) as HTMLTextAreaElement;
    fireEvent.change(bComposer, { target: { value: "B draft stays" } });
    settleSend({ ok: true, commandId: "cmd-a" });
    await waitFor(() => expect(bComposer.value).toBe("B draft stays"));
    expect(sessionB.setDraft).toHaveBeenCalledWith(SECOND_TASK_ID, "B draft stays");
  });

  it("shows unavailable for an unknown owner route without dialing", () => {
    const session = makeSession();
    window.history.replaceState(
      null,
      "",
      `/tasks/019c6e27-e55b-73d1-87d8-4e01f1f75999/${TASK_ID}`,
    );
    render(
      <NativeRemoteApp
        hostPublicId={HOST_ID}
        session={session}
        pageHostPublicId={HOST_ID}
        hostSessions={new Map([[HOST_ID, session]])}
        fleetEntries={[fleetSnapshot(HOST_ID, "Page", session.view(), true)]}
      />,
    );
    expect(screen.getByText(/host unavailable/i)).not.toBeNull();
    expect(session.watchTask).not.toHaveBeenCalled();
  });

  it("keeps Send enabled for live A while offline B has pending outbox", () => {
    const sessionA = makeSession(makeView());
    const sessionB = makeSession(
      makeView({
        hostPublicId: HOST_B,
        connectionStatus: "degraded",
        syncStatus: "error",
        tasks: new Map([
          [SECOND_TASK_ID, task(SECOND_TASK_ID, "Remote task", "open", 1)],
        ]),
        conversations: new Map(),
        outbox: new Map([
          [
            "cmd-b",
            {
              hostPublicId: HOST_B,
              clientId: CLIENT_ID,
              commandId: "019c6e27-e55b-73d1-87d8-4e01f1f75070",
              taskId: SECOND_TASK_ID,
              commandPayload: {},
              text: "queued on B",
              issuedAtMs: 1,
              status: "pending",
              updatedAtMs: 1,
            },
          ],
        ]),
      }),
    );
    render(
      <NativeRemoteApp
        hostPublicId={HOST_ID}
        session={sessionA}
        pageHostPublicId={HOST_ID}
        hostSessions={new Map([
          [HOST_ID, sessionA],
          [HOST_B, sessionB],
        ])}
        fleetEntries={[
          fleetSnapshot(HOST_ID, "Page", sessionA.view(), true),
          fleetSnapshot(HOST_B, "Studio", sessionB.view(), false),
        ]}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /investigate mobile sync/i }));
    fireEvent.change(screen.getByRole("textbox", { name: /message/i }), {
      target: { value: "Send on A" },
    });
    expect(
      (screen.getByRole("button", { name: /send/i }) as HTMLButtonElement).disabled,
    ).toBe(false);
  });

  it("migrates legacy /tasks/:taskId only to the page host", () => {
    const session = makeSession();
    window.history.replaceState(null, "", `/tasks/${TASK_ID}`);
    render(
      <NativeRemoteApp
        hostPublicId={HOST_ID}
        session={session}
        pageHostPublicId={HOST_ID}
        hostSessions={new Map([[HOST_ID, session]])}
        fleetEntries={[fleetSnapshot(HOST_ID, "Page", session.view(), true)]}
      />,
    );
    expect(session.watchTask).toHaveBeenCalledWith(TASK_ID);
    expect(window.location.pathname).toBe(
      `/tasks/${encodeURIComponent(HOST_ID)}/${encodeURIComponent(TASK_ID)}`,
    );
  });

  it("keeps A and B drafts isolated for the same task UUID across host switches", async () => {
    const sessionA = makeSession(
      makeView({
        drafts: new Map([[SAME_TASK, { taskId: SAME_TASK, text: "A draft", updatedAtMs: 1 }]]),
      }),
    );
    const sessionB = makeSession(
      makeView({
        hostPublicId: HOST_B,
        tasks: new Map([[SAME_TASK, task(SAME_TASK, "Studio copy", "open", 1)]]),
        conversations: new Map(),
        drafts: new Map([[SAME_TASK, { taskId: SAME_TASK, text: "B draft", updatedAtMs: 1 }]]),
      }),
    );
    const sessions = new Map([
      [HOST_ID, sessionA],
      [HOST_B, sessionB],
    ]);
    const { rerender } = render(
      <NativeRemoteApp
        hostPublicId={HOST_ID}
        session={sessionA}
        pageHostPublicId={HOST_ID}
        hostSessions={sessions}
        fleetEntries={[
          fleetSnapshot(HOST_ID, "Page", sessionA.view(), true),
          fleetSnapshot(HOST_B, "Studio", sessionB.view(), false),
        ]}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /investigate mobile sync/i }));
    const composer = screen.getByRole("textbox", { name: /message/i }) as HTMLTextAreaElement;
    expect(composer.value).toBe("A draft");
    fireEvent.change(composer, { target: { value: "A typed newer" } });
    fireEvent.click(screen.getByRole("button", { name: /back to tasks/i }));
    fireEvent.click(screen.getByRole("button", { name: /studio copy/i }));
    const bComposer = screen.getByRole("textbox", { name: /message/i }) as HTMLTextAreaElement;
    expect(bComposer.value).toBe("B draft");
    fireEvent.change(bComposer, { target: { value: "B typed" } });
    // Unrelated fleet snapshot refresh must not re-watch or bleed A into B.
    const watchCalls = (sessionB.watchTask as ReturnType<typeof vi.fn>).mock.calls.length;
    rerender(
      <NativeRemoteApp
        hostPublicId={HOST_ID}
        session={sessionA}
        pageHostPublicId={HOST_ID}
        hostSessions={sessions}
        fleetEntries={[
          fleetSnapshot(HOST_ID, "Page", sessionA.view(), true),
          {
            ...fleetSnapshot(HOST_B, "Studio", sessionB.view(), false),
            notice: "background update",
          },
        ]}
      />,
    );
    expect((screen.getByRole("textbox", { name: /message/i }) as HTMLTextAreaElement).value).toBe(
      "B typed",
    );
    expect((sessionB.watchTask as ReturnType<typeof vi.fn>).mock.calls.length).toBe(watchCalls);
    fireEvent.click(screen.getByRole("button", { name: /send/i }));
    expect(sessionB.sendText).toHaveBeenCalledWith(SAME_TASK, "B typed");
    expect(sessionA.sendText).not.toHaveBeenCalled();
  });

  it("initializes unavailable deep links without dialing the page host", () => {
    const session = makeSession();
    window.history.replaceState(
      null,
      "",
      `/tasks/not-a-uuid/${TASK_ID}`,
    );
    render(
      <NativeRemoteApp
        hostPublicId={HOST_ID}
        session={session}
        pageHostPublicId={HOST_ID}
        hostSessions={new Map([[HOST_ID, session]])}
        fleetEntries={[fleetSnapshot(HOST_ID, "Page", session.view(), true)]}
      />,
    );
    expect(screen.getByText(/host unavailable/i)).not.toBeNull();
    expect(session.watchTask).not.toHaveBeenCalled();
  });

  it("routes delayed mutate outcomes only to the original host after focus switches", async () => {
    let settleMutate: (value: { ok: true; commandId: string }) => void = () => {
      throw new Error("mutate was not started");
    };
    const mutateA = vi.fn(
      () =>
        new Promise<{ ok: true; commandId: string }>((resolve) => {
          settleMutate = resolve;
        }),
    );
    const sessionA = makeSession(makeView(), { mutateTask: mutateA });
    const sessionB = makeSession(
      makeView({
        hostPublicId: HOST_B,
        tasks: new Map([[SAME_TASK, task(SAME_TASK, "Studio copy", "open", 1)]]),
        conversations: new Map(),
      }),
    );
    render(
      <NativeRemoteApp
        hostPublicId={HOST_ID}
        session={sessionA}
        pageHostPublicId={HOST_ID}
        hostSessions={new Map([
          [HOST_ID, sessionA],
          [HOST_B, sessionB],
        ])}
        fleetEntries={[
          fleetSnapshot(HOST_ID, "Page", sessionA.view(), true),
          fleetSnapshot(HOST_B, "Studio", sessionB.view(), false),
        ]}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /investigate mobile sync/i }));
    fireEvent.click(screen.getByRole("button", { name: "Task actions" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Archive" }));
    expect(mutateA).toHaveBeenCalledWith(SAME_TASK, { kind: "begin_close" });
    fireEvent.click(screen.getByRole("button", { name: /back to tasks/i }));
    fireEvent.click(screen.getByRole("button", { name: /studio copy/i }));
    settleMutate({ ok: true, commandId: "cmd-a-archive" });
    await waitFor(() => expect(mutateA).toHaveBeenCalled());
    expect(screen.queryByText(/Archive started/i)).toBeNull();
    expect(sessionB.mutateTask).not.toHaveBeenCalled();
    expect(sessionA.sendText).not.toHaveBeenCalled();
  });
});
