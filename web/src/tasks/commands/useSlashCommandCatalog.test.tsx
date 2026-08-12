// @vitest-environment jsdom

import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  clearSlashCommandCatalogCacheForTests,
  useSlashCommandCatalog,
} from "./useSlashCommandCatalog";

function response(body: unknown, ok = true): Response {
  return {
    ok,
    status: ok ? 200 : 503,
    json: async () => body,
  } as Response;
}

function deferred<T>() {
  let resolve: (value: T) => void = () => {};
  const promise = new Promise<T>((resolvePromise) => {
    resolve = resolvePromise;
  });
  return { promise, resolve };
}

beforeEach(() => {
  clearSlashCommandCatalogCacheForTests();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("slash command discovery hook", () => {
  it("returns built-ins immediately then merges an encoded live-session response", async () => {
    const fetchMock = vi.spyOn(globalThis, "fetch").mockResolvedValue(
      response({
        provider: "claude",
        commands: [
          {
            name: "/project-check",
            description: "Check this project.",
            source: "project",
          },
        ],
      }),
    );

    const { result } = renderHook(() =>
      useSlashCommandCatalog({
        scopeKey: "runtime-a:tab:claude-a",
        sessionKey: "tab:claude-a",
        provider: "claude",
        enabled: true,
      }),
    );

    expect(result.current.commands.some((command) => command.name === "/compact")).toBe(true);
    expect(result.current.loading).toBe(true);
    await waitFor(() =>
      expect(
        result.current.commands.some((command) => command.name === "/project-check"),
      ).toBe(true),
    );
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/slash-commands?sessionKey=tab%3Aclaude-a",
      expect.objectContaining({ credentials: "include" }),
    );
    expect(result.current.loading).toBe(false);
  });

  it("keeps provider built-ins when discovery fails or returns another provider", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      response({ provider: "codex", commands: [] }),
    );
    const { result, rerender } = renderHook(
      ({ scopeKey }) =>
        useSlashCommandCatalog({
          scopeKey,
          sessionKey: "tab:claude-a",
          provider: "claude",
          enabled: true,
        }),
      { initialProps: { scopeKey: "runtime-a:tab:claude-a" } },
    );
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.commands.some((command) => command.name === "/advisor")).toBe(true);
    expect(result.current.commands.some((command) => command.name === "/debug-config")).toBe(false);

    clearSlashCommandCatalogCacheForTests();
    vi.mocked(globalThis.fetch).mockRejectedValueOnce(new Error("offline"));
    rerender({ scopeKey: "runtime-b:tab:claude-a" });
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.commands.some((command) => command.name === "/compact")).toBe(true);
  });

  it("does not publish a late response after the session scope changes", async () => {
    const oldRequest = deferred<Response>();
    const newRequest = deferred<Response>();
    vi.spyOn(globalThis, "fetch")
      .mockReturnValueOnce(oldRequest.promise)
      .mockReturnValueOnce(newRequest.promise);
    const { result, rerender } = renderHook(
      ({ scopeKey, sessionKey }) =>
        useSlashCommandCatalog({
          scopeKey,
          sessionKey,
          provider: "claude",
          enabled: true,
        }),
      {
        initialProps: {
          scopeKey: "runtime-a:tab:old",
          sessionKey: "tab:old",
        },
      },
    );

    rerender({ scopeKey: "runtime-a:tab:new", sessionKey: "tab:new" });
    await act(async () => {
      newRequest.resolve(
        response({
          provider: "claude",
          commands: [{ name: "/new-command", description: "New", source: "project" }],
        }),
      );
      await newRequest.promise;
    });
    await waitFor(() =>
      expect(result.current.commands.some((command) => command.name === "/new-command")).toBe(true),
    );

    await act(async () => {
      oldRequest.resolve(
        response({
          provider: "claude",
          commands: [{ name: "/old-command", description: "Old", source: "project" }],
        }),
      );
      await oldRequest.promise;
    });
    expect(result.current.commands.some((command) => command.name === "/old-command")).toBe(false);
  });

  it("deduplicates simultaneous discovery for the same scoped session", async () => {
    const pending = deferred<Response>();
    const fetchMock = vi.spyOn(globalThis, "fetch").mockReturnValue(pending.promise);
    const props = {
      scopeKey: "runtime-a:tab:claude-a",
      sessionKey: "tab:claude-a",
      provider: "claude" as const,
      enabled: true,
    };
    const first = renderHook(() => useSlashCommandCatalog(props));
    const second = renderHook(() => useSlashCommandCatalog(props));

    expect(fetchMock).toHaveBeenCalledTimes(1);
    await act(async () => {
      pending.resolve(response({ provider: "claude", commands: [] }));
      await pending.promise;
    });
    await waitFor(() => expect(first.result.current.loading).toBe(false));
    await waitFor(() => expect(second.result.current.loading).toBe(false));
  });
});
