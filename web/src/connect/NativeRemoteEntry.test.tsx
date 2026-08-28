// @vitest-environment jsdom

import { cleanup, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

type SessionListener = (view: { connectionStatus: string }) => void;

const { fetchRoster, mockRegistry } = vi.hoisted(() => {
  let pageStatus = "connecting";
  const pageListeners = new Set<SessionListener>();
  const registryListeners = new Set<() => void>();

  const pageDescriptor = {
    hostPublicId: "01234567-89ab-7000-8000-000000000001",
    hostPublicKey: "aa".repeat(32),
    origin: "https://phone.example",
    label: "This device",
    generation: 1,
    protocolMajor: 1 as const,
    protocolMinor: 0,
    isPageHost: true,
  };

  const pageSession = {
    subscribe(listener: SessionListener) {
      pageListeners.add(listener);
      listener({ connectionStatus: pageStatus });
      return () => pageListeners.delete(listener);
    },
    view() {
      return {
        connectionStatus: pageStatus,
        tasks: new Map(),
        conversations: new Map(),
        drafts: new Map(),
        outbox: new Map(),
      };
    },
    stop() {
      return undefined;
    },
  };

  class NativeHostRegistry {
    start() {
      return undefined;
    }
    stop() {
      return undefined;
    }
    subscribe(listener: () => void) {
      registryListeners.add(listener);
      return () => registryListeners.delete(listener);
    }
    snapshots() {
      return [
        {
          descriptor: pageDescriptor,
          view: pageSession.view(),
          hydrationKnown: true,
          pairingState: pageStatus === "ready" ? "ready" : "transport_attached",
          transportAttached: true,
          authenticated: pageStatus === "ready",
          notice: null,
          cacheAvailable: false,
        },
      ];
    }
    entries() {
      return [
        {
          descriptor: pageDescriptor,
          session: pageSession,
          transport: { isAttached: () => true },
          hydrationKnown: true,
          pairingState: "transport_attached",
          transportAttached: true,
          notice: null,
          pendingGrant: null,
        },
      ];
    }
    pageHost() {
      return this.entries()[0];
    }
    get() {
      return this.entries()[0];
    }
    membershipEpoch() {
      return 0;
    }
    updateSafety() {
      return {
        hasDraft: false,
        pendingMutations: 0,
        selectedAttachments: 0,
        attachmentLoads: 0,
      };
    }
    reconcileHosts() {
      return { ok: true as const, membershipChanged: false, heldPin: false };
    }
    retryHost() {
      return undefined;
    }
    submitPairGrant() {
      return undefined;
    }
  }

  return {
    fetchRoster: vi.fn(),
    mockRegistry: {
      NativeHostRegistry,
      setPageStatus(status: string) {
        pageStatus = status;
        for (const listener of pageListeners) {
          listener({ connectionStatus: status });
        }
        for (const listener of registryListeners) listener();
      },
      reset() {
        pageStatus = "connecting";
        pageListeners.clear();
        registryListeners.clear();
      },
    },
  };
});

vi.mock("./fleetRoster", async () => {
  const actual = await vi.importActual<typeof import("./fleetRoster")>(
    "./fleetRoster",
  );
  return {
    ...actual,
    fetchAuthenticatedFleetRoster: (...args: unknown[]) => fetchRoster(...args),
  };
});

vi.mock("./nativeHostRegistry", () => ({
  NativeHostRegistry: mockRegistry.NativeHostRegistry,
}));

vi.mock("./identity", async () => {
  const actual = await vi.importActual<typeof import("./identity")>("./identity");
  return {
    ...actual,
    hasCompleteConnectHostBinding: () => true,
    resolveConfiguredFleetHosts: () => ({
      hosts: [
        {
          hostPublicId: "01234567-89ab-7000-8000-000000000001",
          hostPublicKey: "aa".repeat(32),
          origin: "https://phone.example",
          label: "This device",
          generation: 1,
          protocolMajor: 1,
          protocolMinor: 0,
          isPageHost: true,
        },
      ],
      heldAdditions: false,
      holdReason: null,
    }),
  };
});

vi.mock("./NativeRemoteApp", () => ({
  NativeRemoteApp: () => <div data-testid="native-remote-app">app</div>,
}));

import { NativeRemoteEntry } from "./NativeRemoteEntry";
import type { ConnectHostPublication } from "./identity";
import {
  claimFleetDocumentReloadAttempt,
  createFleetDocumentReloadCoordinator,
} from "../pwa/register";
import {
  documentRemoteOriginsFromHosts,
  fleetDocumentReloadFingerprint,
  fleetRosterRequiresDocumentReload,
} from "./fleetRoster";

const marker: ConnectHostPublication = {
  transport: "connect",
  endpoint: "/api/connect",
  generation: 1,
  protocolMajor: 1,
  protocolMinor: 0,
  hostPublicId: "01234567-89ab-7000-8000-000000000001",
  hostPublicKey: "aa".repeat(32),
};

const rosterResult = {
  hosts: [
    {
      hostPublicId: "01234567-89ab-7000-8000-000000000001",
      hostPublicKey: "aa".repeat(32),
      origin: "https://phone.example",
      label: "This device",
      generation: 1,
      protocolMajor: 1,
      protocolMinor: 0,
      isPageHost: true,
    },
    {
      hostPublicId: "01234567-89ab-7000-8000-000000000002",
      hostPublicKey: "bb".repeat(32),
      origin: "https://studio.example",
      label: "Studio",
      generation: 1,
      protocolMajor: 1,
      protocolMinor: 0,
      isPageHost: false,
    },
  ],
  fingerprint: "fp-b",
  fromCache: false,
  changed: true,
  held: false,
};

describe("NativeRemoteEntry production roster path", () => {
  beforeEach(() => {
    mockRegistry.reset();
    fetchRoster.mockReset();
    fetchRoster.mockResolvedValue(rosterResult);
    vi.stubGlobal("location", {
      origin: "https://phone.example",
      protocol: "https:",
      host: "phone.example",
      href: "https://phone.example/",
      reload: vi.fn(),
    });
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("coalesces roster fetch on page authenticated transition, not before auth", async () => {
    render(<NativeRemoteEntry marker={marker} />);
    await new Promise((resolve) => setTimeout(resolve, 30));
    expect(fetchRoster).not.toHaveBeenCalled();

    mockRegistry.setPageStatus("ready");
    await waitFor(() => expect(fetchRoster).toHaveBeenCalledTimes(1));

    mockRegistry.setPageStatus("ready");
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(fetchRoster).toHaveBeenCalledTimes(1);
  });

  it("refetches roster on ready→degraded→ready without per-message spam", async () => {
    render(<NativeRemoteEntry marker={marker} />);
    mockRegistry.setPageStatus("ready");
    await waitFor(() => expect(fetchRoster).toHaveBeenCalledTimes(1));

    mockRegistry.setPageStatus("degraded");
    mockRegistry.setPageStatus("degraded");
    mockRegistry.setPageStatus("syncing");
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(fetchRoster).toHaveBeenCalledTimes(1);

    mockRegistry.setPageStatus("ready");
    await waitFor(() => expect(fetchRoster).toHaveBeenCalledTimes(2));

    mockRegistry.setPageStatus("ready");
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(fetchRoster).toHaveBeenCalledTimes(2);
  });
});

describe("document allowlist reload via production coordinator", () => {
  it("cached B + document self-only triggers guarded reload; same fingerprint next mount blocked; drafts block", () => {
    const documentOrigins = documentRemoteOriginsFromHosts([
      {
        hostPublicId: "01234567-89ab-7000-8000-000000000001",
        hostPublicKey: "aa".repeat(32),
        origin: "https://phone.example",
        label: "This device",
        generation: 1,
        protocolMajor: 1,
        protocolMinor: 0,
        isPageHost: true,
      },
    ]);
    const apiHosts = [
      {
        hostPublicId: "01234567-89ab-7000-8000-000000000001",
        hostPublicKey: "aa".repeat(32),
        origin: "https://phone.example",
        label: "This device",
        generation: 1,
        protocolMajor: 1 as const,
        protocolMinor: 0,
        isPageHost: true,
      },
      {
        hostPublicId: "01234567-89ab-7000-8000-000000000002",
        hostPublicKey: "bb".repeat(32),
        origin: "https://studio.example",
        label: "Studio",
        generation: 1,
        protocolMajor: 1 as const,
        protocolMinor: 0,
        isPageHost: false,
      },
    ];
    expect(fleetRosterRequiresDocumentReload(documentOrigins, apiHosts)).toBe(
      true,
    );

    const sessionStore = new Map<string, string>();
    const reloadPage = vi.fn();
    let safety = {
      hasDraft: true,
      pendingMutations: 0,
      selectedAttachments: 0,
      attachmentLoads: 0,
    };
    const fingerprint = fleetDocumentReloadFingerprint(
      documentOrigins,
      "fp-b",
    );
    const coordinator = createFleetDocumentReloadCoordinator({
      isVisible: () => true,
      readSafetyState: () => safety,
      reloadPage,
      claimAttempt: (fp) =>
        claimFleetDocumentReloadAttempt(
          {
            getItem: (key) => sessionStore.get(key) ?? null,
            setItem: (key, value) => sessionStore.set(key, value),
          },
          fp,
        ),
    });

    expect(coordinator.requestReload(fingerprint)).toBe(false);
    expect(reloadPage).not.toHaveBeenCalled();

    safety = { ...safety, hasDraft: false };
    expect(coordinator.notifySafePoint()).toBe(true);
    expect(reloadPage).toHaveBeenCalledTimes(1);

    const nextMount = createFleetDocumentReloadCoordinator({
      isVisible: () => true,
      readSafetyState: () => ({ hasDraft: false, pendingMutations: 0 }),
      reloadPage,
      claimAttempt: (fp) =>
        claimFleetDocumentReloadAttempt(
          {
            getItem: (key) => sessionStore.get(key) ?? null,
            setItem: (key, value) => sessionStore.set(key, value),
          },
          fp,
        ),
    });
    reloadPage.mockClear();
    expect(nextMount.requestReload(fingerprint)).toBe(false);
    expect(reloadPage).not.toHaveBeenCalled();
  });
});
