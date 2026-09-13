/**
 * Authenticated fleet roster fetch and bounded presentation cache.
 * Credentials stay same-origin on A; never sent to remote hosts.
 */

import {
  CONNECT_FLEET_MAX_HOSTS,
  type NativeFleetHostDescriptor,
  parseConnectFleetDescriptors,
} from "./fleetDescriptor";
import {
  hasCompleteConnectHostBinding,
  type ConnectHostPublication,
} from "./identity";
import { readBoundedResponseText, withResponseAbort } from "./boundedResponse";

export const CONNECT_FLEET_HOSTS_PATH = "/api/connect/fleet-hosts" as const;
export const CONNECT_FLEET_ROSTER_CACHE_KEY =
  "devmanager.connect.fleet-roster.v1" as const;
export const CONNECT_FLEET_ROSTER_MAX_BYTES = 16_384;
export const CONNECT_FLEET_ROSTER_DEADLINE_MS = 8_000;

export interface FleetRosterCacheRecord {
  pageOrigin: string;
  pageHostPublicId: string;
  pageHostPublicKey: string;
  remotes: Array<Omit<NativeFleetHostDescriptor, "isPageHost">>;
  fingerprint: string;
  updatedAtMs: number;
}

export interface FleetRosterFetchResult {
  hosts: NativeFleetHostDescriptor[];
  fingerprint: string;
  fromCache: boolean;
  changed: boolean;
  held: boolean;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function fingerprintHosts(hosts: readonly NativeFleetHostDescriptor[]): string {
  return hosts
    .map(
      (host) =>
        `${host.hostPublicId}|${host.hostPublicKey}|${host.origin}|${host.generation}|${host.label}`,
    )
    .join(";");
}

export function fleetRosterStorageKey(pageOrigin: string, pageHostPublicId: string): string {
  return `${CONNECT_FLEET_ROSTER_CACHE_KEY}:${pageOrigin}:${pageHostPublicId}`;
}

function browserRosterStorage(): Storage | undefined {
  try {
    return globalThis.localStorage;
  } catch {
    // Accessing the property itself can throw in restricted browser contexts.
    return undefined;
  }
}

export function readCachedFleetRoster(
  pageOrigin: string,
  pageHostPublicId: string,
  storage?: Pick<Storage, "getItem">,
): FleetRosterCacheRecord | null {
  try {
    const raw = (storage ?? browserRosterStorage())?.getItem(
      fleetRosterStorageKey(pageOrigin, pageHostPublicId),
    );
    if (!raw || raw.length > CONNECT_FLEET_ROSTER_MAX_BYTES) return null;
    const parsed = JSON.parse(raw) as unknown;
    if (!isRecord(parsed)) return null;
    if (
      parsed.pageOrigin !== pageOrigin ||
      parsed.pageHostPublicId !== pageHostPublicId ||
      typeof parsed.pageHostPublicKey !== "string" ||
      typeof parsed.fingerprint !== "string" ||
      typeof parsed.updatedAtMs !== "number" ||
      !Number.isSafeInteger(parsed.updatedAtMs) ||
      !Array.isArray(parsed.remotes)
    ) {
      return null;
    }
    const validated = parseConnectFleetDescriptors({
      pageHost: { hostPublicId: pageHostPublicId, hostPublicKey: parsed.pageHostPublicKey,
        origin: pageOrigin, label: "This device", generation: 1, protocolMajor: 1, protocolMinor: 0 },
      fleetJson: { version: 1, hosts: parsed.remotes },
    });
    if (validated.heldAdditions) return null;
    return { pageOrigin, pageHostPublicId, pageHostPublicKey: parsed.pageHostPublicKey,
      fingerprint: parsed.fingerprint, updatedAtMs: parsed.updatedAtMs,
      remotes: validated.hosts.filter((host) => !host.isPageHost).map(({ isPageHost: _ignored, ...host }) => host) };
  } catch {
    return null;
  }
}

export function writeCachedFleetRoster(
  record: FleetRosterCacheRecord,
  storage?: Pick<Storage, "setItem">,
): void {
  try {
    const encoded = JSON.stringify(record);
    if (encoded.length > CONNECT_FLEET_ROSTER_MAX_BYTES) return;
    (storage ?? browserRosterStorage())?.setItem(
      fleetRosterStorageKey(record.pageOrigin, record.pageHostPublicId),
      encoded,
    );
  } catch {
    // Presentation cache is best-effort.
  }
}

/**
 * Merge page marker + remote-only API body. Errors preserve the previous host
 * list and never authorize an empty forget.
 */
export function mergeFleetRosterResponse(input: {
  marker: ConnectHostPublication & { hostPublicId: string; hostPublicKey: string };
  pageOrigin: string;
  remotesJson: unknown;
  previousHosts?: readonly NativeFleetHostDescriptor[];
}): FleetRosterFetchResult {
  const parsed = parseConnectFleetDescriptors({
    pageHost: {
      hostPublicId: input.marker.hostPublicId,
      hostPublicKey: input.marker.hostPublicKey,
      origin: input.pageOrigin,
      generation: input.marker.generation,
      protocolMajor: input.marker.protocolMajor,
      protocolMinor: input.marker.protocolMinor,
      label: "This device",
    },
    fleetJson: input.remotesJson,
  });
  if (parsed.heldAdditions || parsed.hosts.length === 0) {
    const previous = input.previousHosts ?? [
      {
        hostPublicId: input.marker.hostPublicId,
        hostPublicKey: input.marker.hostPublicKey,
        origin: input.pageOrigin,
        label: "This device",
        generation: input.marker.generation,
        protocolMajor: 1 as const,
        protocolMinor: input.marker.protocolMinor,
        isPageHost: true,
      },
    ];
    return {
      hosts: [...previous],
      fingerprint: fingerprintHosts(previous),
      fromCache: false,
      changed: false,
      held: true,
    };
  }
  if (parsed.hosts.length > CONNECT_FLEET_MAX_HOSTS) {
    const previous = input.previousHosts ?? parsed.hosts.slice(0, 1);
    return {
      hosts: [...previous],
      fingerprint: fingerprintHosts(previous),
      fromCache: false,
      changed: false,
      held: true,
    };
  }
  const nextFingerprint = fingerprintHosts(parsed.hosts);
  const previousFingerprint = input.previousHosts
    ? fingerprintHosts(input.previousHosts)
    : "";
  return {
    hosts: parsed.hosts,
    fingerprint: nextFingerprint,
    fromCache: false,
    changed: nextFingerprint !== previousFingerprint,
    held: false,
  };
}

/**
 * Bounded authenticated roster fetch. Same-origin credentials only; never
 * widens CSP client-side. On failure keeps previous/cached descriptors.
 */
export async function fetchAuthenticatedFleetRoster(input: {
  marker: ConnectHostPublication | null;
  pageOrigin: string;
  previousHosts?: readonly NativeFleetHostDescriptor[];
  fetch?: typeof globalThis.fetch;
  storage?: Pick<Storage, "getItem" | "setItem">;
  deadlineMs?: number;
  now?: () => number;
}): Promise<FleetRosterFetchResult> {
  const marker = input.marker;
  if (!hasCompleteConnectHostBinding(marker)) {
    return {
      hosts: input.previousHosts ? [...input.previousHosts] : [],
      fingerprint: input.previousHosts
        ? fingerprintHosts(input.previousHosts)
        : "",
      fromCache: false,
      changed: false,
      held: true,
    };
  }
  const storage = input.storage ?? browserRosterStorage();
  const cached = readCachedFleetRoster(
    input.pageOrigin,
    marker.hostPublicId,
    storage,
  );

  const controller = new AbortController();
  const deadline = input.deadlineMs ?? CONNECT_FLEET_ROSTER_DEADLINE_MS;
  const timer = globalThis.setTimeout(() => controller.abort(), deadline);
  const fetcher = input.fetch ?? globalThis.fetch.bind(globalThis);
  try {
    const response = await withResponseAbort(fetcher(CONNECT_FLEET_HOSTS_PATH, {
      method: "GET",
      credentials: "same-origin",
      cache: "no-store",
      signal: controller.signal,
    }), controller.signal);
    if (!response.ok) {
      throw new Error("roster unavailable");
    }
    const text = await readBoundedResponseText(
      response,
      CONNECT_FLEET_ROSTER_MAX_BYTES,
      controller.signal,
      "roster body rejected",
    );
    const remotesJson = JSON.parse(text) as unknown;
    const merged = mergeFleetRosterResponse({
      marker,
      pageOrigin: input.pageOrigin,
      remotesJson,
      previousHosts: input.previousHosts,
    });
    if (!merged.held) {
      writeCachedFleetRoster(
        {
          pageOrigin: input.pageOrigin,
          pageHostPublicId: marker.hostPublicId,
          pageHostPublicKey: marker.hostPublicKey,
          remotes: merged.hosts
            .filter((host) => !host.isPageHost)
            .map(({ isPageHost: _ignored, ...remote }) => remote),
          fingerprint: merged.fingerprint,
          updatedAtMs: (input.now ?? Date.now)(),
        },
        storage,
      );
    }
    return merged;
  } catch {
    if (input.previousHosts && input.previousHosts.length > 0) {
      return {
        hosts: [...input.previousHosts],
        fingerprint: fingerprintHosts(input.previousHosts),
        fromCache: false,
        changed: false,
        held: true,
      };
    }
    if (cached && cached.pageHostPublicKey === marker.hostPublicKey) {
      return {
        ...mergeFleetRosterResponse({
          marker,
          pageOrigin: input.pageOrigin,
          remotesJson: { version: 1, hosts: cached.remotes },
        }),
        fromCache: true,
        changed: false,
        held: false,
      };
    }
    return {
      hosts: [
        {
          hostPublicId: marker.hostPublicId,
          hostPublicKey: marker.hostPublicKey,
          origin: input.pageOrigin,
          label: "This device",
          generation: marker.generation,
          protocolMajor: 1,
          protocolMinor: marker.protocolMinor,
          isPageHost: true,
        },
      ],
      fingerprint: "",
      fromCache: false,
      changed: false,
      held: true,
    };
  } finally {
    globalThis.clearTimeout(timer);
  }
}

/**
 * Immutable document remote origins captured from exact HTML fleet meta.
 * Cached/API descriptors must not widen this set without a fresh document.
 */
export function documentRemoteOriginsFromHosts(
  hosts: readonly NativeFleetHostDescriptor[],
): ReadonlySet<string> {
  return new Set(
    hosts.filter((host) => !host.isPageHost).map((host) => host.origin),
  );
}

/**
 * True when the API roster includes a remote origin not present in the
 * immutable document allowlist (exact HTML meta). Cached B cannot prove CSP.
 */
export function fleetRosterRequiresDocumentReload(
  documentRemoteOrigins: ReadonlySet<string>,
  next: readonly NativeFleetHostDescriptor[],
): boolean {
  return next.some(
    (host) => !host.isPageHost && !documentRemoteOrigins.has(host.origin),
  );
}

/** Stable fingerprint for document+roster reload claim guards. */
export function fleetDocumentReloadFingerprint(
  documentRemoteOrigins: ReadonlySet<string>,
  rosterFingerprint: string,
): string {
  const documentPart = [...documentRemoteOrigins].sort().join(",");
  return `${documentPart}|${rosterFingerprint}`;
}
