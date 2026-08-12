import { describe, expect, it } from "vitest";
import { inviteIsLive, shouldShowCollaborationUi, type TaskInvite } from "./invites";

describe("connect invites", () => {
  const invite: TaskInvite = {
    inviteId: "inv-1",
    taskId: "task-1",
    nickname: "review",
    role: "watcher",
    usePolicy: "singleUse",
    expiresAtMs: 100,
    revoked: false,
  };

  it("keeps collaboration UI hidden without invites", () => {
    expect(shouldShowCollaborationUi([])).toBe(false);
    expect(shouldShowCollaborationUi([invite])).toBe(true);
  });

  it("treats expiry and revocation as not live", () => {
    expect(inviteIsLive(invite, 100)).toBe(false);
    expect(inviteIsLive(invite, 101)).toBe(false);
    expect(inviteIsLive({ ...invite, revoked: true }, 50)).toBe(false);
  });
});
