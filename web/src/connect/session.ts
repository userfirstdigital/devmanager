export type TurnEpoch = number;
export type FocusEpoch = number;

export interface LastSenderHint {
  taskId: string;
  clientId: string;
  observedAtMs: number;
  turnEpoch: TurnEpoch;
  focusEpoch: FocusEpoch;
}

export interface DeviceInput {
  taskId: string;
  clientId: string;
  commandId: string;
  operationId: string;
  expectedRevision: number;
  inputSequence: number;
  turnEpoch: TurnEpoch;
  focusEpoch: FocusEpoch;
  observedAtMs: number;
}

export type SessionAdmitResult =
  | { kind: "acceptedDurable"; settled: false; operationId: string }
  | { kind: "duplicate"; settled: false; operationId: string }
  | { kind: "answerAccepted"; requestId: string }
  | {
      kind:
        | "staleTurn"
        | "staleFocus"
        | "alreadyResolved"
        | "staleAction"
        | "clientDisconnected"
        | "noOutstandingRequest";
    };

export interface ActionAnswer {
  taskId: string;
  clientId: string;
  requestId: string;
  actionEpoch: number;
  runtimeGeneration: number;
}

export interface ConnectClientSession {
  taskId: string;
  revision: number;
  turnEpoch: TurnEpoch;
  focusEpoch: FocusEpoch;
  runtimeGeneration: number;
  lastSender: LastSenderHint | null;
  accepted: Map<string, string>;
  connected: Set<string>;
  outstanding: Map<string, number>;
  settled: Set<string>;
}

export function createConnectClientSession(taskId: string): ConnectClientSession {
  return {
    taskId,
    revision: 1,
    turnEpoch: 1,
    focusEpoch: 1,
    runtimeGeneration: 1,
    lastSender: null,
    accepted: new Map(),
    connected: new Set(),
    outstanding: new Map(),
    settled: new Set(),
  };
}

export function visibleController(_session: ConnectClientSession): null {
  return null;
}

export function ownerBadge(_session: ConnectClientSession): null {
  return null;
}

export function connectClient(
  session: ConnectClientSession,
  clientId: string,
): void {
  session.connected.add(clientId);
}

export function openRequest(
  session: ConnectClientSession,
  requestId: string,
  actionEpoch: number,
): void {
  session.outstanding.set(requestId, actionEpoch);
}

export function admitClientInput(
  session: ConnectClientSession,
  input: DeviceInput,
): SessionAdmitResult {
  if (!session.connected.has(input.clientId)) {
    return { kind: "clientDisconnected" };
  }
  if (input.turnEpoch !== session.turnEpoch) return { kind: "staleTurn" };
  if (input.focusEpoch !== session.focusEpoch) return { kind: "staleFocus" };
  const existing = session.accepted.get(input.commandId);
  if (existing) {
    return { kind: "duplicate", settled: false, operationId: existing };
  }
  if (session.lastSender && session.lastSender.clientId !== input.clientId) {
    session.turnEpoch += 1;
  }
  session.accepted.set(input.commandId, input.operationId);
  session.revision += 1;
  session.lastSender = {
    taskId: session.taskId,
    clientId: input.clientId,
    observedAtMs: input.observedAtMs,
    turnEpoch: session.turnEpoch,
    focusEpoch: session.focusEpoch,
  };
  return {
    kind: "acceptedDurable",
    settled: false,
    operationId: input.operationId,
  };
}

export function answerClientRequest(
  session: ConnectClientSession,
  answer: ActionAnswer,
): SessionAdmitResult {
  if (answer.taskId !== session.taskId) return { kind: "staleAction" };
  if (!session.connected.has(answer.clientId)) {
    return { kind: "clientDisconnected" };
  }
  if (answer.runtimeGeneration !== session.runtimeGeneration) {
    return { kind: "staleAction" };
  }
  if (session.settled.has(answer.requestId)) {
    return { kind: "alreadyResolved" };
  }
  const expected = session.outstanding.get(answer.requestId);
  if (expected === undefined) return { kind: "noOutstandingRequest" };
  if (expected !== answer.actionEpoch) return { kind: "staleAction" };
  session.outstanding.delete(answer.requestId);
  session.settled.add(answer.requestId);
  return { kind: "answerAccepted", requestId: answer.requestId };
}

export function reconcileEcho(
  session: ConnectClientSession,
  commandId: string,
): string | undefined {
  return session.accepted.get(commandId);
}

export function requiresManualRefresh(_session: ConnectClientSession): false {
  return false;
}
