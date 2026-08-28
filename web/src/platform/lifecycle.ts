export interface AppLifecycleCallbacks {
  foreground(): void;
  setVisibility(visible: boolean): void;
  /**
   * pagehide/bfcache suspension. The App shell is the sole lifecycle owner and
   * must supply this so Connect can suspend without a second pagehide binder.
   */
  suspend(): void;
}

export function syncVisualViewportHeight(
  root: HTMLElement = document.documentElement,
  viewport: Pick<VisualViewport, "height"> | null = window.visualViewport,
): void {
  const height = viewport?.height ?? window.innerHeight;
  root.style.setProperty("--visual-viewport-height", `${Math.round(height)}px`);
}

/**
 * Single browser lifecycle owner for visibility, foreground coalescing, and
 * Connect pagehide suspension. pageshow reads the real visibilityState so a
 * restored page does not resume as permanently hidden. Hidden focus/online do
 * not call foreground (no writer-authority claim while hidden).
 */
export function bindAppLifecycle(
  callbacks: AppLifecycleCallbacks,
): () => void {
  const foregroundWhenVisible = () => {
    if (document.visibilityState === "hidden") return;
    callbacks.foreground();
  };
  const visibility = () => {
    const visible = document.visibilityState === "visible";
    callbacks.setVisibility(visible);
    if (visible) callbacks.foreground();
  };
  const pageshow = () => {
    const visible = document.visibilityState === "visible";
    callbacks.setVisibility(visible);
    if (visible) callbacks.foreground();
  };
  const pagehide = () => {
    callbacks.setVisibility(false);
    callbacks.suspend();
  };
  const viewport = () => syncVisualViewportHeight();

  document.addEventListener("visibilitychange", visibility);
  window.addEventListener("focus", foregroundWhenVisible);
  window.addEventListener("pageshow", pageshow);
  window.addEventListener("online", foregroundWhenVisible);
  window.addEventListener("pagehide", pagehide);
  window.addEventListener("resize", viewport);
  window.visualViewport?.addEventListener("resize", viewport);
  window.visualViewport?.addEventListener("scroll", viewport);
  viewport();

  return () => {
    document.removeEventListener("visibilitychange", visibility);
    window.removeEventListener("focus", foregroundWhenVisible);
    window.removeEventListener("pageshow", pageshow);
    window.removeEventListener("online", foregroundWhenVisible);
    window.removeEventListener("pagehide", pagehide);
    window.removeEventListener("resize", viewport);
    window.visualViewport?.removeEventListener("resize", viewport);
    window.visualViewport?.removeEventListener("scroll", viewport);
  };
}
