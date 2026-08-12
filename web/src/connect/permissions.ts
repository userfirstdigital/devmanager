export type ConnectRole = "owner" | "watcher" | "collaborator";

export type ConnectAction =
  | "readTask"
  | "readPresence"
  | "mutateTask"
  | "sendPrompt"
  | "answerRequest"
  | "approveDangerous"
  | "readPersonalPrompts";

export interface CapabilityGrant {
  role: ConnectRole;
  taskId: string;
  actions: readonly ConnectAction[];
}

export type ComposerMode = "hidden" | "disabled" | "enabled";

export type ConnectConnectionKind =
  | "idle"
  | "connecting"
  | "open"
  | "closed"
  | "unauthorized";

export type ConnectUiGate =
  | { kind: "allowed" }
  | {
      kind: "denied";
      reason: "watcher" | "missingGrant" | "roleDenied" | "missingAction";
    }
  | { kind: "disabled"; reason: "reconnecting" | "unauthorized" };

export const PAIRED_OWNER_ACTIONS: readonly ConnectAction[] = [
  "readTask",
  "readPresence",
  "mutateTask",
  "sendPrompt",
  "answerRequest",
  "approveDangerous",
  "readPersonalPrompts",
];

export function deriveComposerMode(grant: CapabilityGrant | null): ComposerMode {
  if (!grant || grant.role === "watcher") return "hidden";
  if (grant.role === "collaborator" && grant.actions.includes("sendPrompt")) {
    return "enabled";
  }
  if (grant.role === "owner" && grant.actions.includes("sendPrompt")) {
    return "enabled";
  }
  return "disabled";
}

export function canPerform(
  grant: CapabilityGrant | null,
  action: ConnectAction,
): boolean {
  if (!grant) return false;
  if (action === "approveDangerous" || action === "readPersonalPrompts") {
    return grant.role === "owner" && grant.actions.includes(action);
  }
  if (grant.role === "watcher") {
    return (
      (action === "readTask" || action === "readPresence") &&
      grant.actions.includes(action)
    );
  }
  if (grant.role === "collaborator") {
    return grant.actions.includes(action);
  }
  return grant.actions.includes(action);
}

export function collaborationUiVisible(inviteCount: number): boolean {
  return inviteCount > 0;
}

/** Host-authoritative role labels only; never invent owner from local state. */
export function roleFromHostGrant(grant: CapabilityGrant | null): ConnectRole | null {
  return grant?.role ?? null;
}

export function canUseOwnerControls(grant: CapabilityGrant | null): boolean {
  return grant?.role === "owner";
}

export function resolveCapabilityGrant(input: {
  statusKind: ConnectConnectionKind;
  taskId: string;
  grant?: CapabilityGrant | null;
}): CapabilityGrant | null {
  if (input.statusKind === "unauthorized") return null;
  if (input.grant) return input.grant;
  return null;
}

export function deriveConnectUiGate(input: {
  grant: CapabilityGrant | null;
  action: ConnectAction;
  statusKind: ConnectConnectionKind;
}): ConnectUiGate {
  if (input.statusKind === "unauthorized") {
    return { kind: "disabled", reason: "unauthorized" };
  }
  if (!input.grant) {
    return { kind: "denied", reason: "missingGrant" };
  }
  if (!canPerform(input.grant, input.action)) {
    if (input.grant.role === "watcher") {
      return { kind: "denied", reason: "watcher" };
    }
    if (
      input.action === "approveDangerous" ||
      input.action === "readPersonalPrompts"
    ) {
      return { kind: "denied", reason: "roleDenied" };
    }
    return { kind: "denied", reason: "missingAction" };
  }
  if (input.statusKind !== "open") {
    return { kind: "disabled", reason: "reconnecting" };
  }
  return { kind: "allowed" };
}
