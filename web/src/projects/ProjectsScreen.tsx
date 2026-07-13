import { ChevronRight, FolderKanban } from "lucide-react";

import { isLiveStatus, type WebWorkspaceSnapshot } from "../api/types";
import type { AppRoute } from "../app/router";

interface ProjectsScreenProps {
  workspace: WebWorkspaceSnapshot;
  onNavigate(route: AppRoute): void;
}

export function ProjectsScreen({ workspace, onNavigate }: ProjectsScreenProps) {
  return (
    <section className="dm-screen" aria-labelledby="projects-title">
      <header className="dm-large-title-header">
        <div>
          <p className="dm-eyebrow">Start and manage work</p>
          <h1 id="projects-title">Projects</h1>
        </div>
      </header>
      <div className="dm-screen-scroll">
        {workspace.projects.length === 0 ? (
          <div className="dm-native-empty">
            <span className="dm-native-empty-icon" aria-hidden="true">
              <FolderKanban size={28} />
            </span>
            <h2>No projects configured</h2>
            <p>Add a project in the DevManager desktop app. It will appear here automatically.</p>
          </div>
        ) : (
          <section className="dm-list-section dm-list-section-first">
            <h2>All projects</h2>
            <div className="dm-grouped-list">
              {workspace.projects.map((project) => {
                const commands = project.folders.flatMap((folder) => folder.commands);
                const liveCount = commands.filter((command) => isLiveStatus(command.status)).length;
                const sessionCount = workspace.sessions.filter(
                  (session) => session.projectId === project.id && isLiveStatus(session.status),
                ).length;
                return (
                  <button
                    key={project.id}
                    type="button"
                    className="dm-project-row"
                    onClick={() => onNavigate({ name: "project", projectId: project.id })}
                  >
                    <span
                      className="dm-project-mark"
                      style={{ backgroundColor: project.color ?? undefined }}
                      aria-hidden="true"
                    >
                      {project.name.slice(0, 1).toUpperCase()}
                    </span>
                    <span className="dm-project-copy">
                      <strong>{project.name}</strong>
                      <small>
                        {sessionCount > 0
                          ? `${sessionCount} active ${sessionCount === 1 ? "session" : "sessions"}`
                          : liveCount > 0
                            ? `${liveCount} running`
                            : `${commands.length} ${commands.length === 1 ? "command" : "commands"}`}
                      </small>
                    </span>
                    <ChevronRight className="dm-row-chevron" size={18} aria-hidden="true" />
                  </button>
                );
              })}
            </div>
          </section>
        )}
      </div>
    </section>
  );
}
