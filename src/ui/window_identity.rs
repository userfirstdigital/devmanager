//! Original DevManager artwork and desktop identity for every native window.

pub(super) fn init(cx: &mut gpui::App) {
    cx.observe_new::<gpui_component::Root>(|_, window, _| {
        if let Some(window) = window {
            apply(window);
        }
    })
    .detach();
}

fn apply(window: &mut gpui::Window) {
    // Matches the installed desktop file on both X11 and Wayland.
    window.set_app_id("devmanager");

    #[cfg(any(target_os = "linux", windows))]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Ok(handle) = window.window_handle() else {
            return;
        };
        match handle.as_raw() {
            #[cfg(target_os = "linux")]
            RawWindowHandle::Xcb(handle) => set_x11_icon(handle.window.get()),
            #[cfg(target_os = "linux")]
            RawWindowHandle::Xlib(handle) => set_x11_icon(handle.window as u32),
            #[cfg(windows)]
            RawWindowHandle::Win32(handle) => {
                super::native_shell::apply_embedded_window_icons(windows::Win32::Foundation::HWND(
                    handle.hwnd.get() as *mut _,
                ));
            }
            _ => {}
        }
    }
}

#[cfg(target_os = "linux")]
fn set_x11_icon(window: u32) {
    use std::sync::OnceLock;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, PropMode};
    use x11rb::wrapper::ConnectionExt as _;

    // Use the unchanged 0.4.1 PNGs. Small and high-DPI taskbar sizes fit in one
    // ordinary X11 request, without depending on BIG-REQUESTS support.
    static ICON: OnceLock<Vec<u32>> = OnceLock::new();
    let pixels = ICON.get_or_init(|| {
        let mut pixels = Vec::new();
        for png in [
            include_bytes!("../../packaging/icons/devmanager-32.png").as_slice(),
            include_bytes!("../../packaging/icons/devmanager-128.png").as_slice(),
        ] {
            let image = image::load_from_memory(png)
                .expect("embedded DevManager icon is a valid PNG")
                .into_rgba8();
            pixels.extend([image.width(), image.height()]);
            pixels.extend(image.pixels().map(|pixel| {
                let [r, g, b, a] = pixel.0;
                u32::from_be_bytes([a, r, g, b])
            }));
        }
        pixels
    });
    let result = (|| -> gpui::Result<()> {
        let (connection, _) = x11rb::connect(None)?;
        let icon = connection
            .intern_atom(false, b"_NET_WM_ICON")?
            .reply()?
            .atom;
        connection
            .change_property32(PropMode::REPLACE, window, icon, AtomEnum::CARDINAL, pixels)?
            .check()?;
        connection.flush()?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("Could not attach DevManager window icon: {error}");
    }
}
