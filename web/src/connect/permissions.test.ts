import { describe, expect, it } from "vitest";
import {
  canPerform,
  collaborationUiVisible,
  deriveComposerMode,
  roleFromHostGrant,
  type CapabilityGrant,
} from "./permissions";

describe("connect permissions", () => {
  const watcher: CapabilityGrant = {
    role: "watcher",
    taskId: "task-1",
    actions: ["readTask", "readPresence"],
  };
  const collaborator: CapabilityGrant = {
    role: "collaborator",
    taskId: "task-1",
    actions: ["readTask", "sendPrompt", "mutateTask"],
  };
  const owner: CapabilityGrant = {
    role: "owner",
    taskId: "task-1",
    actions: ["readTask", "sendPrompt", "approveDangerous", "readPersonalPrompts"],
  };

  it("hides collaboration UI until an invite exists", () => {
    expect(collaborationUiVisible(0)).toBe(false);
    expect(collaborationUiVisible(1)).toBe(true);
  });

  it("derives composer mode from host-authoritative grants", () => {
    expect(deriveComposerMode(null)).toBe("hidden");
    expect(deriveComposerMode(watcher)).toBe("hidden");
    expect(deriveComposerMode(collaborator)).toBe("enabled");
    expect(deriveComposerMode(owner)).toBe("enabled");
  });

  it("denies owner-only and ungranted actions", () => {
    expect(canPerform(null, "readTask")).toBe(false);
    expect(canPerform(watcher, "sendPrompt")).toBe(false);
    expect(canPerform(collaborator, "approveDangerous")).toBe(false);
    expect(canPerform(owner, "approveDangerous")).toBe(true);
    expect(roleFromHostGrant(watcher)).toBe("watcher");
  });
});
