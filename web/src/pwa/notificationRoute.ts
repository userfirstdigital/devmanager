const DEFAULT_NOTIFICATION_ROUTE = "/tasks";

export function safeRoute(route: unknown, origin: string): string {
  if (typeof route !== "string") return DEFAULT_NOTIFICATION_ROUTE;

  try {
    const url = new URL(route, origin);
    if (url.origin !== origin) return DEFAULT_NOTIFICATION_ROUTE;
    if (url.pathname === "/tasks") {
      return `${url.pathname}${url.search}${url.hash}`;
    }
    if (
      /^\/tasks\/[^/]+(?:\/(?:chat|terminal|browser))?$/u.test(url.pathname)
    ) {
      const encodedId = url.pathname.split("/")[2];
      if (!encodedId) return DEFAULT_NOTIFICATION_ROUTE;
      try {
        const id = decodeURIComponent(encodedId);
        if (!id || id.includes("\0")) return DEFAULT_NOTIFICATION_ROUTE;
      } catch {
        return DEFAULT_NOTIFICATION_ROUTE;
      }
      return `${url.pathname}${url.search}${url.hash}`;
    }
    return DEFAULT_NOTIFICATION_ROUTE;
  } catch {
    return DEFAULT_NOTIFICATION_ROUTE;
  }
}
