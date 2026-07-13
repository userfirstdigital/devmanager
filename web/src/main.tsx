import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./index.css";
import { notifyPwaSafetyStateChanged, registerPwa } from "./pwa/register";
import { applyAppBadge } from "./pwa/notifications";
import { readStoreUpdateSafetyState } from "./pwa/storeSafety";
import { selectAggregateBadgeCount, useStore } from "./store";

// Deliberately NOT using <StrictMode>. Double-invoking effects at dev time
// is useful for surfacing cleanup bugs, but xterm.js's terminal lifecycle
// (open + bootstrap write + addon loading + ResizeObserver) is extremely
// sensitive to mount-unmount-remount racing on the same container, and we
// test against production builds anyway.
const root = document.getElementById("root");
if (!root) throw new Error("root element missing");
const readSafetyState = () => readStoreUpdateSafetyState(useStore.getState());
let previousSafetyState = readSafetyState();
let previousBadgeCount = selectAggregateBadgeCount(useStore.getState());
if (previousBadgeCount !== null) void applyAppBadge(previousBadgeCount);
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
  const nextBadgeCount = selectAggregateBadgeCount(state);
  if (nextBadgeCount !== null && nextBadgeCount !== previousBadgeCount) {
    previousBadgeCount = nextBadgeCount;
    void applyAppBadge(nextBadgeCount);
  }
});
void registerPwa(readSafetyState, () => {
  useStore.setState({
    lastError:
      "DevManager could not reconcile the web bundle automatically without risking a reload loop.",
  });
});
createRoot(root).render(<App />);
