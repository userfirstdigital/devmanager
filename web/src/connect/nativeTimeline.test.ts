import { describe, expect, it } from "vitest";

import type { SemanticJournalFact } from "./nativeProtocol";
import { buildNativeTimeline } from "./nativeTimeline";

function fact(
  sequence: number,
  payload: SemanticJournalFact["payload"],
): SemanticJournalFact {
  return {
    id: `fact-${sequence}`,
    sequence,
    occurredAtMs: sequence * 1000,
    provider: "native",
    schemaVersion: 1,
    kind: payload.kind,
    visibility: "local",
    privacyClass: "local_only",
    redacted: false,
    payload,
  };
}

describe("buildNativeTimeline", () => {
  it("groups adjacent assistant facts and reasoning without losing fact boundaries", () => {
    const timeline = buildNativeTimeline([
      fact(1, { kind: "assistant_text", text: "First " }),
      fact(2, { kind: "reasoning_summary", text: "Checked the task state." }),
      fact(3, { kind: "assistant_text", text: "answer" }),
    ]);

    expect(timeline).toEqual([
      {
        kind: "assistant",
        id: "assistant-fact-1",
        sequence: 1,
        messages: [
          { id: "fact-1", text: "First " },
          { id: "fact-3", text: "answer" },
        ],
        reasoning: ["Checked the task state."],
      },
    ]);
  });

  it("keeps user boundaries and folds adjacent tool facts into a compact detail item", () => {
    const timeline = buildNativeTimeline([
      fact(1, { kind: "user_message", text: "Please inspect this." }),
      fact(2, { kind: "tool_call", tool_name: "read_file", call_id: "call-1" }),
      fact(3, { kind: "tool_result", call_id: "call-1", status: "success" }),
      fact(4, { kind: "assistant_text", text: "It is ready." }),
    ]);

    expect(timeline).toEqual([
      {
        kind: "user",
        id: "user-fact-1",
        sequence: 1,
        text: "Please inspect this.",
      },
      {
        kind: "activity",
        id: "activity-fact-2",
        sequence: 2,
        title: "Tool activity",
        details: ["read_file", "read_file success"],
      },
      {
        kind: "assistant",
        id: "assistant-fact-4",
        sequence: 4,
        messages: [{ id: "fact-4", text: "It is ready." }],
        reasoning: [],
      },
    ]);
  });

  it("uses the latest stable fact-id upsert without concatenating stale partial text", () => {
    const initial = fact(1, { kind: "assistant_text", text: "partial" });
    const replacement = {
      ...fact(3, { kind: "assistant_text", text: "complete message" }),
      id: initial.id,
    };
    const timeline = buildNativeTimeline([
      initial,
      replacement,
      fact(2, { kind: "assistant_text", text: "separate message" }),
    ]);

    expect(timeline).toEqual([
      {
        kind: "assistant",
        id: "assistant-fact-2",
        sequence: 2,
        messages: [
          { id: "fact-2", text: "separate message" },
          { id: "fact-1", text: "complete message" },
        ],
        reasoning: [],
      },
    ]);
  });
});
