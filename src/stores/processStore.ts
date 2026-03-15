import { create } from 'zustand';
import type { ProcessTreeInfo } from '../types/config';
import { playNotificationSound } from '../utils/notificationSound';

export type ProcessStatus = 'stopped' | 'starting' | 'running' | 'stopping' | 'crashed';
export type TerminalActivity = 'thinking' | 'idle';

export interface ProcessState {
  pid: number | null;
  status: ProcessStatus;
  logs: string[];
  startedAt: number | null;
  exitCode: number | null;
  unseenErrorCount: number;
}

interface ProcessStore {
  processes: Record<string, ProcessState>;
  resources: Record<string, ProcessTreeInfo>;
  terminalTitles: Record<string, string>;
  terminalActivity: Record<string, TerminalActivity>;
  thinkingStartedAt: Record<string, number>;  // when each session started thinking
  unseenReady: Record<string, boolean>;        // session finished thinking while tab not active

  // Process lifecycle
  setProcessState: (commandId: string, state: Partial<ProcessState>) => void;
  initProcess: (commandId: string) => void;
  appendLog: (commandId: string, line: string, isError?: boolean) => void;
  clearLogs: (commandId: string) => void;
  removeProcess: (commandId: string) => void;
  resetUnseenErrors: (commandId: string) => void;
  incrementUnseenErrors: (commandId: string) => void;

  // Resources
  updateResources: (commandId: string, info: ProcessTreeInfo) => void;
  clearResources: (commandId: string) => void;

  // Terminal activity
  setTerminalTitle: (id: string, title: string) => void;
  setTerminalActivity: (id: string, activity: TerminalActivity, activeSessionId?: string | null, notificationSound?: string) => void;
  clearUnseenReady: (id: string) => void;

  // Getters
  getProcess: (commandId: string) => ProcessState | undefined;
  getRunningCount: () => number;
  getTotalMemory: () => number;
}

const DEFAULT_PROCESS_STATE: ProcessState = {
  pid: null,
  status: 'stopped',
  logs: [],
  startedAt: null,
  exitCode: null,
  unseenErrorCount: 0,
};

export const useProcessStore = create<ProcessStore>((set, get) => ({
  processes: {},
  resources: {},
  terminalTitles: {},
  terminalActivity: {},
  thinkingStartedAt: {},
  unseenReady: {},

  setProcessState: (commandId, partial) => {
    set(state => ({
      processes: {
        ...state.processes,
        [commandId]: {
          ...(state.processes[commandId] ?? { ...DEFAULT_PROCESS_STATE }),
          ...partial,
        },
      },
    }));
  },

  initProcess: (commandId) => {
    set(state => ({
      processes: {
        ...state.processes,
        [commandId]: { ...DEFAULT_PROCESS_STATE },
      },
    }));
  },

  appendLog: (commandId, line, isError = false) => {
    set(state => {
      const proc = state.processes[commandId];
      if (!proc) return state;
      const logs = [...proc.logs, isError ? `\x1b[31m${line}\x1b[0m` : line];
      // Limit log buffer to 10000 lines
      if (logs.length > 10000) logs.splice(0, logs.length - 10000);
      return {
        processes: {
          ...state.processes,
          [commandId]: { ...proc, logs },
        },
      };
    });
  },

  clearLogs: (commandId) => {
    set(state => {
      const proc = state.processes[commandId];
      if (!proc) return state;
      return {
        processes: {
          ...state.processes,
          [commandId]: { ...proc, logs: [] },
        },
      };
    });
  },

  removeProcess: (commandId) => {
    set(state => {
      const { [commandId]: _, ...rest } = state.processes;
      const { [commandId]: _r, ...restResources } = state.resources;
      return { processes: rest, resources: restResources };
    });
  },

  resetUnseenErrors: (commandId) => {
    set(state => {
      const proc = state.processes[commandId];
      if (!proc) return state;
      return {
        processes: {
          ...state.processes,
          [commandId]: { ...proc, unseenErrorCount: 0 },
        },
      };
    });
  },

  incrementUnseenErrors: (commandId) => {
    set(state => {
      const proc = state.processes[commandId];
      if (!proc) return state;
      return {
        processes: {
          ...state.processes,
          [commandId]: { ...proc, unseenErrorCount: proc.unseenErrorCount + 1 },
        },
      };
    });
  },

  updateResources: (commandId, info) => {
    set(state => ({
      resources: { ...state.resources, [commandId]: info },
    }));
  },

  clearResources: (commandId) => {
    set(state => {
      const { [commandId]: _, ...rest } = state.resources;
      return { resources: rest };
    });
  },

  setTerminalTitle: (id, title) => {
    set(state => ({
      terminalTitles: { ...state.terminalTitles, [id]: title },
    }));
  },

  setTerminalActivity: (id, activity, activeSessionId, notificationSound) => {
    const prev = get().terminalActivity[id];
    const now = Date.now();

    // Track when thinking started
    if (activity === 'thinking' && prev !== 'thinking') {
      set(state => ({
        terminalActivity: { ...state.terminalActivity, [id]: activity },
        thinkingStartedAt: { ...state.thinkingStartedAt, [id]: now },
      }));
      return;
    }

    // Transition from thinking → idle: check if it was long enough for a notification
    if (activity === 'idle' && prev === 'thinking') {
      const startedAt = get().thinkingStartedAt[id];
      const wasLongThinking = startedAt && (now - startedAt >= 30_000);
      const isBackground = activeSessionId !== id;

      const wasVeryLongThinking = startedAt && (now - startedAt >= 60_000);

      if (wasLongThinking && isBackground) {
        // Background tab, 30s+ thinking: sound + badge
        playNotificationSound(notificationSound || 'glass');
        set(state => ({
          terminalActivity: { ...state.terminalActivity, [id]: activity },
          unseenReady: { ...state.unseenReady, [id]: true },
        }));
      } else if (wasVeryLongThinking && !isBackground) {
        // Active tab, 60s+ thinking: sound only, no badge
        playNotificationSound(notificationSound || 'glass');
        set(state => ({
          terminalActivity: { ...state.terminalActivity, [id]: activity },
        }));
      } else {
        set(state => ({
          terminalActivity: { ...state.terminalActivity, [id]: activity },
        }));
      }
      return;
    }

    set(state => ({
      terminalActivity: { ...state.terminalActivity, [id]: activity },
    }));
  },

  clearUnseenReady: (id) => {
    set(state => {
      const { [id]: _, ...rest } = state.unseenReady;
      return { unseenReady: rest };
    });
  },

  getProcess: (commandId) => get().processes[commandId],
  getRunningCount: () => Object.values(get().processes).filter(p => p.status === 'running').length,
  getTotalMemory: () => Object.values(get().resources).reduce((sum, r) => sum + r.total_memory_mb, 0),
}));
