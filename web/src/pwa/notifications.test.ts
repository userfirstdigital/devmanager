// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";

import {
  applyAppBadge,
  describePushNotification,
  disablePushNotifications,
  enablePushNotifications,
  notificationAvailability,
  readPushRegistrationState,
  readPushStatus,
  type PushBrowserDependencies,
} from "./notifications";

function response(body: unknown, status = 200): Response {
  return new Response(body === null ? null : JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function base64Url(bytes: Uint8Array): string {
  return btoa(String.fromCharCode(...bytes))
    .replace(/\+/gu, "-")
    .replace(/\//gu, "_")
    .replace(/=+$/u, "");
}

function pushDependencies(overrides: Partial<PushBrowserDependencies> = {}) {
  const vapidPublicKey = Uint8Array.from(
    { length: 65 },
    (_, index) => (index === 0 ? 4 : index),
  );
  const p256dh = Uint8Array.from(
    { length: 65 },
    (_, index) => (index === 0 ? 4 : index),
  ).buffer;
  const auth = Uint8Array.from({ length: 16 }, (_, index) => 255 - index).buffer;
  const subscription = {
    endpoint: "https://web.push.apple.com/Q1/test-endpoint",
    options: { applicationServerKey: null },
    getKey: vi.fn((name: PushEncryptionKeyName) =>
      name === "p256dh" ? p256dh : auth,
    ),
    unsubscribe: vi.fn(async () => true),
  };
  const pushManager = {
    getSubscription: vi.fn(
      async (): Promise<typeof subscription | null> => null,
    ),
    subscribe: vi.fn(async () => subscription),
  };
  const fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === "/api/push" && (!init?.method || init.method === "GET")) {
      return response({
        publicKey: base64Url(vapidPublicKey),
        subscribed: false,
      });
    }
    return response(null, 204);
  });
  const dependencies: PushBrowserDependencies = {
    notification: {
      permission: "default",
      requestPermission: vi.fn(
        async (): Promise<NotificationPermission> => "granted",
      ),
    },
    serviceWorker: {
      ready: Promise.resolve({ pushManager }),
    },
    fetch,
    ...overrides,
  };
  return {
    dependencies,
    fetch,
    pushManager,
    subscription,
    vapidPublicKey,
    p256dh,
    auth,
  };
}

describe("Web Push setup", () => {
  it("is available only in a secure installed app with every required browser API", () => {
    const ready = {
      secureContext: true,
      standalone: true,
      serviceWorkerAvailable: true,
      pushManagerAvailable: true,
      notificationAvailable: true,
    };

    expect(notificationAvailability(ready)).toEqual({
      supported: true,
      reason: "available",
    });
    expect(
      notificationAvailability({ ...ready, secureContext: false }),
    ).toEqual({ supported: false, reason: "insecure" });
    expect(
      notificationAvailability({ ...ready, standalone: false }),
    ).toEqual({ supported: false, reason: "notInstalled" });
    expect(
      notificationAvailability({ ...ready, pushManagerAvailable: false }),
    ).toEqual({ supported: false, reason: "unsupported" });
  });

  it("reads only the authenticated host public status", async () => {
    const fetch = vi.fn(async () =>
      response({
        publicKey: "public-vapid-key",
        subscribed: true,
        vapidPrivateKey: "must-never-enter-the-browser-model",
      }),
    );

    await expect(readPushStatus(fetch)).resolves.toEqual({
      publicKey: "public-vapid-key",
      subscribed: true,
    });
    expect(fetch).toHaveBeenCalledWith("/api/push", {
      credentials: "same-origin",
      headers: { Accept: "application/json" },
    });
  });

  it("reports enabled only when the host and this installed app both retain the subscription", async () => {
    const hostEnabledFetch = vi.fn(async () =>
      response({ publicKey: "public-vapid-key", subscribed: true }),
    );
    const { dependencies, pushManager, subscription } = pushDependencies({
      fetch: hostEnabledFetch,
    });

    await expect(readPushRegistrationState(dependencies)).resolves.toEqual({
      publicKey: "public-vapid-key",
      subscribed: false,
    });

    pushManager.getSubscription.mockResolvedValue(subscription);
    await expect(readPushRegistrationState(dependencies)).resolves.toEqual({
      publicKey: "public-vapid-key",
      subscribed: true,
    });
  });

  it("requests permission, subscribes with the host key, and registers exact browser keys", async () => {
    const {
      dependencies,
      fetch,
      pushManager,
      subscription,
      vapidPublicKey,
      p256dh,
      auth,
    } =
      pushDependencies();

    await expect(enablePushNotifications(dependencies)).resolves.toEqual({
      publicKey: base64Url(vapidPublicKey),
      subscribed: true,
    });

    expect(dependencies.notification.requestPermission).toHaveBeenCalledTimes(1);
    expect(pushManager.subscribe).toHaveBeenCalledWith({
      userVisibleOnly: true,
      applicationServerKey: vapidPublicKey,
    });
    expect(fetch).toHaveBeenLastCalledWith("/api/push", {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        endpoint: subscription.endpoint,
        keys: {
          p256dh: base64Url(new Uint8Array(p256dh)),
          auth: base64Url(new Uint8Array(auth)),
        },
      }),
    });
  });

  it("rejects malformed host application keys before touching PushManager", async () => {
    const invalidFetch = vi.fn(async () =>
      response({ publicKey: "BAMCAQ", subscribed: false }),
    );
    const { dependencies, pushManager } = pushDependencies({
      fetch: invalidFetch,
    });

    await expect(enablePushNotifications(dependencies)).rejects.toThrow(
      /notification key/i,
    );
    expect(pushManager.subscribe).not.toHaveBeenCalled();
  });

  it("does not touch the service worker or host when permission is denied", async () => {
    const { dependencies, fetch, pushManager } = pushDependencies({
      notification: {
        permission: "default",
        requestPermission: vi.fn(
          async (): Promise<NotificationPermission> => "denied",
        ),
      },
    });

    await expect(enablePushNotifications(dependencies)).rejects.toThrow(
      /permission/i,
    );
    expect(pushManager.subscribe).not.toHaveBeenCalled();
    expect(fetch).not.toHaveBeenCalled();
  });

  it("removes the authenticated host endpoint and browser subscription", async () => {
    const { dependencies, fetch, pushManager, subscription } =
      pushDependencies();
    pushManager.getSubscription.mockResolvedValue(subscription);

    await disablePushNotifications(dependencies);

    expect(fetch).toHaveBeenCalledWith("/api/push/unsubscribe", {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ endpoint: subscription.endpoint }),
    });
    expect(subscription.unsubscribe).toHaveBeenCalledTimes(1);
  });
});

describe("app badge", () => {
  it("applies positive aggregate counts and clears zero", async () => {
    const badge = {
      setAppBadge: vi.fn(async () => undefined),
      clearAppBadge: vi.fn(async () => undefined),
    };

    await applyAppBadge(3, badge);
    await applyAppBadge(0, badge);

    expect(badge.setAppBadge).toHaveBeenCalledWith(3);
    expect(badge.clearAppBadge).toHaveBeenCalledTimes(1);
  });

  it("keeps stable routing and action metadata without copying arbitrary content", () => {
    const descriptor = describePushNotification(
      {
        title: "DevManager needs input",
        body: "Project · Claude",
        route: "/session/tab/tab-1",
        tag: "devmanager-event-1",
        eventId: "event-1",
        runtimeInstanceId: "runtime-1",
        action: "needsInput",
        badge: 2,
      },
      "https://devmanager.test",
    );

    expect(descriptor).toEqual({
      title: "DevManager needs input",
      body: "Project · Claude",
      tag: "devmanager-event-1",
      route: "/session/tab/tab-1",
      data: {
        route: "/session/tab/tab-1",
        eventId: "event-1",
        runtimeInstanceId: "runtime-1",
        action: "needsInput",
      },
      badge: 2,
    });
    expect(JSON.stringify(descriptor)).not.toContain("PROMPT_SENTINEL");
  });
});
