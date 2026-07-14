import { existsSync, readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const webRoot = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const bundleRoot = join(webRoot, "bundle");
const repoRoot = join(webRoot, "..");

function readBundleFile(path: string): string {
  return readFileSync(join(bundleRoot, path), "utf8");
}

describe("production PWA bundle", () => {
  it("contains the install manifest and required icons", () => {
    const manifest = JSON.parse(readBundleFile("manifest.webmanifest")) as {
      id?: string;
      scope?: string;
      start_url?: string;
      display?: string;
      orientation?: string;
      icons?: Array<{ src?: string; sizes?: string; purpose?: string }>;
    };

    expect(manifest).toMatchObject({
      id: "/",
      scope: "/",
      start_url: "/sessions?source=pwa",
      display: "standalone",
    });
    expect(manifest).not.toHaveProperty("orientation");
    expect(manifest.icons).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          src: "/icons/devmanager-192.png",
          sizes: "192x192",
          purpose: "any",
        }),
        expect.objectContaining({
          src: "/icons/devmanager-512.png",
          sizes: "512x512",
          purpose: "any",
        }),
        expect.objectContaining({
          src: "/icons/devmanager-maskable-512.png",
          sizes: "512x512",
          purpose: "maskable",
        }),
      ]),
    );

    for (const icon of [
      "icons/devmanager-180.png",
      "icons/devmanager-192.png",
      "icons/devmanager-512.png",
      "icons/devmanager-maskable-512.png",
    ]) {
      expect(existsSync(join(bundleRoot, icon)), icon).toBe(true);
    }
  });

  it("keeps the iPhone metadata accessible and carries the source fingerprint", () => {
    const index = readBundleFile("index.html");
    const fingerprint = readBundleFile("source-fingerprint.txt").trim();

    expect(index).not.toContain("\r");
    expect(index).toContain("viewport-fit=cover");
    expect(index).not.toContain("maximum-scale");
    expect(index).not.toContain("user-scalable");
    expect(index).toContain("/icons/devmanager-180.png");
    expect(index).toContain('media="(prefers-color-scheme: light)"');
    expect(index).toContain('media="(prefers-color-scheme: dark)"');
    expect(fingerprint).toMatch(/^[0-9a-f]{16}$/);
    expect(index).toContain(
      `name="devmanager-web-build" content="${fingerprint}"`,
    );
  });

  it("pre-caches only the shell assets and includes every built reference", () => {
    const index = readBundleFile("index.html");
    const worker = readBundleFile("sw.js");
    const localReferences = [...index.matchAll(/(?:src|href)="\/?([^"#?]+)"/g)]
      .map((match) => match[1])
      .filter((path) => path.startsWith("assets/"));

    expect(localReferences.length).toBeGreaterThan(0);
    for (const reference of localReferences) {
      expect(existsSync(join(bundleRoot, reference)), reference).toBe(true);
      expect(worker).toContain(reference);
    }
    for (const precached of [
      "index.html",
      "manifest.webmanifest",
      "icons/devmanager-180.png",
      "icons/devmanager-192.png",
      "icons/devmanager-512.png",
      "icons/devmanager-maskable-512.png",
    ]) {
      expect(worker).toContain(precached);
    }
    expect(worker).not.toContain("source-fingerprint.txt");
  });
});

describe("tracked bundle determinism", () => {
  it("disables checkout text conversion for every generated bundle byte", () => {
    const attributes = readFileSync(join(repoRoot, ".gitattributes"), "utf8");
    expect(attributes).toMatch(/^web\/bundle\/\*\*\s+-text$/m);

    const tracked = execFileSync(
      "git",
      ["ls-files", "web/bundle"],
      { cwd: repoRoot, encoding: "utf8" },
    )
      .trim()
      .split(/\r?\n/)
      .filter(Boolean);
    expect(tracked.length).toBeGreaterThan(0);
    for (const path of tracked) {
      const blob = execFileSync("git", ["show", `:${path}`], {
        cwd: repoRoot,
      });
      const filtered = execFileSync(
        "git",
        [
          "-c",
          "core.autocrlf=true",
          "cat-file",
          "--filters",
          `--path=${path}`,
          `:${path}`,
        ],
        { cwd: repoRoot },
      );
      expect(filtered.equals(blob), path).toBe(true);
    }
  });

  it("has pull-request CI that rebuilds and rejects a dirty bundle", () => {
    const workflow = readFileSync(
      join(repoRoot, ".github", "workflows", "web-bundle.yml"),
      "utf8",
    );
    expect(workflow).toMatch(/pull_request:/);
    expect(workflow).toContain("npm --prefix web ci");
    expect(workflow).toContain("npm --prefix web test");
    expect(workflow).toContain("npm --prefix web run typecheck");
    expect(workflow).toContain("npm --prefix web run build");
    expect(workflow).toContain("cargo test --locked remote::web::assets --lib");
    expect(workflow).toContain("git status --porcelain -- web/bundle");
  });
});
