/**
 * App-effect-owned multi-host registry.
 *
 * Lifecycle pattern adapted from T3 Tools EnvironmentRegistry under the MIT
 * License. Copyright (c) 2026 T3 Tools Inc. Full notice: THIRD_PARTY_NOTICES.md
 * ("T3 Tools scoped projection / registry pattern").
 *
 * Ports qualification / per-host projection / start-before-attach lifecycle —
 * not T3's Effect runtime, bearer tokens, or DPoP transport.
 */

import { bindAppLifecycle } from "../platform/lifecycle";
import {
  nativeUpdateSafetyState,
  type NativeProjectionSafetyView,
} from "../pwa/nativeSafety";
import type { UpdateSafetyState } from "../pwa/register";
import { DeferredNativeTransport } from "./deferredNativeTransport";
import type { NativeFleetHostDescriptor } from "./fleetDescriptor";
import { CONNECT_FLEET_MAX_HOSTS } from "./fleetDescriptor";
import {
  ConnectPairingRequiredError,
  ConnectBrowserIdentityHoldError,
  bootstrapConnect,
  bootstrapCrossOriginConnect,
  type ConnectBootstrapHandle,
  type ConnectCrossOriginPairGrant,
} from "./identity";
import { HostTrustHoldError } from "./hostTrust";
import { createIndexedDbNativeCacheStore, type NativeCacheStore } from "./nativeCache";
import { NativeHostSession, type NativeHostSessionView } from "./nativeSession";

export type NativeFleetPairingState =
  | "unknown"
  | "transport_attached"
  | "ready"
  | "pairing_required"
  | "held"
  | "retrying";

export interface NativeFleetEntrySnapshot {
  descriptor: NativeFleetHostDescriptor;
  view: NativeHostSessionView;
  hydrationKnown: boolean;
  pairingState: NativeFleetPairingState;
  /** Real socket/Noise port is wired; not yet proof of authenticated Hello. */
  transportAttached: boolean;
  /** Session reached authenticated ready (lease-bearing). */
  authenticated: boolean;
  notice: string | null;
  cacheAvailable: boolean;
}

export interface NativeFleetEntry {
  descriptor: NativeFleetHostDescriptor;
  readonly session: NativeHostSession;
  readonly transport: DeferredNativeTransport;
  hydrationKnown: boolean;
  pairingState: NativeFleetPairingState;
  transportAttached: boolean;
  notice: string | null;
  /** One-shot grant captured from Pair UI; wiped after bootstrap attempt. */
  pendingGrant: ConnectCrossOriginPairGrant | null;
}

export type NativeHostReconcileResult =
  | { ok: true; membershipChanged: boolean; heldPin: boolean }
  | { ok: false; reason: string };

export interface NativeHostRegistryOptions {
  hosts: readonly NativeFleetHostDescriptor[];
  /**
   * Immutable document remote origins (from exact HTML fleet meta). Remotes
   * outside this set may hydrate cached presentation but must not bootstrap.
   */
  documentRemoteOrigins?: ReadonlySet<string>;
  /** Shared or per-host cache factory; default IndexedDB native cache. */
  createCache?: (hostPublicId: string) => NativeCacheStore;
  bootstrapPageHost?: () => Promise<ConnectBootstrapHandle | null>;
  bootstrapRemoteHost?: (
    entry: NativeFleetEntry,
    grant: ConnectCrossOriginPairGrant | null,
  ) => Promise<ConnectBootstrapHandle>;
  onSafetyChange?: (state: UpdateSafetyState) => void;
  now?: () => number;
}

function holdMessage(error: unknown): string {
  return error instanceof Error
    ? error.message
    : "Connect to this host is held until it can be safely verified.";
}

function hasCachedPresentation(view: NativeHostSessionView): boolean {
  return (
    view.tasks.size > 0 ||
    view.conversations.size > 0 ||
    view.drafts.size > 0 ||
    view.outbox.size > 0
  );
}

function isForegroundRetryableBootstrapError(error: unknown): boolean {
  return (
    error instanceof ConnectBrowserIdentityHoldError &&
    error.message === "Connect pairing status could not be checked"
  );
}

function isIdentityCorruptionHold(error: unknown): boolean {
  if (error instanceof HostTrustHoldError) return true;
  if (!(error instanceof ConnectBrowserIdentityHoldError)) return false;
  if (error instanceof ConnectPairingRequiredError) return false;
  if (isForegroundRetryableBootstrapError(error)) return false;
  const message = error.message;
  return (
    /corrupt|repair|foreign host|host key changed|identity/i.test(message) ||
    message === "Connect host bootstrap did not bind the selected host." ||
    message === "Connect cross-origin bootstrap did not bind the selected host."
  );
}

function isAuthenticated(view: NativeHostSessionView): boolean {
  return view.connectionStatus === "ready";
}

function validateReconcileDescriptors(
  nextHosts: readonly NativeFleetHostDescriptor[],
  pageHost: NativeFleetEntry | undefined,
):
  | { ok: true; hosts: readonly NativeFleetHostDescriptor[] }
  | { ok: false; reason: string } {
  if (!pageHost) {
    return { ok: false, reason: "page host missing" };
  }
  if (nextHosts.length === 0 || nextHosts.length > CONNECT_FLEET_MAX_HOSTS) {
    return { ok: false, reason: "capacity rejected" };
  }
  const seen = new Set<string>();
  let pageCount = 0;
  for (const host of nextHosts) {
    if (seen.has(host.hostPublicId)) {
      return { ok: false, reason: "duplicate host rejected" };
    }
    seen.add(host.hostPublicId);
    if (host.isPageHost) {
      pageCount += 1;
      if (
        host.hostPublicId !== pageHost.descriptor.hostPublicId ||
        host.hostPublicKey !== pageHost.descriptor.hostPublicKey ||
        host.origin !== pageHost.descriptor.origin
      ) {
        return { ok: false, reason: "page host retarget rejected" };
      }
    } else if (!host.origin.startsWith("https:")) {
      return { ok: false, reason: "remote origin rejected" };
    }
  }
  if (pageCount !== 1) {
    return { ok: false, reason: "page host cardinality rejected" };
  }
  return { ok: true, hosts: nextHosts };
}

/**
 * One NativeHostSession + DeferredNativeTransport per configured descriptor.
 * Constructed inside an effect — never during React render.
 */
export class NativeHostRegistry {
  private readonly entriesById = new Map<string, NativeFleetEntry>();
  private readonly entryOrder: string[] = [];
  private readonly listeners = new Set<() => void>();
  private readonly unsubscribers: Array<() => void> = [];
  private readonly entryUnsubscribers = new Map<string, () => void>();
  private alive = true;
  private started = false;
  private unbindLifecycle: (() => void) | null = null;
  private hiddenAt: number | null =
    typeof document !== "undefined" && document.visibilityState === "hidden"
      ? Date.now()
      : null;
  private readonly options: NativeHostRegistryOptions;
  private readonly now: () => number;
  private readonly createCache: (hostPublicId: string) => NativeCacheStore;
  private documentRemoteOrigins: ReadonlySet<string>;
  private membershipEpochValue = 0;
  private readonly bootstrapInFlight = new WeakMap<NativeFleetEntry, Promise<void>>();
  private readonly sessionStarted = new WeakSet<NativeFleetEntry>();
  private readonly identityHold = new WeakSet<NativeFleetEntry>();

  constructor(options: NativeHostRegistryOptions) {
    if (options.hosts.length === 0 || options.hosts.length > CONNECT_FLEET_MAX_HOSTS) {
      throw new Error("Native host registry capacity rejected");
    }
    this.options = options;
    this.now = options.now ?? Date.now;
    this.createCache =
      options.createCache ?? (() => createIndexedDbNativeCacheStore());
    this.documentRemoteOrigins =
      options.documentRemoteOrigins ??
      new Set(
        options.hosts
          .filter((host) => !host.isPageHost)
          .map((host) => host.origin),
      );

    for (const descriptor of options.hosts) {
      if (this.entriesById.has(descriptor.hostPublicId)) {
        throw new Error("Native host registry duplicate host rejected");
      }
      this.createEntry(descriptor);
    }
  }

  entries(): readonly NativeFleetEntry[] {
    return this.entryOrder.map((id) => this.entriesById.get(id)!);
  }

  get(hostPublicId: string): NativeFleetEntry | undefined {
    return this.entriesById.get(hostPublicId);
  }

  pageHost(): NativeFleetEntry | undefined {
    return this.entries().find((entry) => entry.descriptor.isPageHost);
  }

  /** Bumps only when registry membership adds/removes a host identity. */
  membershipEpoch(): number {
    return this.membershipEpochValue;
  }

  /**
   * Replace the immutable document allowlist (exact HTML fleet meta origins).
   * Does not bootstrap newly permitted remotes by itself — call reconcile.
   */
  setDocumentRemoteOrigins(origins: ReadonlySet<string>): void {
    this.documentRemoteOrigins = origins;
  }

  documentAllowlist(): ReadonlySet<string> {
    return this.documentRemoteOrigins;
  }

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /**
   * Bounded registry reconciliation against a validated descriptor set.
   * Unchanged owner/static/origin keep exact session/transport/cache/outbox.
   * Removed hosts are fenced/disconnected immediately (durable cache retained).
   * Changed static pin is HOLD — never silent trust rotation.
   * Page host is never removed or retargeted.
   */
  reconcileHosts(
    nextHosts: readonly NativeFleetHostDescriptor[],
  ): NativeHostReconcileResult {
    if (!this.alive) {
      return { ok: false, reason: "registry stopped" };
    }
    const validated = validateReconcileDescriptors(nextHosts, this.pageHost());
    if (!validated.ok) return validated;

    let membershipChanged = false;
    let heldPin = false;
    const nextById = new Map(
      validated.hosts.map((host) => [host.hostPublicId, host] as const),
    );

    // Fence removals first — even when a later document reload is unsafe.
    for (const id of [...this.entryOrder]) {
      const existing = this.entriesById.get(id);
      if (!existing) continue;
      if (existing.descriptor.isPageHost) continue;
      if (nextById.has(id)) continue;
      this.fenceAndRemoveEntry(existing);
      membershipChanged = true;
    }

    for (const next of validated.hosts) {
      if (next.isPageHost) {
        const page = this.pageHost();
        if (!page) {
          return { ok: false, reason: "page host missing" };
        }
        // Presentation-only refresh for the page host label/generation.
        if (
          page.descriptor.label !== next.label ||
          page.descriptor.generation !== next.generation ||
          page.descriptor.protocolMinor !== next.protocolMinor
        ) {
          page.descriptor = {
            ...page.descriptor,
            label: next.label,
            generation: next.generation,
            protocolMinor: next.protocolMinor,
          };
        }
        continue;
      }

      const existing = this.entriesById.get(next.hostPublicId);
      if (!existing) {
        const entry = this.createEntry(next);
        membershipChanged = true;
        if (this.started) {
          this.bindEntryListener(entry);
          void this.hydrateEntry(entry);
        }
        continue;
      }

      if (
        existing.descriptor.hostPublicKey !== next.hostPublicKey ||
        existing.descriptor.origin !== next.origin
      ) {
        // Static pin changed — HOLD; do not accept silent trust rotation.
        if (
          this.applyIdentityHold(
            existing,
            "Connect host key changed; explicit trust repair is required.",
          )
        ) {
          heldPin = true;
        }
        continue;
      }

      // Same owner/static/origin — keep exact session/transport/cache/outbox.
      if (
        existing.descriptor.label !== next.label ||
        existing.descriptor.generation !== next.generation ||
        existing.descriptor.protocolMinor !== next.protocolMinor
      ) {
        existing.descriptor = {
          ...existing.descriptor,
          label: next.label,
          generation: next.generation,
          protocolMinor: next.protocolMinor,
        };
      }
    }

    // Stable order: page host first, then remaining nextHosts order.
    const ordered: string[] = [];
    for (const host of validated.hosts) {
      if (this.entriesById.has(host.hostPublicId)) {
        ordered.push(host.hostPublicId);
      }
    }
    for (const id of this.entryOrder) {
      if (!ordered.includes(id) && this.entriesById.has(id)) {
        // Held pin hosts may remain even when next rejected the rotation.
        ordered.push(id);
      }
    }
    this.entryOrder.length = 0;
    this.entryOrder.push(...ordered);

    if (membershipChanged) {
      this.membershipEpochValue += 1;
    }
    this.publish();
    return { ok: true, membershipChanged, heldPin };
  }

  snapshots(): NativeFleetEntrySnapshot[] {
    return this.entries().map((entry) => {
      const view = entry.session.view();
      const authenticated = isAuthenticated(view);
      return {
        descriptor: entry.descriptor,
        view,
        hydrationKnown: entry.hydrationKnown,
        pairingState: this.derivedPairingState(entry, authenticated),
        transportAttached: entry.transportAttached,
        authenticated,
        notice: entry.notice,
        cacheAvailable: hasCachedPresentation(view),
      };
    });
  }

  /** Aggregate PWA update safety across ALL registered hosts. */
  updateSafety(): UpdateSafetyState {
    const views: NativeProjectionSafetyView[] = [];
    let allKnown = true;
    for (const entry of this.entries()) {
      if (!entry.hydrationKnown) allKnown = false;
      views.push(entry.session.view());
    }
    return nativeUpdateSafetyState(views, allKnown);
  }

  private derivedPairingState(
    entry: NativeFleetEntry,
    authenticated: boolean,
  ): NativeFleetPairingState {
    if (this.identityHold.has(entry)) return "held";
    if (authenticated) return "ready";
    if (entry.pairingState === "pairing_required") return "pairing_required";
    if (entry.pairingState === "held") return "held";
    if (entry.pairingState === "retrying") return "retrying";
    if (entry.transportAttached) return "transport_attached";
    return entry.pairingState;
  }

  private publish(): void {
    if (!this.alive) return;
    this.options.onSafetyChange?.(this.updateSafety());
    for (const listener of this.listeners) listener();
  }

  /**
   * Hydrate each host independently (cache-first), start listeners, then
   * attach transports. Slow B cannot block A's cache paint. Idempotent.
   */
  start(): void {
    if (!this.alive || this.started) return;
    this.started = true;
    for (const entry of this.entries()) {
      this.bindEntryListener(entry);
      // Independent hydrate — no Promise.all first-paint bottleneck.
      void this.hydrateEntry(entry);
    }

    this.unbindLifecycle = bindAppLifecycle({
      foreground: () => {
        const hiddenDurationMs =
          this.hiddenAt === null ? 0 : this.now() - this.hiddenAt;
        this.hiddenAt = null;
        for (const entry of this.entries()) {
          // Never wake/bootstrap before session subscriptions are installed.
          if (!this.sessionStarted.has(entry)) continue;
          entry.session.wake({ hiddenDurationMs });
          this.requestBootstrap(entry);
        }
      },
      setVisibility: (visible) => {
        if (!visible) this.hiddenAt ??= this.now();
      },
      suspend: () => {
        this.hiddenAt ??= this.now();
        for (const entry of this.entries()) {
          if (!this.sessionStarted.has(entry)) continue;
          entry.session.suspend();
        }
      },
    });
    this.publish();
  }

  /**
   * Supply a one-time pairing grant for a remote host. Allowed even when a
   * transport is already attached so Pair remains reachable after an unauth
   * attach; replaces only that owner's port.
   */
  submitPairGrant(
    hostPublicId: string,
    grant: ConnectCrossOriginPairGrant,
  ): void {
    const entry = this.entriesById.get(hostPublicId);
    if (!entry || entry.descriptor.isPageHost || !this.alive) return;
    if (this.identityHold.has(entry)) return;
    entry.pendingGrant = { grant: grant.grant, label: grant.label };
    entry.pairingState = "retrying";
    entry.notice = null;
    this.publish();
    this.requestBootstrap(entry, { forceReplace: true });
  }

  retryHost(hostPublicId: string): void {
    const entry = this.entriesById.get(hostPublicId);
    if (!entry || !this.alive) return;
    if (this.identityHold.has(entry)) {
      // Trust/identity corruption stays explicit HOLD.
      return;
    }
    entry.pairingState = "retrying";
    entry.notice = null;
    this.publish();
    // Known-pin resume after a lost pair response must not require a second grant.
    this.requestBootstrap(entry, {
      forceReplace: entry.transportAttached,
    });
  }

  /**
   * Stop only on explicit forget/revoke/app cleanup — never on panel navigation.
   */
  stop(): void {
    if (!this.alive) return;
    this.alive = false;
    this.unbindLifecycle?.();
    this.unbindLifecycle = null;
    for (const unsubscribe of this.unsubscribers) unsubscribe();
    this.unsubscribers.length = 0;
    this.entryUnsubscribers.clear();
    for (const entry of this.entries()) {
      entry.pendingGrant = null;
      entry.session.stop();
    }
    this.listeners.clear();
  }

  private createEntry(descriptor: NativeFleetHostDescriptor): NativeFleetEntry {
    const transport = new DeferredNativeTransport();
    const session = new NativeHostSession({
      hostPublicId: descriptor.hostPublicId,
      transport,
      cache: this.createCache(descriptor.hostPublicId),
    });
    const entry: NativeFleetEntry = {
      descriptor,
      session,
      transport,
      hydrationKnown: false,
      pairingState: "unknown",
      transportAttached: false,
      notice: null,
      pendingGrant: null,
    };
    this.entriesById.set(descriptor.hostPublicId, entry);
    this.entryOrder.push(descriptor.hostPublicId);
    return entry;
  }

  private bindEntryListener(entry: NativeFleetEntry): void {
    const existing = this.entryUnsubscribers.get(entry.descriptor.hostPublicId);
    if (existing) return;
    const unsubscribe = entry.session.subscribe(() => {
      if (!this.alive) return;
      if (this.entriesById.get(entry.descriptor.hostPublicId) !== entry) return;
      // Ordinary reconnect → degraded must not become pairing_required.
      // Pair remains reachable for unauthenticated remotes via Hosts UI.
      // Misbound / identity corruption HOLDs and disables sends.
      if (
        !this.identityHold.has(entry) &&
        entry.session.view().connectionStatus === "misbound"
      ) {
        this.applyIdentityHold(
          entry,
          entry.session.view().lastError ??
            "Connect host binding is misbound; explicit trust repair is required.",
        );
        return;
      }
      this.publish();
    });
    this.entryUnsubscribers.set(entry.descriptor.hostPublicId, unsubscribe);
    this.unsubscribers.push(unsubscribe);
  }

  /**
   * Fence+stop the exact registered entry on identity corruption. Never mutates
   * a replacement that reused the same hostPublicId after remove+readd.
   */
  private applyIdentityHold(entry: NativeFleetEntry, notice: string): boolean {
    if (this.entriesById.get(entry.descriptor.hostPublicId) !== entry) {
      return false;
    }
    this.identityHold.add(entry);
    entry.pendingGrant = null;
    entry.session.fenceTransportReplacement();
    entry.session.stop();
    entry.transportAttached = false;
    entry.pairingState = "held";
    entry.notice = notice;
    this.publish();
    return true;
  }

  private fenceAndRemoveEntry(entry: NativeFleetEntry): void {
    const id = entry.descriptor.hostPublicId;
    const unsubscribe = this.entryUnsubscribers.get(id);
    if (unsubscribe) {
      unsubscribe();
      this.entryUnsubscribers.delete(id);
      const idx = this.unsubscribers.indexOf(unsubscribe);
      if (idx >= 0) this.unsubscribers.splice(idx, 1);
    }
    entry.pendingGrant = null;
    entry.session.fenceTransportReplacement();
    entry.session.stop();
    this.entriesById.delete(id);
    const orderIdx = this.entryOrder.indexOf(id);
    if (orderIdx >= 0) this.entryOrder.splice(orderIdx, 1);
  }

  private mayBootstrap(entry: NativeFleetEntry): boolean {
    if (this.entriesById.get(entry.descriptor.hostPublicId) !== entry || this.identityHold.has(entry)) return false;
    if (entry.descriptor.isPageHost) return true;
    return this.documentRemoteOrigins.has(entry.descriptor.origin);
  }

  private async hydrateEntry(entry: NativeFleetEntry): Promise<void> {
    try {
      await entry.session.hydrate();
      if (!this.alive || this.entriesById.get(entry.descriptor.hostPublicId) !== entry) return;
      entry.hydrationKnown = true;
    } catch (error) {
      if (!this.alive || this.entriesById.get(entry.descriptor.hostPublicId) !== entry) return;
      entry.notice = `Cached view unavailable: ${holdMessage(error)}`;
      entry.hydrationKnown = false;
    } finally {
      this.publish();
    }
    if (!this.alive || this.entriesById.get(entry.descriptor.hostPublicId) !== entry || this.identityHold.has(entry)) return;
    // Mark sessionStarted immediately before start so subscriptions exist
    // before any attach/wake; do not wait for the transport promise.
    this.sessionStarted.add(entry);
    void entry.session.start().catch((error: unknown) => {
      if (!this.alive) return;
      entry.notice = holdMessage(error);
      this.publish();
    });
    if (this.hiddenAt !== null) {
      entry.session.suspend();
      return;
    }
    this.requestBootstrap(entry);
  }

  private requestBootstrap(
    entry: NativeFleetEntry,
    options: { forceReplace?: boolean } = {},
  ): void {
    if (!this.alive) return;
    if (!this.sessionStarted.has(entry)) return;
    if (!this.mayBootstrap(entry)) return;
    if (this.identityHold.has(entry) && !entry.pendingGrant) return;
    if (this.bootstrapInFlight.has(entry)) return;
    // Transport owns reconnect / known-pin Noise+Hello. Never re-run identity
    // bootstrap on wake for an already-attached port unless Pair/Retry forces it.
    if (
      entry.transportAttached &&
      !options.forceReplace &&
      !entry.pendingGrant
    ) {
      return;
    }

    const work = (async () => {
      let handle: ConnectBootstrapHandle | null = null;
      try {
        if (entry.descriptor.isPageHost) {
          handle = this.options.bootstrapPageHost
            ? await this.options.bootstrapPageHost()
            : await bootstrapConnect();
          if (!this.alive) {
            handle?.stop();
            return;
          }
          if (
            !handle ||
            handle.marker.hostPublicId !== entry.descriptor.hostPublicId
          ) {
            handle?.stop();
            this.applyIdentityHold(
              entry,
              "Connect host bootstrap did not bind the selected host.",
            );
            return;
          }
        } else {
          const grant = entry.pendingGrant;
          entry.pendingGrant = null;
          handle = this.options.bootstrapRemoteHost
            ? await this.options.bootstrapRemoteHost(entry, grant)
            : await bootstrapCrossOriginConnect({
                descriptor: entry.descriptor,
                grant,
              });
          if (!this.alive) {
            handle.stop();
            return;
          }
          if (handle.marker.hostPublicId !== entry.descriptor.hostPublicId) {
            handle.stop();
            this.applyIdentityHold(
              entry,
              "Connect cross-origin bootstrap did not bind the selected host.",
            );
            return;
          }
        }

        if (this.entriesById.get(entry.descriptor.hostPublicId) !== entry || this.identityHold.has(entry)) {
          // Removed during reconcile while bootstrap was in flight.
          handle.stop();
          return;
        }

        if (entry.transport.isAttached()) {
          entry.session.fenceTransportReplacement();
          await entry.transport.replace(handle.transport);
        } else {
          await entry.transport.attach(handle.transport);
        }
        if (!this.alive || this.entriesById.get(entry.descriptor.hostPublicId) !== entry || this.identityHold.has(entry)) {
          handle.stop();
          return;
        }
        entry.transportAttached = true;
        // Attached ≠ authenticated. Remotes stay pairable until Hello binds.
        entry.pairingState = "transport_attached";
        entry.notice = null;
        this.publish();
      } catch (error) {
        handle?.stop();
        if (!this.alive) return;
        if (this.entriesById.get(entry.descriptor.hostPublicId) !== entry) return;
        if (this.identityHold.has(entry)) return;
        if (error instanceof ConnectPairingRequiredError) {
          entry.pairingState = "pairing_required";
          entry.notice = null;
          this.publish();
          return;
        }
        if (isIdentityCorruptionHold(error)) {
          this.applyIdentityHold(entry, holdMessage(error));
          return;
        }
        if (isForegroundRetryableBootstrapError(error)) {
          entry.pairingState = "retrying";
        } else if (!entry.descriptor.isPageHost) {
          // Unpaired/unauthenticated remotes stay explicitly pairable.
          entry.pairingState = "pairing_required";
        } else {
          entry.pairingState = "held";
        }
        entry.notice = holdMessage(error);
        this.publish();
      } finally {
        this.bootstrapInFlight.delete(entry);
      }
    })();
    this.bootstrapInFlight.set(entry, work);
  }
}
