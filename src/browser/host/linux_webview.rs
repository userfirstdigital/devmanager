//! WebKitGTK callbacks used by the shared Wry host.

use super::*;
use gtk::prelude::*;
use javascriptcore::ValueExt;
use std::io::Write;
use webkit2gtk::{FileChooserRequestExt, PermissionRequestExt, WebViewExt};
use wry::WebViewExtUnix;

// Each callback is admitted under the retained view before WebKit can own it.
// Callbacks hold a weak registry reference, so they cannot keep the view alive.
// Drop cancels native requests before Wry destroys its child window.
struct PendingCall {
    cancel: gtk::gio::Cancellable,
    deadline: Instant,
}

pub(super) struct WebView {
    native: wry::WebView,
    calls: Rc<std::cell::RefCell<HashMap<u64, PendingCall>>>,
    next_call: Cell<u64>,
    upload: Rc<std::cell::RefCell<Option<NativeUpload>>>,
}

struct NativeUpload {
    id: String,
    files: Vec<String>,
    deadline: Instant,
}

struct UploadLease {
    id: String,
    slot: Rc<std::cell::RefCell<Option<NativeUpload>>>,
}
impl Drop for UploadLease {
    fn drop(&mut self) {
        if self
            .slot
            .borrow()
            .as_ref()
            .is_some_and(|upload| upload.id == self.id)
        {
            self.slot.borrow_mut().take();
        }
    }
}

impl std::ops::Deref for WebView {
    type Target = wry::WebView;
    fn deref(&self) -> &Self::Target {
        &self.native
    }
}

struct NativeCall {
    id: u64,
    cancel: gtk::gio::Cancellable,
    calls: std::rc::Weak<std::cell::RefCell<HashMap<u64, PendingCall>>>,
}

impl Drop for NativeCall {
    fn drop(&mut self) {
        if let Some(calls) = self.calls.upgrade() {
            calls.borrow_mut().remove(&self.id);
        }
    }
}

impl WebView {
    pub(super) fn show_at_bounds(&self, bounds: Rect) -> wry::Result<()> {
        // GTK ignores allocation of a hidden widget. Showing after allocating
        // can restore its natural content size and leave most of the dock black.
        self.native.set_visible(true)?;
        self.native.set_bounds(bounds)
    }

    pub(super) fn new(native: wry::WebView) -> Self {
        let upload: Rc<std::cell::RefCell<Option<NativeUpload>>> = Rc::default();
        let slot = Rc::downgrade(&upload);
        native
            .webview()
            .connect_run_file_chooser(move |_, request| {
                let Some(slot) = slot.upgrade() else {
                    request.cancel();
                    return true;
                };
                let Some(upload) = slot.borrow_mut().take() else {
                    return false;
                };
                if upload.deadline <= Instant::now()
                    || (!request.selects_multiple() && upload.files.len() > 1)
                {
                    request.cancel();
                } else {
                    request
                        .select_files(&upload.files.iter().map(String::as_str).collect::<Vec<_>>());
                }
                true
            });
        let slot = Rc::downgrade(&upload);
        native.webview().connect_load_changed(move |_, event| {
            if event == webkit2gtk::LoadEvent::Started {
                if let Some(slot) = slot.upgrade() {
                    slot.borrow_mut().take();
                }
            }
        });
        Self {
            native,
            calls: Rc::default(),
            next_call: Cell::new(0),
            upload,
        }
    }

    fn admit_call(&self) -> Result<NativeCall, ()> {
        self.expire_async_calls();
        if self.calls.borrow().len() >= 128 {
            return Err(());
        }
        let id = self.next_call.get().checked_add(1).ok_or(())?;
        self.next_call.set(id);
        let cancel = gtk::gio::Cancellable::new();
        self.calls.borrow_mut().insert(
            id,
            PendingCall {
                cancel: cancel.clone(),
                deadline: Instant::now() + Duration::from_secs(65),
            },
        );
        Ok(NativeCall {
            id,
            cancel,
            calls: Rc::downgrade(&self.calls),
        })
    }

    pub(super) fn cancel_upload(&self) {
        self.upload.borrow_mut().take();
    }

    pub(super) fn expire_async_calls(&self) {
        let now = Instant::now();
        if self
            .upload
            .borrow()
            .as_ref()
            .is_some_and(|upload| upload.deadline <= now)
        {
            self.upload.borrow_mut().take();
        }
        let canceled: Vec<_> = self
            .calls
            .borrow()
            .values()
            .filter(|call| call.deadline <= now)
            .map(|call| call.cancel.clone())
            .collect();
        for cancel in canceled {
            cancel.cancel();
        }
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        self.upload.borrow_mut().take();
        let calls = std::mem::take(&mut *self.calls.borrow_mut());
        for (_, call) in calls {
            call.cancel.cancel();
        }
    }
}

pub(super) fn initialize_webview_runtime() -> Result<String, ()> {
    if gtk::is_initialized() && !gtk::is_initialized_main_thread() {
        return Err(());
    }
    if !gtk::is_initialized() {
        gtk::gdk::set_allowed_backends("x11");
        gtk::init().map_err(|_| ())?;
    }
    let display = gtk::gdk::Display::default().ok_or(())?;
    if !display.is::<gdkx11::X11Display>() {
        return Err(());
    }
    wry::webview_version().map_err(|_| ())
}

pub(super) fn pump_linux_webview_events() {
    if !gtk::is_initialized_main_thread() {
        return;
    }
    let context = gtk::glib::MainContext::default();
    let deadline = Instant::now() + Duration::from_millis(2);
    for _ in 0..64 {
        if Instant::now() >= deadline || !context.pending() {
            break;
        }
        context.iteration(false);
    }
}

pub(super) fn current_process_creation_time_100ns() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let ticks = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse::<u64>()
        .ok()?;
    let frequency = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
    ticks
        .checked_mul(10_000_000)?
        .checked_div(frequency)
        .filter(|time| *time != 0)
}

pub(super) fn create_host_owned_parking_hwnd(parent: isize) -> Result<u64, ()> {
    super::super::linux_window::create_parking(parent)
}

pub(super) fn destroy_host_owned_parking_hwnd(raw: u64) {
    super::super::linux_window::destroy_parking(raw);
}

pub(super) fn child_hwnd_from_webview(
    view: &WebView,
) -> Result<BrowserWindowHandle, BrowserNativeViewError> {
    // Observe the GDK window actually retained by this WebKit widget. Wry's
    // arbitrary string ID is not an X11 window ID and never becomes authority.
    let widget = view.webview();
    let top = widget
        .toplevel()
        .ok_or(BrowserNativeViewError::LiveWryObservationUnavailable)?;
    let window = top
        .window()
        .ok_or(BrowserNativeViewError::LiveWryObservationUnavailable)?;
    let x11 = window
        .downcast::<gdkx11::X11Window>()
        .map_err(|_| BrowserNativeViewError::LiveWryObservationUnavailable)?;
    let child = BrowserWindowHandle::from_raw(x11.xid())
        .map_err(|_| BrowserNativeViewError::LiveWryObservationUnavailable)?;
    super::super::linux_window::require_live(&child)
        .map_err(|_| BrowserNativeViewError::LiveWryObservationUnavailable)?;
    Ok(child)
}

pub(super) fn attach_navigation_error_handler(
    view: &WebView,
    sender: Sender<BrowserHostEvent>,
    workspace_key: BrowserWorkspaceKey,
    tab_id: String,
) {
    view.webview().connect_load_failed(move |_, _, _, error| {
        if !error.matches(webkit2gtk::NetworkError::Cancelled) {
            // A typed terminal load fact survives secret containment without
            // carrying the failed URL or a WebKit error string.
            let _ = sender.send(BrowserHostEvent::PageLoad {
                workspace_key: workspace_key.clone(),
                tab_id: tab_id.clone(),
                state: BrowserPageLoadState::Failed,
                url: String::new(),
            });
        }
        // The document containment handler remains responsible for suppressing
        // WebKit's alternate error document. Never expose a failing secret URL.
        false
    });
}

pub(super) fn attach_document_lifecycle_handlers(
    view: &WebView,
    state: Arc<BrowserDocumentSecretState>,
) -> Result<(), BrowserError> {
    let navigation = Rc::new(Cell::new(1u64));
    let failed = Rc::new(Cell::new(false));
    let failed_flag = failed.clone();
    let failed_state = state.clone();
    view.webview().connect_load_failed(move |_, _, _, _| {
        failed_flag.set(true);
        failed_state.mark_failed();
        // Do not let WebKit's alternate error document masquerade as a fresh
        // successful navigation that clears secret containment.
        true
    });
    view.webview().connect_load_changed(move |_, event| {
        let result = match event {
            webkit2gtk::LoadEvent::Started => {
                failed.set(false);
                match navigation.get().checked_add(1) {
                    Some(next) => {
                        navigation.set(next);
                        Ok(())
                    }
                    None => Err(BrowserDocumentSecretGenerationError::GenerationExhausted),
                }
            }
            webkit2gtk::LoadEvent::Committed => {
                state.content_loading(navigation.get(), failed.get())
            }
            webkit2gtk::LoadEvent::Finished => {
                state.navigation_completed(navigation.get(), !failed.get())
            }
            _ => Ok(()),
        };
        if result.is_err() {
            state.mark_failed();
        }
    });
    Ok(())
}

pub(super) fn attach_permission_handler(
    view: &WebView,
    sender: Sender<BrowserHostEvent>,
    state: Arc<BrowserDocumentSecretState>,
    workspace_key: BrowserWorkspaceKey,
    tab_id: String,
) -> Result<(), BrowserError> {
    view.webview()
        .connect_permission_request(move |widget, request| {
            if state.is_tainted() {
                request.deny();
                return true;
            }
            let permission = if request.is::<webkit2gtk::UserMediaPermissionRequest>() {
                "camera or microphone"
            } else if request.is::<webkit2gtk::GeolocationPermissionRequest>() {
                "location"
            } else if request.is::<webkit2gtk::NotificationPermissionRequest>() {
                "notifications"
            } else if request.is::<webkit2gtk::PointerLockPermissionRequest>() {
                "mouse pointer control"
            } else if request.is::<webkit2gtk::DeviceInfoPermissionRequest>() {
                "media device information"
            } else if request.is::<webkit2gtk::MediaKeySystemPermissionRequest>() {
                "protected media playback"
            } else {
                request.deny();
                return true;
            };
            let Some(origin) = widget
                .uri()
                .as_deref()
                .and_then(browser_page_origin_from_url)
            else {
                request.deny();
                return true;
            };
            let Ok(generation) = state.document_generation() else {
                request.deny();
                return true;
            };
            let was_visible = widget.is_visible();
            widget.hide();
            let approved = MessageDialog::new()
                .set_level(MessageLevel::Warning)
                .set_title("Browser permission")
                .set_description(format!("Allow {origin} to access {permission}?"))
                .set_buttons(MessageButtons::YesNo)
                .show()
                == MessageDialogResult::Yes;
            let approved = approved
                && !state.is_tainted()
                && state.document_generation() == Ok(generation)
                && widget
                    .uri()
                    .as_deref()
                    .and_then(browser_page_origin_from_url)
                    .as_deref()
                    == Some(origin.as_str());
            if approved {
                request.allow();
            } else {
                request.deny();
            }
            if was_visible {
                widget.show();
            }
            let _ = sender.send(BrowserHostEvent::Diagnostic {
                workspace_key: workspace_key.clone(),
                tab_id: tab_id.clone(),
                level: BrowserDiagnosticLevel::Info,
                message: format!(
                    "{} browser permission {permission}",
                    if approved { "Approved" } else { "Denied" }
                ),
            });
            true
        });
    Ok(())
}

pub(super) trait BrowserScriptEvaluation {
    fn evaluate_browser_script_with_callback(
        &self,
        script: &str,
        callback: impl Fn(String) + Send + 'static,
    ) -> wry::Result<()>;
}

impl BrowserScriptEvaluation for WebView {
    fn evaluate_browser_script_with_callback(
        &self,
        script: &str,
        callback: impl Fn(String) + Send + 'static,
    ) -> wry::Result<()> {
        self.evaluate_local_script(script, callback);
        Ok(())
    }
}

impl WebView {
    fn evaluate_local_script(&self, script: &str, callback: impl FnOnce(String) + 'static) {
        // Wry's legacy run_javascript callback serializes a Promise as `{}`.
        // Await through WebKit's native async API so workflow completion still
        // means the exact operation finished, including cleanup acknowledgments.
        let call = match self.admit_call() {
            Ok(call) => call,
            Err(()) => {
                callback("null".into());
                return;
            }
        };
        let cancel = call.cancel.clone();
        self.webview().call_async_javascript_function(
            &format!("return await ({script});"),
            None,
            None,
            None,
            Some(&cancel),
            move |result| {
                let _call = call;
                let value = result
                    .ok()
                    .and_then(|value| value.to_json(0))
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "null".into());
                callback(value);
            },
        );
    }
}

struct BoundedPng(Vec<u8>);
impl Write for BoundedPng {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let limit = BrowserResourceLimits::default().max_resource_bytes as usize;
        if bytes.len() > limit.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other(
                "browser screenshot exceeds its resource limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn capture_linux_webview(
    view: &WebView,
    full_page: bool,
    callback: impl FnOnce(Result<String, String>) + 'static,
) -> Result<(), BrowserError> {
    let call = view.admit_call().map_err(|()| BrowserError::Interrupted)?;
    let cancel = call.cancel.clone();
    view.webview().snapshot(
        if full_page {
            webkit2gtk::SnapshotRegion::FullDocument
        } else {
            webkit2gtk::SnapshotRegion::Visible
        },
        webkit2gtk::SnapshotOptions::NONE,
        Some(&cancel),
        move |result| {
            let _call = call;
            let result = result
                .map_err(|_| "WebKit screenshot failed".to_string())
                .and_then(|surface| {
                    let mut output = BoundedPng(Vec::new());
                    surface
                        .write_to_png(&mut output)
                        .map_err(|_| "WebKit screenshot encoding failed".to_string())?;
                    Ok(
                        json!({"data": base64::engine::general_purpose::STANDARD.encode(output.0)})
                            .to_string(),
                    )
                });
            callback(result);
        },
    );
    Ok(())
}

impl BrowserWebViewHost {
    pub(super) fn reparent_wry_view(
        view: &WebView,
        destination: &BrowserWindowHandle,
    ) -> Result<(), BrowserNativeViewError> {
        let child = child_hwnd_from_webview(view)?;
        super::super::linux_window::reparent(&child, destination)
            .map_err(|_| BrowserNativeViewError::LiveWryObservationUnavailable)?;
        if child_hwnd_from_webview(view)? != child {
            return Err(BrowserNativeViewError::LiveWryObservationUnavailable);
        }
        Ok(())
    }

    pub(super) fn start_linux_upload(
        &self,
        target: &BrowserOperationTarget,
        operation_id: &str,
        token: &str,
        paths: &[PathBuf],
    ) -> Result<(), BrowserError> {
        let view = self.view(&target.workspace_key, &target.tab_id)?;
        if view.upload.borrow().is_some() {
            return Err(BrowserError::Interrupted);
        }
        let files = paths
            .iter()
            .map(|path| {
                path.to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| BrowserError::InvalidInvocation {
                        field: "paths".into(),
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let names = paths
            .iter()
            .map(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| BrowserError::InvalidInvocation {
                        field: "paths".into(),
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let selector = serde_json::to_string(&format!("[data-devmanager-upload=\"{token}\"]"))
            .map_err(|_| BrowserError::Interrupted)?;
        let names = serde_json::to_string(&names).map_err(|_| BrowserError::Interrupted)?;
        // Arm only this view, for the one marked input and a bounded interval.
        // Native selection preserves file bytes and does not load them into the UI.
        *view.upload.borrow_mut() = Some(NativeUpload {
            id: operation_id.into(),
            files,
            deadline: Instant::now() + Duration::from_secs(3),
        });
        let lease = UploadLease {
            id: operation_id.into(),
            slot: view.upload.clone(),
        };
        let script = format!(
            r#"(async () => {{
            const input = document.querySelector({selector});
            const names = {names};
            if (!(input instanceof HTMLInputElement) || input.type !== 'file' || (!input.multiple && names.length > 1)) return false;
            return await new Promise(resolve => {{
                let timer;
                const finish = ok => {{ clearTimeout(timer); input.removeEventListener('change', changed); input.removeAttribute('data-devmanager-upload'); resolve(ok); }};
                const changed = () => finish(input.files.length === names.length && names.every((name, i) => input.files[i].name === name));
                input.addEventListener('change', changed, {{once:true}});
                timer = setTimeout(() => finish(false), 2500);
                try {{ input.click(); }} catch (_) {{ finish(false); }}
            }});
        }})()"#
        );
        let sender = self.async_sender.clone();
        let target = target.clone();
        let operation_id = operation_id.to_string();
        // The GTK callback is local; the lease contains no cross-thread state.
        view.evaluate_local_script(&script, move |raw| {
            drop(lease);
            let result = if serde_json::from_str::<Value>(&raw)
                .ok()
                .and_then(|v| v.as_bool())
                == Some(true)
            {
                Ok("{}".into())
            } else {
                Err("Browser file selection did not complete".into())
            };
            let _ = sender.send(BrowserAsyncCompletion {
                target,
                operation_id,
                result,
                repair_highlight_authority: None,
                repair_highlight_document_generation: None,
                repair_highlight_rollback: false,
                repair_cleanup: None,
            });
        });
        Ok(())
    }

    pub(super) fn start_cdp(
        &self,
        target: &BrowserOperationTarget,
        operation_id: &str,
        method: &str,
        params: &Value,
    ) -> Result<(), BrowserError> {
        if method != "Page.captureScreenshot" {
            return Err(BrowserError::InvalidInvocation {
                field: "cdp".into(),
            });
        }
        let sender = self.async_sender.clone();
        let target = target.clone();
        let operation_id = operation_id.to_string();
        let full_page = params
            .get("captureBeyondViewport")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        capture_linux_webview(
            self.view(&target.workspace_key, &target.tab_id)?,
            full_page,
            move |result| {
                let _ = sender.send(BrowserAsyncCompletion {
                    target,
                    operation_id,
                    result,
                    repair_highlight_authority: None,
                    repair_highlight_document_generation: None,
                    repair_highlight_rollback: false,
                    repair_cleanup: None,
                });
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        AtomEnum, ConnectionExt, CreateWindowAux, PropMode, WindowClass,
    };
    use x11rb::wrapper::ConnectionExt as _;

    fn until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            pump_linux_webview_events();
            if predicate() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "WebKit callback exceeded the live-test deadline"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    struct Parent {
        connection: x11rb::rust_connection::RustConnection,
        window: u32,
    }
    impl Parent {
        fn new() -> Self {
            let (connection, screen) =
                x11rb::rust_connection::RustConnection::connect(None).unwrap();
            let root = connection.setup().roots[screen].root;
            let window = connection.generate_id().unwrap();
            connection
                .create_window(
                    x11rb::COPY_DEPTH_FROM_PARENT,
                    window,
                    root,
                    20,
                    20,
                    800,
                    600,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    x11rb::COPY_FROM_PARENT,
                    &CreateWindowAux::new(),
                )
                .unwrap()
                .check()
                .unwrap();
            let pid = connection
                .intern_atom(false, b"_NET_WM_PID")
                .unwrap()
                .reply()
                .unwrap()
                .atom;
            connection
                .change_property32(
                    PropMode::REPLACE,
                    window,
                    pid,
                    AtomEnum::CARDINAL,
                    &[std::process::id()],
                )
                .unwrap()
                .check()
                .unwrap();
            connection.map_window(window).unwrap().check().unwrap();
            connection.flush().unwrap();
            Self { connection, window }
        }
    }
    impl HasWindowHandle for Parent {
        fn window_handle(&self) -> Result<BorrowedWindowHandle<'_>, HandleError> {
            let handle = raw_window_handle::XlibWindowHandle::new(u64::from(self.window));
            // SAFETY: this fixture owns the exact window until all child views drop.
            Ok(unsafe { BorrowedWindowHandle::borrow_raw(handle.into()) })
        }
    }
    impl Drop for Parent {
        fn drop(&mut self) {
            let _ = self.connection.destroy_window(self.window);
            let _ = self.connection.flush();
        }
    }

    #[test]
    #[ignore = "requires an X11/XWayland desktop; run alone in a fresh test harness with GDK_BACKEND=x11"]
    fn linux_webkit_native_lifecycle() {
        initialize_webview_runtime().expect("WebKitGTK runtime");
        let temp = tempfile::tempdir().unwrap();
        let parent = Parent::new();
        let mut context = WebContext::new(Some(temp.path().join("webkit")));
        let native = WebViewBuilder::new_with_web_context(&mut context)
            .with_html(r#"<!doctype html><title>Linux adapter acceptance</title><input type="file" multiple id="upload" data-devmanager-upload="live-upload"><p>WebKit native lifecycle</p><div style="background:rgb(16,185,129);width:160px;height:100px"></div>"#)
            .with_bounds(Rect { position: PhysicalPosition::new(0, 0).into(), size: PhysicalSize::new(800, 600).into() })
            .build_as_child(&parent).expect("real Wry child");
        let view = WebView::new(native);
        let state = Arc::new(BrowserDocumentSecretState::default());
        let key = BrowserWorkspaceKey::new("linux-native-test", "conversation").unwrap();
        let (errors_tx, errors_rx) = std::sync::mpsc::channel();
        attach_navigation_error_handler(&view, errors_tx, key.clone(), "live-tab".into());
        attach_document_lifecycle_handlers(&view, state.clone()).unwrap();
        let loaded = Rc::new(Cell::new(false));
        let loaded_callback = loaded.clone();
        view.webview().connect_load_changed(move |_, event| {
            if event == webkit2gtk::LoadEvent::Finished {
                loaded_callback.set(true);
            }
        });
        until(|| loaded.get());
        let child = child_hwnd_from_webview(&view).expect("live child observation");
        assert_eq!(
            super::super::super::linux_window::parent(&child).unwrap(),
            u64::from(parent.window)
        );
        let parking = create_host_owned_parking_hwnd(parent.window as isize).unwrap();
        let parking_handle = BrowserWindowHandle::from_raw(parking).unwrap();
        view.set_visible(false).unwrap();
        BrowserWebViewHost::reparent_wry_view(&view, &parking_handle).unwrap();
        assert_eq!(
            super::super::super::linux_window::parent(&child).unwrap(),
            parking
        );
        BrowserWebViewHost::reparent_wry_view(
            &view,
            &BrowserWindowHandle::from_raw(u64::from(parent.window)).unwrap(),
        )
        .unwrap();
        view.show_at_bounds(Rect {
            position: PhysicalPosition::new(0, 0).into(),
            size: PhysicalSize::new(800, 600).into(),
        })
        .unwrap();
        let result = Rc::new(std::cell::RefCell::new(None));
        let result_callback = result.clone();
        view.evaluate_local_script(
            "new Promise(resolve => setTimeout(() => resolve('awaited'), 30))",
            move |raw| {
                *result_callback.borrow_mut() = Some(raw);
            },
        );
        until(|| result.borrow().is_some());
        assert_eq!(result.borrow().as_deref(), Some("\"awaited\""));

        let target = BrowserOperationTarget::new(key.clone(), "live-tab").unwrap();
        let mut host = BrowserWebViewHost::unavailable("standalone engine adapter acceptance");
        host.views.insert(view_key(&key, "live-tab"), view);
        let file = temp.path().join("upload.txt");
        std::fs::write(&file, b"Linux native upload bytes").unwrap();
        host.start_linux_upload(&target, "live-upload-op", "live-upload", &[file])
            .unwrap();
        let mut completion = None;
        until(|| {
            completion = host.async_receiver.try_recv().ok();
            completion.is_some()
        });
        assert!(
            completion.unwrap().result.is_ok(),
            "native upload must finish on the marked input"
        );
        let view = host.view(&key, "live-tab").unwrap();
        let bytes = Rc::new(std::cell::RefCell::new(None));
        let bytes_callback = bytes.clone();
        view.evaluate_local_script(
            "document.querySelector('#upload').files[0].text()",
            move |raw| {
                *bytes_callback.borrow_mut() = Some(raw);
            },
        );
        until(|| bytes.borrow().is_some());
        assert_eq!(
            bytes.borrow().as_deref(),
            Some("\"Linux native upload bytes\"")
        );
        let screenshot = Rc::new(std::cell::RefCell::new(None));
        let screenshot_callback = screenshot.clone();
        capture_linux_webview(view, false, move |raw| {
            *screenshot_callback.borrow_mut() = Some(raw);
        })
        .unwrap();
        until(|| screenshot.borrow().is_some());
        let raw = screenshot.borrow_mut().take().unwrap().unwrap();
        let png: Value = serde_json::from_str(&raw).unwrap();
        let png = base64::engine::general_purpose::STANDARD
            .decode(png["data"].as_str().unwrap())
            .unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let pixels = image::load_from_memory(&png).unwrap().to_rgb8();
        assert!(pixels.width() >= 800 && pixels.height() >= 600);
        let painted = pixels
            .pixels()
            .filter(|pixel| pixel.0 == [16, 185, 129])
            .count();
        assert!(
            painted >= 10_000,
            "screenshot must contain the rendered page, not just a valid PNG header"
        );
        assert!(!state.is_tainted());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let unavailable = format!("http://{}/unavailable", listener.local_addr().unwrap());
        drop(listener);
        view.load_url(&unavailable).unwrap();
        let mut failed = None;
        until(|| {
            failed = errors_rx.try_recv().ok();
            failed.is_some()
        });
        assert!(matches!(
            contain_queued_host_event(failed.unwrap(), Some(true)),
            Some(BrowserHostEvent::PageLoad { state: BrowserPageLoadState::Failed, url, .. }) if url.is_empty()
        ));
        let canceled = Rc::new(Cell::new(false));
        let canceled_callback = canceled.clone();
        view.evaluate_local_script("new Promise(() => {})", move |_| {
            canceled_callback.set(true)
        });
        assert_eq!(view.calls.borrow().len(), 1);
        host.views.clear();
        until(|| canceled.get());
        assert!(!super::super::super::linux_window::is_window(&child).unwrap());
        destroy_host_owned_parking_hwnd(parking);
        assert!(!super::super::super::linux_window::is_window(&parking_handle).unwrap());
        drop(host);
        drop(context);
        pump_linux_webview_events();
    }
}
