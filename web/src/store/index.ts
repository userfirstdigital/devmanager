import { create } from "zustand";
import {
  DEFAULT_DIMENSIONS,
  type RemoteAction,
  type RemoteAiTabPayload,
  type RemoteWorkspaceDelta,
  type RemoteWorkspaceSnapshot,
  type SessionTab,
  type WebImagePastePayload,
  type WsOutbound,
} from "../api/types";
import type {
  SessionBootstrapFrame,
  SessionOutputFrame,
  WsStatus,
} from "../api/ws";
import { WsClient } from "../api/ws";

const ACTIVE_PROJECT_KEY = "devmanager-active-project-id";
const COLLAPSED_PROJECTS_KEY = "devmanager-collapsed-projects";
const MAX_PENDING_TERMINAL_FRAMES = 256;

/**
 * "Click + Claude" fires an action but doesn't know the tab id yet; the host
 * creates the tab asynchronously and broadcasts it as a delta. We record the
 * pending intent so when the matching tab shows up in the next snapshot/delta
 * we can auto-activate it for the user — single click opens the new terminal
 * end-to-end.
 */
interface PendingLaunch {
  projectId: string;
  tabType: "claude" | "codex";
  knownTabIds: Set<string>;
  issuedAt: number;
}

interface StoreState {
  status: WsStatus;
  snapshot: RemoteWorkspaceSnapshot | null;
  activeProjectId: string | null;
  /** The session whose terminal is currently shown in the main view. */
  activeSessionId: string | null;
  /** Projects the user has manually collapsed in the sidebar. */
  collapsedProjects: Set<string>;
  lastError: string | null;
  client: WsClient | null;
  /** Listeners that want raw session output bytes, keyed by session id. */
  terminalSubscribers: Map<string, Set<(frame: SessionOutputFrame) => void>>;
  /** Output chunks that arrived before the terminal view subscribed. */
  pendingTerminalFrames: Map<string, SessionOutputFrame[]>;
  /** One-shot bootstrap listeners used to seed a mounted xterm instance. */
  bootstrapSubscribers: Map<
    string,
    Set<(bootstrap: SessionBootstrapFrame) => void>
  >;
  /** Session bootstraps delivered by the host, keyed by session id. Phase 4
   * consumers drain these on mount to repaint prior scrollback. */
  pendingBootstraps: Map<string, SessionBootstrapFrame>;
  /** Pending AI launches awaiting their delta-created tab id. */
  pendingLaunches: PendingLaunch[];

  init(): void;
  sendAction(action: RemoteAction): void;
  setActiveProject(projectId: string | null): void;
  setActiveSession(sessionId: string | null): void;
  toggleProjectCollapsed(projectId: string): void;
  subscribeTerminal(
    sessionId: string,
    listener: (frame: SessionOutputFrame) => void,
  ): () => void;
  subscribeBootstrap(
    sessionId: string,
    listener: (bootstrap: SessionBootstrapFrame) => void,
  ): () => void;
  drainBootstrap(sessionId: string): SessionBootstrapFrame | null;
  takeControl(): void;
  releaseControl(): void;
  sendInput(sessionId: string, text: string): void;
  pasteImage(sessionId: string, payload: WebImagePastePayload): void;
  sendResize(sessionId: string, rows: number, cols: number): void;
  /** Issue a launchAi request and activate the returned tab/session. */
  launchAiTab(projectId: string, tabType: "claude" | "codex"): Promise<void>;
  openAiTab(tabId: string): Promise<void>;
  restartAiTab(tabId: string): Promise<void>;
  openSshTab(connectionId: string): void;
  connectSsh(connectionId: string): void;
  restartSsh(connectionId: string): void;
  disconnectSsh(connectionId: string): void;
  stopAllServers(): void;
  /** Close the currently-active tab on the host and hide it. */
  closeActiveTab(): void;
}

function loadCollapsedProjects(): Set<string> {
  try {
    const raw = localStorage.getItem(COLLAPSED_PROJECTS_KEY);
    if (!raw) return new Set();
    const parsed = JSON.parse(raw);
    if (Array.isArray(parsed)) return new Set(parsed);
    return new Set();
  } catch {
    return new Set();
  }
}

function persistCollapsedProjects(set: Set<string>): void {
  try {
    localStorage.setItem(COLLAPSED_PROJECTS_KEY, JSON.stringify([...set]));
  } catch {
    // quota / privacy mode — ignore.
  }
}

/**
 * Apply a `RemoteWorkspaceDelta` onto the current snapshot, mirroring the
 * Rust `apply_workspace_delta` helper. Delta fields are `Option<T>` on the
 * Rust side and serialize as `null` when unchanged — so we treat both
 * `undefined` and `null` as "do not overwrite" and only merge concrete
 * values. Missing this nuance wipes the projects list the moment the host
 * sends a controller-only delta.
 */
function mergeDelta(
  snapshot: RemoteWorkspaceSnapshot,
  delta: RemoteWorkspaceDelta,
): RemoteWorkspaceSnapshot {
  const next = { ...snapshot };
  if (delta.appState != null) next.appState = delta.appState;
  if (delta.runtimeState != null) next.runtimeState = delta.runtimeState;
  if (delta.portStatuses != null) next.portStatuses = delta.portStatuses;
  // controllerClientId is intentionally nullable — `null` means "no one has
  // control right now" which is a real, non-stale value, so we always
  // overwrite whatever field is present in the delta. `youHaveControl` is a
  // plain bool on the Rust side (not Option), so serde serializes it on
  // every delta; merge unconditionally.
  if ("controllerClientId" in delta) {
    next.controllerClientId = delta.controllerClientId ?? null;
  }
  if (delta.youHaveControl !== undefined) {
    next.youHaveControl = delta.youHaveControl;
  }
  return next;
}

function upsertAiTab(
  openTabs: SessionTab[],
  payload: RemoteAiTabPayload,
): SessionTab[] {
  const nextTab: SessionTab = {
    id: payload.tab_id,
    type: payload.tab_type,
    projectId: payload.project_id,
    commandId: null,
    ptySessionId: payload.session_id,
    label: payload.label ?? null,
    sshConnectionId: null,
  };
  const existingIdx = openTabs.findIndex((tab) => tab.id === payload.tab_id);
  if (existingIdx === -1) return [...openTabs, nextTab];
  return openTabs.map((tab, idx) => (idx === existingIdx ? nextTab : tab));
}

function mergeAiTabPayload(
  snapshot: RemoteWorkspaceSnapshot,
  payload: RemoteAiTabPayload,
): RemoteWorkspaceSnapshot {
  const nextSnapshot: RemoteWorkspaceSnapshot = {
    ...snapshot,
    appState: {
      ...snapshot.appState,
      open_tabs: upsertAiTab(snapshot.appState.open_tabs ?? [], payload),
    },
  };
  if (payload.session_view) {
    nextSnapshot.runtimeState = {
      ...snapshot.runtimeState,
      sessions: {
        ...snapshot.runtimeState.sessions,
        [payload.session_id]: payload.session_view.runtime,
      },
    };
    nextSnapshot.sessionViews = {
      ...snapshot.sessionViews,
      [payload.session_id]: payload.session_view,
    };
  }
  return nextSnapshot;
}

function bootstrapFromAiTabPayload(
  payload: RemoteAiTabPayload,
): SessionBootstrapFrame | null {
  if (!payload.session_view) return null;
  return {
    sessionId: payload.session_id,
    bytes: new Uint8Array(0),
    screen: payload.session_view.screen,
  };
}

function isAiTabPayload(payload: unknown): payload is RemoteAiTabPayload {
  return (
    !!payload &&
    typeof payload === "object" &&
    "type" in payload &&
    (payload as { type?: unknown }).type === "aiTab"
  );
}

function resolvePendingLaunches(
  snapshot: RemoteWorkspaceSnapshot,
  pending: PendingLaunch[],
): { nextPending: PendingLaunch[]; autoActivateSessionId: string | null } {
  if (pending.length === 0) {
    return { nextPending: pending, autoActivateSessionId: null };
  }
  const openTabs = snapshot.appState?.open_tabs ?? [];
  const stillPending: PendingLaunch[] = [];
  let autoActivateSessionId: string | null = null;
  const now = Date.now();

  for (const launch of pending) {
    // Drop launches older than 15 s — host probably failed silently.
    if (now - launch.issuedAt > 15_000) continue;

    const newTab = openTabs.find(
      (tab) =>
        tab.type === launch.tabType &&
        tab.projectId === launch.projectId &&
        !launch.knownTabIds.has(tab.id),
    );
    if (newTab) {
      const sessionId =
        newTab.ptySessionId ?? newTab.commandId ?? newTab.id;
      autoActivateSessionId = sessionId;
      // Do NOT add this launch back — it's resolved.
    } else {
      stillPending.push(launch);
    }
  }
  return { nextPending: stillPending, autoActivateSessionId };
}

export const useStore = create<StoreState>((set, get) => {
  const applyAiTabPayload = (payload: RemoteAiTabPayload) => {
    const state = get();
    const patch: Partial<StoreState> = {
      activeProjectId: payload.project_id,
      lastError: null,
    };
    if (state.snapshot) {
      patch.snapshot = mergeAiTabPayload(state.snapshot, payload);
    }
    const bootstrap = bootstrapFromAiTabPayload(payload);
    if (bootstrap) {
      const nextBootstraps = new Map(state.pendingBootstraps);
      nextBootstraps.set(payload.session_id, bootstrap);
      patch.pendingBootstraps = nextBootstraps;
    }
    set(patch);
    get().setActiveSession(payload.session_id);
  };

  const requestAiTabAction = async (action: RemoteAction) => {
    const client = get().client;
    if (!client) {
      set({ lastError: "WebSocket is not connected." });
      return;
    }
    try {
      const result = await client.request(action);
      if (!result.ok) {
        set({ lastError: result.message ?? "Remote AI action failed." });
        return;
      }
      const payload = result.payload;
      if (!isAiTabPayload(payload)) {
        set({
          lastError:
            result.message ?? "Remote AI action did not return a tab payload.",
        });
        return;
      }
      applyAiTabPayload(payload);
    } catch (error) {
      set({
        lastError:
          error instanceof Error ? error.message : "Remote AI action failed.",
      });
    }
  };

  return {
  status: { kind: "idle" },
  snapshot: null,
  activeProjectId:
    (typeof localStorage !== "undefined" &&
      localStorage.getItem(ACTIVE_PROJECT_KEY)) ||
    null,
  activeSessionId: null,
  collapsedProjects:
    typeof localStorage !== "undefined"
      ? loadCollapsedProjects()
      : new Set<string>(),
  lastError: null,
  client: null,
  terminalSubscribers: new Map(),
  pendingTerminalFrames: new Map(),
  bootstrapSubscribers: new Map(),
  pendingBootstraps: new Map(),
  pendingLaunches: [],

  init() {
    if (get().client) return;

    const client = new WsClient({
      onStatus: (status) => set({ status }),
      onMessage: (message: WsOutbound) => {
        switch (message.type) {
          case "snapshot": {
            const snapshot = message.workspace;
            const current = get().activeProjectId;
            let activeProjectId = current;
            const projects = snapshot.appState?.config?.projects ?? [];
            if (
              !activeProjectId ||
              !projects.some((p) => p.id === activeProjectId)
            ) {
              activeProjectId = projects[0]?.id ?? null;
            }
            // Check for newly-created AI tabs that we're waiting on.
            const { nextPending, autoActivateSessionId } =
              resolvePendingLaunches(snapshot, get().pendingLaunches);
            const patch: Partial<StoreState> = {
              snapshot,
              activeProjectId,
              lastError: null,
              pendingLaunches: nextPending,
            };
            if (autoActivateSessionId) {
              patch.activeSessionId = autoActivateSessionId;
            }
            set(patch);
            break;
          }
          case "delta": {
            const current = get().snapshot;
            if (!current) break;
            const merged = mergeDelta(current, message.delta);
            const { nextPending, autoActivateSessionId } =
              resolvePendingLaunches(merged, get().pendingLaunches);
            const patch: Partial<StoreState> = {
              snapshot: merged,
              pendingLaunches: nextPending,
            };
            if (autoActivateSessionId) {
              patch.activeSessionId = autoActivateSessionId;
              // Also send the subscribe message we'd normally send via
              // setActiveSession. The browser tracks its own focused session;
              // it should not steer the native host's active terminal.
              const client = get().client;
              const prev = get().activeSessionId;
              if (client) {
                if (prev && prev !== autoActivateSessionId) {
                  client.send({
                    type: "unsubscribeSessions",
                    sessionIds: [prev],
                  });
                }
                client.send({
                  type: "subscribeSessions",
                  sessionIds: [autoActivateSessionId],
                });
              }
            }
            set(patch);
            break;
          }
          case "sessionBootstrap": {
            try {
              // decodeBase64 → Uint8Array
              const binary = atob(message.replayBase64 || "");
              const bytes = new Uint8Array(binary.length);
              for (let i = 0; i < binary.length; i++) {
                bytes[i] = binary.charCodeAt(i);
              }
              const bootstrap: SessionBootstrapFrame = {
                sessionId: message.sessionId,
                bytes,
                screen: message.screen,
              };
              const subscribers = get().bootstrapSubscribers.get(message.sessionId);
              if (subscribers && subscribers.size > 0) {
                subscribers.forEach((fn) => fn(bootstrap));
              } else {
                const { pendingBootstraps } = get();
                const next = new Map(pendingBootstraps);
                next.set(message.sessionId, bootstrap);
                set({ pendingBootstraps: next });
              }
            } catch {
              // malformed base64 — drop it, the terminal will catch up on
              // live output.
            }
            break;
          }
          case "error": {
            set({ lastError: message.message });
            break;
          }
          case "disconnected": {
            const requiresPairing =
              message.message.includes("no longer trusted") ||
              message.message.includes("revoked");
            if (requiresPairing) {
              get().client?.stop();
            }
            set({
              lastError: message.message,
              status: requiresPairing
                ? { kind: "unauthorized" }
                : { kind: "closed", reason: message.message },
              snapshot: requiresPairing ? null : get().snapshot,
              activeSessionId: requiresPairing ? null : get().activeSessionId,
            });
            break;
          }
          default:
            break;
        }
      },
      onSessionOutput: (frame) => {
        const subscribers = get().terminalSubscribers.get(frame.sessionId);
        if (subscribers && subscribers.size > 0) {
          subscribers.forEach((fn) => fn(frame));
          return;
        }
        const pending = get().pendingTerminalFrames;
        const next = new Map(pending);
        const existing = next.get(frame.sessionId) ?? [];
        const buffered = [...existing, frame];
        if (buffered.length > MAX_PENDING_TERMINAL_FRAMES) {
          buffered.splice(0, buffered.length - MAX_PENDING_TERMINAL_FRAMES);
        }
        next.set(frame.sessionId, buffered);
        set({ pendingTerminalFrames: next });
      },
    });
    set({ client });
    void client.start();
  },

  sendAction(action: RemoteAction) {
    const client = get().client;
    if (!client) return;
    client.send({ type: "action", action });
  },

  setActiveProject(projectId: string | null) {
    set({ activeProjectId: projectId });
    if (typeof localStorage !== "undefined") {
      if (projectId) localStorage.setItem(ACTIVE_PROJECT_KEY, projectId);
      else localStorage.removeItem(ACTIVE_PROJECT_KEY);
    }
  },

  setActiveSession(sessionId: string | null) {
    const prev = get().activeSessionId;
    if (prev === sessionId) return;
    const client = get().client;
    if (client) {
      if (prev) {
        client.send({ type: "unsubscribeSessions", sessionIds: [prev] });
      }
      if (sessionId) {
        client.send({ type: "subscribeSessions", sessionIds: [sessionId] });
      }
    }
    set({ activeSessionId: sessionId });
  },

  toggleProjectCollapsed(projectId: string) {
    const current = get().collapsedProjects;
    const next = new Set(current);
    if (next.has(projectId)) next.delete(projectId);
    else next.add(projectId);
    persistCollapsedProjects(next);
    set({ collapsedProjects: next });
  },

  subscribeTerminal(sessionId, listener) {
    const { terminalSubscribers } = get();
    const next = new Map(terminalSubscribers);
    const existing = next.get(sessionId);
    const bucket = existing ? new Set(existing) : new Set();
    bucket.add(listener);
    next.set(sessionId, bucket as Set<(frame: SessionOutputFrame) => void>);
    const pendingFrames = get().pendingTerminalFrames.get(sessionId) ?? [];
    if (pendingFrames.length > 0) {
      for (const frame of pendingFrames) {
        listener(frame);
      }
      const pendingNext = new Map(get().pendingTerminalFrames);
      pendingNext.delete(sessionId);
      set({ terminalSubscribers: next, pendingTerminalFrames: pendingNext });
    } else {
      set({ terminalSubscribers: next });
    }
    return () => {
      const current = get().terminalSubscribers;
      const after = new Map(current);
      const b = after.get(sessionId);
      if (!b) return;
      const shrunk = new Set(b);
      shrunk.delete(listener);
      if (shrunk.size === 0) after.delete(sessionId);
      else after.set(sessionId, shrunk);
      set({ terminalSubscribers: after });
    };
  },

  subscribeBootstrap(sessionId, listener) {
    const { bootstrapSubscribers } = get();
    const next = new Map(bootstrapSubscribers);
    const existing = next.get(sessionId);
    const bucket = existing ? new Set(existing) : new Set();
    bucket.add(listener);
    next.set(
      sessionId,
      bucket as Set<(bootstrap: SessionBootstrapFrame) => void>,
    );
    set({ bootstrapSubscribers: next });
    return () => {
      const current = get().bootstrapSubscribers;
      const after = new Map(current);
      const b = after.get(sessionId);
      if (!b) return;
      const shrunk = new Set(b);
      shrunk.delete(listener);
      if (shrunk.size === 0) after.delete(sessionId);
      else after.set(sessionId, shrunk);
      set({ bootstrapSubscribers: after });
    };
  },

  drainBootstrap(sessionId) {
    const pending = get().pendingBootstraps;
    const bootstrap = pending.get(sessionId);
    if (!bootstrap) return null;
    const next = new Map(pending);
    next.delete(sessionId);
    set({ pendingBootstraps: next });
    return bootstrap;
  },

  takeControl() {
    get().client?.send({ type: "takeControl" });
  },

  releaseControl() {
    get().client?.send({ type: "releaseControl" });
  },

  sendInput(sessionId, text) {
    get().client?.send({ type: "input", sessionId, text });
  },

  pasteImage(sessionId, payload) {
    const ok = get().client?.send({
      type: "pasteImage",
      sessionId,
      mimeType: payload.mimeType,
      fileName: payload.fileName ?? null,
      dataBase64: payload.dataBase64,
    });
    if (!ok) {
      set({ lastError: "WebSocket is not connected." });
      return;
    }
    set({ lastError: null });
  },

  sendResize(sessionId, rows, cols) {
    get().client?.send({ type: "resize", sessionId, rows, cols });
  },

  closeActiveTab() {
    const state = get();
    const sessionId = state.activeSessionId;
    if (!sessionId) return;
    const tabs = state.snapshot?.appState?.open_tabs ?? [];
    const tab = tabs.find((t) => {
      const sid = t.ptySessionId ?? t.commandId ?? t.id;
      return sid === sessionId;
    });
    // For AI/SSH tabs, ask the host to remove the SessionTab from `open_tabs`
    // so the sidebar row disappears on the next delta. Server rows aren't
    // "closed" from a web-UI perspective (the command config stays configured
    // in the project), so we just hide the terminal view.
    if (tab && (tab.type === "claude" || tab.type === "codex" || tab.type === "ssh")) {
      const youHaveControl = state.snapshot?.youHaveControl ?? false;
      if (!youHaveControl) {
        state.client?.send({ type: "takeControl" });
      }
      state.sendAction(
        tab.type === "claude" || tab.type === "codex"
          ? { type: "closeAiTab", tab_id: tab.id }
          : { type: "closeTab", tab_id: tab.id },
      );
    }
    // Hide the view immediately for snappy UX. The delta removing the tab
    // from `open_tabs` will clean up the sidebar row within ~33ms.
    state.setActiveSession(null);
  },

  launchAiTab(projectId, tabType) {
    return requestAiTabAction({
      type: "launchAi",
      project_id: projectId,
      tab_type: tabType,
      dimensions: DEFAULT_DIMENSIONS,
    });
  },

  openAiTab(tabId) {
    return requestAiTabAction({
      type: "openAiTab",
      tab_id: tabId,
      dimensions: DEFAULT_DIMENSIONS,
    });
  },

  restartAiTab(tabId) {
    return requestAiTabAction({
      type: "restartAiTab",
      tab_id: tabId,
      dimensions: DEFAULT_DIMENSIONS,
    });
  },

  openSshTab(connectionId) {
    get().sendAction({ type: "openSshTab", connection_id: connectionId });
  },

  connectSsh(connectionId) {
    get().sendAction({
      type: "connectSsh",
      connection_id: connectionId,
      dimensions: DEFAULT_DIMENSIONS,
    });
  },

  restartSsh(connectionId) {
    get().sendAction({
      type: "restartSsh",
      connection_id: connectionId,
      dimensions: DEFAULT_DIMENSIONS,
    });
  },

  disconnectSsh(connectionId) {
    get().sendAction({ type: "disconnectSsh", connection_id: connectionId });
  },

  stopAllServers() {
    get().sendAction({ type: "stopAllServers" });
  },
  };
});
