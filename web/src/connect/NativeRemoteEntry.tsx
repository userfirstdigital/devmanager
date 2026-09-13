import { useEffect, useMemo, useRef, useState } from "react";

import { PairingGate } from "../components/PairingGate";
import { bindAppLifecycle } from "../platform/lifecycle";
import {
  publishNativeUpdateSafetyState,
  readNativeUpdateSafetyState,
} from "../pwa/nativeSafety";
import {
  canActivateUpdate,
  createFleetDocumentReloadCoordinator,
  claimFleetDocumentReloadAttempt,
  installFleetDocumentReloadCoordinator,
  notifyPwaSafetyStateChanged,
} from "../pwa/register";
import {
  hasCompleteConnectHostBinding,
  resolveConfiguredFleetHosts,
  type ConnectHostPublication,
} from "./identity";
import {
  documentRemoteOriginsFromHosts,
  fetchAuthenticatedFleetRoster,
  fleetDocumentReloadFingerprint,
  fleetRosterRequiresDocumentReload,
  readCachedFleetRoster,
} from "./fleetRoster";
import type { NativeFleetHostDescriptor } from "./fleetDescriptor";
import { parseConnectFleetDescriptors } from "./fleetDescriptor";
import {
  NativeHostRegistry,
  type NativeFleetEntrySnapshot,
} from "./nativeHostRegistry";
import { NativeRemoteApp } from "./NativeRemoteApp";
import type { NativeHostSession } from "./nativeSession";

export interface NativeRemoteEntryProps {
  /** Captured before React mounts; the host scope must never retarget in-page. */
  marker: ConnectHostPublication | null;
}

/**
 * Cache-first Connect shell with optional multi-host fleet registry.
 * Lifecycle-owned resources are created inside the effect, so StrictMode/Fast
 * Refresh cleanup cannot reuse a stopped session or late-attach a transport.
 */
export function NativeRemoteEntry({ marker }: NativeRemoteEntryProps) {
  const host = useRef(marker);
  const [registry, setRegistry] = useState<NativeHostRegistry | null>(null);
  const [snapshots, setSnapshots] = useState<NativeFleetEntrySnapshot[]>([]);
  const [membershipEpoch, setMembershipEpoch] = useState(0);
  const [notice, setNotice] = useState<string | null>(
    hasCompleteConnectHostBinding(marker)
      ? null
      : "Connect host publication is invalid.",
  );
  const [showPairing, setShowPairing] = useState(false);
  const [fleetHeld, setFleetHeld] = useState(false);
  const [updateAvailable, setUpdateAvailable] = useState(false);
  const hostsRef = useRef<NativeFleetHostDescriptor[]>([]);
  const documentOriginsRef = useRef<ReadonlySet<string>>(new Set());
  const pageAuthenticatedRef = useRef(false);

  if (host.current !== marker) {
    throw new Error("Connect host scope changed after entry mount");
  }

  useEffect(() => {
    const retainedMarker = host.current;
    if (!hasCompleteConnectHostBinding(retainedMarker)) return;

    const fleet = resolveConfiguredFleetHosts({ marker: retainedMarker });
    if (fleet.hosts.length === 0) {
      setNotice("Connect host publication is invalid.");
      return;
    }
    if (fleet.heldAdditions) {
      setFleetHeld(true);
    }
    // Immutable DOCUMENT allowlist — exact HTML meta only, never cached B.
    const documentOrigins = documentRemoteOriginsFromHosts(fleet.hosts);
    documentOriginsRef.current = documentOrigins;

    let initialHosts = fleet.hosts;
    if (fleet.hosts.length === 1) {
      const cached = readCachedFleetRoster(
        globalThis.location.origin,
        retainedMarker.hostPublicId,
      );
      if (cached && cached.pageHostPublicKey === retainedMarker.hostPublicKey) {
        const merged = parseConnectFleetDescriptors({
          pageHost: {
            hostPublicId: retainedMarker.hostPublicId,
            hostPublicKey: retainedMarker.hostPublicKey,
            origin: globalThis.location.origin,
            generation: retainedMarker.generation,
            protocolMajor: retainedMarker.protocolMajor,
            protocolMinor: retainedMarker.protocolMinor,
            label: "This device",
          },
          fleetJson: { version: 1, hosts: cached.remotes },
        });
        if (!merged.heldAdditions && merged.hosts.length > 1) {
          // Cached remotes may hydrate presentation; bootstrap stays document-gated.
          initialHosts = merged.hosts;
        }
      }
    }
    hostsRef.current = initialHosts;

    const documentReload = createFleetDocumentReloadCoordinator({
      isVisible: () =>
        typeof document === "undefined" ||
        document.visibilityState === "visible",
      readSafetyState: () =>
        readNativeUpdateSafetyState() ?? {
          hasDraft: true,
          pendingMutations: 0,
          selectedAttachments: 0,
          attachmentLoads: 0,
        },
      claimAttempt: (fingerprint) => {
        try {
          return claimFleetDocumentReloadAttempt(
            globalThis.sessionStorage,
            fingerprint,
          );
        } catch {
          return false;
        }
      },
      reloadPage: () => globalThis.location.reload(),
    });
    installFleetDocumentReloadCoordinator(documentReload);

    // Never construct session/IDB-bound resources during render.
    const retainedRegistry = new NativeHostRegistry({
      hosts: initialHosts,
      documentRemoteOrigins: documentOrigins,
      onSafetyChange: (state) => {
        publishNativeUpdateSafetyState(state);
        notifyPwaSafetyStateChanged();
        if (documentReload.hasPendingReload()) {
          setUpdateAvailable(true);
          documentReload.notifySafePoint();
        }
      },
    });
    let alive = true;
    let rosterInFlight: Promise<void> | null = null;

    publishNativeUpdateSafetyState(retainedRegistry.updateSafety());
    notifyPwaSafetyStateChanged();
    setRegistry(retainedRegistry);
    setSnapshots(retainedRegistry.snapshots());
    setMembershipEpoch(retainedRegistry.membershipEpoch());

    const unsubscribe = retainedRegistry.subscribe(() => {
      if (!alive) return;
      setSnapshots(retainedRegistry.snapshots());
      setMembershipEpoch(retainedRegistry.membershipEpoch());
    });

    retainedRegistry.start();

    const applyRosterHosts = (
      hosts: NativeFleetHostDescriptor[],
      fingerprint: string,
    ): void => {
      // Reconcile set: page host + remotes still present. Removals fence
      // immediately even when a document reload remains unsafe. New origins
      // outside the immutable document allowlist are omitted until HTML reload.
      const byId = new Map(
        hosts.map((host) => [host.hostPublicId, host] as const),
      );
      const reconcileSet: NativeFleetHostDescriptor[] = [];
      for (const entry of retainedRegistry.entries()) {
        const next = byId.get(entry.descriptor.hostPublicId);
        if (!next) {
          if (entry.descriptor.isPageHost) {
            reconcileSet.push(entry.descriptor);
          }
          continue;
        }
        if (
          next.isPageHost ||
          documentOriginsRef.current.has(next.origin) ||
          (entry.descriptor.origin === next.origin &&
            entry.descriptor.hostPublicKey === next.hostPublicKey)
        ) {
          // Document-allowed, or same pin already registered (label refresh /
          // pin HOLD path when key/origin diverge handled inside reconcile).
          reconcileSet.push(next);
        } else if (
          entry.descriptor.hostPublicId === next.hostPublicId &&
          (entry.descriptor.hostPublicKey !== next.hostPublicKey ||
            entry.descriptor.origin !== next.origin)
        ) {
          // Pin rotation attempt — still pass next so reconcile can HOLD.
          reconcileSet.push(next);
        }
      }
      for (const host of hosts) {
        if (reconcileSet.some((h) => h.hostPublicId === host.hostPublicId)) {
          continue;
        }
        if (host.isPageHost || documentOriginsRef.current.has(host.origin)) {
          reconcileSet.push(host);
        }
      }

      const result = retainedRegistry.reconcileHosts(reconcileSet);
      if (!result.ok) {
        setFleetHeld(true);
        return;
      }
      if (result.heldPin) setFleetHeld(true);
      hostsRef.current = retainedRegistry
        .entries()
        .map((entry) => entry.descriptor);

      if (
        fleetRosterRequiresDocumentReload(documentOriginsRef.current, hosts)
      ) {
        const reloadFingerprint = fleetDocumentReloadFingerprint(
          documentOriginsRef.current,
          fingerprint,
        );
        const reloaded = documentReload.requestReload(reloadFingerprint);
        if (!reloaded) {
          setUpdateAvailable(true);
        }
      }
    };

    const refreshRoster = (): void => {
      if (!alive || rosterInFlight) return;
      if (!pageAuthenticatedRef.current) return;
      rosterInFlight = (async () => {
        const pageOrigin = globalThis.location.origin;
        const result = await fetchAuthenticatedFleetRoster({
          marker: retainedMarker,
          pageOrigin,
          previousHosts: hostsRef.current,
        });
        if (!alive) return;
        if (result.held) {
          // Held/error is not an authoritative empty forget.
          setFleetHeld(true);
          return;
        }
        if (!result.changed && !result.fromCache) {
          // Still reconcile membership if registry drifted; usually no-op.
        }
        applyRosterHosts(result.hosts, result.fingerprint);
      })().finally(() => {
        rosterInFlight = null;
      });
    };

    // Coalesced fetch on page authenticated transition — off first paint.
    // Do not fetch on timer0 before page auth (401 then stall until foreground).
    const pageEntry = retainedRegistry.pageHost();
    const unsubPageAuth = pageEntry
      ? pageEntry.session.subscribe((view) => {
          if (!alive) return;
          if (view.connectionStatus !== "ready") {
            pageAuthenticatedRef.current = false;
            return;
          }
          if (pageAuthenticatedRef.current) return;
          pageAuthenticatedRef.current = true;
          // Off first-paint critical path.
          globalThis.queueMicrotask(() => refreshRoster());
        })
      : () => undefined;

    const unbindRosterLifecycle = bindAppLifecycle({
      foreground: () => {
        if (pageAuthenticatedRef.current) refreshRoster();
        if (documentReload.hasPendingReload()) {
          const safety =
            readNativeUpdateSafetyState() ?? retainedRegistry.updateSafety();
          if (canActivateUpdate(safety)) {
            documentReload.notifySafePoint();
          } else {
            setUpdateAvailable(true);
          }
        }
      },
      setVisibility: () => undefined,
      suspend: () => undefined,
    });

    return () => {
      alive = false;
      unsubPageAuth();
      unbindRosterLifecycle();
      unsubscribe();
      retainedRegistry.stop();
      installFleetDocumentReloadCoordinator(null);
      publishNativeUpdateSafetyState(null);
      notifyPwaSafetyStateChanged();
      setRegistry(null);
    };
  }, []);

  const pageEntry = snapshots.find((entry) => entry.descriptor.isPageHost);
  const anyCache = snapshots.some((entry) => entry.cacheAvailable);
  const pagePairingRequired = pageEntry?.pairingState === "pairing_required";

  const hostSessions = useMemo(() => {
    const map = new Map<string, NativeHostSession>();
    if (!registry) return map;
    for (const entry of registry.entries()) {
      map.set(entry.descriptor.hostPublicId, entry.session);
    }
    return map;
  }, [registry, membershipEpoch]);

  if (showPairing || (pagePairingRequired && !anyCache)) {
    return <PairingGate />;
  }
  if (!hasCompleteConnectHostBinding(host.current)) {
    return (
      <main className="dm-pairing-state" role="alert">
        <div className="dm-pairing-card">
          <h1>Connect host held</h1>
          <p>{notice ?? "Connect host publication is invalid."}</p>
        </div>
      </main>
    );
  }
  if (!registry || !pageEntry) {
    return <main className="dm-pairing-state">Loading cached conversations…</main>;
  }

  return (
    <div className="dm-native-remote-entry">
      {fleetHeld ? (
        <p role="status" className="dm-connect-notice">
          Fleet additions were held; only this page host is active.
        </p>
      ) : null}
      {updateAvailable ? (
        <p role="status" className="dm-connect-notice">
          A host roster update is available. Finish or clear drafts before
          reloading so conversations are preserved.
        </p>
      ) : null}
      {pageEntry.notice ? (
        <p role="status" className="dm-connect-notice">
          {pageEntry.notice}
        </p>
      ) : null}
      {pagePairingRequired ? (
        <aside className="dm-connect-notice" role="alert">
          <span>Pair this browser to refresh live host data.</span>
          <button type="button" onClick={() => setShowPairing(true)}>
            Pair browser
          </button>
        </aside>
      ) : null}
      <NativeRemoteApp
        hostPublicId={pageEntry.descriptor.hostPublicId}
        hostLabel={pageEntry.descriptor.label}
        session={registry.get(pageEntry.descriptor.hostPublicId)!.session}
        fleetEntries={snapshots}
        hostSessions={hostSessions}
        pageHostPublicId={pageEntry.descriptor.hostPublicId}
        onRetryHost={(hostPublicId) => registry.retryHost(hostPublicId)}
        onSubmitPairGrant={(hostPublicId, grant) =>
          registry.submitPairGrant(hostPublicId, { grant })
        }
        onShowPagePairing={() => setShowPairing(true)}
      />
    </div>
  );
}
