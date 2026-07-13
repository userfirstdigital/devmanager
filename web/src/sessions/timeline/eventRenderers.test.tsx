// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vitest";

import type { SemanticEvent } from "../../api/types";
import { EventRenderer, visibleEventsForDensity } from "./eventRenderers";

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

const question = event("question", {
  question_id: "question-1",
  prompt: "Continue with the migration?",
  choices: ["Continue", "Cancel"],
});

afterEach(cleanup);

describe("native semantic event rendering", () => {
  it("collapses successful tools but expands questions in calm density", () => {
    const { rerender } = render(<EventRenderer density="calm" event={successfulTool} />);
    expect(screen.getByRole("button", { name: /read/i }).getAttribute("aria-expanded")).toBe("false");

    rerender(<EventRenderer density="calm" event={question} />);
    expect(screen.getByText("Continue with the migration?").isConnected).toBe(true);
    expect(screen.getByText("Continue").isConnected).toBe(true);
  });

  it("lets the user expand a compact tool without rendering HTML", async () => {
    const user = userEvent.setup();
    render(<EventRenderer density="calm" event={successfulTool} />);

    await user.click(screen.getByRole("button", { name: /read/i }));

    expect(screen.getByText("Read package.json").isConnected).toBe(true);
  });

  it("keeps prose in minimal mode and exposes all details in full mode", () => {
    const prose = event("assistantMessage", {
      message_id: "message-1",
      text: "The build is ready.",
      streaming: false,
    });
    const output = event("output", { stream: "stdout", text: "compiled" });

    expect(visibleEventsForDensity([prose, output], "minimal")).toEqual([prose]);

    const { container } = render(<EventRenderer density="full" event={output} />);
    expect(container.textContent).toContain("compiled");
  });
});
