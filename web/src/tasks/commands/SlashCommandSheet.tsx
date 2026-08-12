import { Command } from "lucide-react";
import { useEffect, useRef } from "react";

import type { WebAiKind } from "../../api/types";
import type { SlashCommand, SlashCommandMatch, SlashCommandSource } from "./types";

function sourceLabel(source: SlashCommandSource): string {
  switch (source) {
    case "builtin":
      return "Built in";
    case "project":
      return "Project";
    case "personal":
      return "Personal";
    case "plugin":
      return "Plugin";
    case "mcp":
      return "MCP";
  }
}

export function SlashCommandSheet({
  provider,
  matches,
  activeIndex,
  loading,
  onActiveIndexChange,
  onSelect,
}: {
  provider: WebAiKind;
  matches: readonly SlashCommandMatch[];
  activeIndex: number;
  loading: boolean;
  onActiveIndexChange(index: number): void;
  onSelect(command: SlashCommand): void;
}) {
  const activeRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    activeRef.current?.scrollIntoView?.({ block: "nearest" });
  }, [activeIndex]);

  const providerLabel = provider === "claude" ? "Claude" : "Codex";
  const status = loading
    ? matches.length > 0
      ? `${matches.length} commands · Finding project commands…`
      : "Finding project commands…"
    : matches.length === 0
      ? "No matching commands"
      : `${matches.length} ${matches.length === 1 ? "command" : "commands"}`;

  return (
    <section className="dm-slash-command-sheet" aria-label={`${providerLabel} command suggestions`}>
      <div className="dm-slash-command-heading">
        <span><Command size={15} aria-hidden="true" /> {providerLabel}</span>
        <span role="status" aria-live="polite">{status}</span>
      </div>
      <div
        className="dm-slash-command-list"
        role="listbox"
        aria-label={`${providerLabel} commands`}
      >
        {matches.map(({ command }, index) => (
          <button
            key={`${command.source}:${command.name}`}
            ref={index === activeIndex ? activeRef : undefined}
            type="button"
            role="option"
            aria-selected={index === activeIndex}
            className="dm-slash-command-option"
            onPointerMove={() => onActiveIndexChange(index)}
            onClick={() => onSelect(command)}
          >
            <span className="dm-slash-command-copy">
              <strong>{command.name}</strong>
              {command.argumentHint && <small>{command.argumentHint}</small>}
              <span>{command.description}</span>
            </span>
            <span className={`dm-slash-command-source is-${command.source}`}>
              {sourceLabel(command.source)}
            </span>
          </button>
        ))}
        {matches.length === 0 && (
          <p className="dm-slash-command-empty">
            {loading ? "Loading…" : "Type another name or enter the command directly."}
          </p>
        )}
      </div>
    </section>
  );
}
