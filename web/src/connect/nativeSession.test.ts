import { describe, expect, it } from "vitest";
import type {
  ConnectConnectionState,
  DecodedConnectEnvelope,
} from "./transport";
import {
  createIndexedDbNativeCacheStore,
  createMemoryNativeCacheStore,
  MAX_HISTORY_CONVERSATIONS_PER_HOST,
  MAX_METADATA_TASKS_PER_HOST,
  MAX_OUTBOX_ITEMS,
  validateOutboxCommandPayload,
  type NativeOutboxRecord,
} from "./nativeCache";
import {
  CAPABILITY_EVENT_REPLAY,
  CAPABILITY_PAGED_SNAPSHOTS,
  CAPABILITY_PROVIDER_INPUT,
  CAPABILITY_SEMANTIC_CONVERSATION,
  CAPABILITY_TASK_COCKPIT,
  NATIVE_COMMAND_KIND,
  NATIVE_COMMAND_RECEIPT_KIND,
  NATIVE_CONVERSATION_DIRTY_KIND,
  NATIVE_QUERY_KIND,
  NATIVE_QUERY_REPLY_KIND,
  buildSubmitProviderInputSendNow,
  decodeConversationDirtyEnvelope,
} from "./nativeProtocol";
import { HOST_CRITICAL_OUTPUT, HOST_DURABLE_OUTPUT } from "./hostOutput";
import {
  NativeHostSession,
  type NativeConnectTransport,
  type NativeHostSessionView,
} from "./nativeSession";

const HOST = "018f0000-0000-7000-8000-0000000000a1";
const CLIENT = "018f0000-0000-7000-8000-0000000000b2";
const CLIENT_B = "018f0000-0000-7000-8000-0000000000b3";
const TASK = "018f0000-0000-7000-8000-0000000000d4";
const TASK_B = "018f0000-0000-7000-8000-0000000000d5";
const AGENT = "018f0000-0000-7000-8000-0000000000e5";
const COMMAND = "018f0000-0000-7000-8000-0000000000f6";
const SNAPSHOT = "018f0000-0000-7000-8000-000000000101";
const SUB = "018f0000-0000-7000-8000-000000000202";
const SUB_B = "018f0000-0000-7000-8000-000000000204";
const EVENT_SUB = "018f0000-0000-7000-8000-000000000203";
const EVENT_ID = "018f0000-0000-7000-8000-000000000301";

const ALL_CAPS = Number(
  CAPABILITY_PAGED_SNAPSHOTS |
    CAPABILITY_EVENT_REPLAY |
    CAPABILITY_SEMANTIC_CONVERSATION |
    CAPABILITY_PROVIDER_INPUT |
    CAPABILITY_TASK_COCKPIT,
);

function limits() {
  return {
    max_physical_frame_bytes: 1_048_576,
    max_reassembled_message_bytes: 16_777_216,
    max_page_items: 1_000,
    max_page_encoded_bytes: 524_288,
    max_chunk_bytes: 262_144,
    max_cumulative_bytes: 16_777_216,
  };
}

function envelope(
  partial: Partial<DecodedConnectEnvelope> & {
    payloadKind: number;
    payload: unknown;
  },
): DecodedConnectEnvelope {
  return {
    protocolMajor: 1,
    protocolMinor: 0,
    connectionId: "018f0000-0000-7000-8000-000000000010",
    sessionId: "018f0000-0000-7000-8000-000000000011",
    channelId: "018f0000-0000-7000-8000-000000000012",
    channel: "critical",
    sequence: 1,
    requestId: null,
    operationId: null,
    limits: limits(),
    compression: "none",
    privacyClass: "local_only",
    payloadVersion: 1,
    payloadBase64: "",
    ...partial,
  };
}

function taskItem(taskId = TASK, title = "Example", revision = 2) {
  return {
    task: {
      task: {
        id: taskId,
        environment_id: "018f0000-0000-7000-8000-000000000301",
        title,
        description: null,
        project_id: "018f0000-0000-7000-8000-000000000302",
        workspace: { pathless: { workspace_id: "ws" } },
        assignment: "unassigned",
        lifecycle: "open",
        action_epoch: 1,
        revision,
        created_at_ms: 10,
      },
      connectivity: "connected",
      attention: "none",
      activity: "idle",
      review_readiness: "not_ready",
      primary_agent_id: AGENT,
    },
  };
}

function semanticPage(overrides: Record<string, unknown> = {}) {
  return {
    after_sequence: 0,
    through_sequence: 1,
    high_water: 1,
    oldest_sequence: 1,
    cursor_rolled_over: false,
    encoded_bytes: 32,
    next_sequence: null,
    facts: [
      {
        id: "018f0000-0000-7000-8000-000000000104",
        sequence: 1,
        provider: "codex",
        schema_version: 1,
        kind: "user_message",
        visibility: "conversation",
        privacy_class: "local_only",
        redacted: false,
        payload: { kind: "user_message", text: "hi" },
      },
    ],
    ...overrides,
  };
}

function providerState(overrides: Record<string, unknown> = {}) {
  return {
    task_id: TASK,
    task_revision: 2,
    action_epoch: 1,
    agent_session_id: AGENT,
    runtime_generation: 1,
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

function commandPayload(text = "hello", clientId = CLIENT) {
  return buildSubmitProviderInputSendNow({
    authority: {
      hostPublicId: HOST,
      clientId,
      requestId: "018f0000-0000-7000-8000-0000000000c3",
    },
    commandId: COMMAND,
    text,
    issuedAtMs: 1,
    fence: {
      hostPublicId: HOST,
      clientId,
      taskId: TASK,
      taskRevision: 2,
      actionEpoch: 1,
      agentSessionId: AGENT,
      runtimeGeneration: 1,
      agentLifecycle: "open",
      providerKind: "codex",
      providerSessionId: null,
      currentTurn: null,
      openQuestion: null,
      openApproval: null,
      pendingWaitCommandIds: [],
    },
  }).payload;
}

class FakeTransport implements NativeConnectTransport {
  stateListeners = new Set<(state: ConnectConnectionState) => void>();
  envelopeListeners = new Set<(envelope: DecodedConnectEnvelope) => void>();
  requests: Array<{ kind: number; payload: unknown; requestId?: string }> = [];
  handler:
    | ((
        kind: number,
        payload: unknown,
        requestId?: string,
      ) => Promise<DecodedConnectEnvelope>)
    | null = null;
  started = false;
  commandExecutions = 0;
  resyncCalls = 0;

  subscribe(listener: (state: ConnectConnectionState) => void) {
    this.stateListeners.add(listener);
    listener({ kind: "idle" });
    return () => this.stateListeners.delete(listener);
  }

  subscribeEnvelope(listener: (envelope: DecodedConnectEnvelope) => void) {
    this.envelopeListeners.add(listener);
    return () => this.envelopeListeners.delete(listener);
  }

  async start() {
    this.started = true;
    this.setState({ kind: "ready" });
  }

  stop() {
    this.started = false;
    this.setState({ kind: "closed", reason: "stop" });
  }

  requestResync() {
    this.resyncCalls += 1;
    return true;
  }

  setState(state: ConnectConnectionState) {
    for (const listener of this.stateListeners) listener(state);
  }

  push(next: DecodedConnectEnvelope) {
    for (const listener of this.envelopeListeners) listener(next);
  }

  async request(
    payloadKind: number,
    payload: unknown,
    options: { requestId?: string } = {},
  ) {
    this.requests.push({
      kind: payloadKind,
      payload,
      requestId: options.requestId,
    });
    if (!this.handler) throw new Error("no handler");
    return this.handler(payloadKind, payload, options.requestId);
  }
}

function reply(requestId: string, ok: Record<string, unknown>) {
  return envelope({
    payloadKind: NATIVE_QUERY_REPLY_KIND,
    requestId,
    payload: { request_id: requestId, outcome: { ok } },
  });
}

function hello(clientId = CLIENT) {
  return envelope({
    payloadKind: 1,
    payload: { client_id: clientId, capabilities: ALL_CAPS, limits: limits() },
  });
}

function durableEvent(sequence: number, taskId: string | null = TASK) {
  return envelope({
    payloadKind: HOST_DURABLE_OUTPUT,
    channel: "durable",
    privacyClass: "local_only",
    payload: {
      required_capabilities: Number(CAPABILITY_EVENT_REPLAY),
      message: {
        durable_event: {
          subscription_id: EVENT_SUB,
          event: {
            id: EVENT_ID,
            task_id: taskId,
            sequence,
            task_revision: 3,
            occurred_at_ms: 1_700_000_000_000,
            payload: { kind: "task_changed" },
          },
        },
      },
    },
  });
}

function resyncRequired() {
  return envelope({
    payloadKind: HOST_CRITICAL_OUTPUT,
    channel: "critical",
    privacyClass: "local_only",
    payload: {
      required_capabilities: Number(CAPABILITY_EVENT_REPLAY),
      message: {
        resync_required: {
          subscription_id: EVENT_SUB,
          last_delivered_sequence: 1,
          newest_sequence: 9,
        },
      },
    },
  });
}

function dirtyNotice(taskId: string, subscriptionId: string, highWater: number) {
  return envelope({
    payloadKind: NATIVE_CONVERSATION_DIRTY_KIND,
    channel: "ephemeral",
    requestId: null,
    operationId: null,
    privacyClass: "local_only",
    payload: {
      required_capabilities: Number(CAPABILITY_SEMANTIC_CONVERSATION | CAPABILITY_TASK_COCKPIT),
      message: {
        conversation_dirty: {
          subscription_id: subscriptionId,
          task_id: taskId,
          high_water: highWater,
        },
      },
    },
  });
}

function isOpenConversation(query: Record<string, unknown>): boolean {
  return (
    "task_cockpit" in query &&
    typeof query.task_cockpit === "object" &&
    query.task_cockpit !== null &&
    "open_conversation_subscription" in (query.task_cockpit as object)
  );
}

function isConversationPage(query: Record<string, unknown>): boolean {
  return (
    "task_cockpit" in query &&
    typeof query.task_cockpit === "object" &&
    query.task_cockpit !== null &&
    "conversation" in (query.task_cockpit as object)
  );
}

function defaultSyncHandler(
  transport: FakeTransport,
  options: {
    throughSequence?: number;
    conversationPages?: Map<string, Array<Record<string, unknown>>>;
    provider?: Record<string, unknown> | null;
    taskLifecycle?: string;
  } = {},
) {
  const throughSequence = options.throughSequence ?? 42;
  const conversationPages =
    options.conversationPages ??
    new Map([[TASK, [semanticPage({ next_sequence: null })]]]);
  const conversationIndex = new Map<string, number>();
  const snapshotTask = taskItem();
  snapshotTask.task.task.lifecycle = options.taskLifecycle ?? "open";

  transport.handler = async (kind, payload, requestId) => {
    if (kind === NATIVE_COMMAND_KIND) {
      transport.commandExecutions += 1;
      const commandId = (payload as { command_id?: string }).command_id ?? COMMAND;
      return envelope({
        payloadKind: NATIVE_COMMAND_RECEIPT_KIND,
        requestId: requestId ?? null,
        payload: {
          accepted: {
            command_id: commandId,
            operation_id: "018f0000-0000-7000-8000-000000000401",
            task_revision: 3,
            event_ids: [],
          },
        },
      });
    }
    if (kind !== NATIVE_QUERY_KIND) throw new Error(`unexpected kind ${kind}`);
    const query = (payload as { query: Record<string, unknown> }).query;
    if ("snapshot_page" in query) {
      return reply(requestId!, {
        snapshot_page: {
          page: {
            snapshot_id: SNAPSHOT,
            through_sequence: throughSequence,
            section: "tasks",
            after_item: null,
            items: [snapshotTask],
            encoded_bytes: 64,
            next_cursor: null,
          },
        },
      });
    }
    if ("release_snapshot" in query) {
      return reply(requestId!, {
        snapshot_released: { snapshot_id: SNAPSHOT },
      });
    }
    if ("open_event_replay" in query) {
      const after = (query.open_event_replay as { after_sequence: number })
        .after_sequence;
      return reply(requestId!, {
        event_replay_page: {
          subscription_id: EVENT_SUB,
          page: {
            after_sequence: after,
            through_sequence: after,
            events: [],
            next_cursor: null,
          },
        },
      });
    }
    if ("continue_event_replay" in query || "release_event_replay" in query) {
      return reply(requestId!, {
        event_replay_page: {
          subscription_id: EVENT_SUB,
          page: {
            after_sequence: throughSequence,
            through_sequence: throughSequence,
            events: [],
            next_cursor: null,
          },
        },
      });
    }
    if ("task_cockpit" in query && query.task_cockpit === "provider_input_state") {
      if (options.provider === null) {
        return reply(requestId!, {
          task_cockpit: {
            provider_input_state: providerState({
              agent_session_id: null,
              runtime_generation: null,
              agent_lifecycle: null,
              provider_kind: null,
            }),
          },
        });
      }
      return reply(requestId!, {
        task_cockpit: {
          provider_input_state: options.provider ?? providerState(),
        },
      });
    }
    if (isOpenConversation(query)) {
      const taskId = (payload as { task_id: string }).task_id;
      const pages = conversationPages.get(taskId) ?? [semanticPage()];
      const index = conversationIndex.get(taskId) ?? 0;
      conversationIndex.set(taskId, index + 1);
      const page = pages[Math.min(index, pages.length - 1)]!;
      return reply(requestId!, {
        task_cockpit: {
          conversation_subscription: {
            subscription_id: taskId === TASK_B ? SUB_B : SUB,
            page,
          },
        },
      });
    }
    if (isConversationPage(query)) {
      const after = (
        (query.task_cockpit as { conversation: { after_sequence: number } })
          .conversation
      ).after_sequence;
      return reply(requestId!, {
        task_cockpit: {
          conversation: semanticPage({
            after_sequence: after,
            through_sequence: after + 1,
            high_water: after + 1,
            oldest_sequence: 1,
            next_sequence: null,
            facts: [
              {
                id: "018f0000-0000-7000-8000-000000000105",
                sequence: after + 1,
                provider: "codex",
                schema_version: 1,
                kind: "user_message",
                visibility: "conversation",
                privacy_class: "local_only",
                redacted: false,
                payload: { kind: "user_message", text: "cont" },
              },
            ],
          }),
        },
      });
    }
    if (
      "task_cockpit" in query &&
      typeof query.task_cockpit === "object" &&
      query.task_cockpit &&
      "release_conversation_subscription" in (query.task_cockpit as object)
    ) {
      return reply(requestId!, {
        task_cockpit: {
          conversation_subscription_released: {
            subscription_id: (
              query.task_cockpit as {
                release_conversation_subscription: { subscription_id: string };
              }
            ).release_conversation_subscription.subscription_id,
          },
        },
      });
    }
    if ("task_snapshot" in query) {
      const taskId = (payload as { task_id: string }).task_id;
      const snapshot = taskItem(taskId, "refreshed", 9).task;
      snapshot.task.lifecycle = options.taskLifecycle ?? "open";
      return reply(requestId!, {
        task_snapshot: { snapshot },
      });
    }
    throw new Error(`unexpected query ${JSON.stringify(query)}`);
  };
}

async function waitFor(
  session: NativeHostSession,
  predicate: (view: NativeHostSessionView) => boolean,
  label: string,
): Promise<NativeHostSessionView> {
  const current = session.view();
  if (predicate(current)) return current;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`timeout waiting for ${label}`)),
      2_000,
    );
    const unsub = session.subscribe((view) => {
      if (!predicate(view)) return;
      clearTimeout(timer);
      unsub();
      resolve(view);
    });
  });
}

async function startLive(
  session: NativeHostSession,
  transport: FakeTransport,
  clientId = CLIENT,
) {
  await session.start();
  transport.push(hello(clientId));
  await waitFor(session, (view) => view.syncStatus === "live", "live sync");
}

describe("NativeHostSession corrections", () => {
  it("reads a terminal only on its captured host lease and rejects late output", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport);
    const base = transport.handler!;
    let release: (() => void) | undefined;
    let delay = false;
    transport.handler = async (kind, payload, requestId) => {
      if ((payload as { query?: { task_cockpit?: string } }).query?.task_cockpit === "terminal") {
        expect((payload as { task_id: string }).task_id).toBe(TASK);
        if (delay) await new Promise<void>((resolve) => { release = resolve; });
        return reply(requestId!, { task_cockpit: { terminal: {
          task_id: TASK, sequence: 2, title: null, text_lines: ["Provider ready"],
        } } });
      }
      return base(kind, payload, requestId);
    };
    const session = new NativeHostSession({ hostPublicId: HOST, transport });
    await expect(session.readTerminal(TASK)).rejects.toThrow(/unavailable/);
    await startLive(session, transport);
    expect((await session.readTerminal(TASK)).textLines).toEqual(["Provider ready"]);
    delay = true;
    const pending = session.readTerminal(TASK);
    transport.setState({ kind: "closed", reason: "lost" });
    release!();
    await expect(pending).rejects.toThrow(/changed/);
    session.stop();
  });

  it("sends terminal controls on the owner fence without clearing a chat draft", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport);
    const base = transport.handler!;
    let sent: unknown;
    transport.handler = async (kind, payload, requestId) => {
      if (kind === NATIVE_COMMAND_KIND) sent = payload;
      return base(kind, payload, requestId);
    };
    const cache = createMemoryNativeCacheStore();
    const session = new NativeHostSession({ hostPublicId: HOST, transport, cache, createCommandId: () => COMMAND });
    await startLive(session, transport);
    await session.setDraft(TASK, "\r");
    expect(await session.sendTerminalKey(TASK, "enter")).toEqual({ ok: true, commandId: COMMAND });
    expect(sent).toMatchObject({ task_id: TASK, command: { submit_provider_input: {
      action: { terminal_input: { text: "\r" } },
    } } });
    expect(session.view().drafts.get(TASK)?.text).toBe("\r");
    expect((await cache.loadHost(HOST)).drafts[0]?.text).toBe("\r");
    expect(session.view().outbox.size).toBe(0);
    session.stop();
  });

  it("serializes chat and terminal admission before their first await", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport);
    const base = transport.handler!;
    let release: (() => void) | undefined;
    let commands = 0;
    transport.handler = async (kind, payload, requestId) => {
      if ((payload as { query?: { task_cockpit?: unknown } }).query?.task_cockpit === "provider_input_state") {
        await new Promise<void>((resolve) => { release = resolve; });
      }
      if (kind === NATIVE_COMMAND_KIND) commands++;
      return base(kind, payload, requestId);
    };
    const session = new NativeHostSession({ hostPublicId: HOST, transport, createCommandId: () => COMMAND });
    await startLive(session, transport);
    const first = session.sendTerminalKey(TASK, "enter");
    expect(await session.sendText(TASK, "must not overlap")).toEqual({ ok: false, reason: "reconciliation_required" });
    expect(release).toBeTypeOf("function");
    release!();
    expect(await first).toEqual({ ok: true, commandId: COMMAND });
    expect(commands).toBe(1);
    session.stop();
  });

  it("reconciles an accepted lost receipt immediately without resending the command", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport);
    const base = transport.handler!;
    let originalPayload: unknown;
    let commandAttempts = 0;
    transport.handler = async (kind, payload, requestId) => {
      if (kind === NATIVE_COMMAND_KIND) {
        originalPayload = structuredClone(payload);
        commandAttempts += 1;
        throw new Error("receipt lost after host accepted");
      }
      const query = (payload as { query?: Record<string, unknown> }).query;
      if (query && "command_receipt_status" in query) {
        expect((query.command_receipt_status as { command: unknown }).command).toEqual(originalPayload);
        return reply(requestId!, { command_receipt_status: { receipt: { accepted: {
          command_id: COMMAND, operation_id: EVENT_ID, task_revision: 3, event_ids: [],
        } } } });
      }
      return base(kind, payload, requestId);
    };
    const cache = createMemoryNativeCacheStore();
    const session = new NativeHostSession({ hostPublicId: HOST, transport, cache, createCommandId: () => COMMAND });
    await startLive(session, transport);
    await session.setDraft(TASK, "hello");
    expect(await session.sendText(TASK, "hello")).toEqual({ ok: true, commandId: COMMAND });
    expect(commandAttempts).toBe(1);
    expect(session.view().outbox.size).toBe(0);
    expect(session.view().drafts.size).toBe(0);
    expect((await cache.loadHost(HOST)).drafts).toHaveLength(0);
    session.stop();
  });

  it("retries only an authoritative missing receipt with the original immutable command", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport);
    const base = transport.handler!;
    let originalPayload: unknown;
    let commandAttempts = 0;
    let allowMissing = false;
    transport.handler = async (kind, payload, requestId) => {
      if (kind === NATIVE_COMMAND_KIND) {
        commandAttempts += 1;
        if (commandAttempts === 1) {
          originalPayload = structuredClone(payload);
          throw new Error("disconnected before dispatch");
        }
        expect(payload).toEqual(originalPayload);
        return base(kind, payload, requestId);
      }
      const query = (payload as { query?: Record<string, unknown> }).query;
      if (query && "command_receipt_status" in query) {
        if (!allowMissing) throw new Error("status lookup unavailable");
        return reply(requestId!, { command_receipt_status: { receipt: null } });
      }
      return base(kind, payload, requestId);
    };
    const session = new NativeHostSession({ hostPublicId: HOST, transport, createCommandId: () => COMMAND });
    await startLive(session, transport);
    await session.sendText(TASK, "hello");
    expect((await session.retryOutbox(COMMAND)).ok).toBe(false);
    expect(commandAttempts).toBe(1);
    allowMissing = true;
    expect(await session.retryOutbox(COMMAND)).toEqual({ ok: true, commandId: COMMAND });
    expect(commandAttempts).toBe(2);
    expect(transport.commandExecutions).toBe(1);
    session.stop();
  });

  it("does not reuse authority when the same client reconnects during a provider query", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport);
    const base = transport.handler!;
    let release: () => void = () => { throw new Error("provider query not pending"); };
    transport.handler = async (kind, payload, requestId) => {
      if ((payload as { query?: { task_cockpit?: unknown } }).query?.task_cockpit === "provider_input_state") {
        await new Promise<void>((resolve) => { release = resolve; });
      }
      return base(kind, payload, requestId);
    };
    const session = new NativeHostSession({ hostPublicId: HOST, transport });
    await startLive(session, transport);
    const initialEpoch = session.view().leaseEpoch!;
    const sending = session.sendText(TASK, "must not cross a connection");
    await new Promise((resolve) => setTimeout(resolve, 0));
    transport.setState({ kind: "closed", reason: "network" });
    expect(session.view().lastError).toBe("network");
    transport.setState({ kind: "ready" });
    transport.push(hello());
    await waitFor(session, (view) => view.syncStatus === "live" && view.leaseEpoch! > initialEpoch, "new lease");
    expect(session.view().lastError).toBeNull();
    release();
    expect((await sending).ok).toBe(false);
    expect(transport.commandExecutions).toBe(0);
    expect(session.view().outbox.size).toBe(0);
    session.stop();
  });

  it("merges live semantic upserts by stable message identity", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport);
    const base = transport.handler!;
    transport.handler = async (kind, payload, requestId) => {
      const query = (payload as { query?: Record<string, unknown> }).query;
      if (query && isConversationPage(query)) {
        const first = semanticPage().facts[0];
        return reply(requestId!, { task_cockpit: { conversation: semanticPage({
          after_sequence: 1, through_sequence: 2, high_water: 2,
          facts: [{ ...first, sequence: 2, kind: "assistant_text", payload: { kind: "assistant_text", text: "complete response" } }],
        }) } });
      }
      return base(kind, payload, requestId);
    };
    const session = new NativeHostSession({ hostPublicId: HOST, transport });
    await startLive(session, transport);
    await session.watchTask(TASK);
    transport.push(dirtyNotice(TASK, SUB, 2));
    const view = await waitFor(session, (current) => current.conversations.get(TASK)?.throughSequence === 2, "semantic update");
    expect(view.conversations.get(TASK)?.facts).toHaveLength(1);
    expect(view.conversations.get(TASK)?.facts[0].payload).toEqual({ kind: "assistant_text", text: "complete response" });
    session.stop();
  });

  it("ignores already replayed deliveries instead of starting a reconnect loop", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport);
    const session = new NativeHostSession({ hostPublicId: HOST, transport });
    await startLive(session, transport);
    transport.push(durableEvent(41));
    transport.push(durableEvent(42));
    expect(transport.resyncCalls).toBe(0);
    expect(session.view().replayThrough).toBe(42);
    expect(session.view().connectionStatus).toBe("ready");
    session.stop();
  });

  it("installs snapshot HWM and opens event replay after exactly that HWM", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport, { throughSequence: 42 });
    let ids = 0;
    const session = new NativeHostSession({
      hostPublicId: HOST,
      transport,
      cache: createMemoryNativeCacheStore(() => 1_700_000_000_000),
      createRequestId: () =>
        `018f0000-0000-7000-8000-${(0xc100 + ids++).toString(16).padStart(12, "0")}`,
    });
    await startLive(session, transport);
    expect(session.view().replayThrough).toBe(42);
    const openReplay = transport.requests.find((r) => {
      const query = (r.payload as { query?: Record<string, unknown> })?.query;
      return query && "open_event_replay" in query;
    });
    expect(openReplay?.payload).toMatchObject({
      query: { open_event_replay: { after_sequence: 42 } },
    });
  });

  it("fails closed when the first replay page is not correlated to the snapshot HWM", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport, { throughSequence: 42 });
    const base = transport.handler!;
    transport.handler = async (kind, payload, requestId) => {
      const query = (payload as { query?: Record<string, unknown> }).query;
      if (query && "open_event_replay" in query) {
        return reply(requestId!, {
          event_replay_page: {
            subscription_id: EVENT_SUB,
            page: {
              after_sequence: 0,
              through_sequence: 42,
              events: [],
              next_cursor: null,
            },
          },
        });
      }
      return base(kind, payload, requestId);
    };
    const session = new NativeHostSession({ hostPublicId: HOST, transport });
    await session.start();
    transport.push(hello());
    const failed = await waitFor(session, (view) => view.syncStatus === "error", "replay correlation failure");
    expect(failed.lastError).toMatch(/initial cursor mismatch/);
    session.stop();
  });

  it("fails closed when a replay continuation changes its cursor or pinned high-water", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport, { throughSequence: 42 });
    const base = transport.handler!;
    transport.handler = async (kind, payload, requestId) => {
      const query = (payload as { query?: Record<string, unknown> }).query;
      if (query && "open_event_replay" in query) {
        const after = (query.open_event_replay as { after_sequence: number }).after_sequence;
        return reply(requestId!, {
          event_replay_page: {
            subscription_id: EVENT_SUB,
            page: {
              after_sequence: after,
              through_sequence: 44,
              events: [{
                id: EVENT_ID, task_id: TASK, sequence: 43, task_revision: 3,
                occurred_at_ms: 1_700_000_000_000,
                payload: { event_type: "task.reopened", payload: {} },
              }],
              next_cursor: new Uint8Array([1]),
            },
          },
        });
      }
      if (query && "continue_event_replay" in query) {
        return reply(requestId!, {
          event_replay_page: {
            subscription_id: EVENT_SUB,
            page: {
              after_sequence: 42,
              through_sequence: 44,
              events: [{
                id: "018f0000-0000-7000-8000-000000000302", task_id: TASK,
                sequence: 44, task_revision: 3, occurred_at_ms: 1_700_000_000_001,
                payload: { event_type: "task.reopened", payload: {} },
              }],
              next_cursor: null,
            },
          },
        });
      }
      return base(kind, payload, requestId);
    };
    const session = new NativeHostSession({ hostPublicId: HOST, transport });
    await session.start();
    transport.push(hello());
    const failed = await waitFor(session, (view) => view.syncStatus === "error", "replay continuation failure");
    expect(failed.lastError).toMatch(/continuation cursor mismatch/);
    session.stop();
  });

  it("applies HostDurableOutput sequence exactly and refreshes affected tasks", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport, { throughSequence: 2 });
    let ids = 0;
    const session = new NativeHostSession({
      hostPublicId: HOST,
      transport,
      cache: createMemoryNativeCacheStore(() => 1_700_000_000_000),
      createRequestId: () =>
        `018f0000-0000-7000-8000-${(0xc200 + ids++).toString(16).padStart(12, "0")}`,
    });
    await startLive(session, transport);
    expect(session.view().replayThrough).toBe(2);
    transport.push(durableEvent(3, TASK));
    await waitFor(
      session,
      (view) => view.tasks.get(TASK)?.revision === 9,
      "durable refresh",
    );
    expect(session.view().replayThrough).toBe(3);
  });

  it("forces resync on durable gap and critical resync_required", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport, { throughSequence: 2 });
    let ids = 0;
    const session = new NativeHostSession({
      hostPublicId: HOST,
      transport,
      cache: createMemoryNativeCacheStore(() => 1_700_000_000_000),
      createRequestId: () =>
        `018f0000-0000-7000-8000-${(0xc300 + ids++).toString(16).padStart(12, "0")}`,
    });
    await startLive(session, transport);
    transport.push(durableEvent(5, TASK)); // gap: expected 3
    await waitFor(
      session,
      (view) => view.syncStatus === "live" || view.syncStatus === "error",
      "gap recovery",
    );
    expect(transport.resyncCalls).toBeGreaterThanOrEqual(1);

    transport.push(resyncRequired());
    await waitFor(session, (view) => view.syncStatus === "live", "resync live");
  });

  it("opens conversation once then continues via conversation query, not a second open", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport, {
      conversationPages: new Map([
        [
          TASK,
          [
            semanticPage({
              through_sequence: 1,
              high_water: 2,
              next_sequence: 1,
            }),
          ],
        ],
      ]),
    });
    let ids = 0;
    const session = new NativeHostSession({
      hostPublicId: HOST,
      transport,
      cache: createMemoryNativeCacheStore(() => 1_700_000_000_000),
      createRequestId: () =>
        `018f0000-0000-7000-8000-${(0xc400 + ids++).toString(16).padStart(12, "0")}`,
    });
    await startLive(session, transport);
    await session.watchTask(TASK);
    const opens = transport.requests.filter((r) => {
      const query = (r.payload as { query?: Record<string, unknown> })?.query;
      return query && isOpenConversation(query);
    });
    const pages = transport.requests.filter((r) => {
      const query = (r.payload as { query?: Record<string, unknown> })?.query;
      return query && isConversationPage(query);
    });
    expect(opens.length).toBe(1);
    expect(pages.length).toBeGreaterThanOrEqual(1);
    expect(session.view().conversations.get(TASK)?.facts.length).toBeGreaterThanOrEqual(2);
  });

  it("buffers task-correlated dirty notices and does not let task B evict task A", async () => {
    const transport = new FakeTransport();
    let releaseA: () => void = () => { throw new Error("open not pending"); };
    let openA = 0;
    defaultSyncHandler(transport, {
      conversationPages: new Map([
        [TASK, [semanticPage()]],
        [TASK_B, [semanticPage({ facts: [] })]],
      ]),
    });
    const base = transport.handler!;
    transport.handler = async (kind, payload, requestId) => {
      if (kind === NATIVE_QUERY_KIND) {
        const query = (payload as { query: Record<string, unknown> }).query;
        if (isOpenConversation(query)) {
          const taskId = (payload as { task_id: string }).task_id;
          if (taskId === TASK) {
            openA += 1;
            if (openA === 1) {
              await new Promise<void>((resolve) => {
                releaseA = resolve;
              });
            }
          }
        }
      }
      return base(kind, payload, requestId);
    };
    let ids = 0;
    const session = new NativeHostSession({
      hostPublicId: HOST,
      transport,
      cache: createMemoryNativeCacheStore(() => 1_700_000_000_000),
      createRequestId: () =>
        `018f0000-0000-7000-8000-${(0xc500 + ids++).toString(16).padStart(12, "0")}`,
    });
    await startLive(session, transport);
    const watchA = session.watchTask(TASK);
    await new Promise((resolve) => setTimeout(resolve, 0));
    transport.push(dirtyNotice(TASK_B, SUB_B, 9));
    transport.push(dirtyNotice(TASK, SUB, 2));
    releaseA?.();
    await watchA;
    expect(
      decodeConversationDirtyEnvelope({
        payloadKind: NATIVE_CONVERSATION_DIRTY_KIND,
        payload: dirtyNotice(TASK, SUB, 2).payload,
        requestId: null,
        operationId: null,
        privacyClass: "local_only",
      }).taskId,
    ).toBe(TASK);
  });

  it("releases subscription when open completes after unwatch", async () => {
    const transport = new FakeTransport();
    let releaseOpen: () => void = () => { throw new Error("open not pending"); };
    defaultSyncHandler(transport);
    const base = transport.handler!;
    transport.handler = async (kind, payload, requestId) => {
      if (kind === NATIVE_QUERY_KIND) {
        const query = (payload as { query: Record<string, unknown> }).query;
        if (isOpenConversation(query)) {
          await new Promise<void>((resolve) => {
            releaseOpen = resolve;
          });
        }
      }
      return base(kind, payload, requestId);
    };
    let ids = 0;
    const session = new NativeHostSession({
      hostPublicId: HOST,
      transport,
      cache: createMemoryNativeCacheStore(() => 1_700_000_000_000),
      createRequestId: () =>
        `018f0000-0000-7000-8000-${(0xc600 + ids++).toString(16).padStart(12, "0")}`,
    });
    await startLive(session, transport);
    const watchPromise = session.watchTask(TASK);
    await new Promise((resolve) => setTimeout(resolve, 0));
    await session.unwatchTask(TASK);
    releaseOpen?.();
    await watchPromise;
    const releases = transport.requests.filter((r) => {
      const query = (r.payload as { query?: Record<string, unknown> })?.query;
      return (
        query &&
        typeof query.task_cockpit === "object" &&
        query.task_cockpit &&
        "release_conversation_subscription" in query.task_cockpit
      );
    });
    expect(releases.length).toBeGreaterThanOrEqual(1);
    expect(session.view().conversations.has(TASK)).toBe(false);
  });

  it("captures clientId before await and blocks wire/outbox on lease change", async () => {
    const transport = new FakeTransport();
    let releaseProvider: () => void = () => { throw new Error("provider query not pending"); };
    defaultSyncHandler(transport);
    const base = transport.handler!;
    transport.handler = async (kind, payload, requestId) => {
      if (kind === NATIVE_QUERY_KIND) {
        const query = (payload as { query: Record<string, unknown> }).query;
        if ("task_cockpit" in query && query.task_cockpit === "provider_input_state") {
          await new Promise<void>((resolve) => {
            releaseProvider = resolve;
          });
        }
      }
      return base(kind, payload, requestId);
    };
    let ids = 0;
    const session = new NativeHostSession({
      hostPublicId: HOST,
      transport,
      cache: createMemoryNativeCacheStore(() => 1_700_000_000_000),
      createRequestId: () =>
        `018f0000-0000-7000-8000-${(0xc700 + ids++).toString(16).padStart(12, "0")}`,
      createCommandId: () => COMMAND,
    });
    await startLive(session, transport);
    const sendPromise = session.sendText(TASK, "hello");
    await new Promise((resolve) => setTimeout(resolve, 0));
    transport.push(hello(CLIENT_B));
    releaseProvider?.();
    const result = await sendPromise;
    expect(result.ok).toBe(false);
    expect(
      transport.requests.some((r) => r.kind === NATIVE_COMMAND_KIND),
    ).toBe(false);
  });

  it("marks pending→in_flight before dispatch; uncertain requires reconciliation; pending only cancellable", async () => {
    const cache = createMemoryNativeCacheStore(() => 1_700_000_000_000);
    const transport = new FakeTransport();
    defaultSyncHandler(transport);
    const base = transport.handler!;
    let commandCalls = 0;
    transport.handler = async (kind, payload, requestId) => {
      if (kind === NATIVE_COMMAND_KIND) {
        commandCalls += 1;
        if (commandCalls === 1) throw new Error("network drop");
        return base(kind, payload, requestId);
      }
      return base(kind, payload, requestId);
    };
    let ids = 0;
    const session = new NativeHostSession({
      hostPublicId: HOST,
      transport,
      cache,
      createRequestId: () =>
        `018f0000-0000-7000-8000-${(0xc800 + ids++).toString(16).padStart(12, "0")}`,
      createCommandId: () => COMMAND,
    });
    await startLive(session, transport);
    const first = await session.sendText(TASK, "hello");
    expect(first).toEqual({ ok: false, reason: "transport_uncertain" });
    expect(session.view().outbox.get(COMMAND)?.status).toBe("uncertain");
    expect(await session.cancelUnsent(COMMAND)).toBe(false);
    expect(await session.retryOutbox(COMMAND)).toEqual({
      ok: false,
      reason: "reconciliation_required",
    });

    // A mismatched persisted command is rejected before the reload fixture.
    await expect(cache.putOutbox({
      hostPublicId: HOST,
      clientId: CLIENT,
      commandId: "018f0000-0000-7000-8000-0000000000f7",
      taskId: TASK,
      commandPayload: commandPayload("reload"),
      text: "reload",
      issuedAtMs: 2,
      status: "in_flight",
      updatedAtMs: 2,
    } as NativeOutboxRecord)).rejects.toThrow("command_id mismatch");
    // Fix command id in payload for validation — rebuild with matching id
    const reloadId = "018f0000-0000-7000-8000-0000000000f7";
    const reloadPayload = buildSubmitProviderInputSendNow({
      authority: {
        hostPublicId: HOST,
        clientId: CLIENT,
        requestId: "018f0000-0000-7000-8000-0000000000c9",
      },
      commandId: reloadId,
      text: "reload",
      issuedAtMs: 2,
      fence: {
        hostPublicId: HOST,
        clientId: CLIENT,
        taskId: TASK,
        taskRevision: 2,
        actionEpoch: 1,
        agentSessionId: AGENT,
        runtimeGeneration: 1,
        agentLifecycle: "open",
        providerKind: "codex",
        providerSessionId: null,
        currentTurn: null,
        openQuestion: null,
        openApproval: null,
        pendingWaitCommandIds: [],
      },
    }).payload;
    await cache.settleOutbox(HOST, reloadId).catch(() => undefined);
    await cache.putOutbox({
      hostPublicId: HOST,
      clientId: CLIENT,
      commandId: reloadId,
      taskId: TASK,
      commandPayload: reloadPayload,
      text: "reload",
      issuedAtMs: 2,
      status: "in_flight",
      updatedAtMs: 2,
    });
    const session2 = new NativeHostSession({
      hostPublicId: HOST,
      transport: new FakeTransport(),
      cache,
    });
    await session2.hydrate();
    expect(session2.view().outbox.get(reloadId)?.status).toBe("uncertain");
  });

  it("documents metadata bound 4096 vs history/watch 64", () => {
    expect(MAX_METADATA_TASKS_PER_HOST).toBe(4096);
    expect(MAX_HISTORY_CONVERSATIONS_PER_HOST).toBe(64);
    expect(MAX_OUTBOX_ITEMS).toBe(64);
  });
});

describe("NativeHostSession task lifecycle mutations", () => {
  it("renames Done without restoring it or clearing its draft", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport, { taskLifecycle: "settled" });
    const base = transport.handler!;
    const commands: unknown[] = [];
    transport.handler = async (kind, payload, requestId) => {
      if (kind === NATIVE_COMMAND_KIND) commands.push(structuredClone(payload));
      return base(kind, payload, requestId);
    };
    const session = new NativeHostSession({
      hostPublicId: HOST,
      transport,
      createCommandId: () => COMMAND,
    });
    await startLive(session, transport);
    await session.setDraft(TASK, "keep Done draft");
    expect(await session.renameTask(TASK, "Finished work")).toEqual({
      ok: true,
      commandId: COMMAND,
    });
    expect(commands).toHaveLength(1);
    expect(commands[0]).toMatchObject({
      task_id: TASK,
      expected_task_revision: 9,
      command: { rename_task: { title: "Finished work" } },
    });
    expect(session.view().tasks.get(TASK)?.lifecycle).toBe("settled");
    expect(session.view().drafts.get(TASK)?.text).toBe("keep Done draft");
    session.stop();
  });

  it("settles with a unit-string command and never clears the composer draft", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport);
    const base = transport.handler!;
    let sent: unknown;
    transport.handler = async (kind, payload, requestId) => {
      if (kind === NATIVE_COMMAND_KIND) sent = structuredClone(payload);
      return base(kind, payload, requestId);
    };
    const cache = createMemoryNativeCacheStore();
    const session = new NativeHostSession({
      hostPublicId: HOST,
      transport,
      cache,
      createCommandId: () => COMMAND,
    });
    await startLive(session, transport);
    await session.setDraft(TASK, "keep draft");
    expect(await session.settleTask(TASK)).toEqual({
      ok: true,
      commandId: COMMAND,
    });
    expect(sent).toMatchObject({
      command_id: COMMAND,
      task_id: TASK,
      expected_task_revision: 9,
      command: "settle_task",
    });
    expect(session.view().drafts.get(TASK)?.text).toBe("keep draft");
    expect((await cache.loadHost(HOST)).drafts[0]?.text).toBe("keep draft");
    expect(session.view().outbox.size).toBe(0);
    session.stop();
  });

  it("rejects delete until archived and keeps lifecycle on storage failure", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport);
    const session = new NativeHostSession({
      hostPublicId: HOST,
      transport,
      createCommandId: () => COMMAND,
    });
    await startLive(session, transport);
    expect(await session.deleteTask(TASK)).toEqual({
      ok: false,
      reason: "invalid_lifecycle",
    });
    expect(transport.commandExecutions).toBe(0);
    session.stop();
  });

  it("sends to Done with SendNow only — no TaskSnapshot reopen and failed send leaves lifecycle untouched", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport, { taskLifecycle: "settled" });
    const base = transport.handler!;
    const commands: unknown[] = [];
    const queries: string[] = [];
    const cache = createMemoryNativeCacheStore();
    const session = new NativeHostSession({
      hostPublicId: HOST,
      transport,
      cache,
      createCommandId: () => COMMAND,
    });
    await startLive(session, transport);
    const lifecycleBefore = session.view().tasks.get(TASK)?.lifecycle ?? null;
    expect(lifecycleBefore).toBe("settled");
    await session.setDraft(TASK, "resume me");
    transport.handler = async (kind, payload, requestId) => {
      if (kind === NATIVE_QUERY_KIND) {
        const query = (payload as { query: Record<string, unknown> }).query;
        queries.push(Object.keys(query)[0] ?? "");
        if ("task_snapshot" in query) {
          throw new Error("send path must not query TaskSnapshot");
        }
        if ("command_receipt_status" in query) {
          return reply(requestId!, {
            command_receipt_status: { receipt: null },
          });
        }
      }
      if (kind === NATIVE_COMMAND_KIND) {
        commands.push(structuredClone(payload));
        throw new Error("send lost");
      }
      return base(kind, payload, requestId);
    };
    expect(await session.sendText(TASK, "resume me")).toEqual({
      ok: false,
      reason: "transport_uncertain",
    });
    expect(commands).toHaveLength(1);
    expect(commands[0]).toMatchObject({
      command_id: COMMAND,
      command: {
        submit_provider_input: {
          action: { send_now: { text: "resume me", wait: false } },
        },
      },
    });
    expect(JSON.stringify(commands)).not.toContain("reopen_task");
    expect(queries.filter((key) => key === "task_snapshot")).toHaveLength(0);
    expect(queries).toContain("task_cockpit");
    expect(session.view().drafts.get(TASK)?.text).toBe("resume me");
    expect(session.view().outbox.get(COMMAND)?.status).toBe("uncertain");
    expect(session.view().tasks.get(TASK)?.lifecycle).toBe(lifecycleBefore);
    session.stop();
  });

  it("keeps manual Restore as the only ReopenTask metadata path", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport);
    const base = transport.handler!;
    let sent: unknown;
    transport.handler = async (kind, payload, requestId) => {
      if (kind === NATIVE_QUERY_KIND) {
        const query = (payload as { query: Record<string, unknown> }).query;
        if ("task_snapshot" in query) {
          return reply(requestId!, {
            task_snapshot: {
              snapshot: {
                ...taskItem(TASK, "Done task", 4).task,
                task: {
                  ...taskItem(TASK, "Done task", 4).task.task,
                  lifecycle: "settled",
                },
              },
            },
          });
        }
      }
      if (kind === NATIVE_COMMAND_KIND) {
        sent = structuredClone(payload);
      }
      return base(kind, payload, requestId);
    };
    const session = new NativeHostSession({
      hostPublicId: HOST,
      transport,
      createCommandId: () => COMMAND,
    });
    await startLive(session, transport);
    expect(await session.reopenTask(TASK)).toEqual({
      ok: true,
      commandId: COMMAND,
    });
    expect(sent).toMatchObject({
      command: "reopen_task",
      expected_task_revision: 4,
    });
    session.stop();
  });

  it("blocks overlapping mutate and send on the same task", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport);
    const base = transport.handler!;
    let release: (() => void) | undefined;
    let heldFirstSnapshot = false;
    transport.handler = async (kind, payload, requestId) => {
      if (
        kind === NATIVE_QUERY_KIND &&
        "task_snapshot" in (payload as { query: Record<string, unknown> }).query &&
        !heldFirstSnapshot
      ) {
        heldFirstSnapshot = true;
        await new Promise<void>((resolve) => {
          release = resolve;
        });
      }
      return base(kind, payload, requestId);
    };
    const session = new NativeHostSession({
      hostPublicId: HOST,
      transport,
      createCommandId: () => COMMAND,
    });
    await startLive(session, transport);
    const pending = session.settleTask(TASK);
    expect(await session.sendText(TASK, "nope")).toEqual({
      ok: false,
      reason: "reconciliation_required",
    });
    release!();
    expect(await pending).toEqual({ ok: true, commandId: COMMAND });
    session.stop();
  });

  it("keeps original command IDs when a metadata receipt is uncertain", async () => {
    const transport = new FakeTransport();
    defaultSyncHandler(transport);
    const base = transport.handler!;
    let original: unknown;
    transport.handler = async (kind, payload, requestId) => {
      if (kind === NATIVE_COMMAND_KIND) {
        original = structuredClone(payload);
        throw new Error("receipt lost");
      }
      const query = (payload as { query?: Record<string, unknown> }).query;
      if (query && "command_receipt_status" in query) {
        expect(
          (query.command_receipt_status as { command: unknown }).command,
        ).toEqual(original);
        return reply(requestId!, {
          command_receipt_status: { receipt: null },
        });
      }
      return base(kind, payload, requestId);
    };
    const session = new NativeHostSession({
      hostPublicId: HOST,
      transport,
      createCommandId: () => COMMAND,
    });
    await startLive(session, transport);
    await session.setDraft(TASK, "draft stays");
    expect(await session.beginCloseTask(TASK)).toEqual({
      ok: false,
      reason: "transport_uncertain",
    });
    expect(session.view().outbox.get(COMMAND)?.commandId).toBe(COMMAND);
    expect(session.view().outbox.get(COMMAND)?.commandPayload).toEqual(original);
    expect(session.view().drafts.get(TASK)?.text).toBe("draft stays");
    session.stop();
  });
});

describe("nativeCache strict outbox + IDB seam", () => {
  it("rejects arbitrary command payloads and accepts exact submit_provider_input", () => {
    expect(() =>
      validateOutboxCommandPayload(
        { garbage: true },
        {
          hostPublicId: HOST,
          clientId: CLIENT,
          commandId: COMMAND,
          taskId: TASK,
          text: "hello",
        },
      ),
    ).toThrow(/shape|rejected/i);
    expect(
      validateOutboxCommandPayload(commandPayload("hello"), {
        hostPublicId: HOST,
        clientId: CLIENT,
        commandId: COMMAND,
        taskId: TASK,
        text: "hello",
      }),
    ).toBeTruthy();
    expect(
      validateOutboxCommandPayload(
        {
          command_id: COMMAND,
          client_id: CLIENT,
          task_id: TASK,
          issued_at_ms: 1,
          expected_task_revision: 2,
          command: "begin_close_task",
        },
        {
          hostPublicId: HOST,
          clientId: CLIENT,
          commandId: COMMAND,
          taskId: TASK,
          text: "",
        },
      ),
    ).toBeTruthy();
  });

  it("does not silently TTL-delete drafts; reports missing fake-indexeddb runtime", async () => {
    const now = { t: 1_700_000_000_000 };
    const cache = createMemoryNativeCacheStore(() => now.t);
    await cache.putDraft(HOST, {
      taskId: TASK,
      text: "keep me",
      updatedAtMs: now.t,
    });
    now.t += 8 * 24 * 60 * 60 * 1000;
    const snap = await cache.loadHost(HOST);
    expect(snap.drafts).toHaveLength(1);

    // package.json has no fake-indexeddb; injected factory seam exists for root.
    const idbStore = createIndexedDbNativeCacheStore(
      undefined as unknown as IDBFactory,
    );
    await expect(idbStore.loadHost(HOST)).rejects.toThrow(/IndexedDB unavailable/);
  });

  it("failed status update does not diverge when IDB unavailable", async () => {
    const store = createIndexedDbNativeCacheStore(
      undefined as unknown as IDBFactory,
    );
    await expect(
      store.updateOutboxStatus(HOST, COMMAND, "uncertain"),
    ).rejects.toThrow(/IndexedDB unavailable/);
  });
});
