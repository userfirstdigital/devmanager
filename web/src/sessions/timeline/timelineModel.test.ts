import { describe, expect, it } from "vitest";

import type { SemanticEvent } from "../../api/types";
import { buildConversationItems } from "./timelineModel";

function event(
  sequence: number,
  kind: SemanticEvent["kind"],
  detail: Record<string, unknown> = {},
): SemanticEvent {
  return {
    ...detail,
    stableSessionKey: "tab:codex",
    sequence,
    occurredAtEpochMs: Date.UTC(2026, 6, 14, 12, 0, sequence),
    source: "codex",
    kind,
  } as SemanticEvent;
}

describe("native conversation presentation", () => {
  it("replaces a streaming assistant message in place and hides routine lifecycle events", () => {
    const items = buildConversationItems([
      event(1, "status", { state: "Starting", detail: null }),
      event(2, "userMessage", { text: "Fix the monitor" }),
      event(3, "assistantMessage", {
        message_id: "answer-1",
        text: "I am checking",
        streaming: true,
      }),
      event(4, "terminalMode", { raw_required: false }),
      event(5, "assistantMessage", {
        message_id: "answer-1",
        text: "I fixed the monitor.",
        streaming: false,
      }),
      event(6, "status", { state: "Running", detail: null }),
    ], "calm");

    expect(items).toMatchObject([
      {
        kind: "message",
        role: "user",
        text: "Fix the monitor",
      },
      {
        kind: "message",
        role: "assistant",
        text: "I fixed the monitor.",
        streaming: false,
      },
    ]);
    expect(items).toHaveLength(2);
  });

  it("groups adjacent activity and replaces tool state updates by tool id", () => {
    const items = buildConversationItems([
      event(1, "userMessage", { text: "Build it" }),
      event(2, "reasoning", { item_id: "reason-1", summary: "Inspecting" }),
      event(3, "tool", {
        tool_id: "tool-1",
        name: "Read",
        state: "running",
        summary: "Reading package.json",
      }),
      event(4, "tool", {
        tool_id: "tool-1",
        name: "Read",
        state: "completed",
        summary: "Read package.json",
      }),
      event(5, "diff", { item_id: "diff-1", unified_diff: "+ fixed" }),
      event(6, "assistantMessage", {
        message_id: "answer-1",
        text: "Built.",
        streaming: false,
      }),
    ], "calm");

    expect(items).toHaveLength(3);
    expect(items[1]).toMatchObject({
      kind: "activity",
      count: 3,
      state: "success",
    });
    if (items[1]?.kind !== "activity") throw new Error("activity missing");
    expect(items[1].events).toHaveLength(3);
    expect(items[1].summary).toBe("3 actions · Thinking · Read · Code changes");
  });

  it("keeps failures and questions top-level while minimal mode omits successful activity", () => {
    const events = [
      event(1, "tool", {
        tool_id: "tool-1",
        name: "Read",
        state: "completed",
        summary: "Done",
      }),
      event(2, "error", { message: "Bridge disconnected" }),
      event(3, "question", {
        question_id: "q-1",
        prompt: "Continue?",
        choices: ["Yes", "No"],
      }),
    ];

    expect(buildConversationItems(events, "minimal").map((item) => item.kind)).toEqual([
      "error",
      "question",
    ]);
  });

  it("bounds and coalesces degraded terminal output into one fallback block", () => {
    const oversized = "x".repeat(20_000);
    const items = buildConversationItems([
      event(1, "output", { stream: "stdout", text: oversized }),
      event(2, "output", { stream: "stdout", text: "final screen" }),
    ], "calm");

    expect(items).toHaveLength(1);
    expect(items[0]?.kind).toBe("fallbackOutput");
    if (items[0]?.kind !== "fallbackOutput") throw new Error("fallback missing");
    expect(items[0].text.length).toBeLessThanOrEqual(12_000);
    expect(items[0].text).toContain("final screen");
  });
});
