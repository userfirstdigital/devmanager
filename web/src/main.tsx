import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./index.css";

// Deliberately NOT using <StrictMode>. Double-invoking effects at dev time
// is useful for surfacing cleanup bugs, but xterm.js's terminal lifecycle
// (open + bootstrap write + addon loading + ResizeObserver) is extremely
// sensitive to mount-unmount-remount racing on the same container, and we
// test against production builds anyway.
const root = document.getElementById("root");
if (!root) throw new Error("root element missing");
createRoot(root).render(<App />);
