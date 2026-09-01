import { describe, expect, it } from "vitest";
import {
  CAPABILITY_EVENT_REPLAY,
  CAPABILITY_PAGED_SNAPSHOTS,
  CAPABILITY_PROVIDER_INPUT,
  CAPABILITY_SEMANTIC_CONVERSATION,
  CAPABILITY_TASK_COCKPIT,
  MAX_RESUME_CURSOR_BASE64_CHARS,
  MAX_RESUME_CURSOR_BYTES,
  MAX_SEMANTIC_PAGE_ENCODED_BYTES,
  MAX_SEMANTIC_PAGE_FACTS,
  NATIVE_COMMAND_KIND,
  NATIVE_CONVERSATION_DIRTY_KIND,
  NATIVE_QUERY_KIND,
  assertCapabilities,
  buildCommandReceiptStatusQuery,
  buildContinueEventReplayQuery,
  buildBeginCloseTaskCommand,
  buildCreateTaskV2Command,
  buildDeleteTaskCommand,
  buildGlobalOpenEventReplayQuery,
  buildOpenConversationSubscriptionQuery,
  buildOpenEventReplayQuery,
  buildOpenTasksSnapshotPageQuery,
  buildProviderInputStateQuery,
  buildReleaseSnapshotQuery,
  buildRenameTaskCommand,
  buildReopenTaskCommand,
  buildResumeTasksSnapshotPageQuery,
  buildSettleTaskCommand,
  buildStartProviderSessionCommand,
  buildSubmitProviderInputSendNow,
  buildSubmitProviderTerminalKey,
  buildTaskCockpitConversationQuery,
  buildTaskCockpitConfigSnapshotQuery,
  buildTaskCockpitTerminalQuery,
  buildSubmitProviderAnswerQuestion,
  buildTaskSnapshotQuery,
  connectBinaryMarker,
  copyResumeCursorBytes,
  decodeCommandReceipt,
  decodeCommandReceiptStatusQueryResult,
  decodeConversationDirtyEnvelope,
  decodeEventReplayPageResult,
  decodeProviderInputState,
  decodeProviderInputStateQueryResult,
  decodeQueryReply,
  decodeSemanticJournalPage,
  decodeTaskCockpitTerminalResult,
  decodeTaskCockpitConfigSnapshotResult,
  decodeTaskSnapshotItem,
  decodeTaskSnapshotQueryResult,
  firstTurnIdFromCommandId,
  isMetadataTaskCommand,
  isTaskCreateV2Command,
  isProviderSendNowCommand,
  requiredCapabilitiesForCommand,
  requiredCapabilitiesForQuery,
  type ProviderInputFence,
} from "./nativeProtocol";

const HOST = "018f0000-0000-7000-8000-0000000000a1";
const CLIENT = "018f0000-0000-7000-8000-0000000000b2";
const REQUEST = "018f0000-0000-7000-8000-0000000000c3";
const TASK = "018f0000-0000-7000-8000-0000000000d4";
const AGENT = "018f0000-0000-7000-8000-0000000000e5";
const RESOURCE = "018f0000-0000-7000-8000-0000000000e6";
const COMMAND = "018f0000-0000-7000-8000-0000000000f6";
const FOREIGN_HOST = "018f0000-0000-7000-8000-00000000aaaa";
const FOREIGN_TASK = "018f0000-0000-7000-8000-00000000dead";
const QUESTION = "018f0000-0000-7000-8000-000000000201";
const APPROVAL = "018f0000-0000-7000-8000-000000000202";
const WAIT_CMD = "018f0000-0000-7000-8000-000000000203";
const EVENT = "018f0000-0000-7000-8000-000000000102";
const SNAPSHOT = "018f0000-0000-7000-8000-000000000101";
const CURRENT_TURN = "018f0000-0000-7000-8000-0000000000aa";

const authority = {
  hostPublicId: HOST,
  clientId: CLIENT,
  requestId: REQUEST,
};

describe("canonical startup terminal keys", () => {
  it("uses fixed control bytes and the existing first-turn fence", () => {
    const request = buildSubmitProviderTerminalKey({ authority, commandId: COMMAND,
      issuedAtMs: 10, fence: { ...openFence(), currentTurn: null }, key: "enter" });
    expect(request.payload).toMatchObject({ command: { submit_provider_input: {
      agent_session_id: AGENT, turn_id: COMMAND, question_id: null, approval_id: null,
      action: { terminal_input: { text: "\r" } },
    } } });
    expect(() => buildSubmitProviderTerminalKey({ authority, commandId: COMMAND,
      issuedAtMs: 10, fence: { ...openFence(), runtimeGeneration: 0 }, key: "enter" })).toThrow();
    expect(() => buildSubmitProviderTerminalKey({ authority, commandId: COMMAND,
      issuedAtMs: 10, fence: { ...openFence(), hostPublicId: FOREIGN_HOST }, key: "enter" })).toThrow();
  });
});

describe("canonical terminal projection", () => {
  const terminalReply = (overrides: Record<string, unknown> = {}) => decodeQueryReply({
    request_id: REQUEST, outcome: { ok: { task_cockpit: { terminal: {
      task_id: TASK, sequence: 5, title: "Codex", text_lines: ["Ready", "hello"], ...overrides,
    } } } },
  }, REQUEST);
  it("decodes bounded text only for the requested task", () => {
    expect(decodeTaskCockpitTerminalResult(terminalReply(), TASK)).toEqual({
      taskId: TASK, sequence: 5, title: "Codex", textLines: ["Ready", "hello"],
    });
    expect(() => decodeTaskCockpitTerminalResult(terminalReply(), FOREIGN_TASK)).toThrow(/mismatch/);
    expect(() => decodeTaskCockpitTerminalResult(terminalReply({ text_lines: [3] }), TASK)).toThrow();
    expect(() => decodeTaskCockpitTerminalResult(terminalReply({ text_lines: Array(4097).fill("") }), TASK)).toThrow();
    expect(() => decodeTaskCockpitTerminalResult(terminalReply({ text_lines: Array(5).fill("x".repeat(65536)) }), TASK)).toThrow();
    expect(() => decodeTaskCockpitTerminalResult(terminalReply({ sequence: -1 }), TASK)).toThrow();
  });
  it("surfaces host terminal availability states in plain language", () => {
    const unavailable = decodeQueryReply({
      request_id: REQUEST,
      outcome: { ok: { task_cockpit: { unavailable: {
        surface: "terminal", reason: "terminal_not_started",
      } } } },
    }, REQUEST);
    expect(() => decodeTaskCockpitTerminalResult(unavailable, TASK))
      .toThrow("No terminal has started for this task yet.");
  });
});

function thinTaskSnapshot() {
  return {
    task: {
      id: TASK,
      environment_id: "018f0000-0000-7000-8000-000000000301",
      title: "Example",
      description: null,
      project_id: "018f0000-0000-7000-8000-000000000302",
      workspace: { pathless: { workspace_id: "ws1" } },
      assignment: "unassigned",
      lifecycle: "open",
      action_epoch: 3,
      revision: 7,
      created_at_ms: 1_700_000_000_000,
    },
    connectivity: "connected",
    attention: "none",
    activity: "idle",
    review_readiness: "not_ready",
    primary_agent_id: AGENT,
  };
}

function providerInputState(overrides: Record<string, unknown> = {}) {
  return {
    task_id: TASK,
    task_revision: 7,
    action_epoch: 3,
    agent_session_id: AGENT,
    resource_id: RESOURCE,
    runtime_generation: 4,
    agent_lifecycle: "open",
    provider_kind: "codex",
    provider_session_id: null,
    current_turn: null,
    open_question: null,
    open_approval: null,
    pending_wait_command_ids: [],
    ...overrides,
  };
}

function openFence(
  overrides: Partial<ProviderInputFence> = {},
): ProviderInputFence {
  return {
    hostPublicId: HOST,
    clientId: CLIENT,
    taskId: TASK,
    taskRevision: 7,
    actionEpoch: 3,
    agentSessionId: AGENT,
    runtimeGeneration: 4,
    agentLifecycle: "open",
    providerKind: "codex",
    providerSessionId: null,
    currentTurn: null,
    openQuestion: null,
    openApproval: null,
    pendingWaitCommandIds: [],
    ...overrides,
  };
}

describe("nativeProtocol query builders", () => {
  it("builds TaskSnapshot / cockpit / replay / snapshot page on Query kind 5", () => {
    expect(NATIVE_QUERY_KIND).toBe(5);
    const snapshot = buildTaskSnapshotQuery({ ...authority, taskId: TASK });
    expect(snapshot.payloadKind).toBe(5);
    expect(snapshot.privacyClass).toBe("local_only");
    expect(snapshot.payload).toEqual({
      request_id: REQUEST,
      client_id: CLIENT,
      task_id: TASK,
      query: { task_snapshot: {} },
    });

    expect(
      buildTaskCockpitConversationQuery({
        ...authority,
        taskId: TASK,
        afterSequence: 12,
      }).payload,
    ).toMatchObject({
      query: { task_cockpit: { conversation: { after_sequence: 12 } } },
    });
    expect(
      buildTaskCockpitTerminalQuery({ ...authority, taskId: TASK }).payload,
    ).toMatchObject({ query: { task_cockpit: "terminal" } });
    expect(buildTaskCockpitConfigSnapshotQuery(authority).payload).toEqual({
      request_id: REQUEST,
      client_id: CLIENT,
      task_id: null,
      query: { task_cockpit: "config_snapshot" },
    });
    expect(
      buildProviderInputStateQuery({ ...authority, taskId: TASK }).payload,
    ).toMatchObject({ query: { task_cockpit: "provider_input_state" } });
    expect(
      buildOpenEventReplayQuery({
        ...authority,
        taskId: TASK,
        afterSequence: 0,
      }).payload,
    ).toMatchObject({
      query: { open_event_replay: { after_sequence: 0 } },
    });
    expect(buildOpenTasksSnapshotPageQuery(authority).payload).toEqual({
      request_id: REQUEST,
      client_id: CLIENT,
      task_id: null,
      query: {
        snapshot_page: {
          section: "tasks",
          snapshot_id: null,
          resume_cursor: null,
        },
      },
    });
  });

  it("builds exact receipt status query and decodes found, missing, and conflict", () => {
    const commandPayload = {
      command_id: COMMAND,
      client_id: CLIENT,
      task_id: TASK,
      issued_at_ms: 1_700_000_000_100,
      expected_task_revision: 7,
      command: {
        submit_provider_input: {
          agent_session_id: AGENT,
          runtime_generation: 4,
          turn_id: COMMAND,
          action_epoch: 3,
          question_id: null,
          approval_id: null,
          action: { send_now: { text: "ship it", wait: false } },
        },
      },
    };
    const request = buildCommandReceiptStatusQuery({
      ...authority,
      taskId: TASK,
      commandPayload,
    });
    expect(request.payloadKind).toBe(NATIVE_QUERY_KIND);
    expect(request.payload).toEqual({
      request_id: REQUEST,
      client_id: CLIENT,
      task_id: TASK,
      query: { command_receipt_status: { command: commandPayload } },
    });
    expect(
      requiredCapabilitiesForQuery({
        command_receipt_status: { command: commandPayload },
      }),
    ).toBe(0n);

    const found = decodeCommandReceiptStatusQueryResult(
      decodeQueryReply(
        {
          request_id: REQUEST,
          outcome: {
            ok: {
              command_receipt_status: {
                receipt: {
                  accepted: {
                    command_id: COMMAND,
                    operation_id: "018f0000-0000-7000-8000-000000000401",
                    task_revision: 8,
                    event_ids: [EVENT],
                  },
                },
              },
            },
          },
        },
        REQUEST,
      ),
      COMMAND,
    );
    expect(found).toMatchObject({ kind: "accepted", commandId: COMMAND });

    const missing = decodeCommandReceiptStatusQueryResult(
      decodeQueryReply(
        {
          request_id: REQUEST,
          outcome: { ok: { command_receipt_status: { receipt: null } } },
        },
        REQUEST,
      ),
      COMMAND,
    );
    expect(missing).toBeNull();

    const conflictReply = decodeQueryReply(
      { request_id: REQUEST, outcome: { err: "conflict" } },
      REQUEST,
    );
    expect(conflictReply.outcome).toEqual({
      kind: "err",
      error: { code: "conflict" },
    });
    expect(() =>
      decodeCommandReceiptStatusQueryResult(conflictReply, COMMAND),
    ).toThrow(/conflict/);

    expect(() =>
      buildCommandReceiptStatusQuery({
        ...authority,
        taskId: TASK,
        commandPayload: { ...commandPayload, client_id: FOREIGN_HOST },
      }),
    ).toThrow(/clientId mismatch/);
  });

  it("requires exact capability subsets and rejects unknown query actions", () => {
    expect(requiredCapabilitiesForQuery({ task_snapshot: {} })).toBe(0n);
    expect(
      requiredCapabilitiesForQuery({
        task_cockpit: { conversation: { after_sequence: 0 } },
      }),
    ).toBe(CAPABILITY_TASK_COCKPIT | CAPABILITY_SEMANTIC_CONVERSATION);
    expect(
      requiredCapabilitiesForQuery({ task_cockpit: "provider_input_state" }),
    ).toBe(CAPABILITY_TASK_COCKPIT | CAPABILITY_PROVIDER_INPUT);
    expect(
      requiredCapabilitiesForQuery({ open_event_replay: { after_sequence: 0 } }),
    ).toBe(CAPABILITY_EVENT_REPLAY);
    expect(
      requiredCapabilitiesForQuery({
        snapshot_page: { section: "tasks", snapshot_id: null, resume_cursor: null },
      }),
    ).toBe(CAPABILITY_PAGED_SNAPSHOTS);
    expect(() => requiredCapabilitiesForQuery({ mystery: {} })).toThrow(
      /unknown query/,
    );
    expect(() =>
      requiredCapabilitiesForQuery({ task_cockpit: { mystery: {} } }),
    ).toThrow(/unknown task_cockpit/);

    assertCapabilities(0, 0n);
    assertCapabilities(
      Number(CAPABILITY_TASK_COCKPIT | CAPABILITY_PROVIDER_INPUT),
      CAPABILITY_PROVIDER_INPUT,
    );
    expect(() =>
      assertCapabilities(Number(CAPABILITY_PAGED_SNAPSHOTS), CAPABILITY_EVENT_REPLAY),
    ).toThrow(/unsupported capability/);
    expect(() => assertCapabilities("nope", 0n)).toThrow(/invalid capability/);
  });

  it("encodes resume_cursor as $connectBinary marker and copies caller bytes", () => {
    const bytes = Uint8Array.from([1, 2, 3, 4]);
    const request = buildResumeTasksSnapshotPageQuery({
      ...authority,
      snapshotId: SNAPSHOT,
      resumeCursor: bytes,
    });
    const query = (request.payload as { query: { snapshot_page: { resume_cursor: unknown } } })
      .query.snapshot_page.resume_cursor;
    expect(query).toEqual(connectBinaryMarker(bytes));
    expect(Array.from(copyResumeCursorBytes(query as { $connectBinary: string }))).toEqual([
      1, 2, 3, 4,
    ]);
    expect(Array.from(copyResumeCursorBytes(bytes))).toEqual([1, 2, 3, 4]);
    expect(Array.from(copyResumeCursorBytes([1, 2, 3, 4]))).toEqual([1, 2, 3, 4]);
    expect(() => copyResumeCursorBytes([1, 256])).toThrow(/cursor byte/);
    expect(() =>
      copyResumeCursorBytes({
        $connectBinary: "A".repeat(MAX_RESUME_CURSOR_BASE64_CHARS + 4),
      }),
    ).toThrow(/base64 length/);
    expect(() =>
      copyResumeCursorBytes({ $connectBinary: "abc" }),
    ).toThrow(/resume cursor marker/);
    expect(() =>
      buildResumeTasksSnapshotPageQuery({
        ...authority,
        snapshotId: SNAPSHOT,
        resumeCursor: new Uint8Array(MAX_RESUME_CURSOR_BYTES + 1),
      }),
    ).toThrow(/resume cursor/);
  });
});

describe("deferred browser task creation", () => {
  it("builds one host-resolved main-workspace task only on first send", () => {
    const environment = "018f0000-0000-7000-8000-000000000401";
    const project = "018f0000-0000-7000-8000-000000000402";
    const request = buildCreateTaskV2Command({
      authority,
      commandId: COMMAND,
      taskId: TASK,
      environmentId: environment,
      projectId: project,
      provider: "claude",
      title: "New Claude task",
      issuedAtMs: 10,
    });
    expect(request.payload).toMatchObject({
      task_id: null,
      expected_task_revision: null,
      command: { create_task_v2: {
        id: TASK,
        environment_id: environment,
        project_id: project,
        workspace: { choice: "main", path: null, branch: null, external_confirmed: false },
        primary_provider: "claude",
        defer_primary_provider_start: false,
      } },
    });
    expect(isTaskCreateV2Command(request.payload)).toBe(true);
    expect(requiredCapabilitiesForCommand((request.payload as { command: unknown }).command)).toBe(0n);
    expect(() => buildCreateTaskV2Command({
      authority,
      commandId: COMMAND,
      taskId: TASK,
      environmentId: environment,
      projectId: project,
      provider: "claude_code" as never,
      title: "Invalid provider",
      issuedAtMs: 10,
    })).toThrow(/provider rejected/);
  });
});

describe("provider launch option authority", () => {
  const base = {
    authority,
    commandId: COMMAND,
    taskId: TASK,
    agentSessionId: AGENT,
    resourceId: RESOURCE,
    expectedTaskRevision: 7,
    actionEpoch: 3,
    issuedAtMs: 10,
  };

  it("encodes provider-owned values while rejecting cross-provider models and efforts", () => {
    expect(buildStartProviderSessionCommand({
      ...base,
      provider: "codex",
      launchOptions: { model: "codex_terra", reasoningEffort: "low", access: "full_access" },
    }).payload).toMatchObject({ command: { start_provider_session: {
      provider_kind: "codex",
      launch_options: {
        model: "codex_terra",
        reasoning_effort: "low",
        access: "full_access",
      },
    } } });
    expect(() => buildStartProviderSessionCommand({
      ...base,
      provider: "codex",
      launchOptions: { model: "claude_opus", reasoningEffort: "low", access: "full_access" },
    })).toThrow(/model rejected/);
    expect(() => buildStartProviderSessionCommand({
      ...base,
      provider: "claude",
      launchOptions: { model: "claude_sonnet", reasoningEffort: "ultra", access: "full_access" },
    })).toThrow(/reasoning effort rejected/);

    const persisted = buildStartProviderSessionCommand({
      ...base,
      provider: "codex",
      launchOptions: { model: "codex_terra", reasoningEffort: "low", access: "full_access" },
    });
    expect(() => buildCommandReceiptStatusQuery({
      ...authority,
      taskId: TASK,
      commandPayload: persisted.payload,
    })).not.toThrow();
  });
});

describe("native config snapshot", () => {
  it("decodes only bounded redacted project and provider metadata", () => {
    const reply = decodeQueryReply({ request_id: REQUEST, outcome: { ok: {
      task_cockpit: { config: {
        revision: 9,
        projects: [{ config_id: "devmanager", label: "DevManager", root_configured: true,
          workspace_id: TASK, folders: [{ config_id: "api", label: "API", server_count: 2 }] }],
        servers: [], ssh_connections: [],
        providers: [{ provider: "codex", command_configured: true },
          { provider: "claude", command_configured: false }],
      } },
    } } }, REQUEST);
    expect(decodeTaskCockpitConfigSnapshotResult(reply)).toEqual({
      revision: 9,
      projects: [{ configId: "devmanager", label: "DevManager", rootConfigured: true,
        workspaceId: TASK, folders: [{ configId: "api", label: "API", serverCount: 2 }] }],
      providers: [{ provider: "codex", commandConfigured: true },
        { provider: "claude", commandConfigured: false }],
    });
  });
});

describe("thin TaskSnapshot has no send authority", () => {
  it("decodes thin list metadata and rejects agents/provider_sessions", () => {
    const item = decodeTaskSnapshotItem({ snapshot: thinTaskSnapshot() }, {
      expectedTaskId: TASK,
    });
    expect(item).toMatchObject({
      taskId: TASK,
      revision: 7,
      actionEpoch: 3,
      primaryAgentId: AGENT,
    });
    expect(() =>
      decodeTaskSnapshotItem({
        snapshot: { ...thinTaskSnapshot(), agents: {}, provider_sessions: {} },
      }),
    ).toThrow(/thin list metadata/);

    const reply = decodeQueryReply(
      {
        request_id: REQUEST,
        outcome: { ok: { task_snapshot: { snapshot: thinTaskSnapshot() } } },
      },
      REQUEST,
    );
    expect(decodeTaskSnapshotQueryResult(reply, TASK).taskId).toBe(TASK);
  });
});

describe("ProviderInputState fence and SendNow", () => {
  it("decodes exact flat projection and builds fence only when agent is bound", () => {
    const withAgent = decodeProviderInputState(
      providerInputState(),
      authority,
      TASK,
    );
    expect(withAgent.fence).toEqual(openFence());
    expect(withAgent.providerSessionId).toBeNull();

    const noAgent = decodeProviderInputState(
      providerInputState({
        agent_session_id: null,
        resource_id: null,
        runtime_generation: null,
        agent_lifecycle: null,
        provider_kind: null,
        provider_session_id: null,
        current_turn: null,
        open_question: null,
        open_approval: null,
        pending_wait_command_ids: [],
      }),
      authority,
      TASK,
    );
    expect(noAgent.fence).toBeNull();

    expect(() =>
      decodeProviderInputState(
        providerInputState({
          agent_session_id: null,
          resource_id: null,
          runtime_generation: null,
          agent_lifecycle: null,
          provider_kind: null,
          current_turn: CURRENT_TURN,
        }),
        authority,
        TASK,
      ),
    ).toThrow(/no-agent/);
    expect(() =>
      decodeProviderInputState(
        providerInputState({ provider_session_id: "  spaced  " }),
        authority,
        TASK,
      ),
    ).toThrow(/provider_session_id/);
    expect(() =>
      decodeProviderInputState(
        providerInputState({
          pending_wait_command_ids: [WAIT_CMD, WAIT_CMD],
        }),
        authority,
        TASK,
      ),
    ).toThrow(/duplicate pending wait/);
  });

  it("builds an exact correlated question answer without weakening SendNow blockers", () => {
    const request = buildSubmitProviderAnswerQuestion({
      authority,
      commandId: COMMAND,
      issuedAtMs: 10,
      text: "Use the first option",
      fence: openFence({ currentTurn: CURRENT_TURN, openQuestion: QUESTION }),
    });
    expect(request.payload).toMatchObject({ task_id: TASK, command: {
      submit_provider_input: {
        turn_id: CURRENT_TURN,
        question_id: QUESTION,
        approval_id: null,
        action: { answer_question: {
          question_id: QUESTION,
          answer: "Use the first option",
        } },
      },
    } });
    expect(() => buildSubmitProviderAnswerQuestion({
      authority,
      commandId: COMMAND,
      issuedAtMs: 10,
      text: "answer",
      fence: openFence({ currentTurn: CURRENT_TURN }),
    })).toThrow(/open question/);
  });

  it("rejects foreign task, missing fields, and missing blockers array", () => {
    expect(() =>
      decodeProviderInputState(providerInputState(), authority, FOREIGN_TASK),
    ).toThrow(/foreign task/);
    expect(() =>
      decodeProviderInputState(
        { task_id: TASK, task_revision: 7 },
        authority,
        TASK,
      ),
    ).toThrow(/missing or unexpected/);
    const missingWaits = { ...providerInputState() };
    delete (missingWaits as { pending_wait_command_ids?: unknown })
      .pending_wait_command_ids;
    expect(() =>
      decodeProviderInputState(missingWaits, authority, TASK),
    ).toThrow(/missing or unexpected/);
  });

  it("SendNow uses fence + authority match, deterministic first turn, stable retry", () => {
    expect(NATIVE_COMMAND_KIND).toBe(6);
    expect(firstTurnIdFromCommandId(COMMAND)).toBe(COMMAND);
    const fence = openFence();
    const request = buildSubmitProviderInputSendNow({
      authority,
      commandId: COMMAND,
      text: "ship it",
      issuedAtMs: 1_700_000_000_100,
      fence,
    });
    expect(request.payloadKind).toBe(6);
    expect(request.payload).toEqual({
      command_id: COMMAND,
      client_id: CLIENT,
      task_id: TASK,
      issued_at_ms: 1_700_000_000_100,
      expected_task_revision: 7,
      command: {
        submit_provider_input: {
          agent_session_id: AGENT,
          runtime_generation: 4,
          turn_id: COMMAND,
          action_epoch: 3,
          question_id: null,
          approval_id: null,
          action: { send_now: { text: "ship it", wait: false } },
        },
      },
    });
    expect(
      requiredCapabilitiesForCommand({ submit_provider_input: {} }),
    ).toBe(CAPABILITY_PROVIDER_INPUT);

    const retry = buildSubmitProviderInputSendNow({
      authority,
      commandId: COMMAND,
      text: "ship it",
      issuedAtMs: 1_700_000_000_101,
      fence,
    });
    expect((retry.payload as { command_id: string }).command_id).toBe(COMMAND);

    const subsequent = buildSubmitProviderInputSendNow({
      authority,
      commandId: COMMAND,
      text: "again",
      issuedAtMs: 1,
      fence: openFence({ currentTurn: CURRENT_TURN }),
    });
    expect(
      (
        subsequent.payload as {
          command: { submit_provider_input: { turn_id: string } };
        }
      ).command.submit_provider_input.turn_id,
    ).toBe(CURRENT_TURN);
  });

  it("rejects foreign host/client, blockers, and non-open agent", () => {
    expect(() =>
      buildSubmitProviderInputSendNow({
        authority,
        commandId: COMMAND,
        text: "x",
        issuedAtMs: 1,
        fence: openFence({ hostPublicId: FOREIGN_HOST }),
      }),
    ).toThrow(/hostPublicId mismatch/);
    expect(() =>
      buildSubmitProviderInputSendNow({
        authority,
        commandId: COMMAND,
        text: "x",
        issuedAtMs: 1,
        fence: openFence({ openQuestion: QUESTION }),
      }),
    ).toThrow(/question blocker/);
    expect(() =>
      buildSubmitProviderInputSendNow({
        authority,
        commandId: COMMAND,
        text: "x",
        issuedAtMs: 1,
        fence: openFence({ openApproval: APPROVAL }),
      }),
    ).toThrow(/approval blocker/);
    expect(() =>
      buildSubmitProviderInputSendNow({
        authority,
        commandId: COMMAND,
        text: "x",
        issuedAtMs: 1,
        fence: openFence({ pendingWaitCommandIds: [WAIT_CMD] }),
      }),
    ).toThrow(/wait blocker/);
    expect(() =>
      buildSubmitProviderInputSendNow({
        authority,
        commandId: COMMAND,
        text: "x",
        issuedAtMs: 1,
        fence: openFence({ agentLifecycle: "closing" }),
      }),
    ).toThrow(/non-open agent/);
  });

  it("decodes ProviderInputState from correlated QueryReply", () => {
    const reply = decodeQueryReply(
      {
        request_id: REQUEST,
        outcome: {
          ok: {
            task_cockpit: {
              provider_input_state: providerInputState({
                pending_wait_command_ids: [WAIT_CMD],
              }),
            },
          },
        },
      },
      REQUEST,
    );
    const view = decodeProviderInputStateQueryResult(reply, authority, TASK);
    expect(view.pendingWaitCommandIds).toEqual([WAIT_CMD]);
    expect(view.fence?.pendingWaitCommandIds).toEqual([WAIT_CMD]);
  });
});

describe("SemanticJournalPage bounds and order", () => {
  function fact(sequence: number, idSuffix: string, text: string) {
    return {
      id: `018f0000-0000-7000-8000-${idSuffix.padStart(12, "0")}`,
      sequence,
      provider: "codex",
      schema_version: 1,
      kind: "user_message",
      visibility: "conversation",
      privacy_class: "local_only",
      redacted: false,
      payload: { kind: "user_message", text },
    };
  }

  function page(overrides: Record<string, unknown> = {}) {
    return {
      after_sequence: 0,
      through_sequence: 2,
      high_water: 9,
      oldest_sequence: 1,
      cursor_rolled_over: false,
      encoded_bytes: 128,
      next_sequence: 2,
      facts: [
        {
          id: EVENT,
          sequence: 1,
          occurred_at_ms: 10,
          provider: "codex",
          schema_version: 1,
          kind: "user_message",
          visibility: "conversation",
          privacy_class: "local_only",
          redacted: false,
          payload: { kind: "user_message", text: "hello" },
        },
        {
          id: "018f0000-0000-7000-8000-000000000103",
          sequence: 2,
          provider: "codex",
          schema_version: 1,
          kind: "assistant_text",
          visibility: "conversation",
          privacy_class: "local_only",
          redacted: false,
          payload: { kind: "assistant_text", text: "world" },
        },
      ],
      ...overrides,
    };
  }

  it("accepts null final page, intermediate page, empty final at high, reset after 0", () => {
    const intermediate = decodeSemanticJournalPage(page());
    expect(intermediate.nextSequence).toBe(2);
    expect(intermediate.throughSequence).toBe(2);

    const finalPage = decodeSemanticJournalPage(
      page({
        after_sequence: 2,
        through_sequence: 9,
        high_water: 9,
        next_sequence: null,
        facts: [
          {
            id: "018f0000-0000-7000-8000-000000000104",
            sequence: 5,
            provider: "codex",
            schema_version: 1,
            kind: "assistant_text",
            visibility: "conversation",
            privacy_class: "local_only",
            redacted: false,
            payload: { kind: "assistant_text", text: "tail" },
          },
        ],
      }),
    );
    expect(finalPage.nextSequence).toBeNull();
    expect(finalPage.throughSequence).toBe(9);

    const emptyFinal = decodeSemanticJournalPage(
      page({
        after_sequence: 9,
        through_sequence: 9,
        high_water: 9,
        next_sequence: null,
        facts: [],
      }),
    );
    expect(emptyFinal.facts).toEqual([]);
    expect(emptyFinal.nextSequence).toBeNull();

    const reset = decodeSemanticJournalPage(
      page({
        after_sequence: 0,
        through_sequence: 0,
        high_water: 0,
        oldest_sequence: 0,
        cursor_rolled_over: true,
        next_sequence: null,
        facts: [],
      }),
    );
    expect(reset.afterSequence).toBe(0);
    expect(reset.cursorRolledOver).toBe(true);

    const fullIntermediate = decodeSemanticJournalPage(
      page({
        after_sequence: 0,
        through_sequence: MAX_SEMANTIC_PAGE_FACTS,
        high_water: MAX_SEMANTIC_PAGE_FACTS + 10,
        oldest_sequence: 1,
        next_sequence: MAX_SEMANTIC_PAGE_FACTS,
        facts: Array.from({ length: MAX_SEMANTIC_PAGE_FACTS }, (_, index) =>
          fact(index + 1, (0x2000 + index).toString(16), "x"),
        ),
      }),
    );
    expect(fullIntermediate.facts).toHaveLength(128);
    expect(fullIntermediate.nextSequence).toBe(128);
  });

  it("accepts bounded long tool-result summaries emitted by the native host", () => {
    const result = decodeSemanticJournalPage(page({
      through_sequence: 1,
      next_sequence: 1,
      facts: [{
        ...fact(1, "0104", "ignored"),
        kind: "tool_result",
        payload: { kind: "tool_result", call_id: "tool-1", status: "x".repeat(4096) },
      }],
    }));
    expect(result.facts[0]?.payload).toMatchObject({
      kind: "tool_result",
      call_id: "tool-1",
      status: "x".repeat(4096),
    });
  });

  it("rejects order/oversize/duplicate/window violations", () => {
    expect(() =>
      decodeSemanticJournalPage(
        page({ after_sequence: 5, through_sequence: 2, next_sequence: 2 }),
      ),
    ).toThrow(/sequence window ordering/);
    expect(() =>
      decodeSemanticJournalPage(page({ next_sequence: 3 })),
    ).toThrow(/next_sequence must equal/);
    expect(() =>
      decodeSemanticJournalPage(
        page({
          next_sequence: null,
          through_sequence: 2,
          high_water: 9,
        }),
      ),
    ).toThrow(/final page through_sequence/);
    expect(() =>
      decodeSemanticJournalPage(
        page({
          facts: [
            fact(2, "000000000201", "a"),
            fact(1, "000000000202", "b"),
          ],
        }),
      ),
    ).toThrow(/strictly ascending/);
    expect(() =>
      decodeSemanticJournalPage(
        page({ encoded_bytes: MAX_SEMANTIC_PAGE_ENCODED_BYTES + 1 }),
      ),
    ).toThrow(/encoded_bytes exceed/);
    expect(() =>
      decodeSemanticJournalPage(
        page({
          facts: Array.from({ length: MAX_SEMANTIC_PAGE_FACTS + 1 }, (_, index) =>
            fact(index + 1, (0x1100 + index).toString(16), "x"),
          ),
          through_sequence: MAX_SEMANTIC_PAGE_FACTS + 1,
          next_sequence: MAX_SEMANTIC_PAGE_FACTS + 1,
          high_water: MAX_SEMANTIC_PAGE_FACTS + 1,
        }),
      ),
    ).toThrow(/facts exceed/);
    expect(() =>
      decodeSemanticJournalPage(
        page({
          oldest_sequence: 20,
          high_water: 9,
          through_sequence: 2,
          next_sequence: 2,
        }),
      ),
    ).toThrow(/oldest_sequence above/);
  });

  it("builds release/continue/global replay and conversation subscription queries", () => {
    expect(
      buildReleaseSnapshotQuery({
        ...authority,
        snapshotId: SNAPSHOT,
      }).payload,
    ).toMatchObject({
      query: { release_snapshot: { snapshot_id: SNAPSHOT } },
    });
    expect(
      buildGlobalOpenEventReplayQuery({
        ...authority,
        afterSequence: 0,
      }).payload,
    ).toMatchObject({
      query: { open_event_replay: { after_sequence: 0 } },
    });
    expect(
      buildContinueEventReplayQuery({
        ...authority,
        subscriptionId: "018f0000-0000-7000-8000-000000000203",
        resumeCursor: new Uint8Array([1, 2, 3]),
      }).payload,
    ).toMatchObject({
      query: {
        continue_event_replay: {
          subscription_id: "018f0000-0000-7000-8000-000000000203",
        },
      },
    });
    expect(
      buildOpenConversationSubscriptionQuery({
        ...authority,
        taskId: TASK,
        afterSequence: 4,
      }).payload,
    ).toMatchObject({
      query: {
        task_cockpit: {
          open_conversation_subscription: { after_sequence: 4 },
        },
      },
    });
    expect(
      decodeCommandReceipt({
        accepted: {
          command_id: COMMAND,
          operation_id: "018f0000-0000-7000-8000-000000000401",
          task_revision: null,
          event_ids: [],
        },
      }).kind,
    ).toBe("accepted");
    expect(
      decodeConversationDirtyEnvelope({
        payloadKind: NATIVE_CONVERSATION_DIRTY_KIND,
        payload: {
          required_capabilities: Number(
            CAPABILITY_TASK_COCKPIT | CAPABILITY_SEMANTIC_CONVERSATION,
          ),
          message: {
            conversation_dirty: {
              subscription_id: "018f0000-0000-7000-8000-000000000202",
              task_id: TASK,
              high_water: 12,
            },
          },
        },
        requestId: null,
        operationId: null,
        privacyClass: "local_only",
      }),
    ).toMatchObject({ taskId: TASK, highWater: 12 });

    expect(() =>
      decodeConversationDirtyEnvelope({
        payloadKind: NATIVE_CONVERSATION_DIRTY_KIND,
        payload: {
          required_capabilities: Number(
            CAPABILITY_TASK_COCKPIT | CAPABILITY_SEMANTIC_CONVERSATION,
          ),
          message: {
            conversation_dirty: {
              subscription_id: "018f0000-0000-7000-8000-000000000202",
              task_id: TASK,
              high_water: 0,
            },
          },
        },
        requestId: null,
        operationId: null,
        privacyClass: "local_only",
      }),
    ).toThrow(/high_water/);

    expect(() =>
      decodeConversationDirtyEnvelope({
        payloadKind: NATIVE_CONVERSATION_DIRTY_KIND,
        payload: {
          required_capabilities: Number(CAPABILITY_SEMANTIC_CONVERSATION),
          message: {
            conversation_dirty: {
              subscription_id: "018f0000-0000-7000-8000-000000000202",
              task_id: TASK,
              high_water: 12,
            },
          },
        },
        requestId: null,
        operationId: null,
        privacyClass: "local_only",
      }),
    ).toThrow(/capability/);
  });
});

describe("EventReplayPage wire bounds and order", () => {
  const subscriptionId = "018f0000-0000-7000-8000-000000000203";

  function replayEvent(sequence: number, taskId: string | null = TASK) {
    return {
      id: `018f0000-0000-7000-8000-${(0x300 + sequence).toString(16).padStart(12, "0")}`,
      task_id: taskId,
      sequence,
      task_revision: 1,
      occurred_at_ms: 1_700_000_000_000,
      payload: { event_type: "task.reopened", payload: {} },
    };
  }

  function replayReply(page: Record<string, unknown>) {
    return decodeQueryReply(
      {
        request_id: REQUEST,
        outcome: {
          ok: {
            event_replay_page: { subscription_id: subscriptionId, page },
          },
        },
      },
      REQUEST,
    );
  }

  it("decodes a bounded ordered replay page without inventing task ids", () => {
    const decoded = decodeEventReplayPageResult(
      replayReply({
        after_sequence: 7,
        through_sequence: 10,
        events: [replayEvent(8), replayEvent(10, null)],
        next_cursor: new Uint8Array([1]),
      }),
    );
    expect(decoded.afterSequence).toBe(7);
    expect(decoded.throughSequence).toBe(10);
    expect(decoded.lastSequence).toBe(10);
    expect(decoded.affectedTaskIds).toEqual([TASK]);
  });

  it("rejects malformed, unordered, and unbounded replay events", () => {
    expect(() =>
      decodeEventReplayPageResult(
        replayReply({
          after_sequence: 7,
          through_sequence: 10,
          events: [{ task_id: TASK, sequence: 8 }],
          next_cursor: null,
        }),
      ),
    ).toThrow(/event replay event/);
    expect(() =>
      decodeEventReplayPageResult(
        replayReply({
          after_sequence: 7,
          through_sequence: 10,
          events: [replayEvent(9), replayEvent(8)],
          next_cursor: null,
        }),
      ),
    ).toThrow(/strictly ascending/);
    expect(() =>
      decodeEventReplayPageResult(
        replayReply({
          after_sequence: 7,
          through_sequence: 10,
          events: [],
          next_cursor: new Uint8Array([1]),
        }),
      ),
    ).toThrow(/nonempty forward progress/);
  });
});

describe("task lifecycle command wire shapes", () => {
  it("encodes Rust unit variants as strings and rename as an object", () => {
    const base = {
      authority,
      commandId: COMMAND,
      taskId: TASK,
      issuedAtMs: 1_700_000_000_100,
      expectedTaskRevision: 7,
    };
    expect(buildSettleTaskCommand(base).payload).toEqual({
      command_id: COMMAND,
      client_id: CLIENT,
      task_id: TASK,
      issued_at_ms: 1_700_000_000_100,
      expected_task_revision: 7,
      command: "settle_task",
    });
    expect(buildReopenTaskCommand(base).payload).toMatchObject({
      command: "reopen_task",
    });
    expect(buildBeginCloseTaskCommand(base).payload).toMatchObject({
      command: "begin_close_task",
    });
    expect(buildDeleteTaskCommand(base).payload).toMatchObject({
      command: "delete_task",
    });
    expect(
      buildRenameTaskCommand({ ...base, title: "Renamed" }).payload,
    ).toEqual({
      command_id: COMMAND,
      client_id: CLIENT,
      task_id: TASK,
      issued_at_ms: 1_700_000_000_100,
      expected_task_revision: 7,
      command: { rename_task: { title: "Renamed" } },
    });
  });

  it("intersects capabilities for unit/object variants and rejects unit objects", () => {
    expect(requiredCapabilitiesForCommand("settle_task")).toBe(0n);
    expect(requiredCapabilitiesForCommand("reopen_task")).toBe(0n);
    expect(requiredCapabilitiesForCommand("begin_close_task")).toBe(0n);
    expect(requiredCapabilitiesForCommand("delete_task")).toBe(0n);
    expect(requiredCapabilitiesForCommand({ rename_task: { title: "x" } })).toBe(
      0n,
    );
    expect(
      requiredCapabilitiesForCommand({ submit_provider_input: {} }),
    ).toBe(CAPABILITY_PROVIDER_INPUT);
    expect(() => requiredCapabilitiesForCommand({ settle_task: null })).toThrow(
      /unknown command/,
    );
    expect(() => requiredCapabilitiesForCommand("create_task")).toThrow(
      /unknown command/,
    );
  });

  it("builds receipt status queries for unit-string command envelopes", () => {
    const commandPayload = {
      command_id: COMMAND,
      client_id: CLIENT,
      task_id: TASK,
      issued_at_ms: 1,
      expected_task_revision: 4,
      command: "settle_task",
    };
    expect(
      buildCommandReceiptStatusQuery({
        ...authority,
        taskId: TASK,
        commandPayload,
      }).payload,
    ).toEqual({
      request_id: REQUEST,
      client_id: CLIENT,
      task_id: TASK,
      query: { command_receipt_status: { command: commandPayload } },
    });
    expect(() =>
      buildCommandReceiptStatusQuery({
        ...authority,
        taskId: TASK,
        commandPayload: { ...commandPayload, command: { settle_task: {} } },
      }),
    ).toThrow(/unit command variant must be a string/);
  });

  it("classifies send-now vs metadata for draft-clear eligibility", () => {
    expect(
      isProviderSendNowCommand({
        command: {
          submit_provider_input: {
            action: { send_now: { text: "hi", wait: false } },
          },
        },
      }),
    ).toBe(true);
    expect(
      isMetadataTaskCommand({ command: "settle_task" }),
    ).toBe(true);
    expect(
      isMetadataTaskCommand({ command: { rename_task: { title: "A" } } }),
    ).toBe(true);
    expect(
      isProviderSendNowCommand({ command: "settle_task" }),
    ).toBe(false);
  });
});
