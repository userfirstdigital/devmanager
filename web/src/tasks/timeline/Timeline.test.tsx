// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { SemanticEvent } from "../../api/types";
import { preserveTimelineAnchor, Timeline } from "./Timeline";

function event(
  sequence: number,
  kind: SemanticEvent["kind"],
  detail: Record<string, unknown> = {},
): SemanticEvent {
  return {
    ...detail,
    stableSessionKey: "tab:claude",
    sequence,
    occurredAtEpochMs: Date.UTC(2026, 6, 14, 12, 0, sequence),
    source: "claude",
    kind,
  } as SemanticEvent;
}

afterEach(cleanup);

describe("timeline scroll anchoring", () => {
  it("keeps the same visible event fixed when replay inserts content above it", () => {
    expect(
      preserveTimelineAnchor({
        scrollTop: 620,
        previousAnchorOffset: 24,
        nextAnchorOffset: 184,
      }),
    ).toBe(780);
  });

  it("never creates a negative scroll position", () => {
    expect(
      preserveTimelineAnchor({
        scrollTop: 12,
        previousAnchorOffset: 90,
        nextAnchorOffset: 10,
      }),
    ).toBe(0);
  });
});

describe("actionable question choices", () => {
  it("disables answered historical questions and older questions while the latest unanswered can submit", async () => {
    const user = userEvent.setup();
    const onQuestionChoice = vi.fn();
    const events = [
      event(1, "question", {
        question_id: "q-old",
        prompt: "Older question?",
        choices: ["Old yes", "Old no"],
      }),
      event(2, "userMessage", { text: "Old yes" }),
      event(3, "question", {
        question_id: "q-answered",
        prompt: "Answered question?",
        choices: ["Answered yes", "Answered no"],
      }),
      event(4, "userMessage", { text: "Answered yes" }),
      event(5, "question", {
        question_id: "q-current",
        prompt: "Current question?",
        choices: ["Current yes", "Current no"],
      }),
    ];

    render(
      <Timeline
        events={events}
        density="calm"
        onQuestionChoice={onQuestionChoice}
      />,
    );

    expect((screen.getByRole("button", { name: "Old yes" }) as HTMLButtonElement).disabled).toBe(
      true,
    );
    expect(
      (screen.getByRole("button", { name: "Answered yes" }) as HTMLButtonElement).disabled,
    ).toBe(true);
    expect(
      (screen.getByRole("button", { name: "Current yes" }) as HTMLButtonElement).disabled,
    ).toBe(false);

    await user.click(screen.getByRole("button", { name: "Current yes" }));
    expect(onQuestionChoice).toHaveBeenCalledTimes(1);
    expect(onQuestionChoice).toHaveBeenCalledWith("Current yes");

    await user.click(screen.getByRole("button", { name: "Old yes" }));
    expect(onQuestionChoice).toHaveBeenCalledTimes(1);
  });

  it("disables every question when global choices are disabled", () => {
    render(
      <Timeline
        events={[
          event(1, "question", {
            question_id: "q-current",
            prompt: "Current?",
            choices: ["Go"],
          }),
        ]}
        density="calm"
        questionChoicesDisabled
        onQuestionChoice={() => {}}
      />,
    );
    expect((screen.getByRole("button", { name: "Go" }) as HTMLButtonElement).disabled).toBe(true);
  });
});
