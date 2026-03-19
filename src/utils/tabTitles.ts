import type { TabInfo } from '../stores/appStore';
import type { AppConfig } from '../types/config';

export const APP_WINDOW_TITLE = 'DevManager';

const WINDOW_TITLE_SEPARATOR = ' \u2022 ';
const TAB_LABEL_SEPARATOR = ' - ';

type TerminalTitleMap = Record<string, string>;

function findProject(tab: TabInfo, config: AppConfig | null) {
  return config?.projects.find(project => project.id === tab.projectId);
}

function findSshConnection(tab: TabInfo, config: AppConfig | null) {
  return config?.sshConnections.find(conn => conn.id === tab.sshConnectionId);
}

function getServerFallbackLabel(tab: TabInfo, config: AppConfig | null): string {
  const project = findProject(tab, config);
  if (!project || !tab.commandId) {
    return tab.commandId || 'Server';
  }

  for (const folder of project.folders) {
    const command = folder.commands.find(entry => entry.id === tab.commandId);
    if (command) {
      const folderPrefix = project.folders.length > 1 ? `${folder.name} / ` : '';
      return `${folderPrefix}${command.label}`;
    }
  }

  return tab.commandId;
}

export function normalizeTerminalTitle(title: string): string | null {
  const normalized = title.trim();
  return normalized.length > 0 ? normalized : null;
}

export function getLiveTerminalTitle(tab: Pick<TabInfo, 'ptySessionId'>, terminalTitles: TerminalTitleMap): string | null {
  if (!tab.ptySessionId) {
    return null;
  }

  return terminalTitles[tab.ptySessionId] ?? null;
}

export function getTabProjectName(tab: TabInfo, config: AppConfig | null): string | null {
  if (tab.type === 'ssh') {
    return 'SSH';
  }

  return findProject(tab, config)?.name ?? null;
}

export function getTabFallbackTerminalLabel(tab: TabInfo, config: AppConfig | null): string {
  switch (tab.type) {
    case 'server':
      return getServerFallbackLabel(tab, config);
    case 'claude':
      return tab.label || 'Claude';
    case 'codex':
      return tab.label || 'Codex';
    case 'ssh':
      return findSshConnection(tab, config)?.label || tab.label || 'SSH';
    default:
      return 'Terminal';
  }
}

export function getSidebarTerminalLabel(tab: TabInfo, config: AppConfig | null, terminalTitles: TerminalTitleMap): string {
  return getSidebarTerminalLabelWithLiveTitle(tab, config, getLiveTerminalTitle(tab, terminalTitles));
}

export function getSidebarTerminalLabelWithLiveTitle(
  tab: TabInfo,
  config: AppConfig | null,
  liveTitle: string | null | undefined,
): string {
  return liveTitle || getTabFallbackTerminalLabel(tab, config);
}

export function getTerminalHeaderLabel(tab: TabInfo, config: AppConfig | null): string {
  const segments = [getTabProjectName(tab, config), getTabFallbackTerminalLabel(tab, config)]
    .filter((segment): segment is string => Boolean(segment));

  return dedupeAdjacentSegments(segments).join(TAB_LABEL_SEPARATOR);
}

export function getWindowTitle(tab: TabInfo | undefined, config: AppConfig | null, terminalTitles: TerminalTitleMap): string {
  return getWindowTitleWithLiveTitle(tab, config, tab ? getLiveTerminalTitle(tab, terminalTitles) : null);
}

export function getWindowTitleWithLiveTitle(
  tab: TabInfo | undefined,
  config: AppConfig | null,
  liveTitle: string | null | undefined,
): string {
  if (!tab) {
    return APP_WINDOW_TITLE;
  }

  const segments = [
    getTabProjectName(tab, config),
    liveTitle || getTabFallbackTerminalLabel(tab, config),
    APP_WINDOW_TITLE,
  ].filter((segment): segment is string => Boolean(segment));

  return dedupeAdjacentSegments(segments).join(WINDOW_TITLE_SEPARATOR);
}

function dedupeAdjacentSegments(segments: string[]): string[] {
  return segments.filter((segment, index) => index === 0 || segment !== segments[index - 1]);
}
