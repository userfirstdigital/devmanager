import { useEffect, useState } from "react";

import type { WebAiKind } from "../../api/types";
import { commandsForProvider, mergeCommandCatalog } from "./commandCatalog";
import type { DiscoveredSlashCommand, SlashCommand } from "./types";

const CATALOG_FRESH_MS = 30_000;

interface CachedCatalog {
  expiresAt: number;
  commands?: DiscoveredSlashCommand[];
  pending?: Promise<DiscoveredSlashCommand[]>;
}

const catalogCache = new Map<string, CachedCatalog>();

export interface SlashCommandCatalogState {
  commands: SlashCommand[];
  loading: boolean;
}

export interface SlashCommandCatalogOptions {
  scopeKey: string;
  sessionKey: string;
  provider: WebAiKind;
  enabled: boolean;
}

function safeDiscoveredCommands(value: unknown): DiscoveredSlashCommand[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((item) => {
    if (typeof item !== "object" || item === null) return [];
    const candidate = item as Record<string, unknown>;
    if (
      typeof candidate.name !== "string" ||
      typeof candidate.description !== "string" ||
      !["project", "personal", "plugin", "mcp"].includes(
        String(candidate.source),
      )
    ) {
      return [];
    }
    return [
      {
        name: candidate.name,
        description: candidate.description,
        source: candidate.source as DiscoveredSlashCommand["source"],
      },
    ];
  });
}

function discover(
  cacheKey: string,
  sessionKey: string,
  provider: WebAiKind,
): Promise<DiscoveredSlashCommand[]> {
  const now = Date.now();
  const cached = catalogCache.get(cacheKey);
  if (cached?.commands && cached.expiresAt > now) {
    return Promise.resolve(cached.commands);
  }
  if (cached?.pending) return cached.pending;

  const pending = fetch(
    `/api/slash-commands?sessionKey=${encodeURIComponent(sessionKey)}`,
    { credentials: "include" },
  )
    .then(async (response) => {
      if (!response.ok) throw new Error(`catalog request failed: ${response.status}`);
      const value = (await response.json()) as unknown;
      if (typeof value !== "object" || value === null) {
        throw new Error("invalid catalog response");
      }
      const record = value as Record<string, unknown>;
      if (record.provider !== provider) {
        throw new Error("catalog provider changed");
      }
      const commands = safeDiscoveredCommands(record.commands);
      catalogCache.set(cacheKey, {
        commands,
        expiresAt: Date.now() + CATALOG_FRESH_MS,
      });
      return commands;
    })
    .catch((error) => {
      catalogCache.delete(cacheKey);
      throw error;
    });
  catalogCache.set(cacheKey, { pending, expiresAt: 0 });
  return pending;
}

export function useSlashCommandCatalog({
  scopeKey,
  sessionKey,
  provider,
  enabled,
}: SlashCommandCatalogOptions): SlashCommandCatalogState {
  const builtins = commandsForProvider(provider);
  const [state, setState] = useState<SlashCommandCatalogState>(() => ({
    commands: [...builtins],
    loading: enabled,
  }));

  useEffect(() => {
    let active = true;
    setState({ commands: [...builtins], loading: enabled });
    if (!enabled || !scopeKey || !sessionKey) {
      return () => {
        active = false;
      };
    }

    const cacheKey = `${scopeKey}:${provider}`;
    void discover(cacheKey, sessionKey, provider).then(
      (discovered) => {
        if (!active) return;
        setState({
          commands: mergeCommandCatalog(provider, builtins, discovered),
          loading: false,
        });
      },
      () => {
        if (!active) return;
        setState({ commands: [...builtins], loading: false });
      },
    );
    return () => {
      active = false;
    };
  }, [builtins, enabled, provider, scopeKey, sessionKey]);

  return state;
}

export function clearSlashCommandCatalogCacheForTests(): void {
  catalogCache.clear();
}
