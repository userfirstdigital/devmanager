// Hand-written mirrors of the web-only Rust protocol in src/remote/web.
// These types intentionally do not describe native AppState/RuntimeState:
// browser state is an allowlisted, flat projection and cannot represent host
// configuration, secrets, environment values, or startup commands.

export const WEB_PROTOCOL_VERSION = 2;

export type StableSessionKey = string;

export type SessionStatus =
  | "Stopped"
  | "Starting"
  | "Running"
  | "Stopping"
  | "Crashed"
  | "Exited"
  | "Failed";

export function isLiveStatus(status: SessionStatus | undefined): boolean {
  return status === "Starting" || status === "Running" || status === "Stopping";
}

export interface SessionDimensions {
  cols: number;
  rows: number;
  cell_width: number;
  cell_height: number;
}

export type WebAiKind = "claude" | "codex";
export type WebTabKind = "server" | "claude" | "codex" | "ssh";
export type WebSessionKind = "shell" | WebTabKind;

export interface WebWriterLeaseState {
  ownerClientInstanceId: string | null;
  generation: number;
  expiresAtEpochMs: number | null;
  youAreOwner: boolean;
}

export const EMPTY_WRITER_LEASE: WebWriterLeaseState = {
  ownerClientInstanceId: null,
  generation: 0,
  expiresAtEpochMs: null,
  youAreOwner: false,
};

export interface WebProjectCommand {
  id: string;
  label: string;
  port: number | null;
  status: SessionStatus;
}

export interface WebProjectFolder {
  id: string;
  name: string;
  commands: WebProjectCommand[];
}

export interface WebProject {
  id: string;
  name: string;
  color: string | null;
  folders: WebProjectFolder[];
}

export interface WebSshConnection {
  id: string;
  label: string;
  host: string;
  port: number;
  username: string;
}

export interface WebTab {
  id: string;
  kind: WebTabKind;
  projectId: string;
  commandId: string | null;
  sessionId: string | null;
  connectionId: string | null;
  label: string | null;
}

export type SemanticAttention = "none" | "unread" | "needsInput" | "failed";
export type SemanticAdapterHealth = "healthy" | "degraded";

export interface WebSessionSummary {
  sessionId: string;
  stableSessionKey: StableSessionKey | null;
  kind: WebSessionKind;
  status: SessionStatus;
  projectId: string | null;
  commandId: string | null;
  tabId: string | null;
  dimensions: SessionDimensions;
  lastActivityEpochMs: number | null;
  attention: SemanticAttention;
  attentionCount: number;
  adapterHealth: SemanticAdapterHealth;
  rawRequired: boolean;
  oldestSequence: number;
  latestSequence: number;
}

export interface WebPortStatus {
  port: number;
  inUse: boolean;
  pid: number | null;
  processName: string | null;
}

export interface WebWorkspaceSnapshot {
  webProtocolVersion: number;
  runtimeInstanceId: string;
  revision: number;
  serverId: string;
  projects: WebProject[];
  sshConnections: WebSshConnection[];
  tabs: WebTab[];
  sessions: WebSessionSummary[];
  portStatuses: WebPortStatus[];
  writerLease: WebWriterLeaseState;
}

// Rust currently sends a complete safe projection for each delta.
export type WebWorkspaceDelta = WebWorkspaceSnapshot;

export type SemanticSource =
  | "claude"
  | "codex"
  | "shell"
  | "server"
  | "ssh"
  | "system";
export type SemanticStream = "stdout" | "stderr";
export type SemanticToolState = "pending" | "running" | "completed" | "failed";

interface SemanticEventBase {
  stableSessionKey: StableSessionKey;
  sequence: number;
  replacesSequence?: number;
  occurredAtEpochMs: number;
  source: SemanticSource;
}

export type SemanticEvent = SemanticEventBase &
  (
    | { kind: "userMessage"; text: string }
    | {
        kind: "assistantMessage";
        message_id: string;
        text: string;
        streaming: boolean;
      }
    | { kind: "reasoning"; item_id: string; summary: string }
    | {
        kind: "tool";
        tool_id: string;
        name: string;
        state: SemanticToolState;
        summary: string;
      }
    | { kind: "diff"; item_id: string; unified_diff: string }
    | {
        kind: "command";
        command_id: string;
        text: string;
        exit_code: number | null;
      }
    | { kind: "output"; stream: SemanticStream; text: string }
    | {
        kind: "question";
        question_id: string;
        prompt: string;
        choices: string[];
      }
    | { kind: "status"; state: string; detail: string | null }
    | { kind: "error"; message: string }
    | { kind: "terminalMode"; raw_required: boolean }
  );

export interface SemanticReplayDescriptor {
  replayId: number;
  stableSessionKey: StableSessionKey;
  fromSequence: number;
  throughSequence: number;
  rollover: boolean;
}

export interface SemanticReplayPage extends SemanticReplayDescriptor {
  nextSequence: number;
  complete: boolean;
  events: SemanticEvent[];
}

export interface SemanticJournalState {
  stableSessionKey: StableSessionKey;
  oldestSequence: number;
  latestSequence: number;
  cursorRolledOver: boolean;
  events: SemanticEvent[];
}

export interface ComposerAttachment {
  mimeType: "image/png" | "image/jpeg";
  fileName: string | null;
  dataBase64: string;
}

export interface ComposerSubmission {
  mutationId: string;
  stableSessionKey: StableSessionKey;
  text: string;
  attachments: ComposerAttachment[];
}

export interface ComposerAccepted {
  mutationId: string;
  stableSessionKey: StableSessionKey;
  acceptedSequence: number;
  leaseGeneration: number;
}

export type ComposerRejectCode =
  | "invalidRequest"
  | "sessionNotFound"
  | "ambiguousSession"
  | "nativeControllerActive"
  | "leaseBusy"
  | "staleGeneration"
  | "mutationInFlight"
  | "mutationConflict"
  | "capacityExceeded"
  | "ptyRejected";

export interface ComposerRejected {
  mutationId: string;
  code: ComposerRejectCode;
  message: string;
  writerLease: WebWriterLeaseState;
}

export type WebAction =
  | { type: "startServer"; command_id: string }
  | { type: "stopServer"; command_id: string }
  | { type: "restartServer"; command_id: string }
  | { type: "launchAi"; project_id: string; tab_type: WebAiKind }
  | { type: "restartAiTab"; tab_id: string }
  | { type: "closeTab"; tab_id: string }
  | { type: "openSshTab"; connection_id: string }
  | { type: "connectSsh"; connection_id: string }
  | { type: "restartSsh"; connection_id: string }
  | { type: "disconnectSsh"; connection_id: string }
  | { type: "stopAllServers" };

export type WebActionPayload = {
  type: "aiTab";
  tabId: string;
  projectId: string;
  tabType: WebAiKind;
  sessionId: string;
  label: string | null;
};

export interface WebActionResult {
  ok: boolean;
  message?: string | null;
  payload?: WebActionPayload | null;
}

export interface ResumeContext {
  seenRuntimeInstanceId: string | null;
  seenRevision: number | null;
  route: string;
  desiredSessionKey: StableSessionKey | null;
  semanticAfterSequence: number | null;
  visible: boolean;
  wantsWriterLease: boolean;
}

export interface ResumeState {
  runtimeInstanceId: string;
  revision: number;
  hardReset: boolean;
  route: string;
  desiredSessionKey: StableSessionKey | null;
  workspace: WebWorkspaceSnapshot | null;
  semanticReplay: SemanticReplayDescriptor | null;
  writerLease: WebWriterLeaseState;
}

export type WsInbound =
  | ({ type: "resume"; clientInstanceId: string } & ResumeContext)
  | { type: "acquireWriterLease"; clientInstanceId: string; visible: boolean }
  | {
      type: "writerLeaseHeartbeat";
      clientInstanceId: string;
      expectedLeaseGeneration: number;
      visible: boolean;
    }
  | { type: "setVisibility"; clientInstanceId: string; visible: boolean }
  | ({ type: "composerSubmit"; expectedLeaseGeneration: number } & ComposerSubmission)
  | {
      type: "subscribeSemantic";
      stableSessionKey: StableSessionKey;
      afterSequence: number;
    }
  | { type: "unsubscribeSemantic"; stableSessionKey: StableSessionKey }
  | {
      type: "interruptSession";
      stableSessionKey: StableSessionKey;
      expectedLeaseGeneration: number;
    }
  // Raw terminal compatibility. Resume owns focus/subscription/control; only
  // terminal-grid IO remains on these PTY-ID frames.
  | {
      type: "input";
      sessionId: string;
      text: string;
      expectedLeaseGeneration?: number;
    }
  | {
      type: "pasteImage";
      sessionId: string;
      mimeType: "image/png" | "image/jpeg";
      fileName?: string | null;
      dataBase64: string;
      expectedLeaseGeneration?: number;
    }
  | {
      type: "resize";
      sessionId: string;
      rows: number;
      cols: number;
      expectedLeaseGeneration?: number;
    }
  | { type: "action"; action: WebAction; expectedLeaseGeneration?: number }
  | {
      type: "request";
      id: number;
      action: WebAction;
      expectedLeaseGeneration?: number;
    }
  | { type: "ping" };

export type WsOutbound =
  | {
      type: "hello";
      clientId: string;
      serverId: string;
      protocolVersion: number;
    }
  | { type: "snapshot"; workspace: WebWorkspaceSnapshot }
  | { type: "delta"; delta: WebWorkspaceDelta }
  | ({ type: "resumeState" } & ResumeState)
  | { type: "writerLeaseState"; writerLease: WebWriterLeaseState }
  | ({ type: "semanticReplayPage" } & SemanticReplayPage)
  | { type: "semanticEvent"; event: SemanticEvent }
  | ({ type: "composerAccepted" } & ComposerAccepted)
  | ({ type: "composerRejected" } & ComposerRejected)
  | {
      type: "controlState";
      controllerClientId: string | null;
      youHaveControl: boolean;
    }
  | {
      type: "sessionBootstrap";
      sessionId: string;
      replayBase64: string;
      screen: TerminalScreenSnapshot;
    }
  | { type: "sessionClosed"; sessionId: string }
  | { type: "sessionRemoved"; sessionId: string }
  | { type: "response"; id: number; result: WebActionResult }
  | { type: "error"; message: string }
  | { type: "pong" }
  | { type: "disconnected"; message: string };

export interface TerminalCursorSnapshot {
  row: number;
  column: number;
  shape: string;
}

export interface TerminalCellSnapshot {
  character: string;
  zero_width: string[];
  foreground: number;
  background: number;
  bold: boolean;
  dim: boolean;
  italic: boolean;
  underline: boolean;
  undercurl: boolean;
  strike: boolean;
  hidden: boolean;
  has_hyperlink: boolean;
  default_background: boolean;
}

export interface TerminalModeSnapshot {
  alternate_screen: boolean;
  app_cursor: boolean;
  bracketed_paste: boolean;
  focus_in_out: boolean;
  mouse_report_click: boolean;
  mouse_drag: boolean;
  mouse_motion: boolean;
  sgr_mouse: boolean;
  utf8_mouse: boolean;
  alternate_scroll: boolean;
}

export interface TerminalScreenSnapshot {
  lines: TerminalCellSnapshot[][];
  cursor: TerminalCursorSnapshot | null;
  display_offset: number;
  history_size: number;
  total_lines: number;
  rows: number;
  cols: number;
  mode: TerminalModeSnapshot;
}

export interface WebImagePastePayload {
  mimeType: "image/png" | "image/jpeg";
  fileName?: string | null;
  dataBase64: string;
}

// Temporary, safe compatibility projection for the terminal-first UI. This is
// derived from WebWorkspaceSnapshot in the store and is never accepted from
// the wire or persisted.
export type RunCommand = WebProjectCommand;
export type ProjectFolder = WebProjectFolder & { hidden?: boolean };
export type Project = Omit<WebProject, "folders"> & {
  folders: ProjectFolder[];
  pinned?: boolean;
};
export type SSHConnection = WebSshConnection;
export type TabType = WebTabKind;
export interface SessionTab {
  id: string;
  type: TabType;
  projectId: string;
  commandId?: string | null;
  ptySessionId?: string | null;
  label?: string | null;
  sshConnectionId?: string | null;
}
export interface SessionRuntimeState {
  session_id: string;
  stable_session_key: StableSessionKey | null;
  pid: number | null;
  status: SessionStatus;
  session_kind: WebSessionKind | null;
  command_id: string | null;
  project_id: string | null;
  tab_id: string | null;
  exit_code: number | null;
  title: string | null;
  dimensions: SessionDimensions;
}
export interface LegacyWorkspaceProjection {
  appState: {
    config: { projects: Project[]; sshConnections: SSHConnection[] };
    open_tabs: SessionTab[];
  };
  runtimeState: { sessions: Record<string, SessionRuntimeState> };
  portStatuses: Record<string, WebPortStatus>;
  controllerClientId: string | null;
  youHaveControl: boolean;
  serverId: string;
}

// Compatibility aliases retained only while Tasks 5-6 replace the old views.
export type RemoteAction = WebAction;
export type RemoteActionResult = WebActionResult;
export type RemoteAiTabPayload = WebActionPayload;
export type RemoteWorkspaceSnapshot = LegacyWorkspaceProjection;
export type RemoteWorkspaceDelta = never;

export const DEFAULT_DIMENSIONS: SessionDimensions = {
  cols: 100,
  rows: 30,
  cell_width: 10,
  cell_height: 20,
};
