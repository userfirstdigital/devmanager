const DEFAULT_NOTIFICATION_ROUTE = "/sessions";

export function safeRoute(route: unknown, origin: string): string {
  if (typeof route !== "string") return DEFAULT_NOTIFICATION_ROUTE;

  try {
    const url = new URL(route, origin);
    if (url.origin !== origin) return DEFAULT_NOTIFICATION_ROUTE;
    if (url.pathname === "/sessions") {
      return `${url.pathname}${url.search}${url.hash}`;
    }
    if (/^\/session\/(?:server|tab)\/[^/]+$/u.test(url.pathname)) {
      const encodedId = url.pathname.split("/")[3];
      if (!encodedId) return DEFAULT_NOTIFICATION_ROUTE;
      try {
        const id = decodeURIComponent(encodedId);
        if (!id || id.includes("\0")) return DEFAULT_NOTIFICATION_ROUTE;
      } catch {
        return DEFAULT_NOTIFICATION_ROUTE;
      }
      return url.pathname;
    }
    return DEFAULT_NOTIFICATION_ROUTE;
  } catch {
    return DEFAULT_NOTIFICATION_ROUTE;
  }
}
