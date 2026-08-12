import { describe, expect, it } from "vitest";

import {
  admitClientInput,
  answerClientRequest,
  applyHostProjection,
  connectClient,
  createConnectClientSession,
  observeAuthoritativeSender,
  openRequest,
  ownerBadge,
  reconcileEcho,
  requiresManualRefresh,
  visibleController,
  type DeviceInput,
} from "./session";

function input(overrides: Partial<DeviceInput> = {}): DeviceInput {
  return {
    taskId: "tab:a",
    clientId: "device-a",
    commandId: "cmd-1",
    operationId: "op-1",
    expectedRevision: 1,
    inputSequence: 1,
    turnEpoch: 1,
    focusEpoch: 1,
    observedAtMs: 1_000,
    ...overrides,
  };
}

function connectedSession() {
  const session = createConnectClientSession("tab:a");
  connectClient(session, "device-a");
  return session;
}

describe("connect session controller derivation", () => {
  it("derives the internal controller from durable accepted input", () => {
    const session = connectedSession();
    expect(visibleController(session)).toBeNull();
    expect(admitClientInput(session, input())).toMatchObject({
      kind: "acceptedDurable",
      operationId: "op-1",
    });
    expect(visibleController(session)).toEqual({
      clientId: "device-a",
      taskId: "tab:a",
      turnEpoch: 1,
      focusEpoch: 1,
    });
    expect(ownerBadge(session)).toEqual({ clientId: "device-a", revision: 2 });
    expect(requiresManualRefresh(session)).toBe(false);
  });

  it("forces a safe refresh when host epochs leave last-sender state stale", () => {
    const session = connectedSession();
    admitClientInput(session, input());
    applyHostProjection(session, {
      taskId: "tab:a",
      revision: 40,
      turnEpoch: 3,
      focusEpoch: 2,
    });
    expect(visibleController(session)).toBeNull();
    expect(ownerBadge(session)).toBeNull();
    expect(requiresManualRefresh(session)).toBe(true);
    expect(
      admitClientInput(
        session,
        input({ turnEpoch: 1, focusEpoch: 1, commandId: "cmd-stale" }),
      ),
    ).toEqual({ kind: "staleTurn" });
  });

  it("clears stale refresh only after an authoritative sender matches host epochs", () => {
    const session = connectedSession();
    admitClientInput(session, input());
    applyHostProjection(session, {
      taskId: "tab:a",
      revision: 40,
      turnEpoch: 3,
      focusEpoch: 2,
    });
    observeAuthoritativeSender(session, "device-b", 2_000);
    expect(requiresManualRefresh(session)).toBe(false);
    expect(visibleController(session)?.clientId).toBe("device-b");
    expect(ownerBadge(session)).toEqual({ clientId: "device-b", revision: 40 });
  });
});

describe("duplicate and stale input protection", () => {
  it("returns the original operation for a duplicate command id", () => {
    const session = connectedSession();
    expect(admitClientInput(session, input())).toMatchObject({
      kind: "acceptedDurable",
      operationId: "op-1",
    });
    expect(
      admitClientInput(
        session,
        input({ operationId: "op-duplicate", inputSequence: 2 }),
      ),
    ).toEqual({ kind: "duplicate", settled: false, operationId: "op-1" });
    expect(reconcileEcho(session, "cmd-1")).toBe("op-1");
    expect(session.revision).toBe(2);
  });

  it("rejects stale focus without mutating accepted work", () => {
    const session = connectedSession();
    admitClientInput(session, input());
    expect(
      admitClientInput(
        session,
        input({ commandId: "cmd-2", focusEpoch: 9, operationId: "op-2" }),
      ),
    ).toEqual({ kind: "staleFocus" });
    expect(reconcileEcho(session, "cmd-2")).toBeUndefined();
  });

  it("accepts the first matching answer and rejects duplicates", () => {
    const session = createConnectClientSession("tab:a");
    connectClient(session, "desktop");
    connectClient(session, "phone");
    openRequest(session, "req-1", 2);
    expect(
      answerClientRequest(session, {
        taskId: "tab:a",
        clientId: "desktop",
        requestId: "req-1",
        actionEpoch: 2,
        runtimeGeneration: 1,
      }),
    ).toEqual({ kind: "answerAccepted", requestId: "req-1" });
    expect(
      answerClientRequest(session, {
        taskId: "tab:a",
        clientId: "phone",
        requestId: "req-1",
        actionEpoch: 2,
        runtimeGeneration: 1,
      }),
    ).toEqual({ kind: "alreadyResolved" });
  });
});
