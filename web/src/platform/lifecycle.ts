export interface AppLifecycleCallbacks {
  foreground(): void;
  setVisibility(visible: boolean): void;
}

export function syncVisualViewportHeight(
  root: HTMLElement = document.documentElement,
  viewport: Pick<VisualViewport, "height"> | null = window.visualViewport,
): void {
  const height = viewport?.height ?? window.innerHeight;
  root.style.setProperty("--visual-viewport-height", `${Math.round(height)}px`);
}

export function bindAppLifecycle(
  callbacks: AppLifecycleCallbacks,
): () => void {
  const foreground = () => callbacks.foreground();
  const visibility = () => {
    const visible = document.visibilityState === "visible";
    callbacks.setVisibility(visible);
    if (visible) callbacks.foreground();
  };
  const viewport = () => syncVisualViewportHeight();

  document.addEventListener("visibilitychange", visibility);
  window.addEventListener("focus", foreground);
  window.addEventListener("pageshow", foreground);
  window.addEventListener("online", foreground);
  window.addEventListener("resize", viewport);
  window.visualViewport?.addEventListener("resize", viewport);
  window.visualViewport?.addEventListener("scroll", viewport);
  viewport();

  return () => {
    document.removeEventListener("visibilitychange", visibility);
    window.removeEventListener("focus", foreground);
    window.removeEventListener("pageshow", foreground);
    window.removeEventListener("online", foreground);
    window.removeEventListener("resize", viewport);
    window.visualViewport?.removeEventListener("resize", viewport);
    window.visualViewport?.removeEventListener("scroll", viewport);
  };
}
