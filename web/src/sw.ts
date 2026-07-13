/// <reference lib="webworker" />

import { clientsClaim } from "workbox-core";
import {
  cleanupOutdatedCaches,
  createHandlerBoundToURL,
  precacheAndRoute,
} from "workbox-precaching";
import { NavigationRoute, registerRoute } from "workbox-routing";
import { NetworkOnly } from "workbox-strategies";
import { isNetworkOnlyPath } from "./pwa/cachePolicy";
import { safeRoute } from "./pwa/notificationRoute";
import { parsePushPayload, type PushPayload } from "./pwa/pushPayload";

declare let self: ServiceWorkerGlobalScope & {
  __WB_MANIFEST: Array<{ revision?: string; url: string }>;
};

cleanupOutdatedCaches();
precacheAndRoute(self.__WB_MANIFEST);

const networkOnly = new NetworkOnly();
const networkOnlyMatch = ({ url }: { url: URL }) =>
  url.origin === self.location.origin && isNetworkOnlyPath(url.pathname);

for (const method of ["GET", "POST", "PUT", "PATCH", "DELETE"] as const) {
  registerRoute(networkOnlyMatch, networkOnly, method);
}

registerRoute(
  new NavigationRoute(createHandlerBoundToURL("/index.html"), {
    denylist: [/^\/api(?:\/|$)/, /^\/pair(?:[/?]|$)/],
  }),
);

self.addEventListener("message", (event) => {
  if (event.data?.type === "SKIP_WAITING") {
    void self.skipWaiting();
  }
});

clientsClaim();

function readPushPayload(event: PushEvent): PushPayload {
  if (!event.data) return {};
  try {
    return parsePushPayload(event.data.json());
  } catch {
    return { body: event.data.text() };
  }
}

self.addEventListener("push", (event) => {
  const payload = readPushPayload(event);
  // Task 10 will add the authenticated actionable payload schema. Task 7 keeps
  // this listener deliberately generic and stores no push or API response.
  const route = safeRoute(payload.route, self.location.origin);
  event.waitUntil(
    self.registration.showNotification(payload.title ?? "DevManager", {
      body: payload.body ?? "DevManager needs your attention.",
      icon: "/icons/devmanager-192.png",
      badge: "/icons/devmanager-192.png",
      tag: payload.tag,
      data: { route },
    }),
  );
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const route = safeRoute(
    (event.notification.data as { route?: unknown } | undefined)?.route,
    self.location.origin,
  );
  const destination = new URL(route, self.location.origin).href;

  event.waitUntil(
    (async () => {
      const windows = await self.clients.matchAll({
        type: "window",
        includeUncontrolled: true,
      });
      const existing = windows.find(
        (client): client is WindowClient => "focus" in client,
      );
      if (existing) {
        await existing.navigate(destination);
        await existing.focus();
        return;
      }
      await self.clients.openWindow(destination);
    })(),
  );
});
