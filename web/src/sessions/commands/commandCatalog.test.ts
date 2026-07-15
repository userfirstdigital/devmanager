import { describe, expect, it } from "vitest";

import {
  commandsForProvider,
  filterCommandCatalog,
  mergeCommandCatalog,
  replaceLeadingSlashToken,
} from "./commandCatalog";
import type { DiscoveredSlashCommand, SlashCommand } from "./types";

describe("provider slash command catalog", () => {
  it("keeps complete reviewed Claude and Codex built-ins separate", () => {
    const claude = commandsForProvider("claude");
    const codex = commandsForProvider("codex");

    expect(claude.length).toBeGreaterThanOrEqual(99);
    expect(codex).toHaveLength(50);
    expect(claude.some((command) => command.name === "/compact")).toBe(true);
    expect(claude.some((command) => command.name === "/permissions")).toBe(true);
    expect(claude.some((command) => command.name === "/advisor")).toBe(true);
    expect(codex.some((command) => command.name === "/permissions")).toBe(true);
    expect(codex.some((command) => command.name === "/debug-config")).toBe(true);
    expect(codex.some((command) => command.name === "/pets")).toBe(true);
    expect(claude.some((command) => command.name === "/debug-config")).toBe(false);
    expect(codex.some((command) => command.name === "/advisor")).toBe(false);
    expect(new Set(claude.map((command) => command.name)).size).toBe(claude.length);
    expect(new Set(codex.map((command) => command.name)).size).toBe(codex.length);
  });

  it("keeps stable argument suggestions and provider-owned interaction metadata", () => {
    const claude = commandsForProvider("claude");
    const codex = commandsForProvider("codex");

    expect(claude.find((command) => command.name === "/fast")).toMatchObject({
      argumentHint: "on | off",
      suggestions: [
        { label: "On", value: "on" },
        { label: "Off", value: "off" },
      ],
    });
    expect(claude.find((command) => command.name === "/model")?.interaction).toBe(
      "providerMenu",
    );
    expect(codex.find((command) => command.name === "/model")?.interaction).toBe(
      "providerMenu",
    );
    expect(codex.find((command) => command.name === "/status")?.interaction).toBe(
      "providerMenu",
    );
    expect(codex.find((command) => command.name === "/compact")?.interaction).toBe(
      "inline",
    );
  });

  it("merges custom metadata with project-first provider precedence", () => {
    const builtin: SlashCommand[] = [
      {
        name: "/review",
        description: "Built in review",
        provider: "claude",
        source: "builtin",
        category: "workflow",
        argumentHint: null,
        suggestions: [],
        aliases: [],
        interaction: "inline",
      },
    ];
    const discovered: DiscoveredSlashCommand[] = [
      { name: "/review", description: "Plugin review", source: "plugin" },
      { name: "/review", description: "Personal review", source: "personal" },
      { name: "/review", description: "Project review", source: "project" },
      { name: "/deploy", description: "Deploy safely", source: "project" },
      { name: "not/a/command", description: "Ignored", source: "project" },
    ];

    expect(mergeCommandCatalog("claude", builtin, discovered)).toMatchObject([
      { name: "/deploy", source: "project", description: "Deploy safely" },
      { name: "/review", source: "project", description: "Project review" },
    ]);
  });

  it("filters the leading slash token by name alias description and category", () => {
    const commands = commandsForProvider("claude");

    expect(filterCommandCatalog(commands, "/mod")[0].command.name).toBe("/model");
    expect(
      filterCommandCatalog(commands, "/allowed").some(
        (match) => match.command.name === "/permissions",
      ),
    ).toBe(true);
    expect(
      filterCommandCatalog(commands, "/conversation").some(
        (match) => match.command.name === "/compact",
      ),
    ).toBe(true);
    expect(filterCommandCatalog(commands, "please /model")).toEqual([]);
    expect(filterCommandCatalog(commands, "/model sonnet")).toEqual([]);
  });

  it("preserves arguments when replacing only the leading command token", () => {
    expect(replaceLeadingSlashToken("/mod keep this", "/model")).toBe(
      "/model keep this",
    );
    expect(replaceLeadingSlashToken("/", "/compact")).toBe("/compact");
    expect(replaceLeadingSlashToken("ordinary text", "/model")).toBe(
      "ordinary text",
    );
  });

  it("bounds broad results while retaining deterministic score ordering", () => {
    const commands = commandsForProvider("claude");
    const first = filterCommandCatalog(commands, "/", 12);
    const second = filterCommandCatalog(commands, "/", 12);

    expect(first).toHaveLength(12);
    expect(second).toEqual(first);
    expect(first.every((match) => match.command.name.startsWith("/"))).toBe(true);
  });
});
