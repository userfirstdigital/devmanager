import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
// Vite config for devmanager's embedded web UI.
// Output goes to web/bundle/ (not dist/, to avoid the repo-root .gitignore rule).
// Paths are relative so the SPA works regardless of the URL the axum server mounts it at.
export default defineConfig({
    plugins: [react(), tailwindcss()],
    base: "./",
    build: {
        outDir: "bundle",
        emptyOutDir: true,
        assetsDir: "assets",
        sourcemap: false,
        target: "es2020",
    },
    server: {
        port: 5199,
        strictPort: false,
    },
});
