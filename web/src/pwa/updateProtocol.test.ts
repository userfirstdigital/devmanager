import { afterEach, describe, expect, it, vi } from "vitest";

import {
  UPDATE_SAFETY_QUERY,
  createLocalReloadGate,
  createWorkerUpdateGate,
  requestWaitingWorkerActivation,
  type WorkerUpdateClient,
} from "./updateProtocol";

describe("worker-wide update safety gate", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  function harness() {
    const clients = new Map<string, WorkerUpdateClient>();
    const skipWaiting = vi.fn(async () => undefined);
    const gate = createWorkerUpdateGate({
      listClients: async () => [...clients.values()],
      skipWaiting,
      ackTimeoutMs: 25,
    });

    const addClient = (
      id: string,
      visibilityState: "visible" | "hidden",
      response: boolean | "silent",
    ) => {
      clients.set(id, {
        id,
        visibilityState,
        postMessage(message) {
          expect(message).toMatchObject({ type: UPDATE_SAFETY_QUERY });
          if (response !== "silent") {
            queueMicrotask(() => gate.acknowledge(message.nonce, id, response));
          }
        },
      });
    };

    return { addClient, clients, gate, skipWaiting };
  }

  it("activates only after two safe tabs ACK the same nonce", async () => {
    const { addClient, gate, skipWaiting } = harness();
    addClient("tab-a", "visible", true);
    addClient("tab-b", "visible", true);

    await expect(gate.requestActivation("nonce-safe")).resolves.toBe(true);
    expect(skipWaiting).toHaveBeenCalledTimes(1);
  });

  it("keeps the worker waiting when either tab reports unsafe state", async () => {
    const { addClient, gate, skipWaiting } = harness();
    addClient("tab-a", "visible", true);
    addClient("tab-b", "hidden", false);

    await expect(gate.requestActivation("nonce-unsafe")).resolves.toBe(false);
    expect(skipWaiting).not.toHaveBeenCalled();
  });

  it("fails closed when a visible tab does not ACK before the timeout", async () => {
    vi.useFakeTimers();
    const { addClient, gate, skipWaiting } = harness();
    addClient("tab-a", "visible", true);
    addClient("tab-b", "visible", "silent");

    const activation = gate.requestActivation("nonce-timeout");
    await vi.advanceTimersByTimeAsync(25);

    await expect(activation).resolves.toBe(false);
    expect(skipWaiting).not.toHaveBeenCalled();
  });

  it("allows a frozen hidden tab to defer its own reload after the timeout", async () => {
    vi.useFakeTimers();
    const { addClient, gate, skipWaiting } = harness();
    addClient("tab-a", "visible", true);
    addClient("tab-b", "hidden", "silent");

    const activation = gate.requestActivation("nonce-hidden");
    await vi.advanceTimersByTimeAsync(25);

    await expect(activation).resolves.toBe(true);
    expect(skipWaiting).toHaveBeenCalledTimes(1);
  });

  it("re-enumerates live clients so a closed tab no longer blocks", async () => {
    vi.useFakeTimers();
    const { addClient, clients, gate, skipWaiting } = harness();
    addClient("tab-a", "visible", true);
    clients.set("closing-tab", {
      id: "closing-tab",
      visibilityState: "visible",
      postMessage() {
        clients.delete("closing-tab");
      },
    });

    const activation = gate.requestActivation("nonce-closed");
    await vi.advanceTimersByTimeAsync(25);

    await expect(activation).resolves.toBe(true);
    expect(skipWaiting).toHaveBeenCalledTimes(1);
  });

  it("queries a tab that appears during the pre-activation re-enumeration", async () => {
    const { addClient, clients, gate, skipWaiting } = harness();
    clients.set("tab-a", {
      id: "tab-a",
      visibilityState: "visible",
      postMessage(message) {
        queueMicrotask(() => {
          gate.acknowledge(message.nonce, "tab-a", true);
          addClient("tab-b", "visible", true);
        });
      },
    });

    await expect(gate.requestActivation("nonce-new-tab")).resolves.toBe(true);
    expect(skipWaiting).toHaveBeenCalledTimes(1);
    expect(clients.get("tab-b")).toBeDefined();
  });

  it("fails closed when a silent hidden tab becomes visible at the final check", async () => {
    vi.useFakeTimers();
    let enumeration = 0;
    const skipWaiting = vi.fn(async () => undefined);
    const gate = createWorkerUpdateGate({
      listClients: async () => {
        enumeration += 1;
        return [
          {
            id: "silent-tab",
            visibilityState: enumeration >= 3 ? "visible" : "hidden",
            postMessage: vi.fn(),
          },
        ];
      },
      skipWaiting,
      ackTimeoutMs: 25,
    });

    const activation = gate.requestActivation("nonce-visible-finally");
    await vi.advanceTimersByTimeAsync(25);

    await expect(activation).resolves.toBe(false);
    expect(skipWaiting).not.toHaveBeenCalled();
  });
});

describe("per-client reload gate", () => {
  it("holds a controller-change reload while hidden or locally unsafe", () => {
    let visible = false;
    let safety = {
      hasDraft: false,
      pendingMutations: 0,
      selectedAttachments: 0,
      attachmentLoads: 0,
    };
    const reload = vi.fn();
    const gate = createLocalReloadGate({
      isVisible: () => visible,
      readSafetyState: () => safety,
      reload,
    });

    expect(gate.notifyControllerChanged()).toBe(false);
    visible = true;
    safety = {
      hasDraft: true,
      pendingMutations: 0,
      selectedAttachments: 0,
      attachmentLoads: 0,
    };
    expect(gate.notifySafePoint()).toBe(false);
    safety = {
      hasDraft: false,
      pendingMutations: 0,
      selectedAttachments: 1,
      attachmentLoads: 0,
    };
    expect(gate.notifySafePoint()).toBe(false);
    safety = {
      hasDraft: false,
      pendingMutations: 0,
      selectedAttachments: 0,
      attachmentLoads: 0,
    };
    expect(gate.notifySafePoint()).toBe(true);
    expect(reload).toHaveBeenCalledTimes(1);
  });
});

describe("activation request result", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("matches the worker result by nonce", async () => {
    let listener: ((event: MessageEvent) => void) | undefined;
    const messages = {
      addEventListener: vi.fn((_type: "message", next: typeof listener) => {
        listener = next;
      }),
      removeEventListener: vi.fn(),
    };
    const worker = { postMessage: vi.fn() };
    const result = requestWaitingWorkerActivation({
      worker,
      messages,
      nonce: "nonce-result",
      timeoutMs: 100,
    });

    listener?.({
      data: {
        type: "DEVMANAGER_UPDATE_ACTIVATION_RESULT",
        nonce: "other-nonce",
        activated: true,
      },
    } as MessageEvent);
    listener?.({
      data: {
        type: "DEVMANAGER_UPDATE_ACTIVATION_RESULT",
        nonce: "nonce-result",
        activated: true,
      },
    } as MessageEvent);

    await expect(result).resolves.toBe(true);
    expect(worker.postMessage).toHaveBeenCalledWith({
      type: "DEVMANAGER_UPDATE_ACTIVATION_REQUEST",
      nonce: "nonce-result",
    });
    expect(messages.removeEventListener).toHaveBeenCalledTimes(1);
  });

  it("fails closed when the worker never returns a result", async () => {
    vi.useFakeTimers();
    const messages = {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    };
    const result = requestWaitingWorkerActivation({
      worker: { postMessage: vi.fn() },
      messages,
      nonce: "nonce-no-result",
      timeoutMs: 50,
    });

    await vi.advanceTimersByTimeAsync(50);
    await expect(result).resolves.toBe(false);
  });
});
