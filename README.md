# DevManager

A desktop app for managing all your npm/Node.js dev projects in one place. Start/stop servers, view logs, track ports, and monitor resource usage without juggling multiple terminals.

Built with **Tauri v2** (Rust + React/TypeScript).

## Prerequisites

- **Node.js** >= 18
- **npm** >= 9
- **Rust** >= 1.88 (install via [rustup](https://rustup.rs/))
- **Windows 10/11** (uses Windows-specific process management)
- **Microsoft Edge WebView2** (pre-installed on Windows 10 21H2+ and Windows 11)

## Getting Started

### Install dependencies

```bash
npm install
```

### Run in development mode

```bash
npm run tauri dev
```

This starts both the Vite dev server (hot reload for frontend) and the Tauri Rust backend. The app window opens automatically.

### Build for production

```bash
npm run tauri build
```

The installer is output to `src-tauri/target/release/bundle/`. On Windows this produces an `.msi` installer and a portable `.exe` in `src-tauri/target/release/`.

## Project Structure

```
devmanager/
  src/                          # React frontend
    components/
      layout/                   # AppLayout, Sidebar, StatusBar
      projects/                 # ProjectList, ProjectCard, AddProjectDialog, EnvEditor, ProjectNotes
      servers/                  # ServerTabBar, ServerControls, ResourceMonitor
      logs/                     # LogViewer (xterm.js), LogToolbar
      settings/                 # SettingsDialog, ImportExport
    stores/                     # Zustand state (appStore, processStore)
    hooks/                      # useConfig, useProcess, useSessionRestore
    types/                      # TypeScript interfaces
  src-tauri/                    # Rust backend
    src/
      commands/                 # Tauri IPC commands (config, scanner, process, ports, resources, session, terminal, env)
      services/                 # Business logic (config_service, scanner_service, process_manager, resource_service, session_service)
      models/                   # Serde data structures
      state.rs                  # AppState (shared process tracking)
      lib.rs                    # App builder, plugins, tray, window events
    capabilities/default.json   # Tauri permissions (shell, fs, dialog, notification)
    tauri.conf.json             # Tauri app configuration
    Cargo.toml                  # Rust dependencies
```

## Features

### Core
- **Add projects** via folder picker with automatic npm script discovery
- **Start/stop/restart** servers with real-time log streaming
- **Process tree management** — kills all child processes (node, esbuild, etc.) on stop
- **Tabbed log viewer** with xterm.js, ANSI color support, 10k line scrollback
- **Session restore** — re-opens tabs and starts servers on next launch

### Process & Port Management
- **Port conflict detection** across projects
- **Check port in use** and kill occupying process
- **Resource monitoring** — per-server CPU and memory tracking via process tree walking
- **Auto-restart** on crash with exponential backoff

### Developer Workflow
- **npm install detection** — warns when `node_modules` is missing or outdated
- **One-click npm install** from the project context menu
- **.env file editor** — inline editing with comment preservation
- **Git branch display** in sidebar (polled every 10 seconds)
- **Open in browser** button for servers with configured ports
- **Open terminal** in project folder (Windows Terminal with cmd fallback)
- **Bulk actions** — Start All / Stop All per project or globally

### Organization
- **Project color tags** with preset palette
- **Pin/favorite projects** to sidebar top
- **Project notes** with auto-save
- **Import/export configuration** (merge or replace)

### UI
- **Dark dev-tool theme** (zinc/slate color scheme)
- **Error/warning highlighting** in log toolbar
- **Error count badges** on inactive tabs
- **Status bar** showing running server count, total memory, clock
- **System tray** — minimize to tray, tray menu with Show/Stop All/Quit
- **Confirm on close** when servers are running
- **Crash notifications** via Windows toast notifications
- **Log export** to text files with native save dialog

### Settings
- Confirm on close (default: on)
- Minimize to tray (default: off)
- Resume session on startup (default: on)
- Configurable log buffer size

## Config Storage

Configuration is stored at:

```
%APPDATA%/com.userfirst.devmanager/config.json
```

Session state (open tabs, sidebar) is stored separately at:

```
%APPDATA%/com.userfirst.devmanager/session.json
```

Both use atomic writes (write to temp file, then rename) to prevent corruption.

## Tech Stack

| Layer | Technology |
|-------|-----------|
| Desktop framework | Tauri v2 |
| Frontend | React 19 + TypeScript + Vite |
| Styling | Tailwind CSS v4 |
| Terminal | xterm.js (with fit, search, serialize addons) |
| State | Zustand |
| Icons | Lucide React |
| Backend | Rust (serde, tokio, sysinfo, regex) |
| Process mgmt | Windows `taskkill /T /F` for process tree killing |
| Config | JSON files in AppData |

## Development

### Frontend only (no Tauri)

```bash
npm run dev
```

Starts the Vite dev server on `http://localhost:1420`. Tauri IPC calls will fail, but useful for UI iteration.

### Rust backend only

```bash
cd src-tauri
cargo check
```

### Rebuild icons

Place a source image and run:

```bash
npm run tauri icon path/to/icon.png
```

## Troubleshooting

- **`npm.cmd` not found**: The app spawns processes via `cmd /C npm ...` to handle Windows `.cmd` shims. Make sure `npm` is in your system PATH.
- **Orphaned node processes**: If processes survive after closing, the app's shutdown handler may not have fired. Use Task Manager to kill remaining `node.exe` processes.
- **WebView2 missing**: Download from [Microsoft](https://developer.microsoft.com/en-us/microsoft-edge/webview2/).
- **Rust version too old**: Run `rustup update stable` to get the latest toolchain.
