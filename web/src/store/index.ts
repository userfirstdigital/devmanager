import { create } from "zustand";

import {
  EMPTY_WRITER_LEASE,
  WEB_PROTOCOL_VERSION,
  type ComposerAccepted,
  type ComposerAttachment,
  type LegacyWorkspaceProjection,
  type RemoteAction,
  type ResumeContext,
  type ResumeState,
  type SemanticEvent,
  type SemanticJournalState,
  type SemanticReplayDescriptor,
  type SemanticReplayPage,
  type StableSessionKey,
  type WebActionPayload,
  type WebImagePastePayload,
  type WebSessionSummary,
  type WebWorkspaceSnapshot,
  type WebWriterLeaseState,
  type WsOutbound,
} from "../api/types";
import type {
  SessionBootstrapFrame,
  SessionOutputFrame,
  WsStatus,
} from "../api/ws";
import { isTransientComposerRejection, WsClient } from "../api/ws";

const ACTIVE_PROJECT_KEY = "devmanager-active-project-id";
const COLLAPSED_PROJECTS_KEY = "devmanager-collapsed-projects";
const MAX_PENDING_TERMINAL_FRAMES = 256;

export interface PendingComposerMutation {
  mutationId: string;
  stableSessionKey: StableSessionKey;
  text: string;
  attachments: ComposerAttachment[];
}

export interface PendingSemanticReplay extends SemanticReplayDescriptor {
  /** Inclusive cursor which the next page must name as fromSequence. */
  nextSequence: number;
}

export interface RawTerminalSlice {
  activeStreamSessionId: string | null;
  streamSessionIdByStableKey: Record<StableSessionKey, string>;
  terminalSubscribers: Map<
    string,
    Set<(frame: SessionOutputFrame) => void>
  >;
  pendingTerminalFrames: Map<string, SessionOutputFrame[]>;
  bootstrapSubscribers: Map<
    string,
    Set<(bootstrap: SessionBootstrapFrame) => void>
  >;
  pendingBootstraps: Map<string, SessionBootstrapFrame>;
}

export interface StoreState {
  status: WsStatus;
  workspace: WebWorkspaceSnapshot | null;
  /** Temporary safe projection consumed by the terminal-first UI only. */
  snapshot: LegacyWorkspaceProjection | null;
  runtimeInstanceId: string | null;
  revision: number | null;
  sessions: Record<StableSessionKey, WebSessionSummary>;
  writerLease: WebWriterLeaseState;
  activeSessionKey: StableSessionKey | null;
  activeProjectId: string | null;
  collapsedProjects: Set<string>;
  journals: Record<StableSessionKey, SemanticJournalState>;
  semanticReplay: PendingSemanticReplay | null;
  drafts: Record<StableSessionKey, string>;
  unread: Record<StableSessionKey, number>;
  /** In-memory route handoff only; Task 6 owns durable route restoration. */
  pendingRoute: string | null;
  pendingMutations: Record<StableSessionKey, PendingComposerMutation>;
  rawTerminal: RawTerminalSlice;
  lastError: string | null;
  client: WsClient | null;

  init(): void;
  applySnapshot(snapshot: WebWorkspaceSnapshot): void;
  applyResumeState(state: ResumeState): void;
  beginSemanticReplay(descriptor: SemanticReplayDescriptor): void;
  applySemanticReplayPage(page: SemanticReplayPage): void;
  appendSemanticEvent(event: SemanticEvent): void;
  setDraft(stableSessionKey: StableSessionKey, text: string): void;
  submitComposer(
    stableSessionKey: StableSessionKey,
    text: string,
    attachments?: ComposerAttachment[],
  ): Promise<ComposerAccepted>;
  sendAction(action: RemoteAction): void;
  setActiveProject(projectId: string | null): void;
  setActiveSession(sessionIdentifier: string | null): void;
  setConnectionVisibility(visible: boolean): void;
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
  drainTerminalFrames(sessionId: string): SessionOutputFrame[];
  refreshActiveConnection(): void;
  takeControl(): void;
  releaseControl(): void;
  sendInput(sessionId: string, text: string): void;
  pasteImage(sessionId: string, payload: WebImagePastePayload): void;
  sendResize(sessionId: string, rows: number, cols: number): void;
  launchAiTab(projectId: string, tabType: "claude" | "codex"): Promise<void>;
  openAiTab(tabId: string): Promise<void>;
  restartAiTab(tabId: string): Promise<void>;
  openSshTab(connectionId: string): void;
  connectSsh(connectionId: string): void;
  restartSsh(connectionId: string): void;
  disconnectSsh(connectionId: string): void;
  stopAllServers(): void;
  closeActiveTab(): void;
}

function emptyRawTerminal(): RawTerminalSlice {
  return {
    activeStreamSessionId: null,
    streamSessionIdByStableKey: {},
    terminalSubscribers: new Map(),
    pendingTerminalFrames: new Map(),
    bootstrapSubscribers: new Map(),
    pendingBootstraps: new Map(),
  };
}

function loadCollapsedProjects(): Set<string> {
  try {
    const raw = globalThis.localStorage?.getItem(COLLAPSED_PROJECTS_KEY);
    if (!raw) return new Set();
    const parsed = JSON.parse(raw) as unknown;
    return Array.isArray(parsed)
      ? new Set(parsed.filter((value): value is string => typeof value === "string"))
      : new Set();
  } catch {
    return new Set();
  }
}

function persistCollapsedProjects(projectIds: Set<string>): void {
  try {
    globalThis.localStorage?.setItem(
      COLLAPSED_PROJECTS_KEY,
      JSON.stringify([...projectIds]),
    );
  } catch {
    // UI preferences are best effort in privacy/quota-limited contexts.
  }
}

function loadActiveProjectId(): string | null {
  try {
    return globalThis.localStorage?.getItem(ACTIVE_PROJECT_KEY) || null;
  } catch {
    return null;
  }
}

function routeForStableKey(stableSessionKey: StableSessionKey): string {
  if (stableSessionKey.startsWith("tab:")) {
    return `/session/tab/${stableSessionKey.slice("tab:".length)}`;
  }
  if (stableSessionKey.startsWith("server:")) {
    return `/session/server/${stableSessionKey.slice("server:".length)}`;
  }
  return "/sessions";
}

function isVisible(): boolean {
  return typeof document === "undefined" || document.visibilityState !== "hidden";
}

function sessionIndex(
  snapshot: WebWorkspaceSnapshot,
): Record<StableSessionKey, WebSessionSummary> {
  const sessions: Record<StableSessionKey, WebSessionSummary> = {};
  for (const session of snapshot.sessions) {
    if (session.stableSessionKey) sessions[session.stableSessionKey] = session;
  }
  return sessions;
}

function streamSessionIndex(
  snapshot: WebWorkspaceSnapshot,
): Record<StableSessionKey, string> {
  const result: Record<StableSessionKey, string> = {};
  for (const session of snapshot.sessions) {
    if (session.stableSessionKey) {
      result[session.stableSessionKey] = session.sessionId;
    }
  }
  return result;
}

function unreadIndex(
  snapshot: WebWorkspaceSnapshot,
): Record<StableSessionKey, number> {
  const result: Record<StableSessionKey, number> = {};
  for (const session of snapshot.sessions) {
    if (session.stableSessionKey && session.attentionCount > 0) {
      result[session.stableSessionKey] = session.attentionCount;
    }
  }
  return result;
}

function projectLegacySnapshot(
  workspace: WebWorkspaceSnapshot,
): LegacyWorkspaceProjection {
  const runtimeSessions: LegacyWorkspaceProjection["runtimeState"]["sessions"] = {};
  for (const session of workspace.sessions) {
    runtimeSessions[session.sessionId] = {
      session_id: session.sessionId,
      stable_session_key: session.stableSessionKey,
      pid: null,
      status: session.status,
      session_kind: session.kind,
      command_id: session.commandId,
      project_id: session.projectId,
      tab_id: session.tabId,
      exit_code: null,
      title: null,
      dimensions: session.dimensions,
    };
  }
  const portStatuses = Object.fromEntries(
    workspace.portStatuses.map((status) => [String(status.port), status]),
  );
  return {
    appState: {
      config: {
        projects: workspace.projects.map((project) => ({
          ...project,
          pinned: false,
          folders: project.folders.map((folder) => ({
            ...folder,
            hidden: false,
          })),
        })),
        sshConnections: workspace.sshConnections,
      },
      open_tabs: workspace.tabs.map((tab) => ({
        id: tab.id,
        type: tab.kind,
        projectId: tab.projectId,
        commandId: tab.commandId,
        ptySessionId: tab.sessionId,
        label: tab.label,
        sshConnectionId: tab.connectionId,
      })),
    },
    runtimeState: { sessions: runtimeSessions },
    portStatuses,
    controllerClientId: workspace.writerLease.ownerClientInstanceId,
    youHaveControl: workspace.writerLease.youAreOwner,
    serverId: workspace.serverId,
  };
}

function updateWorkspaceLease(
  workspace: WebWorkspaceSnapshot | null,
  writerLease: WebWriterLeaseState,
): WebWorkspaceSnapshot | null {
  return workspace ? { ...workspace, writerLease } : null;
}

function resolveStableSessionKey(
  state: StoreState,
  identifier: string,
): StableSessionKey | null {
  if (state.sessions[identifier]) return identifier;
  const direct = state.workspace?.sessions.find(
    (session) => session.sessionId === identifier,
  )?.stableSessionKey;
  if (direct) return direct;
  const tab = state.workspace?.tabs.find(
    (candidate) =>
      candidate.id === identifier ||
      candidate.sessionId === identifier ||
      candidate.commandId === identifier,
  );
  if (tab) {
    return tab.kind === "server" && tab.commandId
      ? `server:${tab.commandId}`
      : `tab:${tab.id}`;
  }
  const commandExists = state.workspace?.projects.some((project) =>
    project.folders.some((folder) =>
      folder.commands.some((command) => command.id === identifier),
    ),
  );
  return commandExists ? `server:${identifier}` : null;
}

function streamIdForKey(
  rawTerminal: RawTerminalSlice,
  stableSessionKey: StableSessionKey | null,
): string | null {
  if (!stableSessionKey) return null;
  const mapped = rawTerminal.streamSessionIdByStableKey[stableSessionKey];
  if (mapped) return mapped;
  return stableSessionKey.startsWith("server:")
    ? stableSessionKey.slice("server:".length)
    : null;
}

function mutationId(): string {
  if (typeof globalThis.crypto?.randomUUID === "function") {
    return globalThis.crypto.randomUUID();
  }
  return `mutation-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

function sameSubmission(
  pending: PendingComposerMutation,
  text: string,
  attachments: ComposerAttachment[],
): boolean {
  return pending.text === text &&
    JSON.stringify(pending.attachments) === JSON.stringify(attachments);
}

function deduplicateEvents(events: SemanticEvent[]): SemanticEvent[] {
  const bySequence = new Map<number, SemanticEvent>();
  for (const event of events) bySequence.set(event.sequence, event);
  return [...bySequence.values()].sort((left, right) => left.sequence - right.sequence);
}

function pageContinuesReplay(
  replay: PendingSemanticReplay,
  page: SemanticReplayPage,
): boolean {
  if (
    page.replayId !== replay.replayId ||
    page.stableSessionKey !== replay.stableSessionKey ||
    page.fromSequence !== replay.nextSequence ||
    page.throughSequence !== replay.throughSequence ||
    page.rollover !== replay.rollover ||
    page.nextSequence < page.fromSequence ||
    page.nextSequence > page.throughSequence
  ) {
    return false;
  }

  let previousSequence = page.fromSequence;
  for (const event of page.events) {
    if (
      event.stableSessionKey !== page.stableSessionKey ||
      event.sequence <= previousSequence ||
      event.sequence > page.nextSequence
    ) {
      return false;
    }
    previousSequence = event.sequence;
  }

  if (page.complete) return page.nextSequence === page.throughSequence;
  return (
    page.events.length > 0 &&
    previousSequence === page.nextSequence &&
    page.nextSequence < page.throughSequence
  );
}

function decodeBootstrap(message: {
  sessionId: string;
  replayBase64: string;
  screen: SessionBootstrapFrame["screen"];
}): SessionBootstrapFrame | null {
  try {
    const binary = atob(message.replayBase64 || "");
    const bytes = new Uint8Array(binary.length);
    for (let index = 0; index < binary.length; index += 1) {
      bytes[index] = binary.charCodeAt(index);
    }
    return { sessionId: message.sessionId, bytes, screen: message.screen };
  } catch {
    return null;
  }
}

function isAiTabPayload(payload: WebActionPayload | null | undefined): payload is WebActionPayload {
  return payload?.type === "aiTab";
}

export const useStore = create<StoreState>((set, get) => {
  const reconcileSnapshot = (
    snapshot: WebWorkspaceSnapshot,
    forceRuntimeReset = false,
  ): void => {
    if (snapshot.webProtocolVersion !== WEB_PROTOCOL_VERSION) {
      set({
        lastError: `Unsupported web protocol ${snapshot.webProtocolVersion}; expected ${WEB_PROTOCOL_VERSION}.`,
      });
      return;
    }

    const current = get();
    const runtimeChanged =
      forceRuntimeReset ||
      (current.runtimeInstanceId !== null &&
        current.runtimeInstanceId !== snapshot.runtimeInstanceId);
    if (runtimeChanged) current.client?.resetRuntime("host runtime changed");

    const nextSessions = sessionIndex(snapshot);
    const nextStreamIds = streamSessionIndex(snapshot);
    const activeSessionKey =
      !runtimeChanged &&
      current.activeSessionKey &&
      nextSessions[current.activeSessionKey]
        ? current.activeSessionKey
        : null;
    const rawTerminal = runtimeChanged
      ? {
          ...emptyRawTerminal(),
          streamSessionIdByStableKey: nextStreamIds,
        }
      : {
          ...current.rawTerminal,
          activeStreamSessionId: streamIdForKey(
            {
              ...current.rawTerminal,
              streamSessionIdByStableKey: nextStreamIds,
            },
            activeSessionKey,
          ),
          streamSessionIdByStableKey: nextStreamIds,
        };
    const activeProjectId =
      current.activeProjectId &&
      snapshot.projects.some((project) => project.id === current.activeProjectId)
        ? current.activeProjectId
        : snapshot.projects[0]?.id ?? null;

    set({
      workspace: snapshot,
      snapshot: projectLegacySnapshot(snapshot),
      runtimeInstanceId: snapshot.runtimeInstanceId,
      revision: snapshot.revision,
      sessions: nextSessions,
      writerLease: snapshot.writerLease,
      activeSessionKey,
      activeProjectId,
      journals: runtimeChanged ? {} : current.journals,
      semanticReplay: runtimeChanged ? null : current.semanticReplay,
      drafts: runtimeChanged ? {} : current.drafts,
      unread: unreadIndex(snapshot),
      pendingRoute: runtimeChanged ? null : current.pendingRoute,
      pendingMutations: runtimeChanged ? {} : current.pendingMutations,
      rawTerminal,
      lastError: null,
    });
  };

  const updateLease = (writerLease: WebWriterLeaseState): void => {
    const workspace = updateWorkspaceLease(get().workspace, writerLease);
    set({
      writerLease,
      workspace,
      snapshot: workspace ? projectLegacySnapshot(workspace) : get().snapshot,
    });
  };

  const handleResumeState = (resumeState: ResumeState): void => {
    if (resumeState.workspace) {
      reconcileSnapshot(resumeState.workspace, resumeState.hardReset);
    } else if (
      resumeState.hardReset ||
      (get().runtimeInstanceId !== null &&
        get().runtimeInstanceId !== resumeState.runtimeInstanceId)
    ) {
      get().client?.resetRuntime("host runtime changed");
      set({
        workspace: null,
        snapshot: null,
        runtimeInstanceId: resumeState.runtimeInstanceId,
        revision: resumeState.revision,
        sessions: {},
        writerLease: resumeState.writerLease,
        activeSessionKey: null,
        journals: {},
        semanticReplay: null,
        drafts: {},
        unread: {},
        pendingRoute: null,
        pendingMutations: {},
        rawTerminal: emptyRawTerminal(),
      });
    } else {
      set({
        runtimeInstanceId: resumeState.runtimeInstanceId,
        revision: resumeState.revision,
      });
      updateLease(resumeState.writerLease);
    }

    if (!resumeState.hardReset) {
      const desiredSessionKey =
        resumeState.desiredSessionKey && get().sessions[resumeState.desiredSessionKey]
          ? resumeState.desiredSessionKey
          : null;
      set((state) => ({
        activeSessionKey: desiredSessionKey,
        pendingRoute: resumeState.route,
        rawTerminal: {
          ...state.rawTerminal,
          activeStreamSessionId: streamIdForKey(
            state.rawTerminal,
            desiredSessionKey,
          ),
        },
      }));
    }
    updateLease(resumeState.writerLease);
    if (
      !resumeState.hardReset &&
      resumeState.semanticReplay &&
      resumeState.semanticReplay.stableSessionKey === get().activeSessionKey
    ) {
      get().beginSemanticReplay(resumeState.semanticReplay);
    } else {
      set({ semanticReplay: null });
    }
  };

  const handleMessage = (message: WsOutbound): void => {
    switch (message.type) {
      case "snapshot":
        reconcileSnapshot(message.workspace);
        break;
      case "delta":
        reconcileSnapshot(message.delta);
        break;
      case "resumeState":
        handleResumeState(message);
        break;
      case "writerLeaseState":
        updateLease(message.writerLease);
        break;
      case "semanticReplayPage":
        get().applySemanticReplayPage(message);
        break;
      case "semanticEvent":
        get().appendSemanticEvent(message.event);
        break;
      case "composerRejected":
        updateLease(message.writerLease);
        if (!isTransientComposerRejection(message.code)) {
          set({ lastError: message.message });
        }
        break;
      case "sessionBootstrap": {
        const bootstrap = decodeBootstrap(message);
        if (!bootstrap) break;
        const subscribers = get().rawTerminal.bootstrapSubscribers.get(message.sessionId);
        if (subscribers?.size) {
          subscribers.forEach((listener) => listener(bootstrap));
        } else {
          set((state) => {
            const pendingBootstraps = new Map(
              state.rawTerminal.pendingBootstraps,
            );
            pendingBootstraps.set(message.sessionId, bootstrap);
            return {
              rawTerminal: { ...state.rawTerminal, pendingBootstraps },
            };
          });
        }
        break;
      }
      case "sessionRemoved":
      case "sessionClosed":
        set((state) => {
          if (state.rawTerminal.activeStreamSessionId !== message.sessionId) {
            return state;
          }
          return {
            activeSessionKey: null,
            pendingRoute: "/sessions",
            rawTerminal: {
              ...state.rawTerminal,
              activeStreamSessionId: null,
            },
          };
        });
        break;
      case "error":
        set({ lastError: message.message });
        break;
      case "disconnected": {
        const requiresPairing =
          message.message.includes("no longer trusted") ||
          message.message.includes("revoked");
        if (requiresPairing) {
          get().client?.stop();
          get().client?.resetRuntime("browser pairing was revoked");
          set({
            status: { kind: "unauthorized" },
            workspace: null,
            snapshot: null,
            runtimeInstanceId: null,
            revision: null,
            sessions: {},
            writerLease: { ...EMPTY_WRITER_LEASE },
            activeSessionKey: null,
            journals: {},
            semanticReplay: null,
            drafts: {},
            unread: {},
            pendingRoute: null,
            pendingMutations: {},
            rawTerminal: emptyRawTerminal(),
            lastError: message.message,
          });
        } else {
          set({
            status: { kind: "closed", reason: message.message },
            lastError: message.message,
          });
        }
        break;
      }
      default:
        break;
    }
  };

  const handleSessionOutput = (frame: SessionOutputFrame): void => {
    const subscribers = get().rawTerminal.terminalSubscribers.get(frame.sessionId);
    if (subscribers?.size) {
      subscribers.forEach((listener) => listener(frame));
      return;
    }
    set((state) => {
      const pendingTerminalFrames = new Map(
        state.rawTerminal.pendingTerminalFrames,
      );
      const buffered = [
        ...(pendingTerminalFrames.get(frame.sessionId) ?? []),
        frame,
      ];
      if (buffered.length > MAX_PENDING_TERMINAL_FRAMES) {
        buffered.splice(0, buffered.length - MAX_PENDING_TERMINAL_FRAMES);
      }
      pendingTerminalFrames.set(frame.sessionId, buffered);
      return {
        rawTerminal: { ...state.rawTerminal, pendingTerminalFrames },
      };
    });
  };

  const requestAiTabAction = async (action: RemoteAction): Promise<void> => {
    const client = get().client;
    if (!client) {
      set({ lastError: "WebSocket is not connected." });
      return;
    }
    try {
      const result = await client.request(action);
      if (!result.ok || !isAiTabPayload(result.payload)) {
        set({ lastError: result.message ?? "Remote AI action failed." });
        return;
      }
      const payload = result.payload;
      const stableSessionKey = `tab:${payload.tabId}`;
      set((state) => ({
        activeProjectId: payload.projectId,
        activeSessionKey: stableSessionKey,
        pendingRoute: routeForStableKey(stableSessionKey),
        rawTerminal: {
          ...state.rawTerminal,
          activeStreamSessionId: payload.sessionId,
          streamSessionIdByStableKey: {
            ...state.rawTerminal.streamSessionIdByStableKey,
            [stableSessionKey]: payload.sessionId,
          },
        },
        lastError: null,
      }));
      client.wake();
    } catch (error) {
      set({
        lastError:
          error instanceof Error ? error.message : "Remote AI action failed.",
      });
    }
  };

  return {
    status: { kind: "idle" },
    workspace: null,
    snapshot: null,
    runtimeInstanceId: null,
    revision: null,
    sessions: {},
    writerLease: { ...EMPTY_WRITER_LEASE },
    activeSessionKey: null,
    activeProjectId: loadActiveProjectId(),
    collapsedProjects: loadCollapsedProjects(),
    journals: {},
    semanticReplay: null,
    drafts: {},
    unread: {},
    pendingRoute: null,
    pendingMutations: {},
    rawTerminal: emptyRawTerminal(),
    lastError: null,
    client: null,

    init() {
      if (get().client) return;
      const client = new WsClient({
        onStatus: (status) => {
          if (
            status.kind === "connecting" ||
            status.kind === "closed" ||
            status.kind === "unauthorized" ||
            status.kind === "idle"
          ) {
            updateLease({ ...EMPTY_WRITER_LEASE });
          }
          set({ status });
        },
        onMessage: handleMessage,
        onSessionOutput: handleSessionOutput,
        getResumeContext: (): ResumeContext => {
          const state = get();
          const stableSessionKey = state.activeSessionKey;
          const visible = isVisible();
          return {
            seenRuntimeInstanceId: state.runtimeInstanceId,
            seenRevision: state.revision,
            route:
              state.pendingRoute ??
              (stableSessionKey ? routeForStableKey(stableSessionKey) : "/sessions"),
            desiredSessionKey: stableSessionKey,
            semanticAfterSequence: stableSessionKey
              ? state.journals[stableSessionKey]?.latestSequence ?? null
              : null,
            visible,
            wantsWriterLease: visible,
          };
        },
      });
      set({ client });
      void client.start();
    },

    applySnapshot(snapshot) {
      reconcileSnapshot(snapshot);
    },

    applyResumeState(resumeState) {
      handleResumeState(resumeState);
    },

    beginSemanticReplay(descriptor) {
      if (descriptor.throughSequence < descriptor.fromSequence) {
        set({
          semanticReplay: null,
          lastError: "Host sent an invalid semantic replay range.",
        });
        return;
      }
      set((state) => {
        const journals = descriptor.rollover
          ? {
              ...state.journals,
              [descriptor.stableSessionKey]: {
                stableSessionKey: descriptor.stableSessionKey,
                oldestSequence: 0,
                latestSequence: descriptor.fromSequence,
                cursorRolledOver: true,
                events: [],
              },
            }
          : state.journals;
        return {
          journals,
          semanticReplay: {
            ...descriptor,
            nextSequence: descriptor.fromSequence,
          },
          lastError: null,
        };
      });
    },

    applySemanticReplayPage(page) {
      set((state) => {
        const replay = state.semanticReplay;
        if (!replay || !pageContinuesReplay(replay, page)) return state;

        const existing = state.journals[page.stableSessionKey];
        const events = deduplicateEvents([
          ...(existing?.events ?? []),
          ...page.events,
        ]);
        const journal: SemanticJournalState = {
          stableSessionKey: page.stableSessionKey,
          oldestSequence: events[0]?.sequence ?? 0,
          latestSequence: Math.max(
            existing?.latestSequence ?? page.fromSequence,
            page.nextSequence,
          ),
          cursorRolledOver: page.rollover,
          events,
        };
        return {
          journals: {
            ...state.journals,
            [page.stableSessionKey]: journal,
          },
          semanticReplay: page.complete
            ? null
            : { ...replay, nextSequence: page.nextSequence },
        };
      });
    },

    appendSemanticEvent(event) {
      set((state) => {
        const existing = state.journals[event.stableSessionKey];
        if (
          existing?.cursorRolledOver &&
          existing.oldestSequence > 0 &&
          event.sequence < existing.oldestSequence
        ) {
          return state;
        }
        const events = deduplicateEvents([...(existing?.events ?? []), event]);
        const journal: SemanticJournalState = {
          stableSessionKey: event.stableSessionKey,
          oldestSequence:
            existing?.oldestSequence || events[0]?.sequence || event.sequence,
          latestSequence: Math.max(existing?.latestSequence ?? 0, event.sequence),
          cursorRolledOver: existing?.cursorRolledOver ?? false,
          events,
        };
        return {
          journals: {
            ...state.journals,
            [event.stableSessionKey]: journal,
          },
        };
      });
    },

    setDraft(stableSessionKey, text) {
      const pending = get().pendingMutations[stableSessionKey];
      if (pending && pending.text !== text) {
        get().client?.cancelComposer(
          pending.mutationId,
          "composer mutation superseded",
        );
      }
      set((state) => ({
        drafts: { ...state.drafts, [stableSessionKey]: text },
        pendingMutations:
          pending && pending.text !== text
            ? Object.fromEntries(
                Object.entries(state.pendingMutations).filter(
                  ([key, mutation]) =>
                    key !== stableSessionKey ||
                    mutation.mutationId !== pending.mutationId,
                ),
              )
            : state.pendingMutations,
      }));
    },

    async submitComposer(stableSessionKey, text, attachments = []) {
      const state = get();
      const existing = state.pendingMutations[stableSessionKey];
      if (existing && !sameSubmission(existing, text, attachments)) {
        state.client?.cancelComposer(
          existing.mutationId,
          "composer mutation superseded",
        );
      }
      const pending =
        existing && sameSubmission(existing, text, attachments)
          ? existing
          : {
              mutationId: mutationId(),
              stableSessionKey,
              text,
              attachments,
            };
      set((current) => ({
        drafts: { ...current.drafts, [stableSessionKey]: text },
        pendingMutations: {
          ...current.pendingMutations,
          [stableSessionKey]: pending,
        },
      }));
      const client = get().client;
      if (!client) {
        const error = new Error("WebSocket is not connected.");
        set({ lastError: error.message });
        throw error;
      }
      try {
        const accepted = await client.submitComposer(pending);
        set((current) => {
          if (
            current.pendingMutations[stableSessionKey]?.mutationId !==
            accepted.mutationId
          ) {
            return current;
          }
          const pendingMutations = { ...current.pendingMutations };
          delete pendingMutations[stableSessionKey];
          return {
            pendingMutations,
            drafts: {
              ...current.drafts,
              [stableSessionKey]:
                current.drafts[stableSessionKey] === pending.text
                  ? ""
                  : current.drafts[stableSessionKey] ?? "",
            },
            lastError: null,
          };
        });
        return accepted;
      } catch (error) {
        if (
          get().pendingMutations[stableSessionKey]?.mutationId ===
          pending.mutationId
        ) {
          set({
            lastError:
              error instanceof Error ? error.message : "Composer submission failed.",
          });
        }
        throw error;
      }
    },

    sendAction(action) {
      const client = get().client;
      if (!client) {
        set({ lastError: "WebSocket is not connected." });
        return;
      }
      void client
        .request(action)
        .then((result) => {
          set({
            lastError: result.ok
              ? null
              : result.message ?? "Remote action failed.",
          });
        })
        .catch((error: unknown) => {
          set({
            lastError:
              error instanceof Error ? error.message : "Remote action failed.",
          });
        });
    },

    setActiveProject(projectId) {
      set({ activeProjectId: projectId });
      try {
        if (projectId) globalThis.localStorage?.setItem(ACTIVE_PROJECT_KEY, projectId);
        else globalThis.localStorage?.removeItem(ACTIVE_PROJECT_KEY);
      } catch {
        // UI preference only.
      }
    },

    setActiveSession(sessionIdentifier) {
      const state = get();
      const stableSessionKey = sessionIdentifier
        ? resolveStableSessionKey(state, sessionIdentifier)
        : null;
      set((current) => ({
        activeSessionKey: stableSessionKey,
        pendingRoute: stableSessionKey
          ? routeForStableKey(stableSessionKey)
          : "/sessions",
        rawTerminal: {
          ...current.rawTerminal,
          activeStreamSessionId: streamIdForKey(
            current.rawTerminal,
            stableSessionKey,
          ),
        },
      }));
      get().client?.wake();
    },

    setConnectionVisibility(visible) {
      get().client?.setVisibility(visible);
    },

    toggleProjectCollapsed(projectId) {
      const collapsedProjects = new Set(get().collapsedProjects);
      if (collapsedProjects.has(projectId)) collapsedProjects.delete(projectId);
      else collapsedProjects.add(projectId);
      persistCollapsedProjects(collapsedProjects);
      set({ collapsedProjects });
    },

    subscribeTerminal(sessionId, listener) {
      set((state) => {
        const terminalSubscribers = new Map(
          state.rawTerminal.terminalSubscribers,
        );
        const listeners = new Set(terminalSubscribers.get(sessionId) ?? []);
        listeners.add(listener);
        terminalSubscribers.set(sessionId, listeners);
        return {
          rawTerminal: { ...state.rawTerminal, terminalSubscribers },
        };
      });
      return () => {
        set((state) => {
          const terminalSubscribers = new Map(
            state.rawTerminal.terminalSubscribers,
          );
          const listeners = new Set(terminalSubscribers.get(sessionId) ?? []);
          listeners.delete(listener);
          if (listeners.size) terminalSubscribers.set(sessionId, listeners);
          else terminalSubscribers.delete(sessionId);
          return {
            rawTerminal: { ...state.rawTerminal, terminalSubscribers },
          };
        });
      };
    },

    subscribeBootstrap(sessionId, listener) {
      set((state) => {
        const bootstrapSubscribers = new Map(
          state.rawTerminal.bootstrapSubscribers,
        );
        const listeners = new Set(bootstrapSubscribers.get(sessionId) ?? []);
        listeners.add(listener);
        bootstrapSubscribers.set(sessionId, listeners);
        return {
          rawTerminal: { ...state.rawTerminal, bootstrapSubscribers },
        };
      });
      return () => {
        set((state) => {
          const bootstrapSubscribers = new Map(
            state.rawTerminal.bootstrapSubscribers,
          );
          const listeners = new Set(bootstrapSubscribers.get(sessionId) ?? []);
          listeners.delete(listener);
          if (listeners.size) bootstrapSubscribers.set(sessionId, listeners);
          else bootstrapSubscribers.delete(sessionId);
          return {
            rawTerminal: { ...state.rawTerminal, bootstrapSubscribers },
          };
        });
      };
    },

    drainBootstrap(sessionId) {
      const bootstrap = get().rawTerminal.pendingBootstraps.get(sessionId);
      if (!bootstrap) return null;
      set((state) => {
        const pendingBootstraps = new Map(state.rawTerminal.pendingBootstraps);
        pendingBootstraps.delete(sessionId);
        return { rawTerminal: { ...state.rawTerminal, pendingBootstraps } };
      });
      return bootstrap;
    },

    drainTerminalFrames(sessionId) {
      const frames = get().rawTerminal.pendingTerminalFrames.get(sessionId) ?? [];
      if (!frames.length) return [];
      set((state) => {
        const pendingTerminalFrames = new Map(
          state.rawTerminal.pendingTerminalFrames,
        );
        pendingTerminalFrames.delete(sessionId);
        return {
          rawTerminal: { ...state.rawTerminal, pendingTerminalFrames },
        };
      });
      return frames;
    },

    refreshActiveConnection() {
      get().client?.wake();
    },

    takeControl() {
      get().client?.wake();
    },

    releaseControl() {
      get().client?.setVisibility(false);
    },

    sendInput(sessionId, text) {
      const state = get();
      state.client?.ensureWriterLease();
      state.client?.send({
        type: "input",
        sessionId,
        text,
        expectedLeaseGeneration: state.writerLease.generation,
      });
    },

    pasteImage(sessionId, payload) {
      const state = get();
      state.client?.ensureWriterLease();
      const sent = state.client?.send({
        type: "pasteImage",
        sessionId,
        mimeType: payload.mimeType,
        fileName: payload.fileName ?? null,
        dataBase64: payload.dataBase64,
        expectedLeaseGeneration: state.writerLease.generation,
      });
      set({ lastError: sent ? null : "WebSocket is not connected." });
    },

    sendResize(sessionId, rows, cols) {
      const state = get();
      state.client?.ensureWriterLease();
      state.client?.send({
        type: "resize",
        sessionId,
        rows,
        cols,
        expectedLeaseGeneration: state.writerLease.generation,
      });
    },

    launchAiTab(projectId, tabType) {
      return requestAiTabAction({
        type: "launchAi",
        project_id: projectId,
        tab_type: tabType,
      });
    },

    async openAiTab(tabId) {
      get().setActiveSession(`tab:${tabId}`);
    },

    restartAiTab(tabId) {
      return requestAiTabAction({ type: "restartAiTab", tab_id: tabId });
    },

    openSshTab(connectionId) {
      get().sendAction({ type: "openSshTab", connection_id: connectionId });
    },

    connectSsh(connectionId) {
      get().sendAction({ type: "connectSsh", connection_id: connectionId });
    },

    restartSsh(connectionId) {
      get().sendAction({ type: "restartSsh", connection_id: connectionId });
    },

    disconnectSsh(connectionId) {
      get().sendAction({ type: "disconnectSsh", connection_id: connectionId });
    },

    stopAllServers() {
      get().sendAction({ type: "stopAllServers" });
    },

    closeActiveTab() {
      const state = get();
      const stableSessionKey = state.activeSessionKey;
      if (!stableSessionKey) return;
      if (stableSessionKey.startsWith("tab:")) {
        state.sendAction({
          type: "closeTab",
          tab_id: stableSessionKey.slice("tab:".length),
        });
      }
      state.setActiveSession(null);
    },
  };
});
