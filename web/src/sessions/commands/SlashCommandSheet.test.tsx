// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SlashCommandSheet } from "./SlashCommandSheet";
import type { SlashCommandMatch } from "./types";

afterEach(cleanup);

const matches: SlashCommandMatch[] = [
  {
    score: 1,
    command: {
      name: "/model",
      description: "Choose the active model.",
      provider: "codex",
      source: "builtin",
      category: "model",
      argumentHint: null,
      suggestions: [],
      aliases: [],
      interaction: "providerMenu",
    },
  },
  {
    score: 1,
    command: {
      name: "/project-check",
      description: "Check this project.",
      provider: "codex",
      source: "project",
      category: "custom",
      argumentHint: "optional arguments",
      suggestions: [],
      aliases: [],
      interaction: "inline",
    },
  },
];

describe("native slash command sheet", () => {
  it("renders one compact accessible list and supports touch selection", async () => {
    const user = userEvent.setup();
    const onSelect = vi.fn();
    render(
      <SlashCommandSheet
        provider="codex"
        matches={matches}
        activeIndex={0}
        loading={false}
        onActiveIndexChange={() => {}}
        onSelect={onSelect}
      />,
    );

    expect(screen.getByRole("listbox", { name: /codex commands/i })).toBeTruthy();
    expect(screen.getAllByRole("option")).toHaveLength(2);
    expect(screen.getByText("Project")).toBeTruthy();
    expect(screen.getByText(/2 commands/i)).toBeTruthy();

    await user.click(screen.getByRole("option", { name: /project-check/i }));
    expect(onSelect).toHaveBeenCalledWith(matches[1].command);
  });

  it("announces empty and loading states without creating card grids", () => {
    const { rerender } = render(
      <SlashCommandSheet
        provider="claude"
        matches={[]}
        activeIndex={0}
        loading
        onActiveIndexChange={() => {}}
        onSelect={() => {}}
      />,
    );
    expect(screen.getByText(/finding project commands/i)).toBeTruthy();

    rerender(
      <SlashCommandSheet
        provider="claude"
        matches={[]}
        activeIndex={0}
        loading={false}
        onActiveIndexChange={() => {}}
        onSelect={() => {}}
      />,
    );
    expect(screen.getByText(/no matching commands/i)).toBeTruthy();
    expect(document.querySelectorAll(".dm-slash-command-sheet")).toHaveLength(1);
  });
});
