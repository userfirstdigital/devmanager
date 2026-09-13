import type { WebAiKind } from "../../api/types";
import {
  CLAUDE_BUILTIN_COMMANDS,
  CODEX_BUILTIN_COMMANDS,
} from "./builtinCatalog";
import type {
  DiscoveredSlashCommand,
  SlashCommand,
  SlashCommandMatch,
  SlashCommandSource,
} from "./types";

const SAFE_COMMAND_NAME = /^\/[A-Za-z0-9_.:-]{1,128}$/;

const SOURCE_PRECEDENCE: Record<SlashCommandSource, number> = {
  builtin: 0,
  mcp: 1,
  plugin: 2,
  personal: 3,
  project: 4,
};

export function commandsForProvider(provider: WebAiKind): readonly SlashCommand[] {
  return provider === "claude" ? CLAUDE_BUILTIN_COMMANDS : CODEX_BUILTIN_COMMANDS;
}

export function mergeCommandCatalog(
  provider: WebAiKind,
  builtins: readonly SlashCommand[],
  discovered: readonly DiscoveredSlashCommand[],
): SlashCommand[] {
  const merged = new Map<string, SlashCommand>();
  for (const command of builtins) {
    if (command.provider !== provider || !SAFE_COMMAND_NAME.test(command.name)) continue;
    merged.set(command.name.toLowerCase(), command);
  }
  for (const command of discovered) {
    if (
      !SAFE_COMMAND_NAME.test(command.name) ||
      command.description.trim().length === 0 ||
      !Object.prototype.hasOwnProperty.call(SOURCE_PRECEDENCE, command.source)
    ) {
      continue;
    }
    const key = command.name.toLowerCase();
    const current = merged.get(key);
    if (
      current &&
      SOURCE_PRECEDENCE[current.source] > SOURCE_PRECEDENCE[command.source]
    ) {
      continue;
    }
    merged.set(key, {
      name: command.name,
      description: command.description.trim().slice(0, 240),
      provider,
      source: command.source,
      category: "custom",
      argumentHint: "optional arguments",
      suggestions: [],
      aliases: [],
      interaction: "inline",
    });
  }
  return [...merged.values()].sort((left, right) =>
    left.name.localeCompare(right.name),
  );
}

function slashQuery(draft: string): string | null {
  if (!draft.startsWith("/") || /\s/.test(draft)) return null;
  return draft.slice(1).toLowerCase();
}

function matchScore(command: SlashCommand, query: string): number | null {
  if (query.length === 0) return 10;
  const name = command.name.slice(1).toLowerCase();
  const aliases = command.aliases.map((alias) => alias.replace(/^\//, "").toLowerCase());
  if (name === query) return 0;
  if (name.startsWith(query)) return 1;
  if (aliases.some((alias) => alias.startsWith(query))) return 2;
  if (name.includes(query)) return 3;
  if (aliases.some((alias) => alias.includes(query))) return 4;
  if (command.category.includes(query)) return 5;
  if (command.description.toLowerCase().includes(query)) return 6;
  return null;
}

export function filterCommandCatalog(
  commands: readonly SlashCommand[],
  draft: string,
  limit = 80,
): SlashCommandMatch[] {
  const query = slashQuery(draft);
  if (query === null || limit <= 0) return [];
  return commands
    .flatMap((command) => {
      const score = matchScore(command, query);
      return score === null ? [] : [{ command, score }];
    })
    .sort((left, right) =>
      left.score - right.score || left.command.name.localeCompare(right.command.name),
    )
    .slice(0, limit);
}

export function replaceLeadingSlashToken(draft: string, commandName: string): string {
  if (!draft.startsWith("/") || !SAFE_COMMAND_NAME.test(commandName)) return draft;
  const tokenEnd = draft.search(/\s/);
  if (tokenEnd < 0) return commandName;
  return `${commandName}${draft.slice(tokenEnd)}`;
}
