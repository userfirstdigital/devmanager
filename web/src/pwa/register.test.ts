import { describe, expect, it, vi } from "vitest";
import {
  canActivateUpdate,
  claimCompatibleBuildReloadAttempt,
  createCompatibleBuildRecoveryCoordinator,
  createPwaUpdateCoordinator,
  createViteServiceWorkerRegistrar,
  defaultUpdateSafetyState,
  isPwaRuntimeSupported,
  registerPwaServiceWorker,
  type ViteServiceWorkerRegistrationOptions,
} from "./register";

describe("canActivateUpdate", () => {
  it("defers activation while a composer draft exists", () => {
    expect(canActivateUpdate({ hasDraft: true, pendingMutations: 0 })).toBe(false);
  });

  it("defers activation while a mutation is pending", () => {
    expect(canActivateUpdate({ hasDraft: false, pendingMutations: 1 })).toBe(false);
  });

  it("defers activation while attachments are selected or loading", () => {
    expect(
      canActivateUpdate({
        hasDraft: false,
        pendingMutations: 0,
        selectedAttachments: 1,
        attachmentLoads: 0,
      }),
    ).toBe(false);
    expect(
      canActivateUpdate({
        hasDraft: false,
        pendingMutations: 0,
        selectedAttachments: 0,
        attachmentLoads: 1,
      }),
    ).toBe(false);
  });

  it("allows activation when there is no draft or pending mutation", () => {
    expect(canActivateUpdate({ hasDraft: false, pendingMutations: 0 })).toBe(true);
  });

  it("fails closed when no draft/mutation integration is available", () => {
    expect(defaultUpdateSafetyState()).toEqual({
      hasDraft: true,
      pendingMutations: 0,
      selectedAttachments: 0,
      attachmentLoads: 0,
    });
    expect(canActivateUpdate(defaultUpdateSafetyState())).toBe(false);
  });
});

describe("createPwaUpdateCoordinator", () => {
  it("keeps an update waiting until a later safe activation point", async () => {
    let safety = { hasDraft: true, pendingMutations: 0 };
    const activateUpdate = vi.fn(async () => undefined);
    const coordinator = createPwaUpdateCoordinator(
      () => safety,
      activateUpdate,
    );

    expect(await coordinator.notifyUpdateWaiting()).toBe(false);
    expect(coordinator.hasWaitingUpdate()).toBe(true);
    expect(activateUpdate).not.toHaveBeenCalled();

    safety = { hasDraft: false, pendingMutations: 0 };
    expect(await coordinator.activateWaitingUpdateIfSafe()).toBe(true);
    expect(coordinator.hasWaitingUpdate()).toBe(false);
    expect(activateUpdate).toHaveBeenCalledTimes(1);
  });

  it("does not activate while any mutation remains pending", async () => {
    const activateUpdate = vi.fn(async () => undefined);
    const coordinator = createPwaUpdateCoordinator(
      () => ({ hasDraft: false, pendingMutations: 2 }),
      activateUpdate,
    );

    expect(await coordinator.notifyUpdateWaiting()).toBe(false);
    expect(await coordinator.activateWaitingUpdateIfSafe()).toBe(false);
    expect(activateUpdate).not.toHaveBeenCalled();
  });

  it("only marks a safe update waiting until an explicit safe point", async () => {
    const activateUpdate = vi.fn(async () => undefined);
    const coordinator = createPwaUpdateCoordinator(
      () => ({ hasDraft: false, pendingMutations: 0 }),
      activateUpdate,
    );

    expect(await coordinator.notifyUpdateWaiting()).toBe(false);
    expect(coordinator.hasWaitingUpdate()).toBe(true);
    expect(activateUpdate).not.toHaveBeenCalled();

    expect(await coordinator.activateWaitingUpdateIfSafe()).toBe(true);
    expect(activateUpdate).toHaveBeenCalledTimes(1);
  });
});

describe("isPwaRuntimeSupported", () => {
  it("requires both a secure context and service-worker support", () => {
    expect(
      isPwaRuntimeSupported({
        isSecureContext: false,
        serviceWorkerAvailable: true,
      }),
    ).toBe(false);
    expect(
      isPwaRuntimeSupported({
        isSecureContext: true,
        serviceWorkerAvailable: false,
      }),
    ).toBe(false);
    expect(
      isPwaRuntimeSupported({
        isSecureContext: true,
        serviceWorkerAvailable: true,
      }),
    ).toBe(true);
  });
});

describe("registerPwaServiceWorker", () => {
  it("uses the waiting worker protocol and delegates controller reload locally", async () => {
    let viteOptions: ViteServiceWorkerRegistrationOptions | undefined;
    const viteUpdate = vi.fn(async () => undefined);
    const waitingWorker = { postMessage: vi.fn() };
    const requestActivation = vi.fn(async () => true);
    const onNeedReload = vi.fn();
    const registerSW = vi.fn(
      (options: ViteServiceWorkerRegistrationOptions) => {
        viteOptions = options;
        return viteUpdate;
      },
    );
    const registration = registerPwaServiceWorker({
      capabilities: {
        isSecureContext: true,
        serviceWorkerAvailable: true,
      },
      readSafetyState: () => ({ hasDraft: false, pendingMutations: 0 }),
      isVisible: () => true,
      onNeedReload,
      registerServiceWorker: createViteServiceWorkerRegistrar(
        registerSW,
        requestActivation,
      ),
    });

    viteOptions?.onRegisteredSW("/sw.js", {
      waiting: waitingWorker,
      update: vi.fn(async () => undefined),
    });
    expect(await registration.notifyStartupNavigationSafePoint()).toBe(true);
    expect(requestActivation).toHaveBeenCalledWith(waitingWorker);
    expect(viteUpdate).not.toHaveBeenCalled();

    viteOptions?.onNeedReload?.();
    expect(onNeedReload).toHaveBeenCalledTimes(1);
  });

  it("leaves the plain HTTP UI operating without registering a worker", () => {
    const registerServiceWorker = vi.fn();

    const registration = registerPwaServiceWorker({
      capabilities: {
        isSecureContext: false,
        serviceWorkerAvailable: true,
      },
      readSafetyState: () => ({ hasDraft: false, pendingMutations: 0 }),
      registerServiceWorker,
    });

    expect(registration.supported).toBe(false);
    expect(registerServiceWorker).not.toHaveBeenCalled();
  });

  it("keeps a discovered update waiting until the injected state is safe", async () => {
    let safety = { hasDraft: true, pendingMutations: 0 };
    let onNeedRefresh: (() => void) | undefined;
    const updateServiceWorker = vi.fn(async () => undefined);
    const registerServiceWorker = vi.fn(
      (options: { onNeedRefresh: () => void }) => {
        onNeedRefresh = options.onNeedRefresh;
        return updateServiceWorker;
      },
    );

    const registration = registerPwaServiceWorker({
      capabilities: {
        isSecureContext: true,
        serviceWorkerAvailable: true,
      },
      readSafetyState: () => safety,
      registerServiceWorker,
    });

    onNeedRefresh?.();
    await Promise.resolve();
    expect(registration.hasWaitingUpdate()).toBe(true);
    expect(updateServiceWorker).not.toHaveBeenCalled();

    safety = { hasDraft: false, pendingMutations: 0 };
    expect(await registration.activateWaitingUpdateIfSafe()).toBe(true);
    expect(updateServiceWorker).toHaveBeenCalledWith();
  });

  it("survives a synchronous update callback and still uses the returned updater", async () => {
    const updateServiceWorker = vi.fn(async () => undefined);
    const registerServiceWorker = vi.fn(
      (options: { onNeedRefresh: () => void }) => {
        options.onNeedRefresh();
        return updateServiceWorker;
      },
    );

    const registration = registerPwaServiceWorker({
      capabilities: {
        isSecureContext: true,
        serviceWorkerAvailable: true,
      },
      readSafetyState: () => ({ hasDraft: false, pendingMutations: 0 }),
      registerServiceWorker,
    });

    await Promise.resolve();
    expect(registration.hasWaitingUpdate()).toBe(true);
    expect(updateServiceWorker).not.toHaveBeenCalled();
    expect(await registration.activateWaitingUpdateIfSafe()).toBe(true);
    expect(updateServiceWorker).toHaveBeenCalledWith();
  });

  it("uses a startup pageshow that occurs before initial waiting-worker discovery", async () => {
    let onNeedRefresh: (() => void) | undefined;
    let onRegistered: ((hasWaitingWorker: boolean) => void) | undefined;
    const updateServiceWorker = vi.fn(async () => undefined);
    const registration = registerPwaServiceWorker({
      capabilities: {
        isSecureContext: true,
        serviceWorkerAvailable: true,
      },
      readSafetyState: () => ({ hasDraft: false, pendingMutations: 0 }),
      isVisible: () => true,
      registerServiceWorker: vi.fn((options) => {
        onNeedRefresh = options.onNeedRefresh;
        onRegistered = options.onRegistered;
        return updateServiceWorker;
      }),
    });

    expect(await registration.notifyStartupNavigationSafePoint()).toBe(false);
    onNeedRefresh?.();
    await Promise.resolve();
    await Promise.resolve();

    expect(updateServiceWorker).toHaveBeenCalledTimes(1);
    expect(updateServiceWorker).toHaveBeenCalledWith();
    onRegistered?.(true);
    await Promise.resolve();
    expect(updateServiceWorker).toHaveBeenCalledTimes(1);
  });

  it("uses the initial pageshow when waiting-worker discovery occurs first", async () => {
    let onNeedRefresh: (() => void) | undefined;
    const updateServiceWorker = vi.fn(async () => undefined);
    const registration = registerPwaServiceWorker({
      capabilities: {
        isSecureContext: true,
        serviceWorkerAvailable: true,
      },
      readSafetyState: () => ({ hasDraft: false, pendingMutations: 0 }),
      isVisible: () => true,
      registerServiceWorker: vi.fn((options) => {
        onNeedRefresh = options.onNeedRefresh;
        return updateServiceWorker;
      }),
    });

    onNeedRefresh?.();
    await Promise.resolve();
    expect(updateServiceWorker).not.toHaveBeenCalled();

    expect(await registration.notifyStartupNavigationSafePoint()).toBe(true);
    expect(updateServiceWorker).toHaveBeenCalledTimes(1);
  });

  it("does not reuse a completed startup safe point for later discoveries", async () => {
    let onNeedRefresh: (() => void) | undefined;
    let onRegistered: ((hasWaitingWorker: boolean) => void) | undefined;
    const updateServiceWorker = vi.fn(async () => undefined);
    const registration = registerPwaServiceWorker({
      capabilities: {
        isSecureContext: true,
        serviceWorkerAvailable: true,
      },
      readSafetyState: () => ({ hasDraft: false, pendingMutations: 0 }),
      isVisible: () => true,
      registerServiceWorker: vi.fn((options) => {
        onNeedRefresh = options.onNeedRefresh;
        onRegistered = options.onRegistered;
        return updateServiceWorker;
      }),
    });

    await registration.notifyStartupNavigationSafePoint();
    onRegistered?.(false);
    onNeedRefresh?.();
    await Promise.resolve();
    expect(updateServiceWorker).not.toHaveBeenCalled();

    expect(await registration.notifyForegroundSafePoint()).toBe(true);
    expect(updateServiceWorker).toHaveBeenCalledTimes(1);
  });

  it.each([
    { hasDraft: true, pendingMutations: 0 },
    { hasDraft: false, pendingMutations: 1 },
  ])("leaves unsafe startup updates waiting for a later safe point: %o", async (unsafe) => {
    let safety = unsafe;
    let onNeedRefresh: (() => void) | undefined;
    const updateServiceWorker = vi.fn(async () => undefined);
    const registration = registerPwaServiceWorker({
      capabilities: {
        isSecureContext: true,
        serviceWorkerAvailable: true,
      },
      readSafetyState: () => safety,
      isVisible: () => true,
      registerServiceWorker: vi.fn((options) => {
        onNeedRefresh = options.onNeedRefresh;
        return updateServiceWorker;
      }),
    });

    await registration.notifyStartupNavigationSafePoint();
    onNeedRefresh?.();
    await Promise.resolve();
    expect(updateServiceWorker).not.toHaveBeenCalled();
    expect(registration.hasWaitingUpdate()).toBe(true);

    safety = { hasDraft: false, pendingMutations: 0 };
    expect(await registration.notifyForegroundSafePoint()).toBe(true);
    expect(updateServiceWorker).toHaveBeenCalledTimes(1);
  });

  it("deduplicates initial waiting discovery when the Vite registration callback fires first", async () => {
    let viteOptions: ViteServiceWorkerRegistrationOptions | undefined;
    const updateServiceWorker = vi.fn(async () => undefined);
    const registerSW = vi.fn(
      (options: ViteServiceWorkerRegistrationOptions) => {
        viteOptions = options;
        return updateServiceWorker;
      },
    );
    const registration = registerPwaServiceWorker({
      capabilities: {
        isSecureContext: true,
        serviceWorkerAvailable: true,
      },
      readSafetyState: () => ({ hasDraft: false, pendingMutations: 0 }),
      isVisible: () => true,
      registerServiceWorker: createViteServiceWorkerRegistrar(
        registerSW,
        async () => {
          await updateServiceWorker();
          return true;
        },
      ),
    });
    const serviceWorkerRegistration = {
      waiting: {},
      update: vi.fn(async () => undefined),
    };

    viteOptions?.onRegisteredSW("/sw.js", serviceWorkerRegistration);
    viteOptions?.onNeedRefresh();
    expect(await registration.notifyStartupNavigationSafePoint()).toBe(true);
    await Promise.resolve();
    expect(updateServiceWorker).toHaveBeenCalledTimes(1);

    expect(await registration.notifyForegroundSafePoint()).toBe(false);
    expect(updateServiceWorker).toHaveBeenCalledTimes(1);
  });

  it("deduplicates initial waiting discovery when Vite reports refresh before registration", async () => {
    let viteOptions: ViteServiceWorkerRegistrationOptions | undefined;
    const updateServiceWorker = vi.fn(async () => undefined);
    const registerSW = vi.fn(
      (options: ViteServiceWorkerRegistrationOptions) => {
        viteOptions = options;
        return updateServiceWorker;
      },
    );
    const registration = registerPwaServiceWorker({
      capabilities: {
        isSecureContext: true,
        serviceWorkerAvailable: true,
      },
      readSafetyState: () => ({ hasDraft: false, pendingMutations: 0 }),
      isVisible: () => true,
      registerServiceWorker: createViteServiceWorkerRegistrar(
        registerSW,
        async () => {
          await updateServiceWorker();
          return true;
        },
      ),
    });
    const serviceWorkerRegistration = {
      waiting: {},
      update: vi.fn(async () => undefined),
    };

    expect(await registration.notifyStartupNavigationSafePoint()).toBe(false);
    viteOptions?.onNeedRefresh();
    viteOptions?.onRegisteredSW("/sw.js", serviceWorkerRegistration);
    await Promise.resolve();
    await Promise.resolve();
    expect(updateServiceWorker).toHaveBeenCalledTimes(1);

    expect(await registration.notifyForegroundSafePoint()).toBe(false);
    expect(updateServiceWorker).toHaveBeenCalledTimes(1);
  });

  it("does not swallow a later Vite waiting worker when initial discovery only used registration", async () => {
    let viteOptions: ViteServiceWorkerRegistrationOptions | undefined;
    const updateServiceWorker = vi.fn(async () => undefined);
    const registerSW = vi.fn(
      (options: ViteServiceWorkerRegistrationOptions) => {
        viteOptions = options;
        return updateServiceWorker;
      },
    );
    const registration = registerPwaServiceWorker({
      capabilities: {
        isSecureContext: true,
        serviceWorkerAvailable: true,
      },
      readSafetyState: () => ({ hasDraft: false, pendingMutations: 0 }),
      isVisible: () => true,
      registerServiceWorker: createViteServiceWorkerRegistrar(
        registerSW,
        async () => {
          await updateServiceWorker();
          return true;
        },
      ),
    });
    const firstWaitingWorker = {};
    const secondWaitingWorker = {};
    const serviceWorkerRegistration = {
      waiting: firstWaitingWorker,
      update: vi.fn(async () => undefined),
    };

    viteOptions?.onRegisteredSW("/sw.js", serviceWorkerRegistration);
    expect(await registration.notifyStartupNavigationSafePoint()).toBe(true);
    expect(updateServiceWorker).toHaveBeenCalledTimes(1);

    serviceWorkerRegistration.waiting = secondWaitingWorker;
    viteOptions?.onNeedRefresh();
    await Promise.resolve();
    expect(updateServiceWorker).toHaveBeenCalledTimes(1);
    expect(registration.hasWaitingUpdate()).toBe(true);

    expect(await registration.notifyForegroundSafePoint()).toBe(true);
    expect(updateServiceWorker).toHaveBeenCalledTimes(2);
  });
});

describe("compatible build recovery", () => {
  it("preserves unsafe work, then updates and activates through the controlling worker", async () => {
    let safety = { hasDraft: true, pendingMutations: 0 };
    let waiting = false;
    const requestUpdate = vi.fn(async () => {
      waiting = true;
      return true;
    });
    const activateWaitingUpdateIfSafe = vi.fn(async () => {
      if (!waiting || !canActivateUpdate(safety)) return false;
      waiting = false;
      return true;
    });
    const reloadPage = vi.fn();
    const recovery = createCompatibleBuildRecoveryCoordinator({
      clientBuildId: "client-build",
      readSafetyState: () => safety,
      isVisible: () => true,
      hasControllingServiceWorker: () => true,
      claimHardReloadAttempt: () => true,
      reloadPage,
    });
    recovery.attachRegistration({
      supported: true,
      requestUpdate,
      hasWaitingUpdate: () => waiting,
      activateWaitingUpdateIfSafe,
    });

    expect(await recovery.requestCompatibleBuild("host-build")).toBe(false);
    expect(requestUpdate).not.toHaveBeenCalled();
    expect(reloadPage).not.toHaveBeenCalled();

    safety = { hasDraft: false, pendingMutations: 0 };
    expect(await recovery.notifySafePoint()).toBe(true);
    expect(requestUpdate).toHaveBeenCalledTimes(1);
    expect(activateWaitingUpdateIfSafe).toHaveBeenCalledTimes(1);
    expect(reloadPage).not.toHaveBeenCalled();
  });

  it("hard reloads an uncontrolled page once and reports a blocked loop", async () => {
    const reloadPage = vi.fn();
    const onReloadLoopBlocked = vi.fn();
    let allowReload = true;
    const recovery = createCompatibleBuildRecoveryCoordinator({
      clientBuildId: "client-build",
      readSafetyState: () => ({ hasDraft: false, pendingMutations: 0 }),
      isVisible: () => true,
      hasControllingServiceWorker: () => false,
      claimHardReloadAttempt: () => allowReload,
      reloadPage,
      onReloadLoopBlocked,
    });

    expect(await recovery.requestCompatibleBuild("host-build")).toBe(true);
    expect(reloadPage).toHaveBeenCalledTimes(1);

    allowReload = false;
    expect(await recovery.requestCompatibleBuild("host-build")).toBe(false);
    expect(reloadPage).toHaveBeenCalledTimes(1);
    expect(onReloadLoopBlocked).toHaveBeenCalledWith(
      "client-build",
      "host-build",
    );
  });

  it("retries update discovery at a later safe point after no worker was waiting", async () => {
    let waiting = false;
    const requestUpdate = vi.fn(async () => {
      if (requestUpdate.mock.calls.length === 2) waiting = true;
      return waiting;
    });
    const activateWaitingUpdateIfSafe = vi.fn(async () => {
      if (!waiting) return false;
      waiting = false;
      return true;
    });
    const recovery = createCompatibleBuildRecoveryCoordinator({
      clientBuildId: "client-build",
      readSafetyState: () => ({ hasDraft: false, pendingMutations: 0 }),
      isVisible: () => true,
      hasControllingServiceWorker: () => true,
      claimHardReloadAttempt: () => true,
      reloadPage: vi.fn(),
    });
    recovery.attachRegistration({
      supported: true,
      requestUpdate,
      hasWaitingUpdate: () => waiting,
      activateWaitingUpdateIfSafe,
    });

    expect(await recovery.requestCompatibleBuild("host-build")).toBe(false);
    expect(requestUpdate).toHaveBeenCalledTimes(1);

    expect(await recovery.notifySafePoint()).toBe(true);
    expect(requestUpdate).toHaveBeenCalledTimes(2);
    expect(activateWaitingUpdateIfSafe).toHaveBeenCalledTimes(1);
  });

  it("persists one hard-reload attempt per client/host build pair", () => {
    const values = new Map<string, string>();
    const storage = {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
    };

    expect(
      claimCompatibleBuildReloadAttempt(storage, "client-a", "host-b"),
    ).toBe(true);
    expect(
      claimCompatibleBuildReloadAttempt(storage, "client-a", "host-b"),
    ).toBe(false);
    expect(
      claimCompatibleBuildReloadAttempt(storage, "client-a", "host-c"),
    ).toBe(true);
  });
});
