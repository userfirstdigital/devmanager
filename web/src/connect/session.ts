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
  | { kind: "staleTurn" | "staleFocus" | "alreadyResolved" | "staleAction" };

export interface ConnectClientSession {
  taskId: string;
  revision: number;
  turnEpoch: TurnEpoch;
  focusEpoch: FocusEpoch;
  lastSender: LastSenderHint | null;
  accepted: Map<string, string>;
}

export function createConnectClientSession(taskId: string): ConnectClientSession {
  return {
    taskId,
    revision: 1,
    turnEpoch: 1,
    focusEpoch: 1,
    lastSender: null,
    accepted: new Map(),
  };
}

export function visibleController(_session: ConnectClientSession): null {
  return null;
}

export function ownerBadge(_session: ConnectClientSession): null {
  return null;
}

export function admitClientInput(
  session: ConnectClientSession,
  input: DeviceInput,
): SessionAdmitResult {
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

export function reconcileEcho(
  session: ConnectClientSession,
  commandId: string,
): string | undefined {
  return session.accepted.get(commandId);
}

export function requiresManualRefresh(_session: ConnectClientSession): false {
  return false;
}
