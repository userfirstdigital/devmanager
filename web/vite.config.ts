import { existsSync, readFileSync, readdirSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { VitePWA } from "vite-plugin-pwa";
import { fingerprintEntries } from "./config/sourceFingerprint";

const webRoot = dirname(fileURLToPath(import.meta.url));
const fingerprintRoots = ["src", "public", "config"];
const fingerprintFiles = [
  "index.html",
  "package.json",
  "package-lock.json",
  "tsconfig.json",
  "tsconfig.node.json",
  "vite.config.ts",
];

function collectFiles(directory: string): string[] {
  if (!existsSync(directory)) return [];
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const absolutePath = join(directory, entry.name);
    if (entry.isDirectory()) return collectFiles(absolutePath);
    if (!entry.isFile()) return [];
    return [relative(webRoot, absolutePath).replaceAll("\\", "/")];
  });
}

function listSourceFiles(): string[] {
  const files = fingerprintFiles.filter((path) => existsSync(join(webRoot, path)));
  files.push(
    ...fingerprintRoots.flatMap((path) => collectFiles(join(webRoot, path))),
  );
  return [...new Set(files)].sort();
}

function sourceFingerprint(): string {
  return fingerprintEntries(
    listSourceFiles().map((path) => ({
      path,
      contents: readFileSync(join(webRoot, path)),
    })),
  );
}

const webBuildId = sourceFingerprint();

function buildFingerprintPlugin(): Plugin {
  return {
    name: "devmanager-source-fingerprint",
    transformIndexHtml: {
      order: "post",
      handler: (html) => ({
        html: html.replace(/\r\n?/g, "\n"),
        tags: [
          {
            tag: "meta",
            attrs: {
              name: "devmanager-web-build",
              content: webBuildId,
            },
            injectTo: "head",
          },
        ],
      }),
    },
    generateBundle() {
      this.emitFile({
        type: "asset",
        fileName: "source-fingerprint.txt",
        source: `${webBuildId}\n`,
      });
    },
  };
}

export default defineConfig({
  plugins: [
    react(),
    tailwindcss(),
    buildFingerprintPlugin(),
    VitePWA({
      strategies: "injectManifest",
      srcDir: "src",
      filename: "sw.ts",
      injectRegister: null,
      registerType: "prompt",
      manifestFilename: "manifest.webmanifest",
      manifest: {
        id: "/",
        scope: "/",
        start_url: "/sessions?source=pwa",
        display: "standalone",
        name: "DevManager",
        short_name: "DevManager",
        description: "Secure remote control for DevManager sessions.",
        background_color: "#09090b",
        theme_color: "#09090b",
        icons: [
          {
            src: "/icons/devmanager-192.png",
            sizes: "192x192",
            type: "image/png",
            purpose: "any",
          },
          {
            src: "/icons/devmanager-512.png",
            sizes: "512x512",
            type: "image/png",
            purpose: "any",
          },
          {
            src: "/icons/devmanager-maskable-512.png",
            sizes: "512x512",
            type: "image/png",
            purpose: "maskable",
          },
        ],
      },
      injectManifest: {
        globPatterns: [
          "index.html",
          "assets/**/*.{js,css,woff,woff2}",
          "icons/*.png",
          "manifest.webmanifest",
        ],
      },
      devOptions: {
        enabled: false,
      },
    }),
  ],
  base: "/",
  define: {
    __DEVMANAGER_WEB_BUILD_ID__: JSON.stringify(webBuildId),
  },
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
