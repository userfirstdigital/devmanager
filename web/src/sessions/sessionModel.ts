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
  live: SessionListItem[];
  recent: SessionListItem[];
}

function titleCase(value: string): string {
  return value.length ? `${value[0]?.toUpperCase()}${value.slice(1)}` : value;
}

function isMeaningfulRuntimeTitle(title: string | null | undefined): title is string {
  const trimmed = title?.trim() ?? "";
  if (!trimmed) return false;
  if (
    trimmed.includes("\\system32\\") ||
    trimmed.includes("/bin/") ||
    trimmed.includes("/usr/")
  ) {
    return false;
  }
  if (trimmed.endsWith(".exe") && (trimmed.includes("\\") || trimmed.includes("/"))) {
    return false;
  }
  return true;
}

/** Bare provider, session/numbered labels, and decorated provider chrome are not task titles. */
function isGenericAiLabel(label: string, kind: WebSessionSummary["kind"]): boolean {
  if (kind !== "claude" && kind !== "codex") return false;
  const trimmed = label.trim();
  if (kind === "claude") {
    return /^(?:✳\s*)?claude(?:\s+code|\s+session|\s+\d+)?$/i.test(trimmed);
  }
  return /^(?:openai\s+)?codex(?:\s+session|\s+\d+)?$/i.test(trimmed);
}

function statePresentation(session: WebSessionSummary): Pick<
  SessionListItem,
  "stateLabel" | "statusTone"
> {
  if (!isLiveStatus(session.status)) {
    if (session.status === "Failed") {
      return { stateLabel: "Needs attention", statusTone: "danger" };
    }
    if (session.status === "Crashed") {
      return { stateLabel: "Crashed", statusTone: "danger" };
    }
    if (session.kind === "ssh") {
      return { stateLabel: "Disconnected", statusTone: "neutral" };
    }
    if (session.kind === "claude" || session.kind === "codex") {
      return { stateLabel: "Ready to reopen", statusTone: "neutral" };
    }
    return { stateLabel: titleCase(session.status), statusTone: "neutral" };
  }

  if (session.attention === "needsInput") {
    return { stateLabel: "Needs input", statusTone: "attention" };
  }
  if (session.attention === "failed") {
    return { stateLabel: "Needs attention", statusTone: "danger" };
  }
  if (session.kind === "ssh") {
    if (session.status === "Running") return { stateLabel: "Connected", statusTone: "active" };
    if (session.status === "Starting") return { stateLabel: "Connecting", statusTone: "active" };
    return { stateLabel: "Disconnected", statusTone: "neutral" };
  }
  if (session.kind === "claude" || session.kind === "codex") {
    if (session.attention === "unread") {
      return { stateLabel: "Ready", statusTone: "attention" };
    }
    if (session.aiActivity === "Thinking") {
      return { stateLabel: "Thinking", statusTone: "active" };
    }
    if (session.adapterHealth === "degraded") {
      return { stateLabel: "Terminal fallback", statusTone: "attention" };
    }
    if (session.status === "Starting") return { stateLabel: "Starting", statusTone: "active" };
    if (session.status === "Stopping") return { stateLabel: "Stopping", statusTone: "active" };
    if (session.status === "Running") return { stateLabel: "Idle", statusTone: "active" };
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

  let label: string;
  if (session.kind === "ssh") {
    label = connection?.label?.trim() || tab?.label?.trim() || "SSH session";
  } else if (session.kind === "server" || session.kind === "shell") {
    label = command?.label?.trim() || tab?.label?.trim() || "Server session";
  } else {
    const runtimeTitle = session.title?.trim() ?? "";
    if (
      isMeaningfulRuntimeTitle(runtimeTitle) &&
      !isGenericAiLabel(runtimeTitle, session.kind)
    ) {
      label = runtimeTitle;
    } else {
      const taskTitle = session.taskTitle?.trim() ?? "";
      if (taskTitle) {
        label = taskTitle;
      } else {
        const tabLabel = tab?.label?.trim() ?? "";
        if (tabLabel && !isGenericAiLabel(tabLabel, session.kind)) {
          label = tabLabel;
        } else {
          label = `${titleCase(session.kind)} session`;
        }
      }
    }
  }

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

function livePriority(item: SessionListItem): number {
  if (item.attention === "needsInput") return 0;
  if (item.attention === "failed") return 1;
  if (item.attention === "unread") return 2;
  if (item.session.aiActivity === "Thinking") return 3;
  return 4;
}

function liveSort(left: SessionListItem, right: SessionListItem): number {
  const priority = livePriority(left) - livePriority(right);
  return priority || newestFirst(left, right);
}

export function groupSessions(workspace: WebWorkspaceSnapshot): SessionGroups {
  const groups: SessionGroups = {
    live: [],
    recent: [],
  };
  for (const session of workspace.sessions) {
    if (!session.stableSessionKey) continue;
    const item = describeSession(workspace, session);
    if (isLiveStatus(session.status)) {
      groups.live.push(item);
    } else {
      groups.recent.push(item);
    }
  }
  groups.live.sort(liveSort);
  groups.recent.sort(newestFirst);
  return groups;
}

/** Count live non-none attention — never ended/stale historical attention. */
export function countActionableAttention(workspace: WebWorkspaceSnapshot | null | undefined): number {
  if (!workspace) return 0;
  return workspace.sessions.filter(
    (session) => isLiveStatus(session.status) && session.attention !== "none",
  ).length;
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
