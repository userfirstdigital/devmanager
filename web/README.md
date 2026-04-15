# devmanager web UI

Browser-based client for devmanager's remote-host feature. React + Vite + TypeScript.

## Dev

```
npm install
npm run dev
```

Point the dev server at a running devmanager host by setting the `VITE_WS_URL`
env var (added in a later phase).

## Build

```
npm run build
```

Outputs to `bundle/` (not `dist/`, to avoid the repo-root `.gitignore`). The Rust
binary embeds `bundle/` at compile time via `rust-embed`.

## Layout

- `src/main.tsx` — entry point
- `src/App.tsx` — root component
- `src/index.css` — global styles
- `bundle/index.html` — stub committed so `cargo build` works without running
  `npm install` first; `npm run build` overwrites it.
