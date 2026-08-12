import { describe, expect, it } from "vitest";
import {
  admitClientInput,
  answerClientRequest,
  connectClient,
  createConnectClientSession,
  openRequest,
  ownerBadge,
  requiresManualRefresh,
  visibleController,
} from "./session";

describe("connect client session", () => {
  it("never invents a controller lease from last-sender metadata", () => {
    const session = createConnectClientSession("task-1");
    connectClient(session, "desktop");
    const result = admitClientInput(session, {
      taskId: "task-1",
      clientId: "desktop",
      commandId: "cmd-1",
      operationId: "op-1",
      expectedRevision: 1,
      inputSequence: 1,
      turnEpoch: 1,
      focusEpoch: 1,
      observedAtMs: 10,
    });
    expect(result.kind).toBe("acceptedDurable");
    expect(visibleController(session)).toBeNull();
    expect(ownerBadge(session)).toBeNull();
    expect(requiresManualRefresh(session)).toBe(false);
  });

  it("accepts the first matching answer and rejects duplicates", () => {
    const session = createConnectClientSession("task-1");
    connectClient(session, "desktop");
    connectClient(session, "phone");
    openRequest(session, "req-1", 2);
    expect(
      answerClientRequest(session, {
        taskId: "task-1",
        clientId: "desktop",
        requestId: "req-1",
        actionEpoch: 2,
        runtimeGeneration: 1,
      }),
    ).toEqual({ kind: "answerAccepted", requestId: "req-1" });
    expect(
      answerClientRequest(session, {
        taskId: "task-1",
        clientId: "phone",
        requestId: "req-1",
        actionEpoch: 2,
        runtimeGeneration: 1,
      }),
    ).toEqual({ kind: "alreadyResolved" });
  });
});
