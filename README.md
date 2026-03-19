# DevManager Native

This branch is the native rewrite of DevManager using:

- `gpui`
- `alacritty_terminal`
- `cargo-packager`

The previous Tauri + React app is archived in:

- `zz-archive/tauri-react-v0.1.11`

## Current State

This is the phase-1 scaffold:

- archived old app for reference
- created a native GPUI shell at the repo root
- added `cargo-packager` metadata to `Cargo.toml`

## Run

```powershell
cargo run
```

## Package

```powershell
cargo install cargo-packager --locked
cargo packager --release
```
