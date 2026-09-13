import { useState } from "react";
import {
  Bot,
  ChevronDown,
  ChevronRight,
  ExternalLink,
  Folder,
  Pin,
  Play,
  RotateCcw,
  Sparkles,
  Square,
  Terminal as TerminalIcon,
  X,
} from "lucide-react";
import {
  isLiveStatus,
  type Project,
  type ProjectFolder,
  type RunCommand,
  type SessionRuntimeState,
  type SessionStatus,
  type SessionTab,
  type WebPortAuthority,
} from "../api/types";
import { useStore } from "../store";
import {
  describeFolderPresentation,
  groupProjectsForSidebar,
} from "./sidebarModel";
import {
  canOpenRemoteSite,
  openRemoteSiteInNewTab,
} from "./remoteSiteLink";

const ACTION_REVEAL_CLASS =
  "flex items-center gap-0.5 opacity-100 md:opacity-0 md:group-hover:opacity-100 transition-opacity shrink-0";

function findSessionForCommand(
  sessions: Record<string, SessionRuntimeState>,
  commandId: string,
): SessionRuntimeState | null {
  for (const session of Object.values(sessions)) {
    if (session.command_id === commandId) return session;
  }
  return null;
}

function authorityDotState(
  authority: WebPortAuthority | undefined,
  sessionId: string | undefined,
): "managed" | "managedUnready" | "external" | "unknown" {
  if (
    !authority ||
    authority.fresh !== true ||
    authority.reapIncomplete !== false ||
    authority.error !== null ||
    authority.diagnostic != null ||
    authority.listeners.length === 0 ||
    !authority.listeners.every(
      (listener) =>
        listener.pid > 0 &&
        listener.creationTime100ns > 0 &&
        listener.executableProven,
    )
  ) {
    return "unknown";
  }
  if (
    authority.kind === "managed" &&
    authority.controlReason === "exactManagedFence" &&
    authority.sessionId != null &&
    authority.sessionId === sessionId
  ) {
    return "managed";
  }
  if (
    authority.kind === "managedUnready" &&
    authority.controlReason === "managedUnready" &&
    authority.sessionId != null &&
    authority.sessionId === sessionId
  ) {
    return "managedUnready";
  }
  if (
    authority.kind === "provenExternal" &&
    authority.controlReason === "provenExternalNoControl"
  ) {
    return "external";
  }
  return "unknown";
}

function statusDotClass(
  status: SessionStatus | undefined,
  sessionId: string | undefined,
  authority: WebPortAuthority | undefined = undefined,
  authorityRequired = false,
): string {
  if (!status && authorityRequired) {
    switch (authorityDotState(authority, sessionId)) {
      case "managed":
        return "bg-emerald-400 dot-live";
      case "managedUnready":
        return "bg-amber-400 dot-live";
      case "external":
        return "bg-sky-400 dot-live";
      case "unknown":
        return "bg-zinc-500";
    }
  }
  if (!status || status === "Stopped" || status === "Exited") {
    return "bg-zinc-600";
  }
  if (status === "Running") {
    if (!authorityRequired) return "bg-emerald-400 dot-live";
    switch (authorityDotState(authority, sessionId)) {
      case "managed":
        return "bg-emerald-400 dot-live";
      case "managedUnready":
        return "bg-amber-400 dot-live";
      case "external":
        return "bg-sky-400 dot-live";
      case "unknown":
        return "bg-zinc-500";
    }
  }
  if (status === "Starting") return "bg-amber-400 dot-live";
  if (status === "Stopping") return "bg-amber-400";
  if (status === "Crashed" || status === "Failed") return "bg-red-400";
  return "bg-zinc-600";
}

function statusDotLabel(
  status: SessionStatus | undefined,
  sessionId: string | undefined,
  authority: WebPortAuthority | undefined,
  authorityRequired = false,
): string {
  if (!status && authorityRequired) {
    switch (authorityDotState(authority, sessionId)) {
      case "managed":
        return "Managed server ready";
      case "managedUnready":
        return "Managed server not ready";
      case "external":
        return "External listener";
      case "unknown":
        return "Port authority unknown";
    }
  }
  if (status === "Running" && authorityRequired) {
    switch (authorityDotState(authority, sessionId)) {
      case "managed":
        return "Managed server ready";
      case "managedUnready":
        return "Managed server not ready";
      case "external":
        return "External listener";
      case "unknown":
        return "Port authority unknown";
    }
  }
  switch (status) {
    case "Running":
      return "Running";
    case "Starting":
      return "Starting";
    case "Stopping":
      return "Stopping";
    case "Crashed":
      return "Crashed";
    case "Failed":
      return "Failed";
    case "Exited":
      return "Exited";
    case "Stopped":
    case undefined:
      return "Stopped";
  }
}

interface ServerActionButtonsProps {
  command: RunCommand;
  live: boolean;
  session: SessionRuntimeState | null;
  authority: WebPortAuthority | undefined;
  onOpenSite(e: React.MouseEvent): void;
  onStart(e: React.MouseEvent): void;
  onStop(e: React.MouseEvent): void;
  onRestart(e: React.MouseEvent): void;
}

function ServerActionButtons({
  command,
  live,
  session,
  authority,
  onOpenSite,
  onStart,
  onStop,
  onRestart,
}: ServerActionButtonsProps) {
  if (!live) {
    return (
      <div className={ACTION_REVEAL_CLASS}>
        <button
          type="button"
          data-sidebar-action="true"
          onClick={onStart}
          title="Start server"
          className="p-1 rounded hover:bg-zinc-600 text-emerald-400"
        >
          <Play className="size-3.5" />
        </button>
      </div>
    );
  }

  return (
    <div className={ACTION_REVEAL_CLASS}>
      {canOpenRemoteSite(command, session, authority) && (
        <button
          type="button"
          data-sidebar-action="true"
          onClick={onOpenSite}
          title="Open site in new tab"
          className="p-1 rounded hover:bg-zinc-600 text-sky-300"
        >
          <ExternalLink className="size-3.5" />
        </button>
      )}
      <button
        type="button"
        data-sidebar-action="true"
        onClick={onRestart}
        title={`Restart ${command.label}`}
        className="p-1 rounded hover:bg-zinc-600 text-amber-400"
      >
        <RotateCcw className="size-3.5" />
      </button>
      <button
        type="button"
        data-sidebar-action="true"
        onClick={onStop}
        title={`Stop ${command.label}`}
        className="p-1 rounded hover:bg-zinc-600 text-red-400"
      >
        <Square className="size-3.5" />
      </button>
    </div>
  );
}

interface CommandRowProps {
  command: RunCommand;
  session: SessionRuntimeState | null;
  authority: WebPortAuthority | undefined;
  indent: number;
}

function CommandRow({ command, session, authority, indent }: CommandRowProps) {
  const activeSessionId = useStore(
    (s) => s.rawTerminal.activeStreamSessionId,
  );
  const setActiveSession = useStore((s) => s.setActiveSession);
  const sendAction = useStore((s) => s.sendAction);

  const live = isLiveStatus(session?.status);
  const rowSessionId = session?.session_id ?? command.id;
  const isActive = activeSessionId === rowSessionId;

  const onRowClick = () => {
    if (session) {
      setActiveSession(session.session_id);
      return;
    }
    setActiveSession(command.id);
    sendAction({
      type: "startServer",
      command_id: command.id,
    });
  };

  const onStart = (e: React.MouseEvent) => {
    e.stopPropagation();
    setActiveSession(command.id);
    sendAction({
      type: "startServer",
      command_id: command.id,
    });
  };

  const onStop = (e: React.MouseEvent) => {
    e.stopPropagation();
    sendAction({ type: "stopServer", command_id: command.id });
  };

  const onRestart = (e: React.MouseEvent) => {
    e.stopPropagation();
    sendAction({
      type: "restartServer",
      command_id: command.id,
    });
  };

  const onOpenSite = (e: React.MouseEvent) => {
    e.stopPropagation();
    if (command.port == null) return;
    const currentState = useStore.getState();
    const currentAuthority = currentState.workspace?.portAuthorities?.find(
      (candidate) => candidate.port === command.port,
    );
    const currentSession = currentState.workspace?.sessions.find(
      (candidate) => candidate.commandId === command.id,
    );
    if (!canOpenRemoteSite(command, currentSession ?? null, currentAuthority)) {
      return;
    }
    openRemoteSiteInNewTab(window.open, window.location, command.port);
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
      className={`group flex items-center gap-2 rounded px-2 py-1 text-xs cursor-pointer ${
        isActive ? "bg-zinc-700/65" : "hover:bg-zinc-700/40"
      }`}
      style={{ paddingLeft: `${indent}px` }}
    >
      <span
        className={`inline-block size-2 rounded-full shrink-0 ${statusDotClass(
          session?.status,
          session?.session_id,
          authority,
          command.port != null,
        )}`}
        role="img"
        aria-label={statusDotLabel(
          session?.status,
          session?.session_id,
          authority,
          command.port != null,
        )}
        title={statusDotLabel(
          session?.status,
          session?.session_id,
          authority,
          command.port != null,
        )}
      />
      <TerminalIcon className="size-3.5 text-zinc-500 shrink-0" />
      <span className="flex-1 truncate text-[11px] text-zinc-300">
        {command.label}
      </span>
      {command.port != null && (
        <span className="text-[10px] text-zinc-500 tabular-nums shrink-0">
          :{command.port}
        </span>
      )}
      <ServerActionButtons
        command={command}
        live={live}
        session={session}
        authority={authority}
        onOpenSite={onOpenSite}
        onStart={onStart}
        onStop={onStop}
        onRestart={onRestart}
      />
    </div>
  );
}

interface FolderSectionProps {
  folder: ProjectFolder;
  sessions: Record<string, SessionRuntimeState>;
  authorities: WebPortAuthority[];
  indent: number;
}

function FolderSection({ folder, sessions, authorities, indent }: FolderSectionProps) {
  const [expanded, setExpanded] = useState(true);
  const activeSessionId = useStore(
    (s) => s.rawTerminal.activeStreamSessionId,
  );
  const setActiveSession = useStore((s) => s.setActiveSession);
  const sendAction = useStore((s) => s.sendAction);

  if (folder.hidden) return null;

  const presentation = describeFolderPresentation(folder);
  const command = folder.commands[0];

  if (presentation.kind === "single-command" && command) {
    const session = findSessionForCommand(sessions, command.id);
    const authority = authorities.find((candidate) => candidate.port === command.port);
    const live = isLiveStatus(session?.status);
    const rowSessionId = session?.session_id ?? command.id;
    const isActive = activeSessionId === rowSessionId;

    const onRowClick = () => {
      if (session) {
        setActiveSession(session.session_id);
        return;
      }
      setActiveSession(command.id);
      sendAction({
        type: "startServer",
        command_id: command.id,
      });
    };

    const onStart = (e: React.MouseEvent) => {
      e.stopPropagation();
      setActiveSession(command.id);
      sendAction({
        type: "startServer",
        command_id: command.id,
      });
    };

    const onStop = (e: React.MouseEvent) => {
      e.stopPropagation();
      sendAction({ type: "stopServer", command_id: command.id });
    };

    const onRestart = (e: React.MouseEvent) => {
      e.stopPropagation();
      sendAction({
        type: "restartServer",
        command_id: command.id,
      });
    };

    const onOpenSite = (e: React.MouseEvent) => {
      e.stopPropagation();
      if (command.port == null) return;
      const currentState = useStore.getState();
      const currentAuthority = currentState.workspace?.portAuthorities?.find(
        (candidate) => candidate.port === command.port,
      );
      const currentSession = currentState.workspace?.sessions.find(
        (candidate) => candidate.commandId === command.id,
      );
      if (!canOpenRemoteSite(command, currentSession ?? null, currentAuthority)) {
        return;
      }
      openRemoteSiteInNewTab(window.open, window.location, command.port);
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
          isActive ? "bg-zinc-700/65" : "hover:bg-zinc-700/40"
        }`}
        style={{ paddingLeft: `${indent}px` }}
      >
        <span
          className={`inline-block size-2 rounded-full shrink-0 ${statusDotClass(
            session?.status,
            session?.session_id,
            authority,
            command.port != null,
          )}`}
          role="img"
          aria-label={statusDotLabel(
            session?.status,
            session?.session_id,
            authority,
            command.port != null,
          )}
          title={statusDotLabel(
            session?.status,
            session?.session_id,
            authority,
            command.port != null,
          )}
        />
        <Folder className="size-3.5 text-zinc-500 shrink-0" />
        <div className="min-w-0 flex-1">
          <div className="truncate text-[11px] font-medium text-zinc-200">
            {presentation.primaryLabel}
          </div>
          <div className="truncate text-[10px] text-zinc-500">
            {presentation.secondaryLabel}
          </div>
        </div>
        {command.port != null && (
          <span className="text-[10px] text-zinc-500 tabular-nums shrink-0">
            :{command.port}
          </span>
        )}
        <ServerActionButtons
          command={command}
          live={live}
          session={session}
          authority={authority}
          onOpenSite={onOpenSite}
          onStart={onStart}
          onStop={onStop}
          onRestart={onRestart}
        />
      </div>
    );
  }

  return (
    <div className="space-y-0.5">
      <button
        type="button"
        onClick={() => setExpanded((value) => !value)}
        className="group flex w-full items-center gap-2 rounded px-2 py-1 text-left hover:bg-zinc-700/30"
        style={{ paddingLeft: `${indent}px` }}
      >
        {expanded ? (
          <ChevronDown className="size-3.5 text-zinc-500 shrink-0" />
        ) : (
          <ChevronRight className="size-3.5 text-zinc-500 shrink-0" />
        )}
        <Folder className="size-3.5 text-zinc-500 shrink-0" />
        <span className="truncate text-[11px] font-medium text-zinc-300 flex-1">
          {folder.name}
        </span>
        <span className="text-[10px] text-zinc-500 shrink-0">
          {folder.commands.length}
        </span>
      </button>
      {expanded &&
        folder.commands.map((nestedCommand) => (
          <CommandRow
            key={nestedCommand.id}
            command={nestedCommand}
            session={findSessionForCommand(sessions, nestedCommand.id)}
            authority={authorities.find(
              (candidate) => candidate.port === nestedCommand.port,
            )}
            indent={indent + 14}
          />
        ))}
    </div>
  );
}

interface AiTabRowProps {
  tab: SessionTab;
  session: SessionRuntimeState | null;
  indent: number;
}

function AiTabRow({ tab, session, indent }: AiTabRowProps) {
  const activeSessionId = useStore(
    (s) => s.rawTerminal.activeStreamSessionId,
  );
  const setActiveSession = useStore((s) => s.setActiveSession);
  const openAiTab = useStore((s) => s.openAiTab);
  const sendAction = useStore((s) => s.sendAction);

  const sessionId = tab.ptySessionId ?? tab.commandId ?? tab.id;
  const isActive = activeSessionId === sessionId;
  const Icon = tab.type === "codex" ? Bot : Sparkles;
  const tone = tab.type === "codex" ? "text-violet-300" : "text-amber-300";

  const onClick = () => {
    setActiveSession(sessionId);
    void openAiTab(tab.id);
  };

  const onClose = (e: React.MouseEvent) => {
    e.stopPropagation();
    sendAction({ type: "closeTab", tab_id: tab.id });
    if (isActive) setActiveSession(null);
  };

  return (
    <div
      role="button"
      data-sidebar-row="true"
      tabIndex={0}
      onClick={onClick}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onClick();
        }
      }}
      className={`group flex items-center gap-2 rounded px-2 py-1 text-xs cursor-pointer ${
        isActive ? "bg-zinc-700/65" : "hover:bg-zinc-700/40"
      }`}
      style={{ paddingLeft: `${indent}px` }}
    >
      <span
        className={`inline-block size-2 rounded-full shrink-0 ${statusDotClass(
          session?.status,
          undefined,
        )}`}
      />
      <Icon className={`size-3.5 shrink-0 ${tone}`} />
      <span className="flex-1 truncate text-[11px] text-zinc-200">
        {tab.label || (tab.type === "codex" ? "Codex" : "Claude")}
      </span>
      <button
        type="button"
        data-sidebar-action="true"
        onClick={onClose}
        title="Close tab"
        className="p-1 rounded hover:bg-zinc-600 text-zinc-400 hover:text-zinc-100 opacity-100 md:opacity-0 md:group-hover:opacity-100 transition-opacity shrink-0"
      >
        <X className="size-3.5" />
      </button>
    </div>
  );
}

interface ProjectSectionProps {
  project: Project;
  sessions: Record<string, SessionRuntimeState>;
  tabs: SessionTab[];
  authorities: WebPortAuthority[];
}

function ProjectSection({ project, sessions, tabs, authorities }: ProjectSectionProps) {
  const activeProjectId = useStore((s) => s.activeProjectId);
  const setActiveProject = useStore((s) => s.setActiveProject);
  const collapsed = useStore((s) => s.collapsedProjects.has(project.id));
  const toggle = useStore((s) => s.toggleProjectCollapsed);
  const launchAiTab = useStore((s) => s.launchAiTab);

  const folders = project.folders.filter((folder) => !folder.hidden);
  const aiTabs = tabs.filter(
    (tab) =>
      tab.projectId === project.id &&
      (tab.type === "claude" || tab.type === "codex"),
  );
  const runningCount = folders
    .flatMap((folder) => folder.commands)
    .reduce((count, command) => {
      return count + (isLiveStatus(findSessionForCommand(sessions, command.id)?.status) ? 1 : 0);
    }, 0);
  const isActiveProject = activeProjectId === project.id;

  const onProjectClick = () => {
    setActiveProject(project.id);
    toggle(project.id);
  };

  const launchAi = (tabType: "claude" | "codex") => {
    setActiveProject(project.id);
    void launchAiTab(project.id, tabType);
  };

  return (
    <section className="space-y-1">
      <div
        className={`group/project flex items-center gap-1 rounded ${
          isActiveProject ? "bg-zinc-800/80" : ""
        }`}
      >
        <button
          type="button"
          onClick={onProjectClick}
          className="flex min-w-0 flex-1 items-center gap-2 rounded px-2 py-1.5 text-left hover:bg-zinc-700/35"
        >
          {collapsed ? (
            <ChevronRight className="size-3.5 text-zinc-500 shrink-0" />
          ) : (
            <ChevronDown className="size-3.5 text-zinc-500 shrink-0" />
          )}
          <span
            className="inline-block size-2.5 rounded-full shrink-0"
            style={{ background: project.color ?? "#64748b" }}
          />
          <span className="truncate text-[11px] font-semibold uppercase tracking-wide text-zinc-100">
            {project.name}
          </span>
          {project.pinned ? (
            <Pin className="size-3 text-amber-300 shrink-0" />
          ) : null}
          {runningCount > 0 && (
            <span className="rounded-full bg-emerald-600/20 px-1.5 text-[10px] font-semibold text-emerald-300 shrink-0">
              {runningCount}
            </span>
          )}
        </button>
        <div className="mr-1 flex items-center gap-0.5 opacity-100 md:opacity-0 md:group-hover/project:opacity-100 transition-opacity shrink-0">
          <button
            type="button"
            data-sidebar-action="true"
            onClick={(e) => {
              e.stopPropagation();
              launchAi("claude");
            }}
            title="New Claude tab"
            className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] font-medium text-amber-300 hover:bg-amber-600/20"
          >
            <Sparkles className="size-3" />
            Claude
          </button>
          <button
            type="button"
            data-sidebar-action="true"
            onClick={(e) => {
              e.stopPropagation();
              launchAi("codex");
            }}
            title="New Codex tab"
            className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] font-medium text-violet-300 hover:bg-violet-600/20"
          >
            <Bot className="size-3" />
            Codex
          </button>
        </div>
      </div>
      {!collapsed && (
        <div className="space-y-0.5">
          {folders.length === 0 && aiTabs.length === 0 ? (
            <div className="px-3 py-1 text-[11px] text-zinc-500">No folders yet.</div>
          ) : null}
          {folders.map((folder) => (
            <FolderSection
              key={folder.id}
              folder={folder}
              sessions={sessions}
              authorities={authorities}
              indent={18}
            />
          ))}
          {aiTabs.map((tab) => (
            <AiTabRow
              key={tab.id}
              tab={tab}
              session={
                (tab.ptySessionId && sessions[tab.ptySessionId]) ||
                (tab.commandId && sessions[tab.commandId]) ||
                null
              }
              indent={18}
            />
          ))}
        </div>
      )}
    </section>
  );
}

function ProjectGroup({
  title,
  projects,
  sessions,
  tabs,
  authorities,
}: {
  title?: string;
  projects: Project[];
  sessions: Record<string, SessionRuntimeState>;
  tabs: SessionTab[];
  authorities: WebPortAuthority[];
}) {
  if (projects.length === 0) return null;

  return (
    <div className="space-y-1">
      {title ? (
        <div className="px-2 text-[10px] font-semibold uppercase tracking-[0.18em] text-zinc-500">
          {title}
        </div>
      ) : null}
      {projects.map((project) => (
        <ProjectSection
          key={project.id}
          project={project}
          sessions={sessions}
          tabs={tabs}
          authorities={authorities}
        />
      ))}
    </div>
  );
}

export function ProjectTree() {
  const snapshot = useStore((s) => s.snapshot);
  const authorities = useStore((s) => s.workspace?.portAuthorities ?? []);
  if (!snapshot) return null;

  const projects = snapshot.appState?.config?.projects ?? [];
  const sessions = snapshot.runtimeState?.sessions ?? {};
  const tabs = snapshot.appState?.open_tabs ?? [];
  const { pinned, standard } = groupProjectsForSidebar(projects);

  if (projects.length === 0) {
    return (
      <div className="px-3 py-4 text-xs text-zinc-500">No projects yet.</div>
    );
  }

  return (
    <div className="space-y-3 px-1">
      <ProjectGroup
        title={pinned.length > 0 && standard.length > 0 ? "Pinned" : undefined}
        projects={pinned}
        sessions={sessions}
        tabs={tabs}
        authorities={authorities}
      />
      <ProjectGroup
        title={pinned.length > 0 && standard.length > 0 ? "Projects" : undefined}
        projects={standard}
        sessions={sessions}
        tabs={tabs}
        authorities={authorities}
      />
    </div>
  );
}
