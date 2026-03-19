import { useEffect, useRef, useState } from 'react';
import { relaunch } from '@tauri-apps/plugin-process';
import { isPermissionGranted, requestPermission, sendNotification } from '@tauri-apps/plugin-notification';
import { check, type Update } from '@tauri-apps/plugin-updater';

const INITIAL_CHECK_DELAY_MS = 5_000;
const CHECK_INTERVAL_MS = 30 * 60 * 1_000;
const VISIBILITY_RECHECK_STALE_MS = 5 * 60 * 1_000;
const ERROR_DISPLAY_MS = 10_000;

export type UpdatePhase = 'idle' | 'checking' | 'downloading' | 'ready' | 'error';

interface UpdateStateSnapshot {
  phase: UpdatePhase;
  version: string | null;
  body: string;
  progress: number | null;
  error: string | null;
}

export interface UpdateCheckState extends UpdateStateSnapshot {
  restartToUpdate: () => Promise<void>;
}

const IDLE_STATE: UpdateStateSnapshot = {
  phase: 'idle',
  version: null,
  body: '',
  progress: null,
  error: null,
};

type UpdateStateUpdater =
  | UpdateStateSnapshot
  | ((prev: UpdateStateSnapshot) => UpdateStateSnapshot);

function formatUpdateError(prefix: string, err: unknown): string {
  const details = err instanceof Error ? err.message : String(err);
  return `${prefix}: ${details}`;
}

function parseVersionIdentifier(identifier: string): number | string {
  if (/^\d+$/.test(identifier)) {
    return Number(identifier);
  }
  return identifier.toLowerCase();
}

function compareVersionIdentifiers(left: number | string, right: number | string): number {
  if (typeof left === 'number' && typeof right === 'number') {
    return left - right;
  }
  if (typeof left === 'number') return -1;
  if (typeof right === 'number') return 1;
  return left.localeCompare(right);
}

function compareVersions(leftVersion: string, rightVersion: string): number {
  const normalize = (version: string) => {
    const cleaned = version.trim().replace(/^v/i, '').split('+', 1)[0];
    const [corePart, prereleasePart] = cleaned.split('-', 2);

    return {
      core: corePart.split('.').map(part => parseVersionIdentifier(part || '0')),
      prerelease: prereleasePart
        ? prereleasePart.split('.').map(part => parseVersionIdentifier(part || '0'))
        : null,
    };
  };

  const left = normalize(leftVersion);
  const right = normalize(rightVersion);
  const maxCoreLength = Math.max(left.core.length, right.core.length);

  for (let index = 0; index < maxCoreLength; index += 1) {
    const comparison = compareVersionIdentifiers(
      left.core[index] ?? 0,
      right.core[index] ?? 0
    );

    if (comparison !== 0) {
      return comparison;
    }
  }

  if (!left.prerelease && !right.prerelease) return 0;
  if (!left.prerelease) return 1;
  if (!right.prerelease) return -1;

  const maxPrereleaseLength = Math.max(left.prerelease.length, right.prerelease.length);
  for (let index = 0; index < maxPrereleaseLength; index += 1) {
    const leftIdentifier = left.prerelease[index];
    const rightIdentifier = right.prerelease[index];

    if (leftIdentifier === undefined) return -1;
    if (rightIdentifier === undefined) return 1;

    const comparison = compareVersionIdentifiers(leftIdentifier, rightIdentifier);
    if (comparison !== 0) {
      return comparison;
    }
  }

  return 0;
}

export function useUpdateCheck(): UpdateCheckState {
  const [state, setState] = useState<UpdateStateSnapshot>(IDLE_STATE);
  const mountedRef = useRef(false);
  const stateRef = useRef<UpdateStateSnapshot>(IDLE_STATE);
  const inFlightRef = useRef(false);
  const installingRef = useRef(false);
  const lastCheckAtRef = useRef(0);
  const pendingUpdateRef = useRef<Update | null>(null);
  const installAppliedRef = useRef(false);
  const errorTimeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const notifiedReadyVersionRef = useRef<string | null>(null);

  const setHookState = (updater: UpdateStateUpdater) => {
    if (!mountedRef.current) return;

    setState(prev => {
      const next = typeof updater === 'function' ? updater(prev) : updater;
      stateRef.current = next;
      return next;
    });
  };

  const clearErrorTimer = () => {
    if (!errorTimeoutRef.current) return;
    clearTimeout(errorTimeoutRef.current);
    errorTimeoutRef.current = null;
  };

  const scheduleErrorClear = () => {
    clearErrorTimer();
    errorTimeoutRef.current = setTimeout(() => {
      setHookState(prev => {
        if (prev.phase === 'error') {
          return { ...IDLE_STATE };
        }
        if (prev.error) {
          return { ...prev, error: null };
        }
        return prev;
      });
    }, ERROR_DISPLAY_MS);
  };

  const closeUpdateResource = async (update: Update | null) => {
    if (!update) return;

    try {
      await update.close();
    } catch (err) {
      console.warn('[update-check] failed to close updater resource', err);
    }
  };

  const closePendingUpdate = async () => {
    const pendingUpdate = pendingUpdateRef.current;
    pendingUpdateRef.current = null;
    installAppliedRef.current = false;
    await closeUpdateResource(pendingUpdate);
  };

  const showCheckFailure = async (message: string) => {
    await closePendingUpdate();
    setHookState({
      ...IDLE_STATE,
      phase: 'error',
      error: message,
    });
    scheduleErrorClear();
  };

  const notifyUpdateReady = async (version: string) => {
    if (notifiedReadyVersionRef.current === version) return;
    notifiedReadyVersionRef.current = version;

    try {
      let permissionGranted = await isPermissionGranted();
      if (!permissionGranted) {
        const permission = await requestPermission();
        permissionGranted = permission === 'granted';
      }

      if (permissionGranted) {
        sendNotification({
          title: 'DevManager update ready',
          body: `Version ${version} has been downloaded. Restart DevManager when convenient.`,
        });
      }
    } catch {
      // Notification failure should not affect update flow.
    }
  };

  const setReadyState = (update: Update) => {
    setHookState({
      phase: 'ready',
      version: update.version,
      body: update.body ?? '',
      progress: 100,
      error: null,
    });
    void notifyUpdateReady(update.version);
  };

  const downloadUpdate = async (update: Update) => {
    let totalLength = 0;
    let downloadedLength = 0;

    setHookState({
      phase: 'downloading',
      version: update.version,
      body: update.body ?? '',
      progress: 0,
      error: null,
    });

    await update.download(event => {
      switch (event.event) {
        case 'Started':
          totalLength = event.data.contentLength ?? 0;
          setHookState(prev => prev.phase === 'downloading'
            ? { ...prev, progress: totalLength > 0 ? 0 : null }
            : prev
          );
          break;
        case 'Progress':
          downloadedLength += event.data.chunkLength;
          if (totalLength > 0) {
            const nextProgress = Math.min(100, Math.round((downloadedLength / totalLength) * 100));
            setHookState(prev => prev.phase === 'downloading'
              ? { ...prev, progress: nextProgress }
              : prev
            );
          }
          break;
        case 'Finished':
          setHookState(prev => prev.phase === 'downloading'
            ? { ...prev, progress: 100 }
            : prev
          );
          break;
      }
    });
  };

  const keepCurrentReadyUpdate = async (update: Update) => {
    await closeUpdateResource(update);
  };

  const replaceReadyUpdateIfNewer = async (nextUpdate: Update) => {
    const currentUpdate = pendingUpdateRef.current;
    const currentState = stateRef.current;

    if (!currentUpdate || currentState.phase !== 'ready') {
      return false;
    }

    if (compareVersions(nextUpdate.version, currentUpdate.version) <= 0) {
      await keepCurrentReadyUpdate(nextUpdate);
      return true;
    }

    pendingUpdateRef.current = nextUpdate;
    installAppliedRef.current = false;

    try {
      await downloadUpdate(nextUpdate);
      setReadyState(nextUpdate);
      await closeUpdateResource(currentUpdate);
      return true;
    } catch (err) {
      pendingUpdateRef.current = currentUpdate;
      installAppliedRef.current = false;
      setHookState(currentState);
      await closeUpdateResource(nextUpdate);
      console.warn('[update-check] newer update download failed; keeping current downloaded version', err);
      return true;
    }
  };

  const checkForUpdates = async () => {
    const currentState = stateRef.current;
    if (inFlightRef.current || installingRef.current || currentState.phase === 'downloading') {
      return;
    }

    inFlightRef.current = true;
    lastCheckAtRef.current = Date.now();
    clearErrorTimer();

    const preserveReadyState = currentState.phase === 'ready' && pendingUpdateRef.current !== null;

    if (!preserveReadyState) {
      setHookState(prev => (prev.phase === 'idle' || prev.phase === 'error')
        ? { ...prev, phase: 'checking', error: null }
        : prev
      );
    }

    let update: Update | null = null;

    try {
      update = await check();
    } catch (err) {
      if (preserveReadyState) {
        console.warn('[update-check] background re-check failed; keeping downloaded update', err);
      } else {
        await showCheckFailure(formatUpdateError('Update check failed', err));
      }
      inFlightRef.current = false;
      return;
    }

    try {
      if (!update) {
        if (!preserveReadyState) {
          setHookState({ ...IDLE_STATE });
        }
        return;
      }

      if (preserveReadyState) {
        const handled = await replaceReadyUpdateIfNewer(update);
        if (handled) {
          return;
        }
      }

      await closePendingUpdate();
      pendingUpdateRef.current = update;
      installAppliedRef.current = false;

      try {
        await downloadUpdate(update);
        setReadyState(update);
      } catch (err) {
        await showCheckFailure(formatUpdateError('Update download failed', err));
      }
    } finally {
      inFlightRef.current = false;
    }
  };

  const restartToUpdate = async () => {
    const update = pendingUpdateRef.current;
    if (!update || stateRef.current.phase !== 'ready') {
      throw new Error('No downloaded update is ready.');
    }

    clearErrorTimer();
    installingRef.current = true;

    try {
      if (!installAppliedRef.current) {
        await update.install();
        installAppliedRef.current = true;
      }
      await relaunch();
    } catch (err) {
      const message = formatUpdateError(
        installAppliedRef.current ? 'Restart failed' : 'Update install failed',
        err
      );

      setHookState(prev => prev.phase === 'ready'
        ? { ...prev, error: message }
        : prev
      );
      scheduleErrorClear();
      throw err;
    } finally {
      installingRef.current = false;
    }
  };

  useEffect(() => {
    mountedRef.current = true;

    const startupTimeout = setTimeout(() => {
      void checkForUpdates();
    }, INITIAL_CHECK_DELAY_MS);

    const interval = setInterval(() => {
      void checkForUpdates();
    }, CHECK_INTERVAL_MS);

    const handleVisibilityChange = () => {
      if (document.visibilityState !== 'visible') return;
      if (Date.now() - lastCheckAtRef.current < VISIBILITY_RECHECK_STALE_MS) return;
      void checkForUpdates();
    };

    document.addEventListener('visibilitychange', handleVisibilityChange);

    return () => {
      mountedRef.current = false;
      clearTimeout(startupTimeout);
      clearInterval(interval);
      document.removeEventListener('visibilitychange', handleVisibilityChange);
      clearErrorTimer();
      void closePendingUpdate();
    };
  }, []);

  return {
    ...state,
    restartToUpdate,
  };
}
