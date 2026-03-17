export interface AppConfig {
  version: number;
  projects: Project[];
  settings: Settings;
  sshConnections: SSHConnection[];
}

export interface Project {
  id: string;
  name: string;
  rootPath: string;
  folders: ProjectFolder[];
  color?: string;
  pinned?: boolean;
  notes?: string;
  saveLogFiles?: boolean;  // save server output to .log files in project root (default: true)
  createdAt: string;
  updatedAt: string;
}

export interface ProjectFolder {
  id: string;
  name: string;
  folderPath: string;
  commands: RunCommand[];
  envFilePath?: string;
  portVariable?: string;
  hidden?: boolean;
}

export interface RunCommand {
  id: string;
  label: string;
  command: string;
  args: string[];
  env?: Record<string, string>;
  port?: number;
  autoRestart?: boolean;
  clearLogsOnRestart?: boolean;
}

export interface SSHConnection {
  id: string;
  label: string;
  host: string;
  port: number;
  username: string;
  password?: string;
}

export type DefaultTerminal = 'bash' | 'powershell' | 'cmd';

export interface Settings {
  theme: string;
  logBufferSize: number;
  confirmOnClose: boolean;
  minimizeToTray: boolean;
  restoreSessionOnStart?: boolean;
  defaultTerminal: DefaultTerminal;
  claudeCommand?: string;
  codexCommand?: string;
  notificationSound?: string;
  terminalFontSize?: number;
}

export type TabType = 'server' | 'claude' | 'codex' | 'ssh';

export interface SessionState {
  openTabs: SessionTab[];
  activeTabId: string | null;
  sidebarCollapsed: boolean;
}

export interface SessionTab {
  id: string;
  type: TabType;
  projectId: string;
  commandId?: string;
  ptySessionId?: string;
  label?: string;
  sshConnectionId?: string;
}

export interface ScanResult {
  scripts: ScannedScript[];
  ports: ScannedPort[];
  has_package_json: boolean;
  has_cargo_toml: boolean;
  has_env_file: boolean;
}

export interface ScannedScript {
  name: string;
  command: string;
}

export interface ScannedPort {
  variable: string;
  port: number;
  source: string;
}

export interface RootScanEntry {
  path: string;
  name: string;
  hasEnv: boolean;
  projectType: string;
  scripts: ScannedScript[];
  ports: ScannedPort[];
}

export interface DependencyStatus {
  status: 'missing' | 'outdated' | 'ok';
  message: string;
}

export interface ProcessTreeInfo {
  command_id: string;
  processes: ChildProcessInfo[];
  total_memory_mb: number;
  total_cpu_percent: number;
}

export interface ChildProcessInfo {
  pid: number;
  name: string;
  memory_mb: number;
  cpu_percent: number;
}

export interface PortConflict {
  port: number;
  commands: PortConflictEntry[];
}

export interface PortConflictEntry {
  project_name: string;
  command_label: string;
  command_id: string;
}

export interface PortStatus {
  port: number;
  in_use: boolean;
  pid?: number;
  process_name?: string;
}

export interface EnvEntry {
  type: 'variable' | 'comment' | 'blank';
  key?: string;
  value?: string;
  raw: string;
}
