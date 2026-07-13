import { isStandaloneDisplayMode } from "../app/restore";
import { safeRoute } from "./notificationRoute";
import type { PushPayload } from "./pushPayload";

export const NOTIFICATION_RUNTIME_QUERY = "notificationRuntime";
export const NOTIFICATION_ROUTE_QUERY = "notificationRoute";
const MAX_RUNTIME_INSTANCE_ID_LENGTH = 128;

export interface NotificationCapabilities {
  secureContext: boolean;
  standalone: boolean;
  serviceWorkerAvailable: boolean;
  pushManagerAvailable: boolean;
  notificationAvailable: boolean;
}

export type NotificationAvailability =
  | { supported: true; reason: "available" }
  | {
      supported: false;
      reason: "insecure" | "notInstalled" | "unsupported";
    };

export interface PushStatus {
  publicKey: string;
  enabled: boolean;
  subscribed: boolean;
}

type PushRegistrationMode = "enable" | "reconcile";

interface NotificationApi {
  permission: NotificationPermission;
  requestPermission(): Promise<NotificationPermission>;
}

interface PushSubscriptionLike {
  endpoint: string;
  options?: { applicationServerKey?: ArrayBuffer | null };
  getKey(name: PushEncryptionKeyName): ArrayBuffer | null;
  unsubscribe(): Promise<boolean>;
}

interface PushManagerLike {
  getSubscription(): Promise<PushSubscriptionLike | null>;
  subscribe(options: {
    userVisibleOnly: true;
    applicationServerKey: Uint8Array;
  }): Promise<PushSubscriptionLike>;
}

interface ServiceWorkerContainerLike {
  ready: Promise<{ pushManager: PushManagerLike }>;
}

type FetchLike = (
  input: RequestInfo | URL,
  init?: RequestInit,
) => Promise<Response>;

export interface PushBrowserDependencies {
  notification: NotificationApi;
  serviceWorker: ServiceWorkerContainerLike;
  fetch: FetchLike;
}

export interface AppBadgeApi {
  setAppBadge?(count?: number): Promise<void>;
  clearAppBadge?(): Promise<void>;
}

export interface PushNotificationDescriptor {
  title: string;
  body: string;
  route: string;
  tag?: string;
  badge?: number;
  data: {
    route: string;
    eventId?: string;
    runtimeInstanceId?: string;
    action?: PushPayload["action"];
  };
}

export function notificationAvailability(
  capabilities: NotificationCapabilities,
): NotificationAvailability {
  if (!capabilities.secureContext) {
    return { supported: false, reason: "insecure" };
  }
  if (!capabilities.standalone) {
    return { supported: false, reason: "notInstalled" };
  }
  if (
    !capabilities.serviceWorkerAvailable ||
    !capabilities.pushManagerAvailable ||
    !capabilities.notificationAvailable
  ) {
    return { supported: false, reason: "unsupported" };
  }
  return { supported: true, reason: "available" };
}

export function currentNotificationAvailability(): NotificationAvailability {
  return notificationAvailability({
    secureContext: globalThis.isSecureContext === true,
    standalone: isStandaloneDisplayMode(),
    serviceWorkerAvailable:
      typeof navigator !== "undefined" && "serviceWorker" in navigator,
    pushManagerAvailable: typeof PushManager !== "undefined",
    notificationAvailable: typeof Notification !== "undefined",
  });
}

function browserDependencies(): PushBrowserDependencies {
  return {
    notification: Notification,
    serviceWorker: navigator.serviceWorker,
    fetch: globalThis.fetch.bind(globalThis),
  };
}

let pushOperationTail: Promise<void> = Promise.resolve();
let automaticReconciliation: Promise<PushStatus | null> | null = null;

function serializePushOperation<T>(operation: () => Promise<T>): Promise<T> {
  const result = pushOperationTail.then(operation, operation);
  pushOperationTail = result.then(
    () => undefined,
    () => undefined,
  );
  return result;
}

function assertSuccessful(response: Response, action: string): Response {
  if (!response.ok) {
    throw new Error(`${action} failed (${response.status}).`);
  }
  return response;
}

export async function readPushStatus(
  fetchImpl: FetchLike = globalThis.fetch.bind(globalThis),
): Promise<PushStatus> {
  const response = assertSuccessful(
    await fetchImpl("/api/push", {
      credentials: "same-origin",
      headers: { Accept: "application/json" },
    }),
    "Reading notification status",
  );
  const value = (await response.json()) as unknown;
  const record = value as Record<string, unknown>;
  const enabled =
    typeof record?.enabled === "boolean"
      ? record.enabled
      : record?.subscribed;
  if (
    value === null ||
    typeof value !== "object" ||
    typeof record.publicKey !== "string" ||
    record.publicKey === "" ||
    typeof enabled !== "boolean"
  ) {
    throw new Error("The host returned an invalid notification status.");
  }
  return {
    publicKey: record.publicKey,
    enabled,
    subscribed: enabled,
  };
}

export async function readPushRegistrationState(
  dependencies: PushBrowserDependencies = browserDependencies(),
): Promise<PushStatus> {
  return serializePushOperation(() =>
    readPushRegistrationStateNow(dependencies),
  );
}

async function readPushRegistrationStateNow(
  dependencies: PushBrowserDependencies,
): Promise<PushStatus> {
  const [hostStatus, registration] = await Promise.all([
    readPushStatus(dependencies.fetch),
    dependencies.serviceWorker.ready,
  ]);
  let subscription = await registration.pushManager.getSubscription();
  if (!hostStatus.enabled) {
    if (subscription) await subscription.unsubscribe().catch(() => false);
    return {
      publicKey: hostStatus.publicKey,
      enabled: false,
      subscribed: false,
    };
  }
  if (
    subscription === null &&
    dependencies.notification.permission !== "granted"
  ) {
    return {
      publicKey: hostStatus.publicKey,
      enabled: true,
      subscribed: false,
    };
  }
  const applicationServerKey = decodeBase64Url(hostStatus.publicKey);
  if (
    subscription &&
    !sameBytes(subscription.options?.applicationServerKey, applicationServerKey)
  ) {
    await subscription.unsubscribe().catch(() => false);
    subscription = null;
  }
  if (
    subscription === null &&
    dependencies.notification.permission !== "granted"
  ) {
    return {
      publicKey: hostStatus.publicKey,
      enabled: true,
      subscribed: false,
    };
  }
  const created = subscription === null;
  subscription ??= await registration.pushManager.subscribe({
    userVisibleOnly: true,
    applicationServerKey,
  });
  try {
    const enabled = await registerHostSubscription(
      subscription,
      dependencies.fetch,
      "Refreshing notifications",
      "reconcile",
    );
    if (!enabled) {
      await subscription.unsubscribe().catch(() => false);
      return {
        publicKey: hostStatus.publicKey,
        enabled: false,
        subscribed: false,
      };
    }
  } catch (error) {
    if (created) await subscription.unsubscribe().catch(() => false);
    throw error;
  }
  return {
    publicKey: hostStatus.publicKey,
    enabled: true,
    subscribed: true,
  };
}

function decodeBase64Url(value: string): Uint8Array {
  if (!/^[A-Za-z0-9_-]+$/u.test(value)) {
    throw new Error("The host returned an invalid notification key.");
  }
  const normalized = value.replace(/-/gu, "+").replace(/_/gu, "/");
  const padded = normalized.padEnd(
    normalized.length + ((4 - (normalized.length % 4)) % 4),
    "=",
  );
  let decoded: string;
  try {
    decoded = globalThis.atob(padded);
  } catch {
    throw new Error("The host returned an invalid notification key.");
  }
  if (decoded.length === 0) {
    throw new Error("The host returned an invalid notification key.");
  }
  const bytes = Uint8Array.from(decoded, (character) => character.charCodeAt(0));
  if (bytes.length !== 65 || bytes[0] !== 4) {
    throw new Error("The host returned an invalid notification key.");
  }
  return bytes;
}

function encodeBase64Url(value: ArrayBuffer): string {
  const bytes = new Uint8Array(value);
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return globalThis
    .btoa(binary)
    .replace(/\+/gu, "-")
    .replace(/\//gu, "_")
    .replace(/=+$/u, "");
}

function sameBytes(left: ArrayBuffer | null | undefined, right: Uint8Array) {
  if (!left) return false;
  const leftBytes = new Uint8Array(left);
  return (
    leftBytes.length === right.length &&
    leftBytes.every((value, index) => value === right[index])
  );
}

async function disableHostIntent(fetchImpl: FetchLike): Promise<void> {
  assertSuccessful(
    await fetchImpl("/api/push/unsubscribe", {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ disable: true }),
    }),
    "Disabling notifications",
  );
}

async function registerHostSubscription(
  subscription: PushSubscriptionLike,
  fetchImpl: FetchLike,
  action: string,
  mode: PushRegistrationMode,
): Promise<boolean> {
  const p256dh = subscription.getKey("p256dh");
  const auth = subscription.getKey("auth");
  if (!p256dh || !auth) {
    throw new Error("The browser did not provide notification keys.");
  }
  const response = assertSuccessful(
    await fetchImpl("/api/push", {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        mode,
        endpoint: subscription.endpoint,
        keys: {
          p256dh: encodeBase64Url(p256dh),
          auth: encodeBase64Url(auth),
        },
      }),
    }),
    action,
  );
  if (response.status === 204) return true;
  const value = (await response.json()) as unknown;
  if (
    value === null ||
    typeof value !== "object" ||
    typeof (value as Record<string, unknown>).enabled !== "boolean"
  ) {
    throw new Error("The host returned an invalid notification result.");
  }
  return (value as { enabled: boolean }).enabled;
}

export async function reconcilePushNotificationsOnForeground(
  dependencies?: PushBrowserDependencies,
): Promise<PushStatus | null> {
  if (!dependencies && !currentNotificationAvailability().supported) return null;
  const resolved = dependencies ?? browserDependencies();
  if (resolved.notification.permission !== "granted") return null;
  if (automaticReconciliation) return automaticReconciliation;
  const pending = serializePushOperation(() =>
    readPushRegistrationStateNow(resolved),
  );
  automaticReconciliation = pending;
  pending.then(
    () => {
      if (automaticReconciliation === pending) automaticReconciliation = null;
    },
    () => {
      if (automaticReconciliation === pending) automaticReconciliation = null;
    },
  );
  return pending;
}

export async function reconcileChangedPushSubscription(
  dependencies: Pick<PushBrowserDependencies, "fetch"> & {
    pushManager: PushManagerLike;
  },
  changed: {
    oldSubscription?: PushSubscriptionLike | null;
    newSubscription?: PushSubscriptionLike | null;
  } = {},
): Promise<PushStatus> {
  return serializePushOperation(() =>
    reconcileChangedPushSubscriptionNow(dependencies, changed),
  );
}

async function reconcileChangedPushSubscriptionNow(
  dependencies: Pick<PushBrowserDependencies, "fetch"> & {
    pushManager: PushManagerLike;
  },
  changed: {
    oldSubscription?: PushSubscriptionLike | null;
    newSubscription?: PushSubscriptionLike | null;
  },
): Promise<PushStatus> {
  const hostStatus = await readPushStatus(dependencies.fetch);
  let subscription =
    changed.newSubscription ??
    (await dependencies.pushManager.getSubscription());

  if (!hostStatus.enabled) {
    if (subscription) await subscription.unsubscribe().catch(() => false);
    return {
      publicKey: hostStatus.publicKey,
      enabled: false,
      subscribed: false,
    };
  }

  const applicationServerKey = decodeBase64Url(hostStatus.publicKey);

  if (
    subscription &&
    !sameBytes(subscription.options?.applicationServerKey, applicationServerKey)
  ) {
    await subscription.unsubscribe().catch(() => false);
    subscription = null;
  }

  const created = subscription === null;
  subscription ??= await dependencies.pushManager.subscribe({
    userVisibleOnly: true,
    applicationServerKey,
  });
  let enabled: boolean;
  try {
    enabled = await registerHostSubscription(
      subscription,
      dependencies.fetch,
      "Refreshing notifications",
      "reconcile",
    );
  } catch (error) {
    if (created) await subscription.unsubscribe().catch(() => false);
    throw error;
  }
  if (!enabled) {
    await subscription.unsubscribe().catch(() => false);
    return {
      publicKey: hostStatus.publicKey,
      enabled: false,
      subscribed: false,
    };
  }
  return { publicKey: hostStatus.publicKey, enabled: true, subscribed: true };
}

export async function enablePushNotifications(
  dependencies: PushBrowserDependencies = browserDependencies(),
): Promise<PushStatus> {
  return serializePushOperation(() => enablePushNotificationsNow(dependencies));
}

async function enablePushNotificationsNow(
  dependencies: PushBrowserDependencies,
): Promise<PushStatus> {
  const permission =
    dependencies.notification.permission === "granted"
      ? "granted"
      : await dependencies.notification.requestPermission();
  if (permission !== "granted") {
    throw new Error("Notification permission was not granted.");
  }

  const hostStatus = await readPushStatus(dependencies.fetch);
  const applicationServerKey = decodeBase64Url(hostStatus.publicKey);
  const registration = await dependencies.serviceWorker.ready;
  let subscription = await registration.pushManager.getSubscription();

  if (
    subscription &&
    !sameBytes(subscription.options?.applicationServerKey, applicationServerKey)
  ) {
    await subscription.unsubscribe().catch(() => false);
    subscription = null;
  }

  const created = subscription === null;
  subscription ??= await registration.pushManager.subscribe({
    userVisibleOnly: true,
    applicationServerKey,
  });

  try {
    const enabled = await registerHostSubscription(
      subscription,
      dependencies.fetch,
      "Enabling notifications",
      "enable",
    );
    if (!enabled) throw new Error("The host did not enable notifications.");
  } catch (error) {
    if (created) await subscription.unsubscribe().catch(() => false);
    throw error;
  }

  return { publicKey: hostStatus.publicKey, enabled: true, subscribed: true };
}

export async function disablePushNotifications(
  dependencies: PushBrowserDependencies = browserDependencies(),
): Promise<boolean> {
  return serializePushOperation(() => disablePushNotificationsNow(dependencies));
}

async function disablePushNotificationsNow(
  dependencies: PushBrowserDependencies,
): Promise<boolean> {
  const registrationPromise = dependencies.serviceWorker.ready;
  await disableHostIntent(dependencies.fetch);
  const registration = await registrationPromise;
  const subscription = await registration.pushManager.getSubscription();
  if (!subscription) return false;
  await subscription.unsubscribe();
  return true;
}

export async function applyAppBadge(
  count: number,
  target: AppBadgeApi = navigator as Navigator & AppBadgeApi,
): Promise<void> {
  const normalized = Number.isFinite(count)
    ? Math.max(0, Math.floor(count))
    : 0;
  try {
    if (normalized > 0 && target.setAppBadge) {
      await target.setAppBadge(normalized);
    } else if (target.clearAppBadge) {
      await target.clearAppBadge();
    } else if (target.setAppBadge) {
      await target.setAppBadge(0);
    }
  } catch {
    // Badging is optional platform polish and must never interrupt the app.
  }
}

export function describePushNotification(
  payload: PushPayload,
  origin: string,
): PushNotificationDescriptor {
  const route = safeRoute(payload.route, origin);
  return {
    title: payload.title ?? "DevManager",
    body: payload.body ?? "DevManager needs your attention.",
    route,
    tag: payload.tag,
    badge: payload.badge,
    data: {
      route,
      eventId: payload.eventId,
      runtimeInstanceId: payload.runtimeInstanceId,
      action: payload.action,
    },
  };
}

function isRuntimeInstanceId(value: unknown): value is string {
  return (
    typeof value === "string" &&
    value.length > 0 &&
    value.length <= MAX_RUNTIME_INSTANCE_ID_LENGTH &&
    !/\p{Cc}/u.test(value)
  );
}

export function notificationClickDestination(
  data: { route?: unknown; runtimeInstanceId?: unknown } | null | undefined,
  origin: string,
): string {
  const sessions = new URL("/sessions", origin).href;
  if (!isRuntimeInstanceId(data?.runtimeInstanceId)) return sessions;

  const route = safeRoute(data?.route, origin);
  if (!route.startsWith("/session/")) return sessions;
  const destination = new URL("/sessions", origin);
  destination.searchParams.set(
    NOTIFICATION_RUNTIME_QUERY,
    data.runtimeInstanceId,
  );
  destination.searchParams.set(NOTIFICATION_ROUTE_QUERY, route);
  return destination.href;
}
