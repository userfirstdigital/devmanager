import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./index.css";
import { notifyPwaSafetyStateChanged, registerPwa } from "./pwa/register";
import { applyAppBadge } from "./pwa/notifications";
import { readStoreUpdateSafetyState } from "./pwa/storeSafety";
import { bootstrapConnect } from "./connect/identity";
import {
  selectAppBadgeSyncState,
  shouldApplyAppBadge,
  useStore,
} from "./store";

// Deliberately NOT using <StrictMode>. Double-invoking effects at dev time
// is useful for surfacing cleanup bugs, but xterm.js's terminal lifecycle
// (open + bootstrap write + addon loading + ResizeObserver) is extremely
// sensitive to mount-unmount-remount racing on the same container, and we
// test against production builds anyway.
const root = document.getElementById("root");
if (!root) throw new Error("root element missing");
// The Connect marker is installed before React mounts so the store cannot
// race identity creation and accidentally select the legacy socket. A typed
// HOLD leaves the marker absent; it never downgrades an authenticated route.
const connectBootstrap = bootstrapConnect().catch(() => null);
const readSafetyState = () => readStoreUpdateSafetyState(useStore.getState());
let previousSafetyState = readSafetyState();
let previousBadgeState = selectAppBadgeSyncState(useStore.getState());
if (previousBadgeState.count !== null) {
  void applyAppBadge(previousBadgeState.count);
}
useStore.subscribe((state) => {
  const nextSafetyState = readStoreUpdateSafetyState(state);
  if (
    nextSafetyState.hasDraft !== previousSafetyState.hasDraft ||
    nextSafetyState.pendingMutations !== previousSafetyState.pendingMutations ||
    nextSafetyState.selectedAttachments !==
      previousSafetyState.selectedAttachments ||
    nextSafetyState.attachmentLoads !== previousSafetyState.attachmentLoads
  ) {
    previousSafetyState = nextSafetyState;
    notifyPwaSafetyStateChanged();
  }
  const nextBadgeState = selectAppBadgeSyncState(state);
  if (shouldApplyAppBadge(previousBadgeState, nextBadgeState)) {
    void applyAppBadge(nextBadgeState.count);
  }
  previousBadgeState = nextBadgeState;
});
void registerPwa(readSafetyState, () => {
  useStore.setState({
    lastError:
      "DevManager could not reconcile the web bundle automatically without risking a reload loop.",
  });
});
void connectBootstrap.then((handle) => {
  if (handle) {
    window.addEventListener("pagehide", () => handle.stop(), { once: true });
  }
  createRoot(root).render(<App />);
});
