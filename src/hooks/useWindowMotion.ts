import { useEffect, useState } from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';

const MOTION_SETTLE_MS = 250;

let listenersReady = false;
let movingUntil = 0;
let settleTimer: ReturnType<typeof setTimeout> | null = null;
const subscribers = new Set<() => void>();
let detachListeners: Array<() => void> = [];
let unloadCleanupAttached = false;

function isWindowMotionActiveNow() {
  return movingUntil > Date.now();
}

function notifySubscribers() {
  for (const subscriber of subscribers) {
    subscriber();
  }
}

function scheduleSettleNotification() {
  if (settleTimer) {
    clearTimeout(settleTimer);
  }

  const remaining = movingUntil - Date.now();
  if (remaining <= 0) {
    movingUntil = 0;
    notifySubscribers();
    return;
  }

  settleTimer = setTimeout(() => {
    settleTimer = null;
    if (movingUntil <= Date.now()) {
      movingUntil = 0;
      notifySubscribers();
      return;
    }
    scheduleSettleNotification();
  }, remaining);
}

function markWindowMotion() {
  const wasMoving = isWindowMotionActiveNow();
  movingUntil = Date.now() + MOTION_SETTLE_MS;

  if (!wasMoving) {
    notifySubscribers();
  }

  scheduleSettleNotification();
}

async function ensureWindowMotionListeners() {
  if (listenersReady) {
    return;
  }

  listenersReady = true;

  try {
    const currentWindow = getCurrentWindow();
    detachListeners = await Promise.all([
      currentWindow.onMoved(() => {
        markWindowMotion();
      }),
      currentWindow.onResized(() => {
        markWindowMotion();
      }),
    ]);

    if (!unloadCleanupAttached) {
      const cleanup = () => {
        for (const detach of detachListeners) {
          detach();
        }
        detachListeners = [];
      };

      window.addEventListener('beforeunload', cleanup, { once: true });
      window.addEventListener('pagehide', cleanup, { once: true });
      unloadCleanupAttached = true;
    }
  } catch (error) {
    console.warn('Failed to attach window motion listeners:', error);
  }
}

export function useWindowMotionActive() {
  const [moving, setMoving] = useState(() => isWindowMotionActiveNow());

  useEffect(() => {
    const handleChange = () => {
      setMoving(isWindowMotionActiveNow());
    };

    subscribers.add(handleChange);
    void ensureWindowMotionListeners();

    return () => {
      subscribers.delete(handleChange);
    };
  }, []);

  return moving;
}
