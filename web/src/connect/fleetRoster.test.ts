import { describe, expect, it } from "vitest";

import {
  CONNECT_FLEET_ROSTER_MAX_BYTES,
  documentRemoteOriginsFromHosts,
  fetchAuthenticatedFleetRoster,
  fleetDocumentReloadFingerprint,
  fleetRosterRequiresDocumentReload,
  mergeFleetRosterResponse,
  writeCachedFleetRoster,
  readCachedFleetRoster,
} from "./fleetRoster";
import type { NativeFleetHostDescriptor } from "./fleetDescriptor";

const PAGE: NativeFleetHostDescriptor = {
  hostPublicId: "01234567-89ab-7000-8000-000000000001",
  hostPublicKey: "aa".repeat(32),
  origin: "https://phone.example",
  label: "This device",
  generation: 1,
  protocolMajor: 1,
  protocolMinor: 0,
  isPageHost: true,
};

const REMOTE: Omit<NativeFleetHostDescriptor, "isPageHost"> = {
  hostPublicId: "01234567-89ab-7000-8000-000000000002",
  hostPublicKey: "bb".repeat(32),
  origin: "https://studio.example",
  label: "Studio",
  generation: 2,
  protocolMajor: 1,
  protocolMinor: 0,
};

const MARKER = {
  transport: "connect" as const,
  endpoint: "/api/connect",
  generation: 1,
  protocolMajor: 1,
  protocolMinor: 0,
  hostPublicId: PAGE.hostPublicId,
  hostPublicKey: PAGE.hostPublicKey,
};

function streamedJson(body: unknown, status = 200): Response {
  const bytes = new TextEncoder().encode(JSON.stringify(body));
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(bytes);
      controller.close();
    },
  });
  return new Response(stream, {
    status,
    headers: { "content-type": "application/json" },
  });
}

describe("fleetRoster", () => {
  it("keeps authenticated roster loading when browser storage access is denied", async () => {
    const original = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      get() { throw new DOMException("Storage denied", "SecurityError"); },
    });
    try {
      expect(readCachedFleetRoster(PAGE.origin, PAGE.hostPublicId)).toBeNull();
      const result = await fetchAuthenticatedFleetRoster({
        marker: MARKER, pageOrigin: PAGE.origin, previousHosts: [PAGE],
        fetch: (async () => streamedJson({ version: 1, hosts: [REMOTE] })) as typeof fetch,
      });
      expect(result.held).toBe(false);
      expect(result.hosts.map((host) => host.hostPublicId)).toEqual([
        PAGE.hostPublicId, REMOTE.hostPublicId,
      ]);
    } finally {
      if (original) Object.defineProperty(globalThis, "localStorage", original);
      else Reflect.deleteProperty(globalThis, "localStorage");
    }
  });

  it("preserves previous hosts when roster JSON is held", () => {
    const result = mergeFleetRosterResponse({
      marker: MARKER,
      pageOrigin: PAGE.origin,
      remotesJson: { version: 1, hosts: [{ ...REMOTE, origin: "http://bad" }] },
      previousHosts: [PAGE, { ...REMOTE, isPageHost: false }],
    });
    expect(result.held).toBe(true);
    expect(result.changed).toBe(false);
    expect(result.hosts).toHaveLength(2);
  });

  it("compares API origins to immutable document origins, not prior cache", () => {
    const documentOnly = documentRemoteOriginsFromHosts([PAGE]);
    expect(
      fleetRosterRequiresDocumentReload(documentOnly, [
        PAGE,
        { ...REMOTE, isPageHost: false },
      ]),
    ).toBe(true);
    expect(
      fleetRosterRequiresDocumentReload(
        new Set([REMOTE.origin]),
        [PAGE, { ...REMOTE, isPageHost: false, label: "Renamed" }],
      ),
    ).toBe(false);
    expect(
      fleetDocumentReloadFingerprint(documentOnly, "fp-1"),
    ).toBe("|fp-1");
  });

  it("stores only non-secret presentation fields", () => {
    const storage = new Map<string, string>();
    writeCachedFleetRoster(
      {
        pageOrigin: PAGE.origin,
        pageHostPublicId: PAGE.hostPublicId,
        pageHostPublicKey: PAGE.hostPublicKey,
        remotes: [REMOTE],
        fingerprint: "fp",
        updatedAtMs: 1,
      },
      {
        setItem: (key, value) => storage.set(key, value),
      },
    );
    const encoded = [...storage.values()].join("");
    expect(encoded).not.toMatch(/grant|ticket|cookie|private/i);
    const read = readCachedFleetRoster(PAGE.origin, PAGE.hostPublicId, {
      getItem: (key) => storage.get(key) ?? null,
    });
    expect(read?.remotes[0]?.origin).toBe(REMOTE.origin);
  });

  it("bounds roster body bytes under one absolute deadline and cancels the reader", async () => {
    const result = await fetchAuthenticatedFleetRoster({
      marker: MARKER,
      pageOrigin: PAGE.origin,
      previousHosts: [PAGE],
      deadlineMs: 40,
      storage: {
        getItem: () => null,
        setItem: () => undefined,
      },
      fetch: (async () => {
        const stream = new ReadableStream({
          start(controller) {
            controller.enqueue(new TextEncoder().encode('{"version":1,"hosts":['));
            // Never closes — deadline must cancel even an abort-resistant mock.
          },
          cancel() {
            // Reader cancel path.
          },
        });
        return new Response(stream, { status: 200 });
      }) as typeof fetch,
    });
    expect(result.held).toBe(true);
    expect(result.hosts).toEqual([PAGE]);
  });

  it("fail-closes when the roster response has no stream reader", async () => {
    const result = await fetchAuthenticatedFleetRoster({
      marker: MARKER,
      pageOrigin: PAGE.origin,
      previousHosts: [PAGE],
      storage: {
        getItem: () => null,
        setItem: () => undefined,
      },
      fetch: (async () => {
        const response = {
          ok: true,
          body: null,
          status: 200,
        } as unknown as Response;
        return response;
      }) as typeof fetch,
    });
    expect(result.held).toBe(true);
    expect(result.hosts).toEqual([PAGE]);
  });

  it("accepts a bounded streamed roster within the byte cap", async () => {
    const result = await fetchAuthenticatedFleetRoster({
      marker: MARKER,
      pageOrigin: PAGE.origin,
      previousHosts: [PAGE],
      storage: {
        getItem: () => null,
        setItem: () => undefined,
      },
      fetch: (async () =>
        streamedJson({ version: 1, hosts: [REMOTE] })) as typeof fetch,
    });
    expect(result.held).toBe(false);
    expect(result.hosts).toHaveLength(2);
    expect(CONNECT_FLEET_ROSTER_MAX_BYTES).toBe(16_384);
  });
});
