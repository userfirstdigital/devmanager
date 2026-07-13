import { CLIENT_WEB_BUILD_ID } from "./buildCompatibility";
import {
  UPDATE_SAFETY_ACK,
  createLocalReloadGate,
  isUpdateSafetyQuery,
  requestWaitingWorkerActivation,
} from "./updateProtocol";

export const WEB_BUILD_ID = CLIENT_WEB_BUILD_ID;
const COMPATIBLE_BUILD_RELOAD_GUARD_KEY =
  "devmanager-compatible-build-reload-attempt";

export interface UpdateSafetyState {
  hasDraft: boolean;
  pendingMutations: number;
  selectedAttachments?: number;
  attachmentLoads?: number;
}

export interface PwaRuntimeCapabilities {
  isSecureContext: boolean;
  serviceWorkerAvailable: boolean;
}

export interface ServiceWorkerRegistrationOptions {
  immediate: boolean;
  onNeedRefresh: (waitingWorker?: object | null) => void;
  onNeedReload: () => void;
  onRegistered: (
    hasWaitingWorker: boolean,
    requestUpdate?: () => Promise<object | null>,
    waitingWorker?: object | null,
  ) => void;
}

export type RegisterServiceWorker = (
  options: ServiceWorkerRegistrationOptions,
) => (reloadPage?: boolean) => Promise<boolean | void>;

export interface PwaRegistration {
  supported: boolean;
  activateWaitingUpdateIfSafe: () => Promise<boolean>;
  hasWaitingUpdate: () => boolean;
  requestUpdate: () => Promise<boolean>;
  notifyStartupNavigationSafePoint: () => Promise<boolean>;
  notifyForegroundSafePoint: () => Promise<boolean>;
}

export interface ViteServiceWorkerRegistration {
  waiting: object | null;
  update: () => Promise<void>;
}

export interface ViteServiceWorkerRegistrationOptions {
  immediate: boolean;
  onNeedRefresh: () => void;
  onNeedReload?: () => void;
  onRegisteredSW: (
    scriptUrl: string,
    registration: ViteServiceWorkerRegistration | undefined,
  ) => void;
}

export type ViteRegisterServiceWorker = (
  options: ViteServiceWorkerRegistrationOptions,
) => (reloadPage?: boolean) => Promise<void>;

interface CompatibleBuildRegistration {
  supported: boolean;
  activateWaitingUpdateIfSafe: () => Promise<boolean>;
  hasWaitingUpdate: () => boolean;
  requestUpdate: () => Promise<boolean>;
}

interface ReloadGuardStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

export function canActivateUpdate({
  hasDraft,
  pendingMutations,
  selectedAttachments = 0,
  attachmentLoads = 0,
}: UpdateSafetyState): boolean {
  return (
    !hasDraft &&
    pendingMutations === 0 &&
    selectedAttachments === 0 &&
    attachmentLoads === 0
  );
}

export function isPwaRuntimeSupported({
  isSecureContext,
  serviceWorkerAvailable,
}: PwaRuntimeCapabilities): boolean {
  return isSecureContext && serviceWorkerAvailable;
}

export function createViteServiceWorkerRegistrar(
  registerSW: ViteRegisterServiceWorker,
  requestActivation: (waitingWorker: object) => Promise<boolean>,
): RegisterServiceWorker {
  return (options) => {
    let registration: ViteServiceWorkerRegistration | undefined;
    registerSW({
      immediate: options.immediate,
      onNeedRefresh: () => options.onNeedRefresh(registration?.waiting),
      onNeedReload: options.onNeedReload,
      onRegisteredSW: (_scriptUrl, nextRegistration) => {
        registration = nextRegistration;
        options.onRegistered(
          Boolean(registration?.waiting),
          async () => {
            if (!registration) return null;
            await registration.update();
            return registration.waiting;
          },
          registration?.waiting,
        );
      },
    });
    return async () => {
      const waitingWorker = registration?.waiting;
      if (!waitingWorker) return false;
      return requestActivation(waitingWorker);
    };
  };
}

export function claimCompatibleBuildReloadAttempt(
  storage: ReloadGuardStorage,
  clientBuildId: string,
  hostBuildId: string,
): boolean {
  const pair = `${clientBuildId}->${hostBuildId}`;
  try {
    const saved = storage.getItem(COMPATIBLE_BUILD_RELOAD_GUARD_KEY);
    if (saved) {
      const parsed = JSON.parse(saved) as { pair?: unknown; count?: unknown };
      if (parsed.pair === pair && parsed.count === 1) return false;
    }
    storage.setItem(
      COMPATIBLE_BUILD_RELOAD_GUARD_KEY,
      JSON.stringify({ pair, count: 1 }),
    );
    return true;
  } catch {
    // Without durable session storage, reloading could loop forever.
    return false;
  }
}

export function createCompatibleBuildRecoveryCoordinator({
  clientBuildId,
  readSafetyState,
  isVisible,
  hasControllingServiceWorker,
  claimHardReloadAttempt,
  reloadPage,
  onReloadLoopBlocked = () => undefined,
}: {
  clientBuildId: string;
  readSafetyState: () => UpdateSafetyState;
  isVisible: () => boolean;
  hasControllingServiceWorker: () => boolean;
  claimHardReloadAttempt: (
    clientBuildId: string,
    hostBuildId: string,
  ) => boolean;
  reloadPage: () => void;
  onReloadLoopBlocked?: (clientBuildId: string, hostBuildId: string) => void;
}) {
  let registration: CompatibleBuildRegistration | null = null;
  let pendingHostBuildId: string | null = null;
  let updateRequestedForBuild: string | null = null;
  let loopBlockedForBuild: string | null = null;
  let recoveryInProgress = false;

  const attemptRecovery = async (): Promise<boolean> => {
    const hostBuildId = pendingHostBuildId;
    if (
      !hostBuildId ||
      recoveryInProgress ||
      !isVisible() ||
      !canActivateUpdate(readSafetyState())
    ) {
      return false;
    }

    recoveryInProgress = true;
    try {
      if (hasControllingServiceWorker()) {
        if (!registration?.supported) return false;

        if (
          !registration.hasWaitingUpdate() &&
          updateRequestedForBuild !== hostBuildId
        ) {
          updateRequestedForBuild = hostBuildId;
          try {
            const updateFound = await registration.requestUpdate();
            if (!updateFound) updateRequestedForBuild = null;
          } catch {
            updateRequestedForBuild = null;
            return false;
          }
        }

        if (!registration.hasWaitingUpdate()) return false;
        const activated = await registration.activateWaitingUpdateIfSafe();
        if (activated) pendingHostBuildId = null;
        return activated;
      }

      if (claimHardReloadAttempt(clientBuildId, hostBuildId)) {
        pendingHostBuildId = null;
        reloadPage();
        return true;
      }
      if (loopBlockedForBuild !== hostBuildId) {
        loopBlockedForBuild = hostBuildId;
        onReloadLoopBlocked(clientBuildId, hostBuildId);
      }
      return false;
    } finally {
      recoveryInProgress = false;
    }
  };

  return {
    attachRegistration(nextRegistration: CompatibleBuildRegistration) {
      registration = nextRegistration;
      void attemptRecovery();
    },
    requestCompatibleBuild(hostBuildId: string): Promise<boolean> {
      if (pendingHostBuildId !== hostBuildId) {
        updateRequestedForBuild = null;
        loopBlockedForBuild = null;
      }
      pendingHostBuildId = hostBuildId;
      return attemptRecovery();
    },
    notifySafePoint: attemptRecovery,
  };
}

type CompatibleBuildRecoveryCoordinator = ReturnType<
  typeof createCompatibleBuildRecoveryCoordinator
>;

let activeCompatibleBuildRecovery: CompatibleBuildRecoveryCoordinator | null =
  null;
let queuedCompatibleBuildId: string | null = null;
let activePwaSafetyStateNotifier: (() => void) | null = null;

export function requestCompatibleBuild(hostBuildId: string): void {
  if (activeCompatibleBuildRecovery) {
    void activeCompatibleBuildRecovery.requestCompatibleBuild(hostBuildId);
  } else {
    queuedCompatibleBuildId = hostBuildId;
  }
}

export function notifyPwaSafetyStateChanged(): void {
  activePwaSafetyStateNotifier?.();
}

function installCompatibleBuildRecovery(
  recovery: CompatibleBuildRecoveryCoordinator,
): void {
  activeCompatibleBuildRecovery = recovery;
  if (queuedCompatibleBuildId) {
    const hostBuildId = queuedCompatibleBuildId;
    queuedCompatibleBuildId = null;
    void recovery.requestCompatibleBuild(hostBuildId);
  }
}

export function createPwaUpdateCoordinator(
  readSafetyState: () => UpdateSafetyState,
  activateUpdate: () => Promise<boolean | void>,
) {
  let updateWaiting = false;
  let activationInProgress = false;

  const activateWaitingUpdateIfSafe = async (): Promise<boolean> => {
    if (
      !updateWaiting ||
      activationInProgress ||
      !canActivateUpdate(readSafetyState())
    ) {
      return false;
    }

    activationInProgress = true;
    try {
      const activated = await activateUpdate();
      if (activated === false) return false;
      updateWaiting = false;
      return true;
    } finally {
      activationInProgress = false;
    }
  };

  return {
    activateWaitingUpdateIfSafe,
    hasWaitingUpdate: () => updateWaiting,
    notifyUpdateWaiting: async (): Promise<boolean> => {
      updateWaiting = true;
      return false;
    },
    notifyActivated: () => {
      updateWaiting = false;
    },
  };
}

export function registerPwaServiceWorker({
  capabilities,
  readSafetyState,
  registerServiceWorker,
  isVisible = () => true,
  onUpdateWaiting = () => undefined,
  onNeedReload = () => undefined,
}: {
  capabilities: PwaRuntimeCapabilities;
  readSafetyState: () => UpdateSafetyState;
  registerServiceWorker: RegisterServiceWorker;
  isVisible?: () => boolean;
  onUpdateWaiting?: () => void;
  onNeedReload?: () => void;
}): PwaRegistration {
  if (!isPwaRuntimeSupported(capabilities)) {
    return {
      supported: false,
      activateWaitingUpdateIfSafe: async () => false,
      hasWaitingUpdate: () => false,
      requestUpdate: async () => false,
      notifyStartupNavigationSafePoint: async () => false,
      notifyForegroundSafePoint: async () => false,
    };
  }

  let updateServiceWorker: (
    reloadPage?: boolean,
  ) => Promise<boolean | void> =
    async () => undefined;
  let requestRegisteredUpdate = async (): Promise<object | null> => null;
  let updaterReady = false;
  let initialRegistrationSettled = false;
  let needRefreshBeforeRegistration = false;
  let handledWaitingWorker: object | null = null;
  let initialWaitingDiscovered = false;
  let startupSafePointSeen = false;
  let startupAttemptConsumed = false;
  const coordinator = createPwaUpdateCoordinator(
    readSafetyState,
    async () => updateServiceWorker(),
  );

  const activateInitialWaitingAtStartup = async (): Promise<boolean> => {
    if (
      !updaterReady ||
      startupAttemptConsumed ||
      !startupSafePointSeen ||
      !initialWaitingDiscovered
    ) {
      return false;
    }
    startupAttemptConsumed = true;
    if (!isVisible()) return false;
    return coordinator.activateWaitingUpdateIfSafe();
  };

  const noteWaitingWorker = async (initialDiscovery: boolean) => {
    if (initialDiscovery && initialWaitingDiscovered) {
      return activateInitialWaitingAtStartup();
    }
    if (initialDiscovery) initialWaitingDiscovered = true;
    await coordinator.notifyUpdateWaiting();
    onUpdateWaiting();
    return initialDiscovery ? activateInitialWaitingAtStartup() : false;
  };

  updateServiceWorker = registerServiceWorker({
    immediate: true,
    onNeedReload: () => {
      coordinator.notifyActivated();
      onNeedReload();
    },
    onNeedRefresh: (waitingWorker) => {
      if (!initialRegistrationSettled) {
        needRefreshBeforeRegistration = true;
        if (waitingWorker) handledWaitingWorker = waitingWorker;
        void noteWaitingWorker(true);
      } else if (waitingWorker && waitingWorker === handledWaitingWorker) {
        void activateInitialWaitingAtStartup();
      } else {
        if (waitingWorker) handledWaitingWorker = waitingWorker;
        void noteWaitingWorker(false);
      }
    },
    onRegistered: (hasWaitingWorker, requestUpdate, waitingWorker) => {
      if (requestUpdate) requestRegisteredUpdate = requestUpdate;
      initialRegistrationSettled = true;
      if (hasWaitingWorker && !needRefreshBeforeRegistration) {
        if (waitingWorker) handledWaitingWorker = waitingWorker;
        void noteWaitingWorker(true);
      } else if (hasWaitingWorker) {
        if (waitingWorker) handledWaitingWorker = waitingWorker;
        void activateInitialWaitingAtStartup();
      }
    },
  });
  updaterReady = true;
  void activateInitialWaitingAtStartup();

  const notifyStartupNavigationSafePoint = async (): Promise<boolean> => {
    if (!isVisible()) return false;
    if (!startupSafePointSeen) {
      startupSafePointSeen = true;
      if (initialWaitingDiscovered) {
        return activateInitialWaitingAtStartup();
      }
    }
    return coordinator.activateWaitingUpdateIfSafe();
  };

  const requestUpdate = async (): Promise<boolean> => {
    const waitingWorker = await requestRegisteredUpdate();
    if (waitingWorker && waitingWorker !== handledWaitingWorker) {
      handledWaitingWorker = waitingWorker;
      await noteWaitingWorker(false);
    }
    return coordinator.hasWaitingUpdate();
  };

  return {
    supported: true,
    activateWaitingUpdateIfSafe:
      coordinator.activateWaitingUpdateIfSafe,
    hasWaitingUpdate: coordinator.hasWaitingUpdate,
    requestUpdate,
    notifyStartupNavigationSafePoint,
    notifyForegroundSafePoint: coordinator.activateWaitingUpdateIfSafe,
  };
}

export const defaultUpdateSafetyState = (): UpdateSafetyState => ({
  // Task 4 injects the real composer/mutation reader. Until then, an unknown
  // state must leave a new worker waiting rather than risk discarding input.
  hasDraft: true,
  pendingMutations: 0,
  selectedAttachments: 0,
  attachmentLoads: 0,
});

export async function registerPwa(
  readSafetyState: () => UpdateSafetyState = defaultUpdateSafetyState,
  onReloadLoopBlocked: () => void = () => undefined,
): Promise<PwaRegistration> {
  document.documentElement.dataset.webBuildId = WEB_BUILD_ID;

  const isVisible = () => document.visibilityState === "visible";
  const localReloadGate = createLocalReloadGate({
    isVisible,
    readSafetyState,
    reload: () => window.location.reload(),
  });
  const capabilities = {
    isSecureContext: window.isSecureContext,
    serviceWorkerAvailable: "serviceWorker" in navigator,
  };
  const recovery = createCompatibleBuildRecoveryCoordinator({
    clientBuildId: WEB_BUILD_ID,
    readSafetyState,
    isVisible,
    hasControllingServiceWorker: () =>
      "serviceWorker" in navigator && Boolean(navigator.serviceWorker.controller),
    claimHardReloadAttempt: (clientBuildId, hostBuildId) => {
      try {
        return claimCompatibleBuildReloadAttempt(
          window.sessionStorage,
          clientBuildId,
          hostBuildId,
        );
      } catch {
        return false;
      }
    },
    reloadPage: () => window.location.reload(),
    onReloadLoopBlocked,
  });
  installCompatibleBuildRecovery(recovery);

  let registration: PwaRegistration | null = null;
  let startupNavigationPending = false;
  const activateOnPageShow = () => {
    if (!isVisible()) return;
    if (registration) {
      void registration.notifyStartupNavigationSafePoint();
    } else {
      startupNavigationPending = true;
    }
    void recovery.notifySafePoint();
    localReloadGate.notifySafePoint();
  };
  const activateOnSafeForeground = () => {
    if (!isVisible()) return;
    if (registration) void registration.notifyForegroundSafePoint();
    void recovery.notifySafePoint();
    localReloadGate.notifySafePoint();
  };
  activePwaSafetyStateNotifier = activateOnSafeForeground;
  window.addEventListener("pageshow", activateOnPageShow);
  document.addEventListener("visibilitychange", activateOnSafeForeground);

  if (!isPwaRuntimeSupported(capabilities)) {
    registration = registerPwaServiceWorker({
      capabilities,
      readSafetyState,
      isVisible,
      registerServiceWorker: () => async () => undefined,
    });
    recovery.attachRegistration(registration);
    return registration;
  }

  navigator.serviceWorker.addEventListener("message", (event) => {
    if (!isUpdateSafetyQuery(event.data)) return;
    const source = event.source as
      | { postMessage(message: unknown): void }
      | null;
    if (!source || typeof source.postMessage !== "function") return;
    source.postMessage({
      type: UPDATE_SAFETY_ACK,
      nonce: event.data.nonce,
      safe: canActivateUpdate(readSafetyState()),
    });
  });

  const { registerSW } = await import("virtual:pwa-register");
  registration = registerPwaServiceWorker({
    capabilities,
    readSafetyState,
    isVisible,
    onUpdateWaiting: () => void recovery.notifySafePoint(),
    onNeedReload: () => localReloadGate.notifyControllerChanged(),
    registerServiceWorker: createViteServiceWorkerRegistrar(
      registerSW,
      async (waitingWorker) => {
        const worker = waitingWorker as {
          postMessage(message: unknown): void;
        };
        if (typeof worker.postMessage !== "function") return false;
        const nonce =
          globalThis.crypto?.randomUUID?.() ??
          `${Date.now()}-${Math.random().toString(36).slice(2)}`;
        return requestWaitingWorkerActivation({
          worker,
          messages: navigator.serviceWorker,
          nonce,
        });
      },
    ),
  });
  recovery.attachRegistration(registration);

  if (startupNavigationPending) {
    void registration.notifyStartupNavigationSafePoint();
    void recovery.notifySafePoint();
  }

  return registration;
}
