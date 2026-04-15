import { Eye, KeyRound, Play, RotateCcw, Square, Terminal } from "lucide-react";
import {
  isLiveStatus,
  type SessionRuntimeState,
  type SessionStatus,
  type SSHConnection,
} from "../api/types";
import { useStore } from "../store";
import { ProjectTree } from "./ProjectTree";
import { formatSshTarget, summarizeSidebarShell } from "./sidebarModel";

interface SidebarProps {
  onItemPicked?: () => void;
}

export function shouldCloseSidebarAfterClick(
  target: Pick<HTMLElement, "closest">,
): boolean {
  if (target.closest("[data-sidebar-action='true']")) {
    return false;
  }
  return Boolean(target.closest("[data-sidebar-row='true']"));
}

function statusDotClass(status: SessionStatus | undefined): string {
  if (!status || status === "Stopped" || status === "Exited") {
    return "bg-zinc-600";
  }
  if (status === "Running") return "bg-emerald-400 dot-live";
  if (status === "Starting") return "bg-amber-400 dot-live";
  if (status === "Stopping") return "bg-amber-400";
  if (status === "Crashed" || status === "Failed") return "bg-red-400";
  return "bg-zinc-600";
}

function ControlToggle({ compact = false }: { compact?: boolean }) {
  const youHaveControl = useStore(
    (s) => s.snapshot?.youHaveControl ?? false,
  );
  const takeControl = useStore((s) => s.takeControl);
  const releaseControl = useStore((s) => s.releaseControl);
  const sharedClass = compact
    ? "flex items-center justify-center gap-1.5 text-[11px] px-3 py-2 rounded bg-zinc-700 text-zinc-200 hover:bg-zinc-600"
    : "flex items-center gap-1.5 text-[11px] px-2 py-1 rounded";

  if (youHaveControl) {
    return (
      <button
        type="button"
        data-sidebar-action="true"
        onClick={releaseControl}
        title="Release control so the desktop app can type again"
        className={`${sharedClass} ${compact ? "bg-emerald-600/20 text-emerald-300 hover:bg-emerald-600/30" : "bg-emerald-600/20 text-emerald-300 hover:bg-emerald-600/30"}`}
      >
        <KeyRound className="size-3" />
        <span>{compact ? "You Control" : "Control"}</span>
      </button>
    );
  }
  return (
    <button
      type="button"
      data-sidebar-action="true"
      onClick={takeControl}
      title="Take control so this browser can start servers and type"
      className={`${sharedClass} ${compact ? "bg-amber-600/20 text-amber-300 hover:bg-amber-600/30" : "bg-amber-600/20 text-amber-300 hover:bg-amber-600/30"}`}
    >
      <Eye className="size-3" />
      <span>{compact ? "Take Control" : "View"}</span>
    </button>
  );
}

function findSshTab(
  tabs: Array<{
    type: string;
    sshConnectionId?: string | null;
    ptySessionId?: string | null;
  }>,
  connectionId: string,
): {
  type: string;
  sshConnectionId?: string | null;
  ptySessionId?: string | null;
} | null {
  return (
    tabs.find((tab) => tab.type === "ssh" && tab.sshConnectionId === connectionId) ??
    null
  );
}

function findSshSession(
  sessions: Record<string, SessionRuntimeState>,
  tab:
    | {
        ptySessionId?: string | null;
      }
    | null,
): SessionRuntimeState | null {
  const sessionId = tab?.ptySessionId;
  if (!sessionId) return null;
  return sessions[sessionId] ?? null;
}

function SshRow({ connection }: { connection: SSHConnection }) {
  const snapshot = useStore((s) => s.snapshot);
  const activeSessionId = useStore((s) => s.activeSessionId);
  const setActiveSession = useStore((s) => s.setActiveSession);
  const openSshTab = useStore((s) => s.openSshTab);
  const connectSsh = useStore((s) => s.connectSsh);
  const restartSsh = useStore((s) => s.restartSsh);
  const disconnectSsh = useStore((s) => s.disconnectSsh);
  const youHaveControl = snapshot?.youHaveControl ?? false;

  const tabs = snapshot?.appState?.open_tabs ?? [];
  const sessions = snapshot?.runtimeState?.sessions ?? {};
  const tab = findSshTab(tabs, connection.id);
  const session = findSshSession(sessions, tab);
  const live = isLiveStatus(session?.status);
  const isActive = activeSessionId === (session?.session_id ?? null);

  const onRowClick = () => {
    if (session) {
      setActiveSession(session.session_id);
      return;
    }
    if (youHaveControl && tab) {
      openSshTab(connection.id);
    }
  };

  const onConnect = (e: React.MouseEvent) => {
    e.stopPropagation();
    connectSsh(connection.id);
  };

  const onRestart = (e: React.MouseEvent) => {
    e.stopPropagation();
    restartSsh(connection.id);
  };

  const onDisconnect = (e: React.MouseEvent) => {
    e.stopPropagation();
    disconnectSsh(connection.id);
    if (isActive) setActiveSession(null);
  };

  return (
    <div
      role="button"
      data-sidebar-row="true"
      tabIndex={0}
      onClick={onRowClick}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onRowClick();
        }
      }}
      className={`group flex items-center gap-2 rounded px-2 py-1 cursor-pointer ${
        isActive ? "bg-zinc-700/65" : "hover:bg-zinc-700/35"
      }`}
    >
      <span
        className={`inline-block size-2 rounded-full shrink-0 ${statusDotClass(
          session?.status,
        )}`}
      />
      <Terminal className="size-3.5 text-cyan-300 shrink-0" />
      <div className="min-w-0 flex-1">
        <div className="truncate text-[11px] font-medium text-zinc-200">
          {connection.label}
        </div>
        <div className="truncate text-[10px] text-zinc-500">
          {formatSshTarget(connection)}
        </div>
      </div>
      <div className="flex items-center gap-0.5 opacity-100 md:opacity-0 md:group-hover:opacity-100 transition-opacity shrink-0">
        {!live ? (
          <button
            type="button"
            data-sidebar-action="true"
            onClick={onConnect}
            disabled={!youHaveControl}
            title={youHaveControl ? "Connect SSH" : "Take control to connect"}
            className="p-1 rounded hover:bg-zinc-600 text-emerald-400 disabled:opacity-40 disabled:hover:bg-transparent"
          >
            <Play className="size-3.5" />
          </button>
        ) : (
          <>
            <button
              type="button"
              data-sidebar-action="true"
              onClick={onRestart}
              disabled={!youHaveControl}
              title={youHaveControl ? "Restart SSH" : "Take control to restart"}
              className="p-1 rounded hover:bg-zinc-600 text-amber-400 disabled:opacity-40 disabled:hover:bg-transparent"
            >
              <RotateCcw className="size-3.5" />
            </button>
            <button
              type="button"
              data-sidebar-action="true"
              onClick={onDisconnect}
              disabled={!youHaveControl}
              title={youHaveControl ? "Disconnect SSH" : "Take control to disconnect"}
              className="p-1 rounded hover:bg-zinc-600 text-red-400 disabled:opacity-40 disabled:hover:bg-transparent"
            >
              <Square className="size-3.5" />
            </button>
          </>
        )}
      </div>
    </div>
  );
}

function SshSection({ connections }: { connections: SSHConnection[] }) {
  return (
    <section className="border-t border-zinc-700/80 px-2 py-3">
      <div className="mb-2 px-1 text-[10px] font-semibold uppercase tracking-[0.18em] text-zinc-500">
        SSH
      </div>
      <div className="space-y-1">
        {connections.map((connection) => (
          <SshRow key={connection.id} connection={connection} />
        ))}
      </div>
    </section>
  );
}

export function Sidebar({ onItemPicked }: SidebarProps) {
  const snapshot = useStore((s) => s.snapshot);
  const stopAllServers = useStore((s) => s.stopAllServers);
  const sshConnections = snapshot?.appState?.config?.sshConnections ?? [];
  const shell = summarizeSidebarShell(sshConnections);

  return (
    <aside
      className="w-64 shrink-0 bg-zinc-800 border-r border-zinc-700 flex flex-col h-full"
      onClick={(e) => {
        // Only close the mobile drawer for actual row picks. Nested controls
        // like start/stop/restart must stay interactive on touch devices.
        const target = e.target as HTMLElement;
        if (shouldCloseSidebarAfterClick(target)) onItemPicked?.();
      }}
    >
      <header className="px-3 py-3 shrink-0 border-b border-zinc-700">
        <div className="flex items-start gap-3">
          <div className="mt-0.5 rounded-md bg-indigo-500/15 p-2 text-indigo-300">
            <Terminal className="size-4" />
          </div>
          <div className="min-w-0 flex-1">
            <div className="text-[10px] font-semibold uppercase tracking-[0.2em] text-zinc-500">
              Remote
            </div>
            <div className="truncate text-sm font-bold uppercase tracking-wide text-zinc-100">
              DevManager
            </div>
            <div className="truncate text-[10px] text-zinc-500">
              {snapshot?.serverId ?? "Waiting for host"}
            </div>
          </div>
          <div className="hidden md:block">
            <ControlToggle />
          </div>
        </div>
      </header>
      <div className="flex-1 overflow-y-auto">
        <section className="px-2 py-3">
          <div className="mb-2 px-1 text-[10px] font-semibold uppercase tracking-[0.18em] text-zinc-500">
            Projects
          </div>
          <ProjectTree />
        </section>
        {shell.showSshSection ? <SshSection connections={sshConnections} /> : null}
      </div>
      {shell.showFooterActions ? (
        <footer className="border-t border-zinc-700 px-3 py-3 space-y-2">
          <div className="grid grid-cols-2 gap-2">
            <ControlToggle compact />
            <button
              type="button"
              data-sidebar-action="true"
              onClick={stopAllServers}
              disabled={!(snapshot?.youHaveControl ?? false)}
              title={
                snapshot?.youHaveControl
                  ? "Stop all running servers"
                  : "Take control to stop all servers"
              }
              className="flex items-center justify-center gap-1.5 rounded bg-zinc-700 px-3 py-2 text-[11px] font-medium text-zinc-200 hover:bg-zinc-600 disabled:opacity-40 disabled:hover:bg-zinc-700"
            >
              <Square className="size-3.5 text-red-300" />
              <span>Stop All</span>
            </button>
          </div>
          <div className="text-[10px] text-zinc-500">
            Folder-first tree, SSH access, and remote controls stay touch-safe here.
          </div>
        </footer>
      ) : null}
    </aside>
  );
}
