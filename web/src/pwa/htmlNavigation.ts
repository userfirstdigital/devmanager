/**
 * Same-origin HTML navigation helpers for the Connect service worker.
 * Network-first with one canonical private last-good CacheStorage entry,
 * then precache. Persistence never blocks the navigated response.
 */

import { readBoundedResponseText, withResponseAbort } from "../connect/boundedResponse";

export const CONNECT_HTML_NAV_CACHE = "devmanager-connect-html-v1" as const;
/** Single canonical key — distinct task routes share one last-good HTML entry. */
export const CONNECT_HTML_LAST_GOOD_PATH =
  "/__devmanager_last_good_html__" as const;
export const CONNECT_HTML_NAV_DEADLINE_MS = 1_000;
export const CONNECT_HTML_MAX_BYTES = 1_048_576;

/** Generation fence so a late timed-out write cannot overwrite a newer last-good. */
let htmlLastGoodWriteGeneration = 0;
let pendingHtmlWrite: { cache: Cache; key: Request; response: Response; generation: number } | null = null;
let htmlWriteDrain: Promise<void> | null = null;

// At most one physical write plus one newest pending body. Cache.put has no
// cancellation API; serial ownership prevents a late old write winning.
function queueHtmlWrite(cache: Cache, key: Request, response: Response, generation: number): Promise<void> {
  pendingHtmlWrite = { cache, key, response, generation };
  if (!htmlWriteDrain) {
    htmlWriteDrain = Promise.resolve().then(async () => {
      try {
      while (pendingHtmlWrite) {
        const next = pendingHtmlWrite;
        pendingHtmlWrite = null;
        if (next.generation !== htmlLastGoodWriteGeneration) continue;
        try { await next.cache.put(next.key, next.response); } catch { /* Best-effort private cache. */ }
      }
      } finally { htmlWriteDrain = null; }
    });
  }
  return htmlWriteDrain;
}

function decodedHtmlHeaders(original: Headers): Headers {
  const headers = new Headers(original);
  for (const name of ["content-encoding", "content-length", "content-range"]) headers.delete(name);
  return headers;
}

export function connectHtmlLastGoodRequest(origin: string): Request {
  return new Request(new URL(CONNECT_HTML_LAST_GOOD_PATH, origin).href, {
    method: "GET",
    credentials: "same-origin",
  });
}

export function isSameOriginHtmlNavigation(input: {
  request: Pick<Request, "mode" | "method" | "destination" | "url">;
  origin: string;
}): boolean {
  if (input.request.method !== "GET") return false;
  if (input.request.mode !== "navigate" && input.request.destination !== "document") {
    return false;
  }
  try {
    const url = new URL(input.request.url);
    return url.origin === input.origin;
  } catch {
    return false;
  }
}

export function shouldBypassHtmlNavigationCache(pathname: string): boolean {
  return (
    pathname === "/api" ||
    pathname.startsWith("/api/") ||
    pathname === "/pair" ||
    pathname.startsWith("/pair/") ||
    pathname === CONNECT_HTML_LAST_GOOD_PATH
  );
}

function contentTypeIsHtml(headers: Headers): boolean {
  const raw = headers.get("content-type");
  if (!raw) return false;
  const media = raw.split(";", 1)[0]?.trim().toLowerCase() ?? "";
  return media === "text/html";
}

function isForeignRedirect(response: Response, origin: string): boolean {
  if (!response.redirected) return false;
  try {
    return new URL(response.url).origin !== origin;
  } catch {
    return true;
  }
}

async function readCachedHtmlResponse(
  cached: Response,
  maxBytes: number,
  signal: AbortSignal,
): Promise<Response | null> {
  if (!cached.ok || cached.type === "opaque" || !contentTypeIsHtml(cached.headers)) {
    return null;
  }
  try {
    const text = await readBoundedResponseText(
      cached,
      maxBytes,
      signal,
      "cached html rejected",
    );
    return new Response(text, {
      status: cached.status,
      statusText: cached.statusText,
      headers: decodedHtmlHeaders(cached.headers),
    });
  } catch {
    return null;
  }
}

/**
 * Bounded network-first HTML fetch. On success returns the body immediately and
 * schedules a generation-fenced canonical cache write (via waitUntil when provided).
 * On failure returns last-good, then null so the caller can fall back to precache.
 */
export async function networkFirstHtmlNavigation(input: {
  request: Request;
  origin: string;
  caches: CacheStorage;
  fetch: typeof fetch;
  deadlineMs?: number;
  maxBytes?: number;
  precacheUrl?: string;
  /** SW event.waitUntil — keep cache write owned without blocking the response. */
  waitUntil?: (promise: Promise<unknown>) => void;
}): Promise<Response | null> {
  const url = new URL(input.request.url);
  if (
    url.origin !== input.origin ||
    shouldBypassHtmlNavigationCache(url.pathname)
  ) {
    return null;
  }
  const maxBytes = input.maxBytes ?? CONNECT_HTML_MAX_BYTES;
  const deadline = input.deadlineMs ?? CONNECT_HTML_NAV_DEADLINE_MS;
  const lastGoodKey = connectHtmlLastGoodRequest(input.origin);
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), deadline);
  const writeGeneration = ++htmlLastGoodWriteGeneration;
  let cache: Cache | null = null;

  try {
    const response = await withResponseAbort(input.fetch(input.request, {
      signal: controller.signal,
      credentials: "same-origin",
      cache: "no-store",
      redirect: "follow",
    }), controller.signal);
    if (
      !response.ok ||
      response.type === "opaque" ||
      isForeignRedirect(response, input.origin) ||
      !contentTypeIsHtml(response.headers)
    ) {
      throw new Error("html navigation rejected");
    }
    const text = await readBoundedResponseText(
      response,
      maxBytes,
      controller.signal,
      "html body rejected",
    );
    // Preserve exact CSP/metadata headers with the body.
    const headers = decodedHtmlHeaders(response.headers);
    const clientResponse = new Response(text, {
      status: response.status,
      statusText: response.statusText,
      headers,
    });
    const cacheResponse = clientResponse.clone();
    const putWork = (async () => {
      const storageController = new AbortController();
      const storageTimer = setTimeout(() => storageController.abort(), deadline);
      try {
        cache = await withResponseAbort(input.caches.open(CONNECT_HTML_NAV_CACHE), storageController.signal);
      if (writeGeneration !== htmlLastGoodWriteGeneration) return;
        await queueHtmlWrite(cache, lastGoodKey, cacheResponse, writeGeneration);
      } catch {
        // Private HTML cache is best-effort; never fail the navigation.
      } finally {
        clearTimeout(storageTimer);
      }
    })();
    if (input.waitUntil) {
      input.waitUntil(putWork);
    } else {
      void putWork;
    }
    return clientResponse;
  } catch {
    // Fall through to last-good / precache.
  } finally {
    clearTimeout(timer);
  }

  const fallbackController = new AbortController();
  const fallbackTimer = setTimeout(() => fallbackController.abort(), deadline);
  try {
  cache ??= await withResponseAbort(input.caches.open(CONNECT_HTML_NAV_CACHE), fallbackController.signal);
  const cached = await withResponseAbort(cache.match(lastGoodKey), fallbackController.signal);
  if (cached) {
      const restored = await readCachedHtmlResponse(
        cached,
        maxBytes,
        fallbackController.signal,
      );
      if (restored) return restored;
  }
  if (input.precacheUrl) {
    const precached = await withResponseAbort(input.caches.match(input.precacheUrl), fallbackController.signal);
    if (precached) return precached;
  }
  } catch { /* Unavailable cache falls through to the caller's precache route. */ }
  finally { clearTimeout(fallbackTimer); }
  return null;
}

/** Test-only: reset the generation fence between cases. */
export function resetHtmlLastGoodWriteGenerationForTests(): void {
  htmlLastGoodWriteGeneration = 0;
}
