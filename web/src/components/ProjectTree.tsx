import {
  Bot,
  ChevronDown,
  ChevronRight,
  Folder,
  Play,
  RotateCcw,
  Sparkles,
  Square,
  Terminal as TerminalIcon,
  X,
} from "lucide-react";
import {
  DEFAULT_DIMENSIONS,
  isLiveStatus,
  type Project,
  type ProjectFolder,
  type RunCommand,
  type SessionRuntimeState,
  type SessionStatus,
  type SessionTab,
} from "../api/types";
import { useStore } from "../store";

function findSessionForCommand(
  sessions: Record<string, SessionRuntimeState>,
  commandId: string,
): SessionRuntimeState | null {
  for (const session of Object.values(sessions)) {
    if (session.command_id === commandId) return session;
  }
  return null;
}

function statusDotClass(status: SessionStatus | undefined): string {
  if (!status || status === "Stopped" || status === "Exited")
    return "bg-zinc-600";
  if (status === "Running") return "bg-emerald-400 dot-live";
  if (status === "Starting") return "bg-amber-400 dot-live";
  if (status === "Stopping") return "bg-amber-400";
  if (status === "Crashed" || status === "Failed") return "bg-red-400";
  return "bg-zinc-600";
}

interface CommandRowProps {
  command: RunCommand;
  session: SessionRuntimeState | null;
  indent: number;
}

function CommandRow({ command, session, indent }: CommandRowProps) {
  const activeSessionId = useStore((s) => s.activeSessionId);
  const setActiveSession = useStore((s) => s.setActiveSession);
  const sendAction = useStore((s) => s.sendAction);
  const youHaveControl = useStore(
    (s) => s.snapshot?.youHaveControl ?? false,
  );

  const live = isLiveStatus(session?.status);
  const isActive =
    session && activeSessionId && session.session_id === activeSessionId;

  const onRowClick = () => {
    if (session) {
      setActiveSession(session.session_id);
      return;
    }
    // No running session — start it in the background on the host, then
    // show the terminal locally in the web UI without stealing the native
    // app's current active terminal.
    if (youHaveControl) {
      setActiveSession(command.id);
      sendAction({
        type: "startServer",
        command_id: command.id,
        focus: false,
        dimensions: DEFAULT_DIMENSIONS,
      });
    }
  };

  const onStart = (e: React.MouseEvent) => {
    e.stopPropagation();
    setActiveSession(command.id);
    sendAction({
      type: "startServer",
      command_id: command.id,
      focus: false,
      dimensions: DEFAULT_DIMENSIONS,
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
      dimensions: DEFAULT_DIMENSIONS,
    });
  };

  return (
    <div
      role="button"
      data-sidebar-row="true"
      tabIndex={0}
      onClick={onRowClick}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") onRowClick();
      }}
      className={`group flex items-center gap-2 py-1.5 pr-2 rounded hover:bg-zinc-700/40 cursor-pointer ${
        isActive ? "bg-zinc-700/60" : ""
      }`}
      style={{ paddingLeft: `${indent}px` }}
    >
      <span
        className={`inline-block size-2 rounded-full shrink-0 ${statusDotClass(
          session?.status,
        )}`}
      />
      <TerminalIcon className="size-3.5 text-zinc-500 shrink-0" />
      <span className="text-xs text-zinc-200 truncate flex-1">
        {command.label}
      </span>
      {command.port != null && (
        <span className="text-[10px] text-zinc-500 tabular-nums shrink-0">
          :{command.port}
        </span>
      )}
      {/*
        Hover-only reveal is desktop-only. Touch devices don't fire hover
        events so the buttons have to stay visible — otherwise there's no
        way to start/stop a server from a phone. `md:opacity-0` hides them
        on desktop at rest, `group-hover:md:opacity-100` shows them on hover.
      */}
      <div className="flex items-center gap-0.5 opacity-100 md:opacity-0 md:group-hover:opacity-100 transition-opacity shrink-0">
        {!live && (
          <button
            type="button"
            data-sidebar-action="true"
            onClick={onStart}
            disabled={!youHaveControl}
            title={youHaveControl ? "Start server" : "Take control to start"}
            className="p-1 rounded hover:bg-zinc-600 text-emerald-400 disabled:opacity-40 disabled:hover:bg-transparent"
          >
            <Play className="size-3.5" />
          </button>
        )}
        {live && (
          <>
            <button
              type="button"
              data-sidebar-action="true"
              onClick={onRestart}
              disabled={!youHaveControl}
              title={youHaveControl ? "Restart server" : "Take control to restart"}
              className="p-1 rounded hover:bg-zinc-600 text-amber-400 disabled:opacity-40 disabled:hover:bg-transparent"
            >
              <RotateCcw className="size-3.5" />
            </button>
            <button
              type="button"
              data-sidebar-action="true"
              onClick={onStop}
              disabled={!youHaveControl}
              title={youHaveControl ? "Stop server" : "Take control to stop"}
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

interface FolderSectionProps {
  folder: ProjectFolder;
  sessions: Record<string, SessionRuntimeState>;
  indent: number;
}

function FolderSection({ folder, sessions, indent }: FolderSectionProps) {
  if (folder.hidden) return null;
  const commands = folder.commands;

  // Single-command folders render inline (matches archive pattern from
  // ProjectCard.tsx). Multi-command folders expand under a header row.
  if (commands.length === 1) {
    const session = findSessionForCommand(sessions, commands[0].id);
    return <CommandRow command={commands[0]} session={session} indent={indent} />;
  }

  return (
    <div>
      <div
        className="flex items-center gap-2 py-1 text-[11px] text-zinc-400 font-medium uppercase tracking-wide"
        style={{ paddingLeft: `${indent}px` }}
      >
        <Folder className="size-3 shrink-0" />
        <span className="truncate">{folder.name}</span>
      </div>
      {commands.map((command) => (
        <CommandRow
          key={command.id}
          command={command}
          session={findSessionForCommand(sessions, command.id)}
          indent={indent + 16}
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
  const activeSessionId = useStore((s) => s.activeSessionId);
  const setActiveSession = useStore((s) => s.setActiveSession);
  const openAiTab = useStore((s) => s.openAiTab);
  const sendAction = useStore((s) => s.sendAction);
  const youHaveControl = useStore(
    (s) => s.snapshot?.youHaveControl ?? false,
  );

  const sessionId = tab.ptySessionId ?? tab.commandId ?? tab.id;
  const isActive = activeSessionId === sessionId;
  const Icon = tab.type === "codex" ? Bot : Sparkles;
  const tone =
    tab.type === "codex" ? "text-violet-300" : "text-amber-300";

  const onClick = () => {
    setActiveSession(sessionId);
    if (youHaveControl) {
      // Always route AI-tab opens back through the host's ensure/open path.
      // Runtime status alone is not enough here: the host can retain a stale
      // AI runtime entry after the PTY handle is gone, which looks "live" to
      // the sidebar but still needs reopening.
      void openAiTab(tab.id);
    }
  };

  const onClose = (e: React.MouseEvent) => {
    e.stopPropagation();
    sendAction({ type: "closeAiTab", tab_id: tab.id });
    if (isActive) setActiveSession(null);
  };

  return (
    <div
      role="button"
      data-sidebar-row="true"
      tabIndex={0}
      onClick={onClick}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") onClick();
      }}
      className={`group flex items-center gap-2 py-1.5 pr-2 rounded hover:bg-zinc-700/40 cursor-pointer ${
        isActive ? "bg-zinc-700/60" : ""
      }`}
      style={{ paddingLeft: `${indent}px` }}
    >
      <span
        className={`inline-block size-2 rounded-full shrink-0 ${statusDotClass(
          session?.status,
        )}`}
      />
      <Icon className={`size-3.5 ${tone} shrink-0`} />
      <span className="text-xs text-zinc-200 truncate flex-1">
        {tab.label || (tab.type === "codex" ? "Codex" : "Claude")}
      </span>
      <button
        type="button"
        data-sidebar-action="true"
        onClick={onClose}
        disabled={!youHaveControl}
        title={youHaveControl ? "Close tab" : "Take control to close"}
        className="p-1 rounded hover:bg-zinc-600 text-zinc-400 hover:text-zinc-100 opacity-100 md:opacity-0 md:group-hover:opacity-100 disabled:opacity-20 transition-opacity shrink-0"
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
}

function ProjectSection({ project, sessions, tabs }: ProjectSectionProps) {
  const collapsed = useStore((s) => s.collapsedProjects.has(project.id));
  const toggle = useStore((s) => s.toggleProjectCollapsed);
  const launchAiTab = useStore((s) => s.launchAiTab);
  const youHaveControl = useStore(
    (s) => s.snapshot?.youHaveControl ?? false,
  );
  const folders = project.folders.filter((f) => !f.hidden);
  const aiTabs = tabs.filter(
    (tab) =>
      tab.projectId === project.id &&
      (tab.type === "claude" || tab.type === "codex"),
  );

  const runningCount = folders
    .flatMap((f) => f.commands)
    .reduce((acc, cmd) => {
      const session = findSessionForCommand(sessions, cmd.id);
      return acc + (isLiveStatus(session?.status) ? 1 : 0);
    }, 0);

  const launchAi = (tabType: "claude" | "codex") => {
    void launchAiTab(project.id, tabType);
  };

  return (
    <section className="mb-1">
      <div className="group/project flex items-center gap-1 pr-1 rounded hover:bg-zinc-700/40">
        <button
          type="button"
          onClick={() => toggle(project.id)}
          className="flex-1 flex items-center gap-2 py-1.5 px-2 min-w-0"
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
          <span className="text-xs font-medium text-zinc-100 truncate flex-1 text-left">
            {project.name}
          </span>
          {runningCount > 0 && (
            <span className="text-[10px] font-semibold text-emerald-400 bg-emerald-600/20 px-1.5 rounded-full shrink-0">
              {runningCount}
            </span>
          )}
        </button>
        {/*
          Same hover-reveal pattern as CommandRow: always visible on touch,
          hidden at rest on desktop and revealed on project-row hover.
        */}
        <div className="flex items-center gap-0.5 opacity-100 md:opacity-0 md:group-hover/project:opacity-100 transition-opacity shrink-0">
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              launchAi("claude");
            }}
            disabled={!youHaveControl}
            title={youHaveControl ? "New Claude tab" : "Take control to launch Claude"}
            className="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] font-medium text-amber-300 hover:bg-amber-600/20 disabled:opacity-40 disabled:hover:bg-transparent"
          >
            <Sparkles className="size-3" />
            Claude
          </button>
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              launchAi("codex");
            }}
            disabled={!youHaveControl}
            title={youHaveControl ? "New Codex tab" : "Take control to launch Codex"}
            className="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px] font-medium text-violet-300 hover:bg-violet-600/20 disabled:opacity-40 disabled:hover:bg-transparent"
          >
            <Bot className="size-3" />
            Codex
          </button>
        </div>
      </div>
      {!collapsed && (
        <div className="mt-0.5">
          {folders.length === 0 && aiTabs.length === 0 && (
            <div className="text-[11px] text-zinc-500 px-3 py-1">
              No folders
            </div>
          )}
          {folders.map((folder) => (
            <FolderSection
              key={folder.id}
              folder={folder}
              sessions={sessions}
              indent={20}
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
              indent={20}
            />
          ))}
        </div>
      )}
    </section>
  );
}

export function ProjectTree() {
  const snapshot = useStore((s) => s.snapshot);
  if (!snapshot) return null;
  const projects = snapshot.appState?.config?.projects ?? [];
  const sessions = snapshot.runtimeState?.sessions ?? {};
  const tabs = snapshot.appState?.open_tabs ?? [];

  if (projects.length === 0) {
    return (
      <div className="text-xs text-zinc-500 px-3 py-4">No projects yet.</div>
    );
  }

  return (
    <div className="px-1">
      {projects.map((project) => (
        <ProjectSection
          key={project.id}
          project={project}
          sessions={sessions}
          tabs={tabs}
        />
      ))}
    </div>
  );
}
