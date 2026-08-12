import { describe, expect, it } from "vitest";

import {
  admitClientInput,
  createConnectClientSession,
  ownerBadge,
  reconcileEcho,
  requiresManualRefresh,
  visibleController,
} from "./session";

describe("connect session handoff", () => {
  it("alternates desktop and phone without a visible owner badge or refresh", () => {
    const session = createConnectClientSession("task-1");
    const desktop = admitClientInput(session, {
      taskId: "task-1",
      clientId: "desktop",
      commandId: "c1",
      operationId: "o1",
      expectedRevision: 1,
      inputSequence: 1,
      turnEpoch: 1,
      focusEpoch: 1,
      observedAtMs: 1_000,
    });
    expect(desktop).toEqual({
      kind: "acceptedDurable",
      settled: false,
      operationId: "o1",
    });
    const phone = admitClientInput(session, {
      taskId: "task-1",
      clientId: "phone",
      commandId: "c2",
      operationId: "o2",
      expectedRevision: 2,
      inputSequence: 1,
      turnEpoch: session.turnEpoch,
      focusEpoch: session.focusEpoch,
      observedAtMs: 1_000 + 5 * 60 * 1000,
    });
    expect(phone).toMatchObject({ kind: "acceptedDurable", settled: false });
    expect(visibleController(session)).toBeNull();
    expect(ownerBadge(session)).toBeNull();
    expect(requiresManualRefresh(session)).toBe(false);
    expect(session.lastSender?.clientId).toBe("phone");
    expect(session.lastSender?.turnEpoch).toBe(session.turnEpoch);
    expect(reconcileEcho(session, "c1")).toBe("o1");
  });

  it("rejects stale focus and never treats receipt as settlement", () => {
    const session = createConnectClientSession("task-2");
    session.focusEpoch = 2;
    expect(
      admitClientInput(session, {
        taskId: "task-2",
        clientId: "desktop",
        commandId: "c3",
        operationId: "o3",
        expectedRevision: 1,
        inputSequence: 1,
        turnEpoch: 1,
        focusEpoch: 1,
        observedAtMs: 2,
      }),
    ).toEqual({ kind: "staleFocus" });
  });
});
