import { describe, expect, it } from "vitest";

import { sanitizeConnectPush } from "./push";

describe("connect push", () => {
  it("keeps only opaque ids, attention, optional safe title, timestamp, and route", () => {
    expect(
      sanitizeConnectPush(
        {
          hostId: "host-1",
          taskId: "task-1",
          attentionKind: "needsInput",
          timestampMs: 12,
          route: "/connect/tasks/task-1",
          safeTitle: "Needs input",
        },
        true,
      ),
    ).toEqual({
      hostId: "host-1",
      taskId: "task-1",
      attentionKind: "needsInput",
      timestampMs: 12,
      route: "/connect/tasks/task-1",
      safeTitle: "Needs input",
    });
  });

  it("drops raw transcript or prompt-bearing payloads", () => {
    expect(
      sanitizeConnectPush(
        {
          hostId: "host-1",
          taskId: "task-1",
          attentionKind: "completed",
          timestampMs: 12,
          route: "/connect/tasks/task-1",
          body: "RAW_TRANSCRIPT",
        },
        false,
      ),
    ).toBeNull();
    expect(
      sanitizeConnectPush(
        {
          hostId: "host-1",
          taskId: "task-1",
          attentionKind: "completed",
          timestampMs: 12,
          route: "/connect/tasks/task-1?prompt=HELLO",
        },
        false,
      ),
    ).toBeNull();
  });
});
