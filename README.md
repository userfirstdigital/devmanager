# DevManager

A desktop app for managing all your npm/Node.js dev projects in one place. Start and stop servers, view logs, track ports, and monitor resource usage without juggling multiple terminals.

Built with **Tauri v2** (Rust + React/TypeScript).

## Prerequisites

- **Node.js** >= 18
- **npm** >= 9
- **Rust** >= 1.88 (install via [rustup](https://rustup.rs/))
- **Windows 10/11** or **macOS 15+ (Apple Silicon)**
- **Microsoft Edge WebView2** on Windows (pre-installed on Windows 10 21H2+ and Windows 11)
- **Xcode Command Line Tools** on macOS (`xcode-select --install`)

## Getting Started

### Install dependencies

```bash
npm install
```

### Platform setup notes

On Windows, no extra native setup is required beyond Rust, Node, and WebView2.

On macOS:

```bash
xcode-select --install
```

DevManager currently targets Apple Silicon macs. If you install `node`, `npm`, `cargo`, or `ssh` through Homebrew or shell managers, relaunch the app after changing your shell startup files so the captured login-shell environment stays current.

### Run in development mode

```bash
npm run tauri dev
```

This starts both the Vite dev server (hot reload for the frontend) and the Tauri Rust backend. The app window opens automatically.

### Build for production

```bash
npm run tauri build
```

The installer output is written to `src-tauri/target/release/bundle/`.

- On Windows this produces an `.msi` installer and a portable `.exe`.
- On macOS this produces a `.dmg` bundle plus the updater `.app.tar.gz` artifact used by Tauri's updater.

Build on the target OS you want to ship. Windows artifacts should be built on Windows, and macOS artifacts should be built on macOS.

## Project Structure

```text
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
      commands/                 # Tauri IPC commands (config, scanner, process, ports, resources, session, terminal, env, runtime)
      services/                 # Business logic (config_service, scanner_service, process_manager, resource_service, session_service, platform)
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
- **Process tree management** - kills all child processes (node, esbuild, etc.) on stop
- **Tabbed log viewer** with xterm.js, ANSI color support, 10k line scrollback
- **Session restore** - re-opens tabs and starts servers on next launch

### Process and Port Management

- **Port conflict detection** across projects
- **Check port in use** and kill the occupying process
- **Resource monitoring** - per-server CPU and memory tracking via process tree walking
- **Auto-restart** on crash with exponential backoff

### Developer Workflow

- **npm install detection** - warns when `node_modules` is missing or outdated
- **One-click npm install** from the project context menu
- **.env file editor** - inline editing with comment preservation
- **Git branch display** in the sidebar
- **Open in browser** button for servers with configured ports
- **Open terminal** in the project folder
- **Bulk actions** - Start All and Stop All per project or globally

### Organization

- **Project color tags** with preset palette
- **Pin or favorite projects** to the sidebar top
- **Project notes** with auto-save
- **Import and export configuration** (merge or replace)

### UI

- **Dark dev-tool theme** (zinc/slate color scheme)
- **Error and warning highlighting** in the log toolbar
- **Error count badges** on inactive tabs
- **Status bar** showing running server count, total memory, and the clock
- **System tray** - minimize to tray, tray menu with Show, Stop All, and Quit
- **Confirm on close** when servers are running
- **Crash notifications** via native desktop notifications
- **Log export** to text files with the native save dialog

### Settings

- Confirm on close (default: on)
- Minimize to tray (default: off)
- Resume session on startup (default: on)
- Configurable log buffer size
- Platform-aware default terminal shell selection

## Platform Notes

### Windows

- Server commands are launched through `cmd /C` so `.cmd` shims like `npm.cmd` keep working.
- Interactive terminals keep the existing Windows experience: Git Bash, PowerShell, or `cmd`.
- External terminal launch prefers Windows Terminal and falls back to `cmd`.
- Process cleanup uses `taskkill /T /F`.
- Existing Windows terminal behavior is preserved intentionally and should remain the reference experience.

### macOS

- DevManager currently supports Apple Silicon macs.
- Server commands are launched through the selected shell as a login shell.
- DevManager captures your login-shell environment on startup so Finder-launched builds can still resolve tools installed via Homebrew, `nvm`, `asdf`, and similar setups.
- Interactive terminals can use your detected user shell, `zsh`, or `bash`.
- External terminal launch opens Terminal.app in the selected folder using the configured shell.
- Process cleanup uses Unix signals against the process group so child processes are cleaned up on stop and quit.

## Config Storage

Configuration is stored at:

```text
Windows: %APPDATA%/com.userfirst.devmanager/config.json
macOS: ~/Library/Application Support/com.userfirst.devmanager/config.json
```

Session state (open tabs, sidebar) is stored separately at:

```text
Windows: %APPDATA%/com.userfirst.devmanager/session.json
macOS: ~/Library/Application Support/com.userfirst.devmanager/session.json
```

Both use atomic writes (write to a temp file, then rename) to prevent corruption.

## Releases

Pushing to `master` runs the release workflow in `.github/workflows/release.yml`.

- The workflow calculates one version number and uses it for both Windows and macOS.
- It builds `windows-x86_64` and `darwin-aarch64` artifacts in the same run.
- It publishes both platforms under the same GitHub release tag.
- It generates one `latest.json` updater manifest containing both platform entries.
- It commits the version bump back to `master`, including `src-tauri/Cargo.lock`.

The workflow currently signs Tauri updater artifacts for both platforms. Apple code signing and notarization are not part of this repository yet, so macOS release artifacts may still require local approval from Gatekeeper depending on how they are distributed.

## Tech Stack

| Layer              | Technology                                                         |
| ------------------ | ------------------------------------------------------------------ |
| Desktop framework  | Tauri v2                                                           |
| Frontend           | React 19 + TypeScript + Vite                                       |
| Styling            | Tailwind CSS v4                                                    |
| Terminal           | xterm.js (with fit, search, serialize addons)                      |
| State              | Zustand                                                            |
| Icons              | Lucide React                                                       |
| Backend            | Rust (serde, tokio, sysinfo, regex)                                |
| Process management | Windows `taskkill /T /F`; macOS process-group shutdown via signals |
| Config             | JSON files in the app config directory                             |

## Development

### Frontend only (no Tauri)

```bash
npm run dev
```

Starts the Vite dev server on `http://localhost:1420`. Tauri IPC calls will fail, but this is useful for UI iteration.

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
- **macOS app cannot find `node` or `npm` from Finder**: DevManager captures your login-shell environment on startup so Homebrew and shell-managed PATH entries are available. Relaunch the app after changing shell startup files.
- **macOS says the app is from an unidentified developer**: The current mac release flow does not notarize artifacts. If Gatekeeper blocks launch, use the normal macOS approval flow or add Apple signing and notarization in your own distribution pipeline.
- **Orphaned node processes**: If processes survive after closing, the app's shutdown handler may not have fired. On Windows use Task Manager; on macOS use Activity Monitor.
- **WebView2 missing**: Download it from [Microsoft](https://developer.microsoft.com/en-us/microsoft-edge/webview2/).
- **Rust version too old**: Run `rustup update stable` to get the latest toolchain.
