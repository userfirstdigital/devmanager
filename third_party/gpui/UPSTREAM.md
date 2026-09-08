# GPUI patch provenance

- Crate: `gpui` 0.2.2 (Apache-2.0), exact crates.io package source.
- Upstream: https://github.com/zed-industries/zed
- Package VCS metadata: `69e2130295c2649963eb639fc70b4f2ee8ea1624` (upstream package records a dirty tree).
- Local change: implement the X11 window/display raw-handle traits, which panic in 0.2.2. Return the XCB window and connection retained by that exact GPUI window, and retain its actual screen index. A destroyed window or unavailable state returns an error.

DevManager converts the observed XCB XID to an Xlib window handle only at Wry's X11 child-builder boundary; both identify the same server-side window. No process/window search supplies identity. Remove this patch when a released upstream version implements these traits.

An additive `Application::new_x11()` constructor selects the existing X11 client without mutating `WAYLAND_DISPLAY` or changing provider child environments. Ordinary `Application::new()` keeps upstream platform selection.
