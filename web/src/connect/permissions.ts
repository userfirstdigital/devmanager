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
