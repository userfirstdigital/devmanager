import type { WebAiKind } from "../../api/types";

export type SlashCommandSource =
  | "builtin"
  | "project"
  | "personal"
  | "plugin"
  | "mcp";

export type SlashCommandCategory =
  | "session"
  | "model"
  | "workflow"
  | "tools"
  | "configuration"
  | "account"
  | "diagnostics"
  | "custom";

export type SlashCommandInteraction = "inline" | "providerMenu";

export interface SlashCommandSuggestion {
  label: string;
  value: string;
}

export interface SlashCommand {
  name: string;
  description: string;
  provider: WebAiKind;
  source: SlashCommandSource;
  category: SlashCommandCategory;
  argumentHint: string | null;
  suggestions: SlashCommandSuggestion[];
  aliases: string[];
  interaction: SlashCommandInteraction;
}

export interface DiscoveredSlashCommand {
  name: string;
  description: string;
  source: Exclude<SlashCommandSource, "builtin">;
}

export interface SlashCommandMatch {
  command: SlashCommand;
  score: number;
}
