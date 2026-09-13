const BROWSER_INSTALL_ID_KEY = "devmanager.browserInstallId";

type LocationLike = {
  protocol: string;
  host: string;
};

export { buildConnectWebSocketUrl } from "../connect/transport";

function createBrowserInstallId(): string {
  if (typeof globalThis.crypto?.randomUUID === "function") {
    return globalThis.crypto.randomUUID();
  }
  return `browser-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

export function getBrowserInstallId(): string {
  const existing = globalThis.localStorage?.getItem(BROWSER_INSTALL_ID_KEY)?.trim();
  if (existing) {
    return existing;
  }
  const created = createBrowserInstallId();
  globalThis.localStorage?.setItem(BROWSER_INSTALL_ID_KEY, created);
  return created;
}

export function buildPairingRequest(token: string): RequestInit {
  return {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ t: token, browserInstallId: getBrowserInstallId() }),
  };
}

export function buildWebSocketUrl(
  locationLike: LocationLike = window.location,
): string {
  const scheme = locationLike.protocol === "https:" ? "wss" : "ws";
  const params = new URLSearchParams();
  params.set("browserInstallId", getBrowserInstallId());
  return `${scheme}://${locationLike.host}/api/ws?${params.toString()}`;
}
