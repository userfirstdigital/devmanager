import { isLiveStatus, type WebSessionSummary, type WebWorkspaceSnapshot } from "../api/types";
import { routeForSessionKey, type AppRoute } from "../app/router";

export interface SessionListItem {
  stableSessionKey: string;
  label: string;
  projectName: string;
  projectColor: string | null;
  kindLabel: string;
  stateLabel: string;
  statusTone: "neutral" | "active" | "attention" | "danger";
  lastActivityEpochMs: number | null;
  attention: WebSessionSummary["attention"];
  attentionCount: number;
  route: AppRoute;
  session: WebSessionSummary;
}

export interface SessionGroups {
  needsAttention: SessionListItem[];
  active: SessionListItem[];
  recent: SessionListItem[];
}

function titleCase(value: string): string {
  return value.length ? `${value[0]?.toUpperCase()}${value.slice(1)}` : value;
}

function statePresentation(session: WebSessionSummary): Pick<
  SessionListItem,
  "stateLabel" | "statusTone"
> {
  if (session.attention === "failed" || session.status === "Failed" || session.status === "Crashed") {
    return {
      stateLabel: session.status === "Crashed" ? "Crashed" : "Needs attention",
      statusTone: "danger",
    };
  }
  if (session.attention === "needsInput") {
    return { stateLabel: "Needs input", statusTone: "attention" };
  }
  if (session.kind === "ssh") {
    if (session.status === "Running") return { stateLabel: "Connected", statusTone: "active" };
    if (session.status === "Starting") return { stateLabel: "Connecting", statusTone: "active" };
    return { stateLabel: "Disconnected", statusTone: "neutral" };
  }
  if (session.kind === "claude" || session.kind === "codex") {
    if (session.status === "Starting") return { stateLabel: "Starting", statusTone: "active" };
    if (session.status === "Running") return { stateLabel: "Working", statusTone: "active" };
    if (session.status === "Stopping") return { stateLabel: "Stopping", statusTone: "active" };
    return { stateLabel: "Ready to reopen", statusTone: "neutral" };
  }
  if (session.status === "Running") return { stateLabel: "Running", statusTone: "active" };
  if (session.status === "Starting") return { stateLabel: "Starting", statusTone: "active" };
  if (session.status === "Stopping") return { stateLabel: "Stopping", statusTone: "active" };
  return { stateLabel: titleCase(session.status), statusTone: "neutral" };
}

function commandForSession(
  workspace: WebWorkspaceSnapshot,
  session: WebSessionSummary,
) {
  const commandId = session.commandId ??
    (session.stableSessionKey?.startsWith("server:")
      ? session.stableSessionKey.slice("server:".length)
      : null);
  if (!commandId) return null;
  for (const project of workspace.projects) {
    for (const folder of project.folders) {
      const command = folder.commands.find((candidate) => candidate.id === commandId);
      if (command) return command;
    }
  }
  return null;
}

export function describeSession(
  workspace: WebWorkspaceSnapshot,
  session: WebSessionSummary,
): SessionListItem {
  const stableSessionKey = session.stableSessionKey ?? `unavailable:${session.sessionId}`;
  const tab = workspace.tabs.find(
    (candidate) =>
      candidate.id === session.tabId ||
      stableSessionKey === `tab:${candidate.id}`,
  );
  const projectId = session.projectId ?? tab?.projectId ?? null;
  const project = workspace.projects.find((candidate) => candidate.id === projectId);
  const command = commandForSession(workspace, session);
  const connection = tab?.connectionId
    ? workspace.sshConnections.find((candidate) => candidate.id === tab.connectionId)
    : null;
  const fallbackLabel =
    session.kind === "ssh"
      ? connection?.label ?? "SSH session"
      : session.kind === "server" || session.kind === "shell"
        ? command?.label ?? "Server session"
        : `${titleCase(session.kind)} session`;
  const label = tab?.label?.trim() || command?.label?.trim() || fallbackLabel;
  const state = statePresentation(session);

  return {
    stableSessionKey,
    label,
    projectName: project?.name ?? "Project unavailable",
    projectColor: project?.color ?? null,
    kindLabel:
      session.kind === "ssh"
        ? "SSH"
        : session.kind === "shell"
          ? "Shell"
          : titleCase(session.kind),
    ...state,
    lastActivityEpochMs: session.lastActivityEpochMs,
    attention: session.attention,
    attentionCount: session.attentionCount,
    route: routeForSessionKey(stableSessionKey),
    session,
  };
}

function newestFirst(left: SessionListItem, right: SessionListItem): number {
  const activity =
    (right.lastActivityEpochMs ?? Number.NEGATIVE_INFINITY) -
    (left.lastActivityEpochMs ?? Number.NEGATIVE_INFINITY);
  return activity || left.label.localeCompare(right.label);
}

export function groupSessions(workspace: WebWorkspaceSnapshot): SessionGroups {
  const groups: SessionGroups = {
    needsAttention: [],
    active: [],
    recent: [],
  };
  for (const session of workspace.sessions) {
    if (!session.stableSessionKey) continue;
    const item = describeSession(workspace, session);
    if (session.attention === "needsInput" || session.attention === "failed") {
      groups.needsAttention.push(item);
    } else if (isLiveStatus(session.status)) {
      groups.active.push(item);
    } else {
      groups.recent.push(item);
    }
  }
  groups.needsAttention.sort(newestFirst);
  groups.active.sort(newestFirst);
  groups.recent.sort(newestFirst);
  return groups;
}

export function formatRelativeActivity(
  epochMs: number | null,
  nowEpochMs = Date.now(),
): string {
  if (epochMs === null) return "No activity yet";
  const seconds = Math.max(0, Math.floor((nowEpochMs - epochMs) / 1_000));
  if (seconds < 15) return "Now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}
