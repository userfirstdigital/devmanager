import { describe, expect, it, vi } from "vitest";
import type {
  ConnectConnectionState,
  DecodedConnectEnvelope,
} from "./transport";

import {
  DeferredNativeTransport,
  DeferredNativeTransportError,
} from "./deferredNativeTransport";

const ready = { kind: "ready" } as const;

function actualTransport() {
  let stateListener: ((state: ConnectConnectionState) => void) | null = null;
  let envelopeListener: ((envelope: DecodedConnectEnvelope) => void) | null = null;
  return {
    start: vi.fn(async () => undefined),
    stop: vi.fn(),
    suspend: vi.fn(),
    wake: vi.fn(() => "start"),
    requestResync: vi.fn(() => true),
    request: vi.fn(async () => ({ payload: { ok: true } } as DecodedConnectEnvelope)),
    subscribe: vi.fn((listener: (state: ConnectConnectionState) => void) => {
      stateListener = listener;
      listener(ready);
      return () => {
        stateListener = null;
      };
    }),
    subscribeEnvelope: vi.fn((listener: (envelope: DecodedConnectEnvelope) => void) => {
      envelopeListener = listener;
      return () => {
        envelopeListener = null;
      };
    }),
    emitState: () => stateListener?.(ready),
    emitEnvelope: (envelope: DecodedConnectEnvelope) => envelopeListener?.(envelope),
  };
}

describe("DeferredNativeTransport", () => {
  it("retains one subscription and starts it exactly once when boot transport attaches", async () => {
    const deferred = new DeferredNativeTransport();
    const onState = vi.fn();
    const onEnvelope = vi.fn();
    deferred.subscribe(onState);
    deferred.subscribeEnvelope(onEnvelope);

    const pending = deferred.start();
    const actual = actualTransport();
    await deferred.attach(actual);
    await pending;

    expect(actual.subscribe).toHaveBeenCalledTimes(1);
    expect(actual.subscribeEnvelope).toHaveBeenCalledTimes(1);
    expect(actual.start).toHaveBeenCalledTimes(1);
    expect(onState).toHaveBeenCalledWith(ready);
    actual.emitEnvelope({ payload: "incremental" } as DecodedConnectEnvelope);
    expect(onEnvelope).toHaveBeenCalledWith(expect.objectContaining({ payload: "incremental" }));
  });

  it("never puts application requests on a transport that has not attached", async () => {
    const deferred = new DeferredNativeTransport();
    await expect(deferred.request(7, {})).rejects.toBeInstanceOf(
      DeferredNativeTransportError,
    );
  });

  it("rejects a second attach rather than retargeting a retained host session", async () => {
    const deferred = new DeferredNativeTransport();
    await deferred.attach(actualTransport());
    await expect(deferred.attach(actualTransport())).rejects.toBeInstanceOf(
      DeferredNativeTransportError,
    );
  });

  it("stops an attached transport and never starts it after page teardown", async () => {
    const deferred = new DeferredNativeTransport();
    const pending = deferred.start();
    deferred.stop();
    await pending;
    const actual = actualTransport();
    await deferred.attach(actual);

    expect(actual.stop).toHaveBeenCalledTimes(1);
    expect(actual.start).not.toHaveBeenCalled();
    expect(actual.subscribe).not.toHaveBeenCalled();
    expect(actual.subscribeEnvelope).not.toHaveBeenCalled();
  });
});
