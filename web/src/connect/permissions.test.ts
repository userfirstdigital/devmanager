import { describe, expect, it } from "vitest";

import {
  canPerform,
  collaborationUiVisible,
  deriveComposerMode,
} from "./permissions";

describe("connect permissions", () => {
  it("hides collaboration until an invite exists", () => {
    expect(collaborationUiVisible(0)).toBe(false);
    expect(collaborationUiVisible(1)).toBe(true);
  });

  it("keeps watchers read-only and owner-only actions off guests", () => {
    const watcher = {
      role: "watcher" as const,
      taskId: "task-1",
      actions: ["readTask", "readPresence"] as const,
    };
    expect(deriveComposerMode(watcher)).toBe("hidden");
    expect(canPerform(watcher, "mutateTask")).toBe(false);
    expect(canPerform(watcher, "approveDangerous")).toBe(false);
    expect(canPerform(watcher, "readPersonalPrompts")).toBe(false);

    const collaborator = {
      role: "collaborator" as const,
      taskId: "task-1",
      actions: ["sendPrompt", "mutateTask"] as const,
    };
    expect(deriveComposerMode(collaborator)).toBe("enabled");
    expect(canPerform(collaborator, "approveDangerous")).toBe(false);
    expect(canPerform(collaborator, "readPersonalPrompts")).toBe(false);
  });
});
