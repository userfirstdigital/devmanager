import { IDBFactory, IDBKeyRange } from "fake-indexeddb";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  createIndexedDbNativeCacheStore,
  createMemoryNativeCacheStore,
  MAX_HISTORY_CONVERSATIONS_PER_HOST,
  MAX_METADATA_TASKS_PER_HOST,
  MAX_OUTBOX_ITEMS,
  NATIVE_CACHE_DB_NAME,
  NATIVE_CACHE_DB_VERSION,
  tasksFromSnapshotItems,
  validateOutboxCommandPayload,
} from "./nativeCache";
import { buildStartProviderSessionCommand, buildSubmitProviderInputSendNow } from "./nativeProtocol";

const HOST = "018f0000-0000-7000-8000-0000000000a1";
const HOST_B = "018f0000-0000-7000-8000-0000000000a2";
const TASK = "018f0000-0000-7000-8000-0000000000d4";
const CLIENT = "018f0000-0000-7000-8000-0000000000b2";
const COMMAND = "018f0000-0000-7000-8000-0000000000f6";
const AGENT = "018f0000-0000-7000-8000-0000000000e5";
const RESOURCE = "018f0000-0000-7000-8000-0000000000e6";

function exactPayload(text = "hi", commandId = COMMAND) {
  return buildSubmitProviderInputSendNow({
    authority: {
      hostPublicId: HOST,
      clientId: CLIENT,
      requestId: "018f0000-0000-7000-8000-0000000000c3",
    },
    commandId,
    text,
    issuedAtMs: 1,
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
}

function taskId(index: number): string {
  return `018f0000-0000-7000-8000-${index.toString(16).padStart(12, "0")}`;
}

function emptyConversation(taskIdValue: string, updatedAtMs: number) {
  return {
    taskId: taskIdValue,
    afterSequence: 0,
    throughSequence: 0,
    highWater: 0,
    oldestSequence: 0,
    cursorRolledOver: false,
    nextSequence: null,
    facts: [],
    updatedAtMs,
  };
}

function largeConversation(taskIdValue: string, updatedAtMs: number, factSeed: number) {
  const facts = Array.from({ length: 70 }, (_, index) => ({
    id: taskId(9_000 + factSeed * 100 + index),
    sequence: index + 1,
    occurredAtMs: null,
    provider: "test",
    schemaVersion: 1,
    kind: "assistant_text",
    visibility: "task",
    privacyClass: "local_only" as const,
    redacted: false,
    payload: { kind: "assistant_text" as const, text: "x".repeat(60_000) },
  }));
  return {
    taskId: taskIdValue,
    afterSequence: 0,
    throughSequence: facts.length,
    highWater: facts.length,
    oldestSequence: 1,
    cursorRolledOver: false,
    nextSequence: null,
    facts,
    updatedAtMs,
  };
}

function outboxRecord(commandId = COMMAND, text = "hi") {
  return {
    hostPublicId: HOST,
    clientId: CLIENT,
    commandId,
    taskId: TASK,
    commandPayload: exactPayload(text, commandId),
    text,
    issuedAtMs: 1,
    status: "pending" as const,
    updatedAtMs: 1,
  };
}

describe("nativeCache", () => {
  beforeEach(() => {
    vi.stubGlobal("IDBKeyRange", IDBKeyRange);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("hydrates cache-first without network and isolates host namespaces", async () => {
    const cache = createMemoryNativeCacheStore(() => 1_700_000_000_000);
    await cache.putTasks(HOST, [
      {
        taskId: TASK,
        revision: 1,
        actionEpoch: 0,
        title: "A",
        lifecycle: "open",
        projectId: null,
        environmentId: null,
        createdAtMs: 1,
        connectivity: "connected",
        attention: "none",
        activity: "idle",
        primaryAgentId: null,
        updatedAtMs: 1_700_000_000_000,
      },
    ]);
    await cache.putTasks(HOST_B, []);
    expect((await cache.loadHost(HOST)).tasks).toHaveLength(1);
    expect((await cache.loadHost(HOST_B)).tasks).toHaveLength(0);
    await cache.clearHost(HOST);
    expect((await cache.loadHost(HOST)).tasks).toHaveLength(0);
  });

  it("persists exact outbox envelopes and rejects capacity overflow", async () => {
    const cache = createMemoryNativeCacheStore(() => 1_700_000_000_000);
    await cache.putOutbox({
      hostPublicId: HOST,
      clientId: CLIENT,
      commandId: COMMAND,
      taskId: TASK,
      commandPayload: exactPayload(),
      text: "hi",
      issuedAtMs: 1,
      status: "pending",
      updatedAtMs: 1,
    });
    expect((await cache.loadHost(HOST)).outbox).toHaveLength(1);
    await cache.settleOutbox(HOST, COMMAND);
    expect((await cache.loadHost(HOST)).outbox).toHaveLength(0);

    for (let index = 0; index < MAX_OUTBOX_ITEMS; index += 1) {
      const commandId = `018f0000-0000-7000-8000-${(0x1000 + index).toString(16).padStart(12, "0")}`;
      const payload = buildSubmitProviderInputSendNow({
        authority: {
          hostPublicId: HOST,
          clientId: CLIENT,
          requestId: "018f0000-0000-7000-8000-0000000000c3",
        },
        commandId,
        text: "x",
        issuedAtMs: index,
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
      await cache.putOutbox({
        hostPublicId: HOST,
        clientId: CLIENT,
        commandId,
        taskId: TASK,
        commandPayload: payload,
        text: "x",
        issuedAtMs: index,
        status: "pending",
        updatedAtMs: 1,
      });
    }
    await expect(
      cache.putOutbox({
        hostPublicId: HOST,
        clientId: CLIENT,
        commandId: "018f0000-0000-7000-8000-00000000ffff",
        taskId: TASK,
        commandPayload: buildSubmitProviderInputSendNow({
          authority: {
            hostPublicId: HOST,
            clientId: CLIENT,
            requestId: "018f0000-0000-7000-8000-0000000000c3",
          },
          commandId: "018f0000-0000-7000-8000-00000000ffff",
          text: "overflow",
          issuedAtMs: 99,
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
        }).payload,
        text: "overflow",
        issuedAtMs: 99,
        status: "pending",
        updatedAtMs: 1,
      }),
    ).rejects.toThrow(/capacity/);
  });

  it("rejects arbitrary outbox payloads", () => {
    expect(() =>
      validateOutboxCommandPayload(
        { command_id: COMMAND },
        {
          hostPublicId: HOST,
          clientId: CLIENT,
          commandId: COMMAND,
          taskId: TASK,
          text: "hi",
        },
      ),
    ).toThrow();
  });

  it("rejects a persisted start command with cross-provider launch options", () => {
    const payload = buildStartProviderSessionCommand({
      authority: { hostPublicId: HOST, clientId: CLIENT,
        requestId: "018f0000-0000-7000-8000-0000000000c3" },
      commandId: COMMAND,
      taskId: TASK,
      agentSessionId: AGENT,
      resourceId: RESOURCE,
      provider: "codex",
      expectedTaskRevision: 2,
      actionEpoch: 1,
      issuedAtMs: 1,
      launchOptions: { model: "codex_terra", reasoningEffort: "low", access: "full_access" },
    }).payload as { command: { start_provider_session: { launch_options: { model: string } } } };
    payload.command.start_provider_session.launch_options.model = "claude_opus";
    expect(() => validateOutboxCommandPayload(payload, {
      hostPublicId: HOST,
      clientId: CLIENT,
      commandId: COMMAND,
      taskId: TASK,
      text: "",
    })).toThrow(/content rejected/);
  });

  it("keeps drafts across TTL windows and separates metadata vs history bounds", async () => {
    expect(MAX_METADATA_TASKS_PER_HOST).toBe(4096);
    expect(MAX_HISTORY_CONVERSATIONS_PER_HOST).toBe(64);
    const now = { t: 1_700_000_000_000 };
    const cache = createMemoryNativeCacheStore(() => now.t);
    await cache.putDraft(HOST, {
      taskId: TASK,
      text: "draft",
      updatedAtMs: now.t,
    });
    now.t += 10 * 24 * 60 * 60 * 1000;
    expect((await cache.loadHost(HOST)).drafts[0]?.text).toBe("draft");
  });

  it("maps snapshot list items into task metadata without paths", () => {
    const metas = tasksFromSnapshotItems(
      [
        {
          kind: "task",
          taskId: TASK,
          revision: 3,
          actionEpoch: 1,
          primaryAgentId: null,
          title: "T",
          lifecycle: "open",
          projectId: "018f0000-0000-7000-8000-000000000301",
          environmentId: "018f0000-0000-7000-8000-000000000302",
          createdAtMs: 10,
          connectivity: "connected",
          attention: "none",
          activity: "idle",
        },
      ],
      99,
    );
    expect(metas[0]).toMatchObject({ taskId: TASK, updatedAtMs: 99 });
    expect(JSON.stringify(metas[0])).not.toMatch(/\\\\|C:/);
  });

  it("evicts the oldest history at 64 while preserving the 4096 metadata bound", async () => {
    const now = 1_700_000_000_000;
    const cache = createMemoryNativeCacheStore(() => now);
    for (let index = 0; index <= MAX_HISTORY_CONVERSATIONS_PER_HOST; index += 1) {
      await cache.putConversation(
        HOST,
        emptyConversation(taskId(index), now + index),
      );
    }

    const snapshot = await cache.loadHost(HOST);
    expect(snapshot.conversations).toHaveLength(MAX_HISTORY_CONVERSATIONS_PER_HOST);
    expect(snapshot.conversations.map((item) => item.taskId)).not.toContain(taskId(0));
    expect(snapshot.conversations.map((item) => item.taskId)).toContain(taskId(64));

    await cache.putTasks(
      HOST,
      Array.from({ length: MAX_METADATA_TASKS_PER_HOST }, (_, index) => ({
        taskId: taskId(index + 100),
        revision: 1,
        actionEpoch: 0,
        title: null,
        lifecycle: null,
        projectId: null,
        environmentId: null,
        createdAtMs: null,
        connectivity: null,
        attention: null,
        activity: null,
        primaryAgentId: null,
        updatedAtMs: 1,
      })),
    );
    await expect(
      cache.putTasks(HOST, [
        ...Array.from({ length: MAX_METADATA_TASKS_PER_HOST }, (_, index) => ({
          taskId: taskId(index + 100),
          revision: 1,
          actionEpoch: 0,
          title: null,
          lifecycle: null,
          projectId: null,
          environmentId: null,
          createdAtMs: null,
          connectivity: null,
          attention: null,
          activity: null,
          primaryAgentId: null,
          updatedAtMs: 1,
        })),
        {
          taskId: taskId(5_000),
          revision: 1,
          actionEpoch: 0,
          title: null,
          lifecycle: null,
          projectId: null,
          environmentId: null,
          createdAtMs: null,
          connectivity: null,
          attention: null,
          activity: null,
          primaryAgentId: null,
          updatedAtMs: 1,
        },
      ]),
    ).rejects.toThrow(/task list exceeds bound/);
  });

  it("evicts oldest history by the 8MB budget before the 64-conversation cap", async () => {
    const now = 1_700_000_000_000;
    const cache = createMemoryNativeCacheStore(() => now);
    const oldest = taskId(7_001);
    const newest = taskId(7_002);
    await cache.putConversation(HOST, largeConversation(oldest, now, 1));
    await cache.putConversation(HOST, largeConversation(newest, now + 1, 2));

    expect((await cache.loadHost(HOST)).conversations.map((item) => item.taskId)).toEqual([
      newest,
    ]);
  });

  it("reloads durable drafts and in-flight outbox records without sharing caller payloads", async () => {
    const factory = new IDBFactory();
    const now = Date.now();
    const first = createIndexedDbNativeCacheStore(factory);
    const record = outboxRecord();
    await first.putDraft(HOST, { taskId: TASK, text: "keep", updatedAtMs: 1 });
    await first.putOutbox(record);
    await first.updateOutboxStatus(HOST, COMMAND, "in_flight");
    for (let index = 0; index <= MAX_HISTORY_CONVERSATIONS_PER_HOST; index += 1) {
      await first.putConversation(
        HOST,
        emptyConversation(taskId(index + 1_000), now + index),
      );
    }

    (record.commandPayload as { command: { submit_provider_input: { action: { send_now: { text: string } } } } }).command.submit_provider_input.action.send_now.text = "mutated";

    const reloaded = createIndexedDbNativeCacheStore(factory);
    const snapshot = await reloaded.loadHost(HOST);
    expect(snapshot.drafts).toEqual([{ taskId: TASK, text: "keep", updatedAtMs: 1 }]);
    expect(snapshot.conversations).toHaveLength(MAX_HISTORY_CONVERSATIONS_PER_HOST);
    expect(snapshot.conversations.map((item) => item.taskId)).not.toContain(taskId(1_000));
    expect(snapshot.outbox).toHaveLength(1);
    expect(snapshot.outbox[0]).toMatchObject({ commandId: COMMAND, status: "in_flight" });
    expect(
      (snapshot.outbox[0]?.commandPayload as { command: { submit_provider_input: { action: { send_now: { text: string } } } } }).command.submit_provider_input.action.send_now.text,
    ).toBe("hi");

    (snapshot.outbox[0]?.commandPayload as { command: { submit_provider_input: { action: { send_now: { text: string } } } } }).command.submit_provider_input.action.send_now.text = "mutated after load";
    expect(
      ((await reloaded.loadHost(HOST)).outbox[0]?.commandPayload as { command: { submit_provider_input: { action: { send_now: { text: string } } } } }).command.submit_provider_input.action.send_now.text,
    ).toBe("hi");
  });

  it("durably removes a quarantined legacy outbox row before admitting a new command", async () => {
    const factory = new IDBFactory();
    const cache = createIndexedDbNativeCacheStore(factory);
    await cache.loadHost(HOST);
    const db = await new Promise<IDBDatabase>((resolve, reject) => {
      const request = factory.open(NATIVE_CACHE_DB_NAME, NATIVE_CACHE_DB_VERSION);
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error);
    });
    await new Promise<void>((resolve, reject) => {
      const tx = db.transaction("outbox", "readwrite");
      tx.objectStore("outbox").put(
        { ...outboxRecord(), commandPayload: { command: { create_task_v2: {
          primary_provider: "claude_code",
        } } } },
        `${HOST}:cmd:${COMMAND}`,
      );
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error);
    });

    const next = outboxRecord(taskId(8_123), "next");
    await expect(cache.putOutbox(next)).resolves.toBeUndefined();
    expect((await cache.loadHost(HOST)).outbox).toEqual([next]);
    db.close();
  });

  it("keeps host-prefixed IndexedDB records isolated", async () => {
    const cache = createIndexedDbNativeCacheStore(new IDBFactory());
    await cache.putDraft(HOST, { taskId: TASK, text: "host A", updatedAtMs: 1 });
    await cache.putDraft(HOST_B, { taskId: TASK, text: "host B", updatedAtMs: 1 });
    await cache.clearHost(HOST);

    expect((await cache.loadHost(HOST)).drafts).toEqual([]);
    expect((await cache.loadHost(HOST_B)).drafts).toEqual([
      { taskId: TASK, text: "host B", updatedAtMs: 1 },
    ]);
  });

  it("clears only the accepted matching draft in the same durable outbox transaction", async () => {
    const cache = createIndexedDbNativeCacheStore(new IDBFactory());
    await cache.putDraft(HOST, { taskId: TASK, text: "sent", updatedAtMs: 1 });
    await cache.putOutbox(outboxRecord(COMMAND, "sent"));
    await cache.settleOutbox(HOST, COMMAND, "sent");
    expect(await cache.loadHost(HOST)).toMatchObject({ drafts: [], outbox: [] });

    const laterCommand = taskId(8_001);
    await cache.putDraft(HOST, { taskId: TASK, text: "later draft", updatedAtMs: 2 });
    await cache.putOutbox(outboxRecord(laterCommand, "sent"));
    await cache.settleOutbox(HOST, laterCommand, "sent");
    expect(await cache.loadHost(HOST)).toMatchObject({
      drafts: [{ taskId: TASK, text: "later draft", updatedAtMs: 2 }],
      outbox: [],
    });

    const cancelledCommand = taskId(8_002);
    await cache.putOutbox(outboxRecord(cancelledCommand, "later draft"));
    await cache.settleOutbox(HOST, cancelledCommand);
    expect(await cache.loadHost(HOST)).toMatchObject({
      drafts: [{ taskId: TASK, text: "later draft", updatedAtMs: 2 }],
      outbox: [],
    });
  });

  it("does not update the memory mirror when an IndexedDB write transaction aborts", async () => {
    const backing = new IDBFactory();
    let opened: IDBDatabase | null = null;
    const capturingFactory = {
      open(name: string, version?: number) {
        const request = backing.open(name, version);
        request.addEventListener("success", () => {
          opened = request.result;
        });
        return request;
      },
    } as unknown as IDBFactory;
    const cache = createIndexedDbNativeCacheStore(capturingFactory);
    await cache.loadHost(HOST);
    const database = opened as IDBDatabase | null;
    if (database === null) throw new Error("expected IndexedDB open");
    const transaction = database.transaction.bind(database);
    database.transaction = ((storeNames, mode, options) => {
      const tx = transaction(storeNames, mode, options);
      if (mode === "readwrite") queueMicrotask(() => tx.abort());
      return tx;
    }) as IDBDatabase["transaction"];

    await expect(
      cache.putDraft(HOST, { taskId: TASK, text: "must not mirror", updatedAtMs: 1 }),
    ).rejects.toThrow(/IndexedDB transaction (aborted|failed)/);
    expect((await cache.loadHost(HOST)).drafts).toEqual([]);

    const reloaded = createIndexedDbNativeCacheStore(backing);
    expect((await reloaded.loadHost(HOST)).drafts).toEqual([]);
  });
});
