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
import {
  applyAppBadge,
  describePushNotification,
  notificationClickDestination,
  reconcileChangedPushSubscription,
  type AppBadgeApi,
} from "./pwa/notifications";
import { parsePushEventData, type PushPayload } from "./pwa/pushPayload";
import {
  UPDATE_ACTIVATION_RESULT,
  createWorkerUpdateGate,
  isUpdateActivationRequest,
  isUpdateSafetyAck,
} from "./pwa/updateProtocol";

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

const updateGate = createWorkerUpdateGate({
  listClients: async () => {
    const windows = await self.clients.matchAll({
      type: "window",
      includeUncontrolled: true,
    });
    return windows.map((client) => ({
      id: client.id,
      visibilityState:
        "visibilityState" in client && client.visibilityState === "hidden"
          ? "hidden"
          : "visible",
      postMessage: (message: unknown) => client.postMessage(message),
    }));
  },
  skipWaiting: () => self.skipWaiting(),
});

self.addEventListener("message", (event) => {
  if (isUpdateSafetyAck(event.data)) {
    if (event.source && "id" in event.source) {
      updateGate.acknowledge(
        event.data.nonce,
        event.source.id,
        event.data.safe,
      );
    }
    return;
  }

  if (isUpdateActivationRequest(event.data)) {
    const source = event.source;
    event.waitUntil(
      (async () => {
        const activated = await updateGate.requestActivation(event.data.nonce);
        source?.postMessage({
          type: UPDATE_ACTIVATION_RESULT,
          nonce: event.data.nonce,
          activated,
        });
      })(),
    );
  }
});

clientsClaim();

function readPushPayload(event: PushEvent): PushPayload {
  return parsePushEventData(event.data);
}

self.addEventListener("push", (event) => {
  const payload = readPushPayload(event);
  const notification = describePushNotification(payload, self.location.origin);
  const tasks: Promise<unknown>[] = [
    self.registration.showNotification(notification.title, {
      body: notification.body,
      icon: "/icons/devmanager-192.png",
      badge: "/icons/devmanager-192.png",
      tag: notification.tag,
      data: notification.data,
    }),
  ];
  if (notification.badge !== undefined) {
    tasks.push(
      applyAppBadge(
        notification.badge,
        self.navigator as WorkerNavigator & AppBadgeApi,
      ),
    );
  }
  event.waitUntil(
    Promise.all(tasks).then(() => undefined),
  );
});

interface PushSubscriptionChangeEventLike extends ExtendableEvent {
  oldSubscription?: PushSubscription | null;
  newSubscription?: PushSubscription | null;
}

self.addEventListener("pushsubscriptionchange", (event) => {
  const changed = event as PushSubscriptionChangeEventLike;
  changed.waitUntil(
    reconcileChangedPushSubscription(
      {
        pushManager: self.registration.pushManager,
        fetch: (input, init) => self.fetch(input, init),
      },
      {
        oldSubscription: changed.oldSubscription,
        newSubscription: changed.newSubscription,
      },
    ).then(() => undefined),
  );
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const destination = notificationClickDestination(
    event.notification.data as
      | { route?: unknown; runtimeInstanceId?: unknown }
      | undefined,
    self.location.origin,
  );

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
