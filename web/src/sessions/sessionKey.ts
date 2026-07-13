import type { StableSessionKey } from "../api/types";
import type { SessionRouteKind } from "../app/router";

export interface ParsedSessionKey {
  kind: SessionRouteKind;
  id: string;
}

export function parseSessionKey(
  stableSessionKey: StableSessionKey,
): ParsedSessionKey | null {
  const separator = stableSessionKey.indexOf(":");
  if (separator <= 0 || separator === stableSessionKey.length - 1) return null;
  const kind = stableSessionKey.slice(0, separator);
  if (kind !== "server" && kind !== "tab") return null;
  return { kind, id: stableSessionKey.slice(separator + 1) };
}

export function makeSessionKey(
  kind: SessionRouteKind,
  id: string,
): StableSessionKey {
  return `${kind}:${id}`;
}
