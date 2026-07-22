// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { SemanticEvent } from "../../api/types";
import { AiSessionView } from "./AiSessionView";

function event(
  sequence: number,
  kind: SemanticEvent["kind"],
  detail: Record<string, unknown> = {},
): SemanticEvent {
  return {
    ...detail,
    stableSessionKey: "tab:claude-a",
    sequence,
    occurredAtEpochMs: Date.UTC(2026, 6, 14, 12, 0, sequence),
    source: "claude",
    kind,
  } as SemanticEvent;
}

afterEach(cleanup);

describe("native AI session view", () => {
  it("does not show a permanent Interrupt strip for a live idle session", () => {
    render(
      <AiSessionView
        events={[]}
        density="calm"
        adapterHealth="healthy"
        running
        actionsDisabled={false}
        composer={<div>composer</div>}
        onRestart={() => {}}
      />,
    );

    expect(screen.queryByRole("button", { name: /interrupt/i })).toBeNull();
    expect(screen.queryByRole("button", { name: /reopen/i })).toBeNull();
  });

  it("shows compact Reopen only for an ended session", () => {
    render(
      <AiSessionView
        events={[]}
        density="calm"
        adapterHealth="healthy"
        running={false}
        actionsDisabled={false}
        composer={<div>composer</div>}
        onRestart={() => {}}
      />,
    );

    expect(screen.getByRole("button", { name: /reopen/i }).isConnected).toBe(true);
    expect(screen.queryByRole("button", { name: /interrupt/i })).toBeNull();
  });

  it("keeps the degraded adapter notice compact and non-alarming", () => {
    render(
      <AiSessionView
        events={[]}
        density="calm"
        adapterHealth="degraded"
        running
        actionsDisabled={false}
        composer={<div>composer</div>}
        onRestart={() => {}}
      />,
    );

    const notice = screen.getByRole("status");
    expect(notice.textContent).toMatch(/live text remains available/i);
    expect(notice.textContent).not.toMatch(/rich activity cards are temporarily simplified/i);
  });

  it("forwards question choices and disables them while actions are blocked", async () => {
    const user = userEvent.setup();
    const onQuestionChoice = vi.fn();
    const events = [
      event(1, "question", {
        question_id: "q-1",
        prompt: "Ship it?",
        choices: ["Yes", "No"],
      }),
    ];

    const { rerender } = render(
      <AiSessionView
        events={events}
        density="calm"
        adapterHealth="healthy"
        running
        actionsDisabled
        questionChoicesDisabled
        composer={<div>composer</div>}
        onRestart={() => {}}
        onQuestionChoice={onQuestionChoice}
      />,
    );
    expect((screen.getByRole("button", { name: "Yes" }) as HTMLButtonElement).disabled).toBe(true);

    rerender(
      <AiSessionView
        events={events}
        density="calm"
        adapterHealth="healthy"
        running
        actionsDisabled={false}
        composer={<div>composer</div>}
        onRestart={() => {}}
        onQuestionChoice={onQuestionChoice}
      />,
    );
    await user.click(screen.getByRole("button", { name: "Yes" }));
    expect(onQuestionChoice).toHaveBeenCalledWith("Yes");
  });
});
