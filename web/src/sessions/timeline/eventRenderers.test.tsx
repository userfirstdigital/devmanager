// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { SemanticEvent } from "../../api/types";
import { ConversationItemRenderer } from "./eventRenderers";
import { buildConversationItems } from "./timelineModel";

function event(kind: SemanticEvent["kind"], detail: Record<string, unknown> = {}): SemanticEvent {
  return {
    ...detail,
    stableSessionKey: "tab:a",
    sequence: 1,
    occurredAtEpochMs: Date.UTC(2026, 6, 13),
    source: "claude",
    kind,
  } as unknown as SemanticEvent;
}

const successfulTool = event("tool", {
  tool_id: "tool-1",
  name: "Read",
  state: "completed",
  summary: "Read package.json",
});

const runningTool = event("tool", {
  tool_id: "tool-2",
  name: "Bash",
  state: "running",
  summary: "Running tests",
});

const failedTool = event("tool", {
  tool_id: "tool-3",
  name: "Edit",
  state: "failed",
  summary: "Could not patch",
});

const question = event("question", {
  question_id: "question-1",
  prompt: "Continue with the migration?",
  choices: ["Continue", "Cancel"],
});

afterEach(cleanup);

describe("native semantic event rendering", () => {
  it("collapses grouped successful activity but keeps questions expanded", () => {
    const activity = buildConversationItems([successfulTool], "calm")[0];
    const questionItem = buildConversationItems([question], "calm")[0];
    if (!activity || !questionItem) throw new Error("fixture item missing");

    const { rerender } = render(
      <ConversationItemRenderer density="calm" item={activity} />,
    );
    expect(screen.getByRole("button", { name: /1 action/i }).getAttribute("aria-expanded")).toBe("false");

    rerender(<ConversationItemRenderer density="calm" item={questionItem} />);
    expect(screen.getByText("Continue with the migration?").isConnected).toBe(true);
    expect(screen.getByRole("button", { name: "Continue" }).isConnected).toBe(true);
  });

  it("keeps active activity collapsed in calm mode and opens failures", () => {
    const active = buildConversationItems([runningTool], "calm")[0];
    const failed = buildConversationItems([failedTool], "calm")[0];
    if (!active || !failed) throw new Error("fixture item missing");

    const { rerender } = render(
      <ConversationItemRenderer density="calm" item={active} />,
    );
    expect(screen.getByRole("button", { name: /1 action/i }).getAttribute("aria-expanded")).toBe(
      "false",
    );

    rerender(<ConversationItemRenderer density="calm" item={failed} />);
    expect(screen.getByRole("button", { name: /1 action/i }).getAttribute("aria-expanded")).toBe(
      "true",
    );
  });

  it("expands active activity in full density", () => {
    const active = buildConversationItems([runningTool], "full")[0];
    if (!active) throw new Error("activity missing");
    render(<ConversationItemRenderer density="full" item={active} />);
    expect(screen.getByRole("button", { name: /1 action/i }).getAttribute("aria-expanded")).toBe(
      "true",
    );
  });

  it("submits question choices through the optional callback", async () => {
    const user = userEvent.setup();
    const onChoice = vi.fn();
    const questionItem = buildConversationItems([question], "calm")[0];
    if (!questionItem) throw new Error("question missing");

    const { rerender } = render(
      <ConversationItemRenderer
        density="calm"
        item={questionItem}
        onQuestionChoice={onChoice}
        questionChoicesDisabled
      />,
    );
    expect((screen.getByRole("button", { name: "Continue" }) as HTMLButtonElement).disabled).toBe(
      true,
    );

    rerender(
      <ConversationItemRenderer
        density="calm"
        item={questionItem}
        onQuestionChoice={onChoice}
      />,
    );
    await user.click(screen.getByRole("button", { name: "Continue" }));
    expect(onChoice).toHaveBeenCalledWith("Continue");
  });

  it("lets the user expand grouped activity without rendering HTML", async () => {
    const user = userEvent.setup();
    const activity = buildConversationItems([successfulTool], "calm")[0];
    if (!activity) throw new Error("activity missing");
    render(<ConversationItemRenderer density="calm" item={activity} />);

    await user.click(screen.getByRole("button", { name: /1 action/i }));

    expect(screen.getByText("Read package.json").isConnected).toBe(true);
  });

  it("renders assistant Markdown directly and drops routine AI cards", () => {
    const prose = event("assistantMessage", {
      message_id: "message-1",
      text: "## Ready\n\nThe build is ready.",
      streaming: false,
    });
    const starting = event("status", { state: "Starting", detail: null });
    const restored = event("terminalMode", { raw_required: false });
    const items = buildConversationItems([starting, prose, restored], "calm");
    expect(items).toHaveLength(1);

    const { container } = render(
      <ConversationItemRenderer density="calm" item={items[0]!} />,
    );
    expect(screen.getByRole("heading", { name: "Ready" }).isConnected).toBe(true);
    expect(container.textContent).not.toMatch(/Starting|Native view restored|Output/);
  });
});
