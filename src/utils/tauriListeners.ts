import {
  listen,
  type EventCallback,
  type EventName,
  type UnlistenFn,
} from '@tauri-apps/api/event';

const activeUnlisteners = new Set<UnlistenFn>();
const pendingRegistrations = new Set<Promise<UnlistenFn>>();
let cleanupRegistered = false;
let unloading = false;

function callUnlisten(fn: UnlistenFn) {
  try {
    fn();
  } catch {
    // Best-effort cleanup during unload.
  }
}

function cleanupAllListeners() {
  if (unloading) return;
  unloading = true;

  for (const registration of pendingRegistrations) {
    registration.then(fn => callUnlisten(fn)).catch(() => {});
  }
  pendingRegistrations.clear();

  for (const fn of Array.from(activeUnlisteners)) {
    activeUnlisteners.delete(fn);
    callUnlisten(fn);
  }
}

function ensureUnloadCleanup() {
  if (cleanupRegistered || typeof window === 'undefined') return;
  cleanupRegistered = true;

  window.addEventListener('pagehide', cleanupAllListeners, { once: true });
  window.addEventListener('beforeunload', cleanupAllListeners, { once: true });
}

export function listenWithAutoCleanup<T>(
  event: EventName,
  handler: EventCallback<T>,
): Promise<UnlistenFn> {
  ensureUnloadCleanup();

  let registration!: Promise<UnlistenFn>;
  registration = listen<T>(event, handler)
    .then(unlisten => {
      pendingRegistrations.delete(registration);

      if (unloading) {
        callUnlisten(unlisten);
        return (() => {}) as UnlistenFn;
      }

      const wrappedUnlisten: UnlistenFn = () => {
        if (activeUnlisteners.delete(wrappedUnlisten)) {
          callUnlisten(unlisten);
        }
      };

      activeUnlisteners.add(wrappedUnlisten);
      return wrappedUnlisten;
    })
    .catch(err => {
      pendingRegistrations.delete(registration);
      throw err;
    });

  pendingRegistrations.add(registration);
  return registration;
}
