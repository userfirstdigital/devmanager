import type {
  ConnectConnectionState,
  ConnectRequestOptions,
  DecodedConnectEnvelope,
} from "./transport";

/** A bounded boot-time port for the one retained native host session. */
export interface NativeTransportPort {
  start(): Promise<void>;
  stop(): void;
  subscribe(listener: (state: ConnectConnectionState) => void): () => void;
  subscribeEnvelope(
    listener: (envelope: DecodedConnectEnvelope) => void,
  ): () => void;
  request(
    payloadKind: number,
    payload: unknown,
    options?: ConnectRequestOptions,
  ): Promise<DecodedConnectEnvelope>;
  suspend?(): void;
  wake?(input?: { hiddenDurationMs?: number }): unknown;
  requestResync?(reason?: "gap" | "replay_unavailable"): boolean;
}

const MAX_DEFERRED_LISTENERS = 8;

export class DeferredNativeTransportError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "DeferredNativeTransportError";
  }
}

/**
 * Lets the native projection hydrate and subscribe before crypto/pairing work
 * completes. It has no outbound queue: actions remain unavailable until the
 * exact boot transport is attached, avoiding an unbounded or retargetable
 * pre-trust command lane.
 */
export class DeferredNativeTransport implements NativeTransportPort {
  private actual: NativeTransportPort | null = null;
  private requestedStart = false;
  private stopped = false;
  private suspended = false;
  private startPromise: Promise<void> | null = null;
  private resolveStart: (() => void) | null = null;
  private rejectStart: ((error: unknown) => void) | null = null;
  private unsubscribeState: (() => void) | null = null;
  private unsubscribeEnvelope: (() => void) | null = null;
  private readonly stateListeners = new Set<
    (state: ConnectConnectionState) => void
  >();
  private readonly envelopeListeners = new Set<
    (envelope: DecodedConnectEnvelope) => void
  >();

  subscribe(listener: (state: ConnectConnectionState) => void): () => void {
    this.assertListenerCapacity(this.stateListeners);
    this.stateListeners.add(listener);
    return () => this.stateListeners.delete(listener);
  }

  subscribeEnvelope(
    listener: (envelope: DecodedConnectEnvelope) => void,
  ): () => void {
    this.assertListenerCapacity(this.envelopeListeners);
    this.envelopeListeners.add(listener);
    return () => this.envelopeListeners.delete(listener);
  }

  async start(): Promise<void> {
    if (this.stopped) return;
    this.requestedStart = true;
    this.suspended = false;
    if (this.actual) return this.actual.start();
    return this.awaitAttach();
  }

  async attach(actual: NativeTransportPort): Promise<void> {
    if (this.actual) {
      throw new DeferredNativeTransportError(
        "native boot transport already attached",
      );
    }
    // A bootstrap finishing after entry teardown must not retain callbacks on
    // the real transport. It owns no viable session and may only close it.
    if (this.stopped) {
      actual.stop();
      this.resolvePendingStart();
      return;
    }
    this.actual = actual;
    this.unsubscribeState = actual.subscribe((state) => {
      for (const listener of this.stateListeners) listener(state);
    });
    this.unsubscribeEnvelope = actual.subscribeEnvelope((envelope) => {
      for (const listener of this.envelopeListeners) listener(envelope);
    });

    if (!this.requestedStart || this.suspended) {
      this.resolvePendingStart();
      return;
    }
    try {
      await actual.start();
      this.resolvePendingStart();
    } catch (error) {
      this.rejectPendingStart(error);
      throw error;
    }
  }

  /**
   * Replace the attached port for per-host re-authorization. Detaches and
   * fences the previous callbacks/socket without stopping the deferred port
   * the session still owns.
   */
  async replace(actual: NativeTransportPort): Promise<void> {
    if (this.stopped) {
      actual.stop();
      return;
    }
    this.unsubscribeState?.();
    this.unsubscribeState = null;
    this.unsubscribeEnvelope?.();
    this.unsubscribeEnvelope = null;
    const previous = this.actual;
    this.actual = null;
    try {
      previous?.stop();
    } catch {
      // Best-effort fence; the replacement owns the next channel.
    }
    await this.attach(actual);
  }

  /** True when a real transport is currently wired. */
  isAttached(): boolean {
    return this.actual !== null && !this.stopped;
  }

  stop(): void {
    if (this.stopped) return;
    this.stopped = true;
    this.requestedStart = false;
    this.unsubscribeState?.();
    this.unsubscribeState = null;
    this.unsubscribeEnvelope?.();
    this.unsubscribeEnvelope = null;
    this.actual?.stop();
    // NativeHostSession start is intentionally fire-and-forget at entry; do not
    // leave an unhandled deferred promise behind on a fast page teardown.
    this.resolvePendingStart();
    this.stateListeners.clear();
    this.envelopeListeners.clear();
  }

  suspend(): void {
    if (this.stopped) return;
    this.suspended = true;
    this.actual?.suspend?.();
  }

  wake(input?: { hiddenDurationMs?: number }): unknown {
    if (this.stopped) return "held";
    this.suspended = false;
    if (!this.actual) {
      this.requestedStart = true;
      return "start";
    }
    return this.actual.wake?.(input) ?? this.actual.start();
  }

  requestResync(reason?: "gap" | "replay_unavailable"): boolean {
    return this.actual?.requestResync?.(reason) ?? false;
  }

  request(
    payloadKind: number,
    payload: unknown,
    options?: ConnectRequestOptions,
  ): Promise<DecodedConnectEnvelope> {
    if (!this.actual || this.stopped) {
      return Promise.reject(
        new DeferredNativeTransportError(
          "native transport is not attached; action was not sent",
        ),
      );
    }
    return this.actual.request(payloadKind, payload, options);
  }

  private awaitAttach(): Promise<void> {
    this.startPromise ??= new Promise<void>((resolve, reject) => {
      this.resolveStart = resolve;
      this.rejectStart = reject;
    });
    return this.startPromise;
  }

  private resolvePendingStart(): void {
    this.resolveStart?.();
    this.clearPendingStart();
  }

  private rejectPendingStart(error: unknown): void {
    this.rejectStart?.(error);
    this.clearPendingStart();
  }

  private clearPendingStart(): void {
    this.startPromise = null;
    this.resolveStart = null;
    this.rejectStart = null;
  }

  private assertListenerCapacity(listeners: Set<unknown>): void {
    if (listeners.size >= MAX_DEFERRED_LISTENERS) {
      throw new DeferredNativeTransportError("native deferred listener limit exceeded");
    }
  }
}
