const DEFAULT_NOTIFICATION_ROUTE = "/sessions";

export function safeRoute(route: unknown, origin: string): string {
  if (typeof route !== "string") return DEFAULT_NOTIFICATION_ROUTE;

  try {
    const url = new URL(route, origin);
    if (url.origin !== origin) return DEFAULT_NOTIFICATION_ROUTE;
    return `${url.pathname}${url.search}${url.hash}`;
  } catch {
    return DEFAULT_NOTIFICATION_ROUTE;
  }
}
