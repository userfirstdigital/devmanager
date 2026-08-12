//! Opt-in Windows WebView2 live-surface harness.
//!
//! Default `cargo test` and every non-Windows target are no-ops. Set
//! `DEVMANAGER_BROWSER_WEBVIEW2_E2E=1` only on a UI-thread Windows host that
//! already has WebView2. The harness never installs runtimes, profiles, or
//! extra crates.

#[cfg(not(windows))]
#[test]
fn opt_in_windows_live_surface_harness() {
    let _ = std::env::var_os("DEVMANAGER_BROWSER_WEBVIEW2_E2E");
}

#[cfg(windows)]
#[test]
fn opt_in_windows_live_surface_harness() {
    if std::env::var_os("DEVMANAGER_BROWSER_WEBVIEW2_E2E").as_deref()
        != Some(std::ffi::OsStr::new("1"))
    {
        return;
    }

    windows_live_surface_harness();
}

#[cfg(windows)]
fn windows_live_surface_harness() {
    use raw_window_handle::{
        HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle,
        WindowHandle as BorrowedWindowHandle,
    };
    use std::num::NonZeroIsize;
    use std::path::PathBuf;
    use windows::core::w;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetParent, IsWindow, WS_CLIPCHILDREN, WS_DISABLED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
    };
    use wry::dpi::{LogicalPosition, LogicalSize};
    use wry::{Rect, WebViewBuilder, WebViewExtWindows};

    struct InertParent {
        handle: Win32WindowHandle,
    }

    impl HasWindowHandle for InertParent {
        fn window_handle(&self) -> Result<BorrowedWindowHandle<'_>, HandleError> {
            Ok(unsafe { BorrowedWindowHandle::borrow_raw(RawWindowHandle::Win32(self.handle)) })
        }
    }

    fn hwnd_raw(hwnd: HWND) -> u64 {
        hwnd.0 as usize as u64
    }

    fn win32_handle(hwnd: HWND) -> Win32WindowHandle {
        Win32WindowHandle::new(NonZeroIsize::new(hwnd.0 as isize).expect("HWND must be nonzero"))
    }

    fn reparent_and_verify(webview: &wry::WebView, child: HWND, destination: HWND) {
        webview
            .reparent(destination.0 as isize)
            .expect("Wry WebViewExtWindows reparent");
        let (actual_parent, controller_parent) = unsafe {
            webview
                .controller()
                .SetParentWindow(destination)
                .expect("WebView2 controller SetParentWindow");
            let actual_parent = GetParent(child).expect("GetParent(child)");
            let mut controller_parent = HWND::default();
            webview
                .controller()
                .ParentWindow(&mut controller_parent)
                .expect("controller ParentWindow after reparent");
            (actual_parent, controller_parent)
        };
        assert_eq!(actual_parent, destination);
        assert_eq!(actual_parent, controller_parent);
    }

    unsafe {
        let parking = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            w!("STATIC"),
            w!("devmanager-e2e-parking"),
            WS_POPUP | WS_DISABLED,
            -32_000,
            -32_000,
            8,
            8,
            None,
            None,
            None,
            None,
        )
        .expect("create distinct parking HWND");
        let parent = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            w!("STATIC"),
            w!("devmanager-e2e-parent"),
            WS_POPUP | WS_CLIPCHILDREN | WS_DISABLED,
            -32_000,
            -32_000,
            320,
            200,
            None,
            None,
            None,
            None,
        )
        .expect("create inert parent HWND");
        assert!(IsWindow(Some(parking)).as_bool());
        assert!(IsWindow(Some(parent)).as_bool());
        assert_ne!(hwnd_raw(parent), hwnd_raw(parking));

        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/browser-site/index.html");
        let bounds = Rect {
            position: LogicalPosition::new(0.0, 0.0).into(),
            size: LogicalSize::new(320.0, 200.0).into(),
        };
        let parent_window = InertParent {
            handle: win32_handle(parent),
        };
        let webview = if fixture.is_file() {
            let url = format!(
                "file:///{}",
                fixture.display().to_string().replace('\\', "/")
            );
            WebViewBuilder::new()
                .with_url(url)
                .with_bounds(bounds)
                .build_as_child(&parent_window)
        } else {
            WebViewBuilder::new()
                .with_html("<!doctype html><html><body>e2e</body></html>")
                .with_bounds(bounds)
                .build_as_child(&parent_window)
        }
        .expect("Wry/WebView2 child build");

        let child_raw = webview
            .id()
            .parse::<u64>()
            .expect("WebView::id is the container HWND");
        let child = HWND(child_raw as usize as *mut _);
        assert!(
            IsWindow(Some(child)).as_bool(),
            "WebView::id child HWND must be live after build"
        );
        assert_ne!(
            child_raw,
            hwnd_raw(parking),
            "child HWND must be distinct from parking"
        );
        assert_ne!(
            child_raw,
            hwnd_raw(parent),
            "child HWND must be distinct from parent"
        );

        let mut controller_parent = HWND::default();
        webview
            .controller()
            .ParentWindow(&mut controller_parent)
            .expect("controller ParentWindow");
        let actual_parent = GetParent(child).expect("GetParent(child)");
        assert_eq!(
            actual_parent, controller_parent,
            "GetParent must match ICoreWebView2Controller.ParentWindow"
        );
        assert_eq!(actual_parent, parent);

        reparent_and_verify(&webview, child, parking);
        reparent_and_verify(&webview, child, parent);

        drop(webview);
        assert!(
            !IsWindow(Some(child)).as_bool(),
            "child HWND must be gone after WebView drop"
        );
        assert!(
            IsWindow(Some(parking)).as_bool(),
            "parking HWND stays until the harness destroys it"
        );
        let _ = DestroyWindow(parent);
        let _ = DestroyWindow(parking);
    }
}
