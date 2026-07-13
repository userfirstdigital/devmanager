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
export const MAX_SEMANTIC_EVENTS_PER_SESSION = 5_000;
/** Per-session UTF-8 JSON payload budget suitable for long-running mobile tabs. */
export const MAX_SEMANTIC_BYTES_PER_SESSION = 2 * 1_024 * 1_024;

export interface BoundedSemanticJournalState extends SemanticJournalState {
  retainedBytes: number;
}

export interface PendingComposerMutation {
  mutationId: string;
  stableSessionKey: StableSessionKey;
  text: string;
  attachments: ComposerAttachment[];
}

export interface PendingSemanticReplay extends SemanticReplayDescriptor {
  /** Inclusive cursor which the next page must name as fromSequence. */
  nextSequence: number;
  /** Ordered post-capture events held until the descriptor-led replay completes. */
  bufferedLiveEvents?: SemanticEvent[];
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

export interface WebCompatibilityDiagnostic {
  expectedProtocolVersion: number;
  receivedProtocolVersion: number;
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
  journals: Record<StableSessionKey, BoundedSemanticJournalState>;
  semanticReplay: PendingSemanticReplay | null;
  /** Sessions waiting for an authoritative replay after a detected live gap. */
  semanticGapKeys: Set<StableSessionKey>;
  /** Highest live sequence observed while each gap waits for Resume. */
  semanticGapSequences: Record<StableSessionKey, number>;
  drafts: Record<StableSessionKey, string>;
  unread: Record<StableSessionKey, number>;
  /** In-memory route handoff only; Task 6 owns durable route restoration. */
  pendingRoute: string | null;
  pendingMutations: Record<StableSessionKey, PendingComposerMutation>;
  rawTerminal: RawTerminalSlice;
  /** Fail-closed handoff consumed by automatic PWA compatibility recovery. */
  compatibilityDiagnostic: WebCompatibilityDiagnostic | null;
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
  foregroundConnection(): void;
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
  prepareComposer(): void;
  interruptSession(stableSessionKey: StableSessionKey): void;
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

interface BoundedSemanticEvents {
  events: SemanticEvent[];
  retainedBytes: number;
  evicted: boolean;
}

const semanticEventEncoder = new TextEncoder();

function semanticEventBytes(event: SemanticEvent): number {
  return semanticEventEncoder.encode(JSON.stringify(event)).byteLength;
}

function capSemanticEvents(events: SemanticEvent[]): BoundedSemanticEvents {
  let start = events.length;
  let retainedBytes = 0;
  while (
    start > 0 &&
    events.length - start < MAX_SEMANTIC_EVENTS_PER_SESSION
  ) {
    const nextBytes = semanticEventBytes(events[start - 1]);
    if (retainedBytes + nextBytes > MAX_SEMANTIC_BYTES_PER_SESSION) break;
    retainedBytes += nextBytes;
    start -= 1;
  }
  return {
    events: start === 0 ? events : events.slice(start),
    retainedBytes,
    evicted: start > 0,
  };
}

function appendCappedSemanticEvent(
  existing: BoundedSemanticJournalState | undefined,
  event: SemanticEvent,
): BoundedSemanticEvents {
  return capSemanticEvents(mergeOrderedEvents(existing?.events ?? [], [event]));
}

function mergeOrderedEvents(
  left: SemanticEvent[],
  right: SemanticEvent[],
): SemanticEvent[] {
  const bySequence = new Map<number, SemanticEvent>();
  left.forEach((event) => bySequence.set(event.sequence, event));
  for (const event of right) {
    if (event.replacesSequence !== undefined) {
      bySequence.delete(event.replacesSequence);
    }
    // A replay page or newer live event is authoritative for a repeated sequence.
    bySequence.set(event.sequence, event);
  }
  return [...bySequence.values()].sort((a, b) => a.sequence - b.sequence);
}

function filterRecord<T>(
  record: Record<StableSessionKey, T>,
  validKeys: Set<StableSessionKey>,
): Record<StableSessionKey, T> {
  return Object.fromEntries(
    Object.entries(record).filter(([key]) => validKeys.has(key)),
  );
}

function filterSessionMap<T>(map: Map<string, T>, validIds: Set<string>): Map<string, T> {
  return new Map([...map].filter(([sessionId]) => validIds.has(sessionId)));
}

function stableKeyForStreamSession(
  state: Pick<StoreState, "rawTerminal" | "sessions">,
  sessionId: string,
): StableSessionKey | null {
  const mapped = Object.entries(state.rawTerminal.streamSessionIdByStableKey).find(
    ([, streamSessionId]) => streamSessionId === sessionId,
  )?.[0];
  if (mapped) return mapped;
  return (
    Object.values(state.sessions).find((session) => session.sessionId === sessionId)
      ?.stableSessionKey ?? null
  );
}

function rawTerminalWithoutSession(
  rawTerminal: RawTerminalSlice,
  sessionId: string,
  stableSessionKey: StableSessionKey | null,
): RawTerminalSlice {
  const streamSessionIdByStableKey = Object.fromEntries(
    Object.entries(rawTerminal.streamSessionIdByStableKey).filter(
      ([key, streamId]) => key !== stableSessionKey && streamId !== sessionId,
    ),
  );
  const terminalSubscribers = new Map(rawTerminal.terminalSubscribers);
  const pendingTerminalFrames = new Map(rawTerminal.pendingTerminalFrames);
  const bootstrapSubscribers = new Map(rawTerminal.bootstrapSubscribers);
  const pendingBootstraps = new Map(rawTerminal.pendingBootstraps);
  terminalSubscribers.delete(sessionId);
  pendingTerminalFrames.delete(sessionId);
  bootstrapSubscribers.delete(sessionId);
  pendingBootstraps.delete(sessionId);
  return {
    ...rawTerminal,
    activeStreamSessionId:
      rawTerminal.activeStreamSessionId === sessionId
        ? null
        : rawTerminal.activeStreamSessionId,
    streamSessionIdByStableKey,
    terminalSubscribers,
    pendingTerminalFrames,
    bootstrapSubscribers,
    pendingBootstraps,
  };
}

function knowsRawSession(rawTerminal: RawTerminalSlice, sessionId: string): boolean {
  return Object.values(rawTerminal.streamSessionIdByStableKey).includes(sessionId);
}

function reconcileJournals(
  journals: Record<StableSessionKey, BoundedSemanticJournalState>,
  sessions: Record<StableSessionKey, WebSessionSummary>,
): Record<StableSessionKey, BoundedSemanticJournalState> {
  const reconciled: Record<StableSessionKey, BoundedSemanticJournalState> = {};
  for (const [stableSessionKey, session] of Object.entries(sessions)) {
    const journal = journals[stableSessionKey];
    if (!journal) continue;
    const firstRetainedSequence = journal.events[0]?.sequence ?? 0;
    const hasValidBoundMetadata =
      Number.isSafeInteger(journal.retainedBytes) &&
      journal.retainedBytes >= 0 &&
      journal.retainedBytes <= MAX_SEMANTIC_BYTES_PER_SESSION &&
      journal.events.length <= MAX_SEMANTIC_EVENTS_PER_SESSION &&
      journal.oldestSequence === firstRetainedSequence;
    const hostRequiresTrim =
      session.oldestSequence > 0 &&
      journal.events.length > 0 &&
      firstRetainedSequence < session.oldestSequence;
    if (hasValidBoundMetadata && !hostRequiresTrim) {
      reconciled[stableSessionKey] = journal;
      continue;
    }
    const hostRetained = session.oldestSequence > 0
      ? journal.events.filter((event) => event.sequence >= session.oldestSequence)
      : journal.events;
    const bounded = capSemanticEvents(hostRetained);
    const trimmed = bounded.events.length !== journal.events.length;
    reconciled[stableSessionKey] = {
      ...journal,
      oldestSequence: bounded.events[0]?.sequence ?? 0,
      cursorRolledOver: journal.cursorRolledOver || trimmed,
      events: bounded.events,
      retainedBytes: bounded.retainedBytes,
    };
  }
  return reconciled;
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
  let runtimeOperationEpoch = 0;
  let nextAsyncOperationId = 1;
  const activeAsyncOperations = new Set<number>();

  interface AsyncOperationToken {
    id: number;
    runtimeEpoch: number;
    runtimeInstanceId: string | null;
    client: WsClient;
  }

  const beginAsyncOperation = (client: WsClient): AsyncOperationToken => {
    const token = {
      id: nextAsyncOperationId++,
      runtimeEpoch: runtimeOperationEpoch,
      runtimeInstanceId: get().runtimeInstanceId,
      client,
    };
    activeAsyncOperations.add(token.id);
    return token;
  };

  const completeAsyncOperation = (token: AsyncOperationToken): boolean => {
    if (!activeAsyncOperations.delete(token.id)) return false;
    return (
      token.runtimeEpoch === runtimeOperationEpoch &&
      token.runtimeInstanceId === get().runtimeInstanceId &&
      token.client === get().client
    );
  };

  const invalidateAsyncOperations = (): void => {
    runtimeOperationEpoch += 1;
    activeAsyncOperations.clear();
  };

  const reconcileSnapshot = (
    snapshot: WebWorkspaceSnapshot,
    forceRuntimeReset = false,
  ): void => {
    const current = get();
    if (snapshot.webProtocolVersion !== WEB_PROTOCOL_VERSION) {
      const message = `Host web protocol ${snapshot.webProtocolVersion} is incompatible with browser protocol ${WEB_PROTOCOL_VERSION}.`;
      invalidateAsyncOperations();
      current.client?.resetRuntime(message);
      current.client?.stop();
      set({
        status: { kind: "closed", reason: message },
        workspace: null,
        snapshot: null,
        runtimeInstanceId: null,
        revision: null,
        sessions: {},
        writerLease: { ...EMPTY_WRITER_LEASE },
        activeSessionKey: null,
        journals: {},
        semanticReplay: null,
        semanticGapKeys: new Set(),
        semanticGapSequences: {},
        drafts: {},
        unread: {},
        pendingRoute: null,
        pendingMutations: {},
        rawTerminal: emptyRawTerminal(),
        compatibilityDiagnostic: {
          expectedProtocolVersion: WEB_PROTOCOL_VERSION,
          receivedProtocolVersion: snapshot.webProtocolVersion,
        },
        lastError: message,
        client: null,
      });
      return;
    }

    const runtimeChanged =
      forceRuntimeReset ||
      (current.runtimeInstanceId !== null &&
        current.runtimeInstanceId !== snapshot.runtimeInstanceId);
    if (runtimeChanged) {
      invalidateAsyncOperations();
      current.client?.resetRuntime("host runtime changed");
    }

    const nextSessions = sessionIndex(snapshot);
    const nextStreamIds = streamSessionIndex(snapshot);
    const validStableKeys = new Set(Object.keys(nextSessions));
    const validStreamSessionIds = new Set(
      snapshot.sessions.map((session) => session.sessionId),
    );
    if (!runtimeChanged) {
      const previousStreamSessionIds = new Set([
        ...Object.values(current.rawTerminal.streamSessionIdByStableKey),
        ...Object.values(current.sessions).map((session) => session.sessionId),
      ]);
      for (const sessionId of previousStreamSessionIds) {
        if (!validStreamSessionIds.has(sessionId)) {
          current.client?.discardWriterFramesForSession(sessionId);
        }
      }
      for (const [stableSessionKey, mutation] of Object.entries(
        current.pendingMutations,
      )) {
        if (!validStableKeys.has(stableSessionKey)) {
          current.client?.cancelComposer(
            mutation.mutationId,
            "session removed by host",
          );
        }
      }
    }
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
          terminalSubscribers: filterSessionMap(
            current.rawTerminal.terminalSubscribers,
            validStreamSessionIds,
          ),
          pendingTerminalFrames: filterSessionMap(
            current.rawTerminal.pendingTerminalFrames,
            validStreamSessionIds,
          ),
          bootstrapSubscribers: filterSessionMap(
            current.rawTerminal.bootstrapSubscribers,
            validStreamSessionIds,
          ),
          pendingBootstraps: filterSessionMap(
            current.rawTerminal.pendingBootstraps,
            validStreamSessionIds,
          ),
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
      journals: runtimeChanged ? {} : reconcileJournals(current.journals, nextSessions),
      semanticReplay:
        runtimeChanged ||
        !current.semanticReplay ||
        !validStableKeys.has(current.semanticReplay.stableSessionKey)
          ? null
          : current.semanticReplay,
      semanticGapKeys: runtimeChanged
        ? new Set()
        : new Set(
            [...current.semanticGapKeys].filter((key) => validStableKeys.has(key)),
          ),
      semanticGapSequences: runtimeChanged
        ? {}
        : filterRecord(current.semanticGapSequences, validStableKeys),
      drafts: runtimeChanged ? {} : filterRecord(current.drafts, validStableKeys),
      unread: unreadIndex(snapshot),
      pendingRoute: runtimeChanged
        ? null
        : current.activeSessionKey && !activeSessionKey
          ? "/sessions"
          : current.pendingRoute,
      pendingMutations: runtimeChanged
        ? {}
        : filterRecord(current.pendingMutations, validStableKeys),
      rawTerminal,
      compatibilityDiagnostic: null,
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
      invalidateAsyncOperations();
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
        semanticGapKeys: new Set(),
        semanticGapSequences: {},
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
      // Resume is authoritative: if the host has no replay for a live gap,
      // advance past the live frames we already observed and mark the local
      // history rolled over. This loses only the unavailable gap while letting
      // the next contiguous event flow without a Resume storm.
      set((state) => {
        const stableSessionKey = get().activeSessionKey;
        if (!stableSessionKey || !state.semanticGapKeys.has(stableSessionKey)) {
          return { semanticReplay: null };
        }
        const existing = state.journals[stableSessionKey];
        const latestSequence = Math.max(
          existing?.latestSequence ?? 0,
          state.sessions[stableSessionKey]?.latestSequence ?? 0,
          state.semanticGapSequences[stableSessionKey] ?? 0,
        );
        const semanticGapKeys = new Set(state.semanticGapKeys);
        semanticGapKeys.delete(stableSessionKey);
        const semanticGapSequences = { ...state.semanticGapSequences };
        delete semanticGapSequences[stableSessionKey];
        return {
          journals: {
            ...state.journals,
            [stableSessionKey]: {
              stableSessionKey,
              oldestSequence: existing?.events[0]?.sequence ?? 0,
              latestSequence,
              cursorRolledOver: true,
              events: existing?.events ?? [],
              retainedBytes: existing?.retainedBytes ?? 0,
            },
          },
          semanticReplay: null,
          semanticGapKeys,
          semanticGapSequences,
        };
      });
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
        if (!knowsRawSession(get().rawTerminal, message.sessionId)) break;
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
      case "sessionClosed": {
        const stableSessionKey = stableKeyForStreamSession(get(), message.sessionId);
        get().client?.discardWriterFramesForSession(message.sessionId);
        set((state) => {
          return {
            rawTerminal: rawTerminalWithoutSession(
              state.rawTerminal,
              message.sessionId,
              stableSessionKey,
            ),
          };
        });
        break;
      }
      case "sessionRemoved": {
        const before = get();
        const stableSessionKey = stableKeyForStreamSession(before, message.sessionId);
        const pendingMutation = stableSessionKey
          ? before.pendingMutations[stableSessionKey]
          : undefined;
        before.client?.discardWriterFramesForSession(message.sessionId);
        if (pendingMutation) {
          before.client?.cancelComposer(
            pendingMutation.mutationId,
            "session removed by host",
          );
        }
        set((state) => {
          const workspace = state.workspace
            ? {
                ...state.workspace,
                sessions: state.workspace.sessions.filter(
                  (session) => session.sessionId !== message.sessionId,
                ),
                tabs: state.workspace.tabs.map((tab) =>
                  tab.sessionId === message.sessionId
                    ? { ...tab, sessionId: null }
                    : tab,
                ),
              }
            : null;
          const sessions = { ...state.sessions };
          const journals = { ...state.journals };
          const drafts = { ...state.drafts };
          const unread = { ...state.unread };
          const semanticGapKeys = new Set(state.semanticGapKeys);
          const semanticGapSequences = { ...state.semanticGapSequences };
          const pendingMutations = { ...state.pendingMutations };
          if (stableSessionKey) {
            delete sessions[stableSessionKey];
            delete journals[stableSessionKey];
            delete drafts[stableSessionKey];
            delete unread[stableSessionKey];
            semanticGapKeys.delete(stableSessionKey);
            delete semanticGapSequences[stableSessionKey];
            delete pendingMutations[stableSessionKey];
          }
          const removedActiveSession = state.activeSessionKey === stableSessionKey;
          return {
            workspace,
            snapshot: workspace ? projectLegacySnapshot(workspace) : null,
            sessions,
            activeSessionKey: removedActiveSession ? null : state.activeSessionKey,
            journals,
            semanticReplay:
              state.semanticReplay?.stableSessionKey === stableSessionKey
                ? null
                : state.semanticReplay,
            semanticGapKeys,
            semanticGapSequences,
            drafts,
            unread,
            pendingRoute: removedActiveSession ? "/sessions" : state.pendingRoute,
            pendingMutations,
            rawTerminal: rawTerminalWithoutSession(
              state.rawTerminal,
              message.sessionId,
              stableSessionKey,
            ),
          };
        });
        break;
      }
      case "error":
        set({ lastError: message.message });
        break;
      case "disconnected": {
        const requiresPairing =
          message.message.includes("no longer trusted") ||
          message.message.includes("revoked");
        if (requiresPairing) {
          invalidateAsyncOperations();
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
            semanticGapKeys: new Set(),
            semanticGapSequences: {},
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
    const rawTerminal = get().rawTerminal;
    if (!knowsRawSession(rawTerminal, frame.sessionId)) return;
    const subscribers = rawTerminal.terminalSubscribers.get(frame.sessionId);
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
    const operation = beginAsyncOperation(client);
    try {
      const result = await client.request(action);
      if (!completeAsyncOperation(operation)) return;
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
      if (!completeAsyncOperation(operation)) return;
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
    semanticGapKeys: new Set(),
    semanticGapSequences: {},
    drafts: {},
    unread: {},
    pendingRoute: null,
    pendingMutations: {},
    rawTerminal: emptyRawTerminal(),
    compatibilityDiagnostic: null,
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
                retainedBytes: 0,
              },
            }
          : state.journals;
        return {
          journals,
          semanticReplay: {
            ...descriptor,
            nextSequence: descriptor.fromSequence,
            bufferedLiveEvents: [],
          },
          lastError: null,
        };
      });
    },

    applySemanticReplayPage(page) {
      const replayAtStart = get().semanticReplay;
      const completedBufferedEvents =
        page.complete && replayAtStart && pageContinuesReplay(replayAtStart, page)
          ? replayAtStart.bufferedLiveEvents ?? []
          : [];
      set((state) => {
        const replay = state.semanticReplay;
        if (!replay || !pageContinuesReplay(replay, page)) return state;

        const existing = state.journals[page.stableSessionKey];
        const mergedEvents = mergeOrderedEvents(existing?.events ?? [], page.events);
        const bounded = capSemanticEvents(mergedEvents);
        const journal: BoundedSemanticJournalState = {
          stableSessionKey: page.stableSessionKey,
          oldestSequence: bounded.events[0]?.sequence ?? 0,
          latestSequence: Math.max(
            existing?.latestSequence ?? page.fromSequence,
            page.nextSequence,
          ),
          cursorRolledOver:
            page.rollover ||
            (existing?.cursorRolledOver ?? false) ||
            bounded.evicted,
          events: bounded.events,
          retainedBytes: bounded.retainedBytes,
        };
        const semanticGapKeys = new Set(state.semanticGapKeys);
        const semanticGapSequences = { ...state.semanticGapSequences };
        if (page.complete) {
          semanticGapKeys.delete(page.stableSessionKey);
          delete semanticGapSequences[page.stableSessionKey];
        }
        return {
          journals: {
            ...state.journals,
            [page.stableSessionKey]: journal,
          },
          semanticReplay: page.complete
            ? null
            : { ...replay, nextSequence: page.nextSequence },
          semanticGapKeys,
          semanticGapSequences,
        };
      });
      completedBufferedEvents.forEach((event) => get().appendSemanticEvent(event));
    },

    appendSemanticEvent(event) {
      let requestReplay = false;
      set((state) => {
        const existing = state.journals[event.stableSessionKey];
        const retainedStart = state.sessions[event.stableSessionKey]?.oldestSequence ?? 1;
        const contiguousCursor = existing?.latestSequence ?? Math.max(0, retainedStart - 1);
        if (event.sequence <= contiguousCursor) return state;
        if (state.semanticReplay?.stableSessionKey === event.stableSessionKey) {
          const replay = state.semanticReplay;
          const bufferedLiveEvents = replay.bufferedLiveEvents ?? [];
          const bufferedCursor =
            bufferedLiveEvents[bufferedLiveEvents.length - 1]?.sequence ??
            replay.throughSequence;
          if (event.sequence <= bufferedCursor) return state;
          return {
            semanticReplay: {
              ...replay,
              bufferedLiveEvents: capSemanticEvents([
                ...bufferedLiveEvents,
                event,
              ]).events,
            },
          };
        }
        if (state.semanticGapKeys.has(event.stableSessionKey)) {
          if (
            event.sequence <=
            (state.semanticGapSequences[event.stableSessionKey] ?? 0)
          ) {
            return state;
          }
          return {
            semanticGapSequences: {
              ...state.semanticGapSequences,
              [event.stableSessionKey]: event.sequence,
            },
          };
        }
        if (event.sequence !== contiguousCursor + 1) {
          const semanticGapKeys = new Set(state.semanticGapKeys);
          semanticGapKeys.add(event.stableSessionKey);
          requestReplay = state.activeSessionKey === event.stableSessionKey;
          return {
            semanticGapKeys,
            semanticGapSequences: {
              ...state.semanticGapSequences,
              [event.stableSessionKey]: event.sequence,
            },
          };
        }
        if (
          existing?.cursorRolledOver &&
          existing.oldestSequence > 0 &&
          event.sequence < existing.oldestSequence
        ) {
          return state;
        }
        const bounded = appendCappedSemanticEvent(existing, event);
        const journal: BoundedSemanticJournalState = {
          stableSessionKey: event.stableSessionKey,
          oldestSequence: bounded.events[0]?.sequence ?? 0,
          latestSequence: Math.max(existing?.latestSequence ?? 0, event.sequence),
          cursorRolledOver:
            (existing?.cursorRolledOver ?? false) ||
            bounded.evicted,
          events: bounded.events,
          retainedBytes: bounded.retainedBytes,
        };
        return {
          journals: {
            ...state.journals,
            [event.stableSessionKey]: journal,
          },
        };
      });
      if (requestReplay) get().client?.wake();
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
        set((current) => {
          if (
            current.pendingMutations[stableSessionKey]?.mutationId !==
            pending.mutationId
          ) {
            return { lastError: error.message };
          }
          const pendingMutations = { ...current.pendingMutations };
          delete pendingMutations[stableSessionKey];
          return { pendingMutations, lastError: error.message };
        });
        throw error;
      }
      const operation = beginAsyncOperation(client);
      try {
        const accepted = await client.submitComposer(pending);
        if (!completeAsyncOperation(operation)) return accepted;
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
        if (!completeAsyncOperation(operation)) throw error;
        set((current) => {
          if (
            current.pendingMutations[stableSessionKey]?.mutationId !==
            pending.mutationId
          ) {
            return current;
          }
          const pendingMutations = { ...current.pendingMutations };
          delete pendingMutations[stableSessionKey];
          return {
            pendingMutations,
            lastError:
              error instanceof Error ? error.message : "Composer submission failed.",
          };
        });
        throw error;
      }
    },

    sendAction(action) {
      const client = get().client;
      if (!client) {
        set({ lastError: "WebSocket is not connected." });
        return;
      }
      const operation = beginAsyncOperation(client);
      void client
        .request(action)
        .then((result) => {
          if (!completeAsyncOperation(operation)) return;
          set({
            lastError: result.ok
              ? null
              : result.message ?? "Remote action failed.",
          });
        })
        .catch((error: unknown) => {
          if (!completeAsyncOperation(operation)) return;
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

    foregroundConnection() {
      get().client?.foreground();
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

    prepareComposer() {
      get().client?.ensureWriterLease();
    },

    interruptSession(stableSessionKey) {
      const accepted = get().client?.sendWithWriterLease({
        type: "interruptSession",
        stableSessionKey,
      });
      if (accepted === false) {
        set({ lastError: "Too many actions are waiting to be sent." });
      }
    },

    sendInput(sessionId, text) {
      const accepted = get().client?.sendWithWriterLease({
        type: "input",
        sessionId,
        text,
      });
      if (accepted === false) {
        set({ lastError: "Too much terminal input is waiting to be sent." });
      }
    },

    pasteImage(sessionId, payload) {
      const accepted = get().client?.sendWithWriterLease({
        type: "pasteImage",
        sessionId,
        mimeType: payload.mimeType,
        fileName: payload.fileName ?? null,
        dataBase64: payload.dataBase64,
      });
      set({
        lastError:
          accepted === false ? "Too much terminal input is waiting to be sent." : null,
      });
    },

    sendResize(sessionId, rows, cols) {
      const accepted = get().client?.sendWithWriterLease({
        type: "resize",
        sessionId,
        rows,
        cols,
      });
      if (accepted === false) {
        set({ lastError: "Too much terminal input is waiting to be sent." });
      }
    },

    launchAiTab(projectId, tabType) {
      return requestAiTabAction({
        type: "launchAi",
        project_id: projectId,
        tab_type: tabType,
      });
    },

    async openAiTab(tabId) {
      const stableSessionKey = `tab:${tabId}`;
      if (get().activeSessionKey !== stableSessionKey) {
        get().setActiveSession(stableSessionKey);
      }
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
