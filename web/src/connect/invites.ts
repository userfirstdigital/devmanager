export type InviteRole = "watcher" | "collaborator";
export type InviteUsePolicy = "singleUse" | "multiUse";

export interface TaskInvite {
  inviteId: string;
  taskId: string;
  nickname: string;
  role: InviteRole;
  usePolicy: InviteUsePolicy;
  expiresAtMs: number;
  revoked: boolean;
}

export function shouldShowCollaborationUi(invites: readonly TaskInvite[]): boolean {
  return invites.length > 0;
}

export function inviteIsLive(invite: TaskInvite, nowMs: number): boolean {
  return !invite.revoked && nowMs <= invite.expiresAtMs;
}
