import { describe, expect, it, vi } from "vitest";

import {
  CONNECT_HTML_LAST_GOOD_PATH,
  CONNECT_HTML_NAV_CACHE,
  connectHtmlLastGoodRequest,
  isSameOriginHtmlNavigation,
  networkFirstHtmlNavigation,
  resetHtmlLastGoodWriteGenerationForTests,
  shouldBypassHtmlNavigationCache,
} from "./htmlNavigation";

function navigationRequest(url: string): Request {
  const request = new Request(url, { method: "GET" });
  Object.defineProperty(request, "mode", {
    configurable: true,
    value: "navigate",
  });
  Object.defineProperty(request, "destination", {
    configurable: true,
    value: "document",
  });
  return request;
}

function htmlResponse(body: string, headers: Record<string, string> = {}) {
  const bytes = new TextEncoder().encode(body);
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(bytes);
      controller.close();
    },
  });
  return new Response(stream, {
    status: 200,
    headers: {
      "content-type": "text/html; charset=utf-8",
      "content-security-policy": "default-src 'self'",
      ...headers,
    },
  });
}

function memoryCaches() {
  const store = new Map<string, Response>();
  let putsInFlight = 0;
  let maxPutsInFlight = 0;
  const cache = {
    match: async (req: RequestInfo) => {
      const key = typeof req === "string" ? req : req.url;
      const hit = store.get(key);
      return hit ? hit.clone() : undefined;
    },
    put: async (req: RequestInfo, res: Response) => {
      putsInFlight += 1;
      maxPutsInFlight = Math.max(maxPutsInFlight, putsInFlight);
      const key = typeof req === "string" ? req : req.url;
      store.set(key, res);
      putsInFlight -= 1;
    },
  };
  return {
    store,
    maxPutsInFlight: () => maxPutsInFlight,
    caches: {
      open: async () => cache,
      match: async () => undefined,
    } as unknown as CacheStorage,
  };
}

describe("htmlNavigation", () => {
  it("treats API and pair as network-only and never cacheable navigations", () => {
    expect(shouldBypassHtmlNavigationCache("/api/connect/fleet-hosts")).toBe(true);
    expect(shouldBypassHtmlNavigationCache("/pair")).toBe(true);
    expect(shouldBypassHtmlNavigationCache("/tasks")).toBe(false);
  });

  it("shares one canonical last-good entry across distinct task routes", async () => {
    resetHtmlLastGoodWriteGenerationForTests();
    const { store, caches } = memoryCaches();
    const origin = "https://phone.example";
    const waitUntilPromises: Promise<unknown>[] = [];
    const waitUntil = vi.fn((promise: Promise<unknown>) => {
      waitUntilPromises.push(promise);
    });
    const first = await networkFirstHtmlNavigation({
      request: navigationRequest(`${origin}/tasks/aaaa`),
      origin,
      caches,
      fetch: async () =>
        htmlResponse("<html>devmanager-connect CSP</html>", {
          "content-security-policy": "connect-src https://studio.example",
          "content-encoding": "gzip",
          "content-length": "999",
          "content-range": "bytes 0-1/2",
        }),
      waitUntil,
      deadlineMs: 200,
    });
    expect(first).not.toBeNull();
    expect(first!.headers.get("content-security-policy")).toBe(
      "connect-src https://studio.example",
    );
    expect(first!.headers.get("content-encoding")).toBeNull();
    expect(first!.headers.get("content-length")).toBeNull();
    expect(first!.headers.get("content-range")).toBeNull();
    await Promise.all(waitUntilPromises);

    const second = await networkFirstHtmlNavigation({
      request: navigationRequest(`${origin}/tasks/bbbb`),
      origin,
      caches,
      fetch: async () => {
        throw new Error("offline");
      },
      deadlineMs: 50,
    });
    expect(second).not.toBeNull();
    expect(await second!.text()).toContain("devmanager-connect");
    expect(second!.headers.get("content-security-policy")).toBe(
      "connect-src https://studio.example",
    );
    expect([...store.keys()]).toEqual([
      connectHtmlLastGoodRequest(origin).url,
    ]);
    expect(CONNECT_HTML_LAST_GOOD_PATH).toContain("last_good");
    expect(CONNECT_HTML_NAV_CACHE).toBe("devmanager-connect-html-v1");
  });

  it("does not cache non-HTML successful responses", async () => {
    resetHtmlLastGoodWriteGenerationForTests();
    const { store, caches } = memoryCaches();
    const putWait = vi.fn();
    const response = await networkFirstHtmlNavigation({
      request: navigationRequest("https://phone.example/tasks"),
      origin: "https://phone.example",
      caches,
      fetch: async () =>
        new Response("{}", {
          status: 200,
          headers: { "content-type": "application/json" },
        }),
      waitUntil: putWait,
    });
    expect(response).toBeNull();
    expect(store.size).toBe(0);
    expect(putWait).not.toHaveBeenCalled();
  });

  it("bounds abort-resistant HTML body reads under the deadline", async () => {
    resetHtmlLastGoodWriteGenerationForTests();
    const { caches } = memoryCaches();
    const response = await networkFirstHtmlNavigation({
      request: navigationRequest("https://phone.example/"),
      origin: "https://phone.example",
      caches,
      deadlineMs: 40,
      fetch: async () => {
        const stream = new ReadableStream({
          start(controller) {
            controller.enqueue(new TextEncoder().encode("<html>"));
          },
        });
        return new Response(stream, {
          status: 200,
          headers: { "content-type": "text/html" },
        });
      },
    });
    expect(response).toBeNull();
  });

  it("never treats API fetches as HTML navigation cache hits", async () => {
    const request = navigationRequest("https://phone.example/api/me");
    expect(
      isSameOriginHtmlNavigation({
        request,
        origin: "https://phone.example",
      }),
    ).toBe(true);
    const put = vi.fn();
    const caches = {
      open: async () => ({ match: async () => undefined, put }),
      match: async () => undefined,
    } as unknown as CacheStorage;
    const response = await networkFirstHtmlNavigation({
      request,
      origin: "https://phone.example",
      caches,
      fetch: async () => htmlResponse("<html>nope</html>"),
    });
    expect(response).toBeNull();
    expect(put).not.toHaveBeenCalled();
  });

  it("ignores an old network response arriving after a newer navigation", async () => {
    resetHtmlLastGoodWriteGenerationForTests();
    const { store, caches, maxPutsInFlight } = memoryCaches();
    const origin = "https://phone.example";
    const waitUntilPromises: Promise<unknown>[] = [];
    const waitUntil = (promise: Promise<unknown>) => {
      waitUntilPromises.push(promise);
    };

    let releaseOld!: () => void;
    const oldGate = new Promise<void>((resolve) => {
      releaseOld = resolve;
    });

    const oldNav = networkFirstHtmlNavigation({
      request: navigationRequest(`${origin}/tasks/old`),
      origin,
      caches,
      deadlineMs: 500,
      waitUntil,
      fetch: async () => {
        await oldGate;
        return htmlResponse("<html>OLD</html>", {
          "content-security-policy": "connect-src https://old.example",
        });
      },
    });

    const newNav = networkFirstHtmlNavigation({
      request: navigationRequest(`${origin}/tasks/new`),
      origin,
      caches,
      deadlineMs: 200,
      waitUntil,
      fetch: async () =>
        htmlResponse("<html>NEW</html>", {
          "content-security-policy": "connect-src https://new.example",
        }),
    });

    const newest = await newNav;
    expect(await newest!.text()).toContain("NEW");
    releaseOld?.();
    const oldest = await oldNav;
    expect(oldest).not.toBeNull();
    await Promise.all(waitUntilPromises);

    const cached = store.get(connectHtmlLastGoodRequest(origin).url)!;
    expect(await cached.text()).toContain("NEW");
    expect(cached.headers.get("content-security-policy")).toBe(
      "connect-src https://new.example",
    );
    expect(maxPutsInFlight()).toBeLessThanOrEqual(1);
  });

  it("serializes physical cache writes so a slow old put cannot overwrite new HTML", async () => {
    resetHtmlLastGoodWriteGenerationForTests();
    const origin = "https://phone.example";
    const waits: Promise<unknown>[] = [];
    const store = new Map<string, Response>();
    let releaseOld!: () => void;
    const oldGate = new Promise<void>((resolve) => { releaseOld = resolve; });
    let startedOld!: () => void;
    const oldStarted = new Promise<void>((resolve) => { startedOld = resolve; });
    let inFlight = 0;
    let maximum = 0;
    let writes = 0;
    const cache = {
      match: async () => undefined,
      put: async (request: Request, response: Response) => {
        inFlight += 1;
        maximum = Math.max(maximum, inFlight);
        writes += 1;
        if (writes === 1) { startedOld(); await oldGate; }
        store.set(request.url, response);
        inFlight -= 1;
      },
    };
    const caches = { open: async () => cache } as unknown as CacheStorage;
    const navigate = (body: string) => networkFirstHtmlNavigation({
      request: navigationRequest(`${origin}/tasks/${body}`), origin, caches,
      fetch: async () => htmlResponse(`<html>${body}</html>`),
      waitUntil: (work) => { waits.push(work); },
    });
    await navigate("OLD");
    await oldStarted;
    const newest = await navigate("NEW");
    expect(await newest!.text()).toContain("NEW");
    expect(writes).toBe(1);
    releaseOld();
    await Promise.all(waits);
    expect(writes).toBe(2);
    expect(maximum).toBe(1);
    expect(await store.get(connectHtmlLastGoodRequest(origin).url)!.text()).toContain("NEW");
  });
});
