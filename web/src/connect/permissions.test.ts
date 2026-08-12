import { describe, expect, it } from "vitest";

import {
  canPerform,
  canUseOwnerControls,
  collaborationUiVisible,
  deriveComposerMode,
  deriveConnectUiGate,
  resolveCapabilityGrant,
  roleFromHostGrant,
  type CapabilityGrant,
} from "./permissions";

const watcher: CapabilityGrant = {
  role: "watcher",
  taskId: "tab:a",
  actions: ["readTask", "readPresence"],
};

const collaborator: CapabilityGrant = {
  role: "collaborator",
  taskId: "tab:a",
  actions: ["readTask", "readPresence", "mutateTask", "sendPrompt"],
};

const owner: CapabilityGrant = {
  role: "owner",
  taskId: "tab:a",
  actions: [
    "readTask",
    "readPresence",
    "mutateTask",
    "sendPrompt",
    "answerRequest",
    "approveDangerous",
    "readPersonalPrompts",
  ],
};

describe("connect permission grants", () => {
  it("fail-closes without a grant and keeps watchers read-only", () => {
    expect(canPerform(null, "readTask")).toBe(false);
    expect(canPerform(watcher, "readTask")).toBe(true);
    expect(canPerform(watcher, "sendPrompt")).toBe(false);
    expect(canPerform(watcher, "mutateTask")).toBe(false);
    expect(canPerform(watcher, "approveDangerous")).toBe(false);
    expect(deriveComposerMode(watcher)).toBe("hidden");
    expect(canUseOwnerControls(watcher)).toBe(false);
    expect(roleFromHostGrant(watcher)).toBe("watcher");
  });

  it("lets collaborators send only when the grant includes the action", () => {
    expect(deriveComposerMode(collaborator)).toBe("enabled");
    expect(canPerform(collaborator, "sendPrompt")).toBe(true);
    expect(canPerform(collaborator, "approveDangerous")).toBe(false);
    expect(canUseOwnerControls(collaborator)).toBe(false);
    expect(
      deriveComposerMode({
        ...collaborator,
        actions: ["readTask", "readPresence", "mutateTask"],
      }),
    ).toBe("disabled");
  });

  it("reserves owner-only controls for an authoritative owner", () => {
    expect(canUseOwnerControls(owner)).toBe(true);
    expect(canPerform(owner, "approveDangerous")).toBe(true);
    expect(canPerform(owner, "readPersonalPrompts")).toBe(true);
    expect(deriveComposerMode(owner)).toBe("enabled");
  });
});

describe("connect UI gates", () => {
  it("surfaces denied and reconnecting instead of hiding the failure", () => {
    expect(
      deriveConnectUiGate({
        grant: watcher,
        action: "sendPrompt",
        statusKind: "open",
      }),
    ).toEqual({ kind: "denied", reason: "watcher" });
    expect(
      deriveConnectUiGate({
        grant: owner,
        action: "sendPrompt",
        statusKind: "closed",
      }),
    ).toEqual({ kind: "disabled", reason: "reconnecting" });
    expect(
      deriveConnectUiGate({
        grant: null,
        action: "sendPrompt",
        statusKind: "unauthorized",
      }),
    ).toEqual({ kind: "disabled", reason: "unauthorized" });
    expect(
      deriveConnectUiGate({
        grant: owner,
        action: "sendPrompt",
        statusKind: "open",
      }),
    ).toEqual({ kind: "allowed" });
  });

  it("requires an explicit host grant even for a paired browser", () => {
    expect(
      resolveCapabilityGrant({
        statusKind: "open",
        taskId: "tab:a",
      }),
    ).toBeNull();
    expect(
      resolveCapabilityGrant({
        statusKind: "unauthorized",
        taskId: "tab:a",
      }),
    ).toBeNull();
    expect(
      resolveCapabilityGrant({
        statusKind: "open",
        taskId: "tab:a",
        grant: watcher,
      }),
    ).toEqual(watcher);
    expect(collaborationUiVisible(0)).toBe(false);
    expect(collaborationUiVisible(2)).toBe(true);
  });
});
