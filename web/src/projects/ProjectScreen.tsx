import {
  ArrowLeft,
  Bot,
  ChevronRight,
  CircleStop,
  Play,
  RefreshCw,
  Server,
  TerminalSquare,
} from "lucide-react";

import { isLiveStatus, type WebWorkspaceSnapshot } from "../api/types";
import { routeForSessionKey, type AppRoute } from "../app/router";
import { useStore } from "../store";

interface ProjectScreenProps {
  workspace: WebWorkspaceSnapshot;
  projectId: string;
  connected: boolean;
  onNavigate(route: AppRoute): void;
}

export function ProjectScreen({
  workspace,
  projectId,
  connected,
  onNavigate,
}: ProjectScreenProps) {
  const sendAction = useStore((state) => state.sendAction);
  const launchAiTab = useStore((state) => state.launchAiTab);
  const openAiTab = useStore((state) => state.openAiTab);
  const connectSsh = useStore((state) => state.connectSsh);
  const project = workspace.projects.find((candidate) => candidate.id === projectId);

  if (!project) {
    return (
      <section className="dm-screen">
        <header className="dm-compact-header">
          <button type="button" className="dm-nav-back" onClick={() => onNavigate({ name: "projects" })}>
            <ArrowLeft size={21} aria-hidden="true" />
            Projects
          </button>
        </header>
        <div className="dm-screen-scroll">
          <div className="dm-native-empty">
            <h2>Project unavailable</h2>
            <p>The host no longer includes this project.</p>
          </div>
        </div>
      </section>
    );
  }

  const tabs = workspace.tabs.filter(
    (tab) =>
      tab.projectId === projectId &&
      (tab.kind === "claude" || tab.kind === "codex"),
  );

  const launch = (kind: "claude" | "codex") => {
    if (!connected) return;
    void launchAiTab(project.id, kind).then(() => {
      const key = useStore.getState().activeSessionKey;
      if (key) onNavigate(routeForSessionKey(key));
    });
  };

  return (
    <section className="dm-screen" aria-labelledby="project-title">
      <header className="dm-compact-header dm-project-header">
        <button type="button" className="dm-nav-back" onClick={() => onNavigate({ name: "projects" })}>
          <ArrowLeft size={21} aria-hidden="true" />
          Projects
        </button>
        <div className="dm-compact-title">
          <h1 id="project-title">{project.name}</h1>
          <p>Project workspace</p>
        </div>
      </header>
      <div className="dm-screen-scroll">
        <section className="dm-launch-grid" aria-label="Start an AI session">
          <button type="button" disabled={!connected} onClick={() => launch("claude")}>
            <span className="dm-launch-icon dm-claude" aria-hidden="true"><Bot size={22} /></span>
            <span><strong>New Claude</strong><small>Start coding</small></span>
          </button>
          <button type="button" disabled={!connected} onClick={() => launch("codex")}>
            <span className="dm-launch-icon dm-codex" aria-hidden="true"><TerminalSquare size={22} /></span>
            <span><strong>New Codex</strong><small>Start coding</small></span>
          </button>
        </section>

        {tabs.length ? (
          <section className="dm-list-section">
            <h2>Open AI sessions</h2>
            <div className="dm-grouped-list">
              {tabs.map((tab) => {
                const key = `tab:${tab.id}`;
                const session = workspace.sessions.find((candidate) => candidate.stableSessionKey === key);
                return (
                  <button
                    key={tab.id}
                    type="button"
                    className="dm-simple-row"
                    onClick={() => {
                      if (connected) void openAiTab(tab.id);
                      onNavigate(routeForSessionKey(key));
                    }}
                  >
                    <span className="dm-row-leading" aria-hidden="true"><Bot size={19} /></span>
                    <span className="dm-row-copy">
                      <strong>{tab.label?.trim() || `${tab.kind === "claude" ? "Claude" : "Codex"} session`}</strong>
                      <small>{session ? session.status : "Not running"}</small>
                    </span>
                    <ChevronRight className="dm-row-chevron" size={18} aria-hidden="true" />
                  </button>
                );
              })}
            </div>
          </section>
        ) : null}

        {project.folders.map((folder) => (
          <section className="dm-list-section" key={folder.id}>
            <h2>{folder.name}</h2>
            <div className="dm-grouped-list">
              {folder.commands.map((command) => {
                const key = `server:${command.id}`;
                const session = workspace.sessions.find((candidate) => candidate.stableSessionKey === key);
                const live = isLiveStatus(command.status);
                return (
                  <div className="dm-command-row" key={command.id}>
                    <button
                      type="button"
                      className="dm-command-main"
                      disabled={!session}
                      onClick={() => onNavigate(routeForSessionKey(key))}
                    >
                      <span className="dm-row-leading" aria-hidden="true"><Server size={19} /></span>
                      <span className="dm-row-copy">
                        <strong>{command.label}</strong>
                        <small>{command.port ? `Port ${command.port} · ` : ""}{command.status}</small>
                      </span>
                    </button>
                    <div className="dm-row-actions">
                      {live ? (
                        <>
                          <button
                            type="button"
                            aria-label={`Restart ${command.label}`}
                            disabled={!connected}
                            onClick={() => sendAction({ type: "restartServer", command_id: command.id })}
                          ><RefreshCw size={18} /></button>
                          <button
                            type="button"
                            aria-label={`Stop ${command.label}`}
                            disabled={!connected}
                            onClick={() => sendAction({ type: "stopServer", command_id: command.id })}
                          ><CircleStop size={19} /></button>
                        </>
                      ) : (
                        <button
                          type="button"
                          aria-label={`Start ${command.label}`}
                          disabled={!connected}
                          onClick={() => sendAction({ type: "startServer", command_id: command.id })}
                        ><Play size={19} fill="currentColor" /></button>
                      )}
                    </div>
                  </div>
                );
              })}
            </div>
          </section>
        ))}

        {workspace.sshConnections.length ? (
          <section className="dm-list-section">
            <h2>SSH connections</h2>
            <div className="dm-grouped-list">
              {workspace.sshConnections.map((connection) => (
                <button
                  key={connection.id}
                  type="button"
                  disabled={!connected}
                  className="dm-simple-row"
                  onClick={() => connectSsh(connection.id)}
                >
                  <span className="dm-row-leading" aria-hidden="true"><TerminalSquare size={19} /></span>
                  <span className="dm-row-copy">
                    <strong>{connection.label}</strong>
                    <small>{connection.username}@{connection.host}:{connection.port}</small>
                  </span>
                  <span className="dm-row-action-label">Connect</span>
                </button>
              ))}
            </div>
          </section>
        ) : null}
      </div>
    </section>
  );
}
