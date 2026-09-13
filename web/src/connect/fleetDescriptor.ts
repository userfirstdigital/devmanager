/**
 * Public pinned fleet descriptors emitted beside the page Connect marker.
 * No grants, cookies, or private keys — metadata only.
 */

import { protocolUuid } from "./hostOutput";

export const CONNECT_FLEET_META_NAME = "devmanager-connect-fleet" as const;
export const CONNECT_FLEET_MAX_HOSTS = 16;
export const CONNECT_FLEET_MAX_LABEL_CHARS = 80;
export const CONNECT_FLEET_PUBLICATION_KEY =
  "__DEVMANAGER_CONNECT_FLEET__" as const;

const HOST_KEY_HEX = /^[0-9a-f]{64}$/;

export type FleetDescriptorHoldReason =
  | "malformed"
  | "duplicate_host_retarget"
  | "capacity"
  | "invalid_origin"
  | "missing_page_host";

export interface NativeFleetHostDescriptor {
  hostPublicId: string;
  hostPublicKey: string;
  /** Canonical origin; page host may be http(s); non-page hosts HTTPS only. */
  origin: string;
  label: string;
  generation: number;
  protocolMajor: 1;
  protocolMinor: number;
  /** True when this descriptor is the authoritative page-serving Connect host. */
  isPageHost: boolean;
}

export interface FleetDescriptorParseResult {
  /** Page host plus accepted additions (≤16 total). */
  hosts: NativeFleetHostDescriptor[];
  /** Malformed or conflicting additions are held, never applied via legacy fallback. */
  heldAdditions: boolean;
  holdReason: FleetDescriptorHoldReason | null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function canonicalHttpsOrigin(raw: string): string | null {
  try {
    const url = new URL(raw);
    if (url.protocol !== "https:" || url.origin !== raw) return null;
    return url.origin;
  } catch {
    return null;
  }
}

function canonicalPageOrigin(raw: string): string | null {
  try {
    const url = new URL(raw);
    if (
      (url.protocol !== "https:" && url.protocol !== "http:") ||
      url.origin !== raw
    ) {
      return null;
    }
    return url.origin;
  } catch {
    return null;
  }
}

function parseHostEntry(
  value: unknown,
  options: { requireHttps: boolean },
): Omit<NativeFleetHostDescriptor, "isPageHost"> | null {
  if (!isRecord(value)) return null;
  const hostPublicId = protocolUuid(value.hostPublicId);
  if (!hostPublicId) return null;
  if (typeof value.hostPublicKey !== "string") return null;
  const hostPublicKey = value.hostPublicKey.toLowerCase();
  if (!HOST_KEY_HEX.test(hostPublicKey) || /^0{64}$/.test(hostPublicKey)) {
    return null;
  }
  if (typeof value.origin !== "string") return null;
  const origin = options.requireHttps
    ? canonicalHttpsOrigin(value.origin)
    : canonicalPageOrigin(value.origin);
  if (!origin) return null;
  if (typeof value.label !== "string") return null;
  const label = value.label.trim();
  if (!label || label.length > CONNECT_FLEET_MAX_LABEL_CHARS) return null;
  if (
    typeof value.generation !== "number" ||
    !Number.isSafeInteger(value.generation) ||
    value.generation <= 0
  ) {
    return null;
  }
  if (value.protocolMajor !== 1) return null;
  if (
    typeof value.protocolMinor !== "number" ||
    !Number.isSafeInteger(value.protocolMinor) ||
    value.protocolMinor < 0
  ) {
    return null;
  }
  return {
    hostPublicId,
    hostPublicKey,
    origin,
    label,
    generation: value.generation,
    protocolMajor: 1,
    protocolMinor: value.protocolMinor,
  };
}

/**
 * Merge the authoritative page Connect host with optional fleet metadata.
 * Page marker remains required. Malformed fleet JSON holds additions only.
 */
export function parseConnectFleetDescriptors(input: {
  pageHost: {
    hostPublicId: string;
    hostPublicKey: string;
    origin: string;
    label?: string;
    generation: number;
    protocolMajor: number;
    protocolMinor: number;
  };
  fleetJson: unknown;
}): FleetDescriptorParseResult {
  const pageOrigin = canonicalPageOrigin(input.pageHost.origin);
  const pageId = protocolUuid(input.pageHost.hostPublicId);
  if (!pageOrigin || !pageId || input.pageHost.protocolMajor !== 1) {
    return {
      hosts: [],
      heldAdditions: true,
      holdReason: "missing_page_host",
    };
  }
  const pageKey = input.pageHost.hostPublicKey.toLowerCase();
  if (!HOST_KEY_HEX.test(pageKey) || /^0{64}$/.test(pageKey)) {
    return {
      hosts: [],
      heldAdditions: true,
      holdReason: "missing_page_host",
    };
  }
  const page: NativeFleetHostDescriptor = {
    hostPublicId: pageId,
    hostPublicKey: pageKey,
    origin: pageOrigin,
    label: (input.pageHost.label ?? "This device").slice(
      0,
      CONNECT_FLEET_MAX_LABEL_CHARS,
    ),
    generation: input.pageHost.generation,
    protocolMajor: 1,
    protocolMinor: input.pageHost.protocolMinor,
    isPageHost: true,
  };

  if (input.fleetJson === undefined || input.fleetJson === null) {
    return { hosts: [page], heldAdditions: false, holdReason: null };
  }
  if (!isRecord(input.fleetJson) || input.fleetJson.version !== 1) {
    return { hosts: [page], heldAdditions: true, holdReason: "malformed" };
  }
  if (!Array.isArray(input.fleetJson.hosts)) {
    return { hosts: [page], heldAdditions: true, holdReason: "malformed" };
  }

  const hosts: NativeFleetHostDescriptor[] = [page];
  const byId = new Map<string, NativeFleetHostDescriptor>([
    [page.hostPublicId, page],
  ]);

  for (const raw of input.fleetJson.hosts) {
    const parsed = parseHostEntry(raw, { requireHttps: true });
    if (!parsed) {
      return { hosts: [page], heldAdditions: true, holdReason: "malformed" };
    }
    // Page host may also appear in the fleet list; identical pin is accepted.
    const existing = byId.get(parsed.hostPublicId);
    if (existing) {
      if (
        existing.origin !== parsed.origin ||
        existing.hostPublicKey !== parsed.hostPublicKey
      ) {
        return {
          hosts: [page],
          heldAdditions: true,
          holdReason: "duplicate_host_retarget",
        };
      }
      continue;
    }
    if (hosts.length >= CONNECT_FLEET_MAX_HOSTS) {
      return { hosts: [page], heldAdditions: true, holdReason: "capacity" };
    }
    // Non-page hosts must not silently share the page origin under a new id.
    if (parsed.origin === page.origin && parsed.hostPublicId !== page.hostPublicId) {
      return { hosts: [page], heldAdditions: true, holdReason: "invalid_origin" };
    }
    const entry: NativeFleetHostDescriptor = {
      ...parsed,
      isPageHost: false,
    };
    hosts.push(entry);
    byId.set(entry.hostPublicId, entry);
  }

  return { hosts, heldAdditions: false, holdReason: null };
}

/** Read inert fleet JSON from the document meta tag. */
export function readConnectFleetMetaJson(
  documentSource: Pick<Document, "querySelector"> = document,
): unknown {
  const element = documentSource.querySelector(
    `meta[name="${CONNECT_FLEET_META_NAME}"]`,
  );
  if (!element) return null;
  const raw = element.getAttribute("content");
  if (!raw || raw.length > 16_384) return null;
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}
