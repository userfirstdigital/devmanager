// Hand-written TypeScript mirrors of the Rust types the host serializes.
//
// Serde rules at a glance (all discovered in src/remote/mod.rs and
// src/models/config.rs):
//
//   RemoteWorkspaceSnapshot  : rename_all=camelCase
//   AppConfig/Project/...    : rename_all=camelCase
//   AppState/RuntimeState/
//     SessionRuntimeState    : NO rename_all -> snake_case
//   SessionStatus            : variant names as-is ("Running", "Stopped", ...)
//   TabType                  : rename_all=lowercase
//   RemoteAction             : tag="type", rename_all=camelCase on the tag
//                              but field names inside variants stay snake_case
//                              (no rename_all_fields)
//
// Anything the UI touches is typed here; anything it doesn't is left as
// `unknown` so we don't accidentally lie about a shape.

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

export interface RunCommand {
  id: string;
  label: string;
  command: string;
  args: string[];
  env?: Record<string, string> | null;
  port?: number | null;
  autoRestart?: boolean | null;
  clearLogsOnRestart?: boolean | null;
}

export interface ProjectFolder {
  id: string;
  name: string;
  folderPath: string;
  commands: RunCommand[];
  envFilePath?: string | null;
  portVariable?: string | null;
  hidden?: boolean | null;
}

export interface Project {
  id: string;
  name: string;
  rootPath: string;
  folders: ProjectFolder[];
  color?: string | null;
  pinned?: boolean | null;
  notes?: string | null;
  createdAt: string;
  updatedAt: string;
}

export interface AppConfig {
  version: number;
  projects: Project[];
  // settings, sshConnections, etc. — we don't touch them in Phase 3.
}

/** Serialized with lowercase variant names. */
export type TabType = "server" | "claude" | "codex" | "ssh";

/** SessionTab uses camelCase serialization and `type` as the tab_type key. */
export interface SessionTab {
  id: string;
  type: TabType;
  projectId: string;
  commandId?: string | null;
  ptySessionId?: string | null;
  label?: string | null;
  sshConnectionId?: string | null;
}

export interface AppState {
  config: AppConfig;
  /**
   * Note: AppState has no serde `rename_all`, so this comes over the wire
   * as snake_case `open_tabs`. The nested SessionTab elements DO use
   * camelCase (rename_all on SessionTab).
   */
  open_tabs: SessionTab[];
  active_tab_id: string | null;
  sidebar_collapsed: boolean;
  collapsed_projects: string[];
  window_bounds: unknown;
}

export interface SessionRuntimeState {
  session_id: string;
  pid: number | null;
  status: SessionStatus;
  session_kind: unknown;
  command_id: string | null;
  project_id: string | null;
  tab_id: string | null;
  exit_code: number | null;
  title: string | null;
  dimensions: SessionDimensions;
  // ... lots more fields the UI doesn't use
}

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

export interface RuntimeState {
  sessions: Record<string, SessionRuntimeState>;
  active_session_id: string | null;
  debug_enabled: boolean;
}

export interface TerminalSessionView {
  runtime: SessionRuntimeState;
  screen: TerminalScreenSnapshot;
}

export interface PortStatus {
  port: number;
  inUse: boolean;
  pid: number | null;
  processName: string | null;
}

export interface RemoteWorkspaceSnapshot {
  appState: AppState;
  runtimeState: RuntimeState;
  sessionViews: Record<string, unknown>;
  portStatuses: Record<string, PortStatus>;
  controllerClientId: string | null;
  youHaveControl: boolean;
  serverId: string;
}

export interface RemoteWorkspaceDelta {
  appState?: AppState;
  runtimeState?: RuntimeState;
  portStatuses?: Record<string, PortStatus>;
  controllerClientId?: string | null;
  youHaveControl?: boolean;
}

// ── Action payloads (snake_case field names because RemoteAction has no
// rename_all_fields). The `type` tag is camelCase, everything else inside is
// raw Rust. ───────────────────────────────────────────────────────────────

export type RemoteAction =
  | {
      type: "startServer";
      command_id: string;
      focus: boolean;
      dimensions: SessionDimensions;
    }
  | { type: "stopServer"; command_id: string }
  | {
      type: "restartServer";
      command_id: string;
      dimensions: SessionDimensions;
    }
  | {
      type: "launchAi";
      project_id: string;
      tab_type: TabType;
      dimensions: SessionDimensions;
    }
  | {
      type: "openAiTab";
      tab_id: string;
      dimensions: SessionDimensions;
    }
  | {
      type: "restartAiTab";
      tab_id: string;
      dimensions: SessionDimensions;
    }
  | { type: "closeAiTab"; tab_id: string }
  | { type: "closeTab"; tab_id: string };

export interface RemoteAiTabPayload {
  type: "aiTab";
  tab_id: string;
  project_id: string;
  tab_type: TabType;
  session_id: string;
  label?: string | null;
  session_view?: TerminalSessionView | null;
}

export type RemoteActionPayload = RemoteAiTabPayload | { type: string };

export interface RemoteActionResult {
  ok: boolean;
  message?: string | null;
  payload?: RemoteActionPayload | null;
}

// ── Wire frames ───────────────────────────────────────────────────────────

export type WsInbound =
  | { type: "subscribeSessions"; sessionIds: string[] }
  | { type: "unsubscribeSessions"; sessionIds: string[] }
  | { type: "focusSession"; sessionId: string }
  | { type: "input"; sessionId: string; text: string }
  | { type: "resize"; sessionId: string; rows: number; cols: number }
  | { type: "action"; action: RemoteAction }
  | { type: "request"; id: number; action: RemoteAction }
  | { type: "takeControl" }
  | { type: "releaseControl" }
  | { type: "ping" };

export type WsOutbound =
  | { type: "snapshot"; workspace: RemoteWorkspaceSnapshot }
  | { type: "delta"; delta: RemoteWorkspaceDelta }
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
  | { type: "response"; id: number; result: RemoteActionResult }
  | { type: "error"; message: string }
  | { type: "pong" }
  | { type: "disconnected"; message: string };

// Default dimensions to send with start/restart actions from the web UI.
// These are terminal character cells, not pixels — the host just wants
// something sensible so the PTY opens at a reasonable size.
export const DEFAULT_DIMENSIONS: SessionDimensions = {
  cols: 100,
  rows: 30,
  cell_width: 10,
  cell_height: 20,
};
