//! X11 operations for host-owned WebKit child windows, including XWayland.
//! Window IDs enter this module only from retained GPUI/GDK windows or the
//! host's native-view registry. Observing an arbitrary ID never grants a lease.

use crate::protocol::{BrowserPhysicalBounds, BrowserWindowHandle};
use std::cell::RefCell;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConfigureWindowAux, ConnectionExt, CreateWindowAux, InputFocus, PropMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

thread_local! {
    static CONNECTION: RefCell<Option<RustConnection>> = const { RefCell::new(None) };
}

fn with_connection<T>(f: impl FnOnce(&RustConnection) -> Result<T, String>) -> Result<T, String> {
    CONNECTION.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(
                RustConnection::connect(None)
                    .map_err(|_| "X11 connection unavailable")?
                    .0,
            );
        }
        f(slot.as_ref().expect("connection installed"))
    })
}

fn xid(handle: &BrowserWindowHandle) -> Result<u32, String> {
    u32::try_from(handle.raw_value()).map_err(|_| "invalid X11 window identity".into())
}

pub(super) fn is_window(handle: &BrowserWindowHandle) -> Result<bool, String> {
    with_connection(|connection| {
        match connection
            .get_window_attributes(xid(handle)?)
            .map_err(|_| "X11 request failed")?
            .reply()
        {
            Ok(_) => Ok(true),
            Err(x11rb::errors::ReplyError::X11Error(error)) if error.error_code == 3 => Ok(false),
            Err(_) => Err("X11 window observation failed".into()),
        }
    })
}

pub(super) fn require_live(handle: &BrowserWindowHandle) -> Result<(), String> {
    if is_window(handle)? {
        Ok(())
    } else {
        Err("native browser window is gone".into())
    }
}

pub(super) fn parent(handle: &BrowserWindowHandle) -> Result<u64, String> {
    with_connection(|connection| {
        let tree = connection
            .query_tree(xid(handle)?)
            .map_err(|_| "X11 tree request failed")?
            .reply()
            .map_err(|_| "X11 parent observation failed")?;
        Ok(u64::from(tree.parent))
    })
}

pub(super) fn reparent(
    child: &BrowserWindowHandle,
    destination: &BrowserWindowHandle,
) -> Result<(), String> {
    require_live(child)?;
    require_live(destination)?;
    with_connection(|connection| {
        connection
            .reparent_window(xid(child)?, xid(destination)?, 0, 0)
            .map_err(|_| "X11 reparent request failed")?
            .check()
            .map_err(|_| "X11 reparent failed")?;
        connection
            .flush()
            .map_err(|_| "X11 flush failed".to_string())
    })?;
    if parent(child)? != destination.raw_value() {
        return Err("X11 child parent differs from its lease".into());
    }
    Ok(())
}

pub(super) fn set_bounds(
    child: &BrowserWindowHandle,
    bounds: BrowserPhysicalBounds,
) -> Result<(), String> {
    require_live(child)?;
    with_connection(|connection| {
        connection
            .configure_window(
                xid(child)?,
                &ConfigureWindowAux::new()
                    .x(bounds.x())
                    .y(bounds.y())
                    .width(bounds.width())
                    .height(bounds.height()),
            )
            .map_err(|_| "X11 geometry request failed")?
            .check()
            .map_err(|_| "X11 geometry failed")?;
        connection
            .flush()
            .map_err(|_| "X11 flush failed".to_string())
    })
}

pub(super) fn set_focus(child: &BrowserWindowHandle, focused: bool) -> Result<(), String> {
    require_live(child)?;
    if !focused {
        return Ok(());
    }
    with_connection(|connection| {
        connection
            .set_input_focus(InputFocus::PARENT, xid(child)?, x11rb::CURRENT_TIME)
            .map_err(|_| "X11 focus request failed")?
            .check()
            .map_err(|_| "X11 focus failed")?;
        connection
            .flush()
            .map_err(|_| "X11 flush failed".to_string())
    })
}

pub(super) fn create_parking(parent: isize) -> Result<u64, ()> {
    let parent = u32::try_from(parent).map_err(|_| ())?;
    with_connection(|connection| {
        // The parking owner must be the exact live GPUI process, not a window
        // supplied by a remote descriptor. The child registry fences later use.
        let atom = connection
            .intern_atom(false, b"_NET_WM_PID")
            .map_err(|_| "X11 PID atom request failed")?
            .reply()
            .map_err(|_| "X11 PID atom unavailable")?
            .atom;
        let pid = connection
            .get_property(false, parent, atom, AtomEnum::CARDINAL, 0, 1)
            .map_err(|_| "X11 PID request failed")?
            .reply()
            .map_err(|_| "X11 parent PID unavailable")?;
        if pid.value32().and_then(|mut values| values.next()) != Some(std::process::id()) {
            return Err("browser parent is not this application".into());
        }
        let root = connection
            .query_tree(parent)
            .map_err(|_| "X11 parent request failed")?
            .reply()
            .map_err(|_| "X11 parent unavailable")?
            .root;
        let window = connection.generate_id().map_err(|_| "X11 IDs exhausted")?;
        connection
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                -32000,
                -32000,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new().override_redirect(1),
            )
            .map_err(|_| "X11 parking request failed")?
            .check()
            .map_err(|_| "X11 parking creation failed")?;
        let publish = connection
            .change_property32(
                PropMode::REPLACE,
                window,
                atom,
                AtomEnum::CARDINAL,
                &[std::process::id()],
            )
            .map_err(|_| "X11 parking identity request failed".to_string())
            .and_then(|cookie| {
                cookie
                    .check()
                    .map_err(|_| "X11 parking identity failed".to_string())
            })
            .and_then(|()| {
                connection
                    .flush()
                    .map_err(|_| "X11 parking flush failed".to_string())
            });
        if let Err(error) = publish {
            let _ = connection.destroy_window(window);
            let _ = connection.flush();
            return Err(error);
        }
        Ok(u64::from(window))
    })
    .map_err(|_| ())
}

pub(super) fn destroy_parking(raw: u64) {
    let Ok(window) = u32::try_from(raw) else {
        return;
    };
    let _ = with_connection(|connection| {
        connection
            .destroy_window(window)
            .map_err(|_| "X11 parking destroy request failed")?
            .check()
            .map_err(|_| "X11 parking destroy failed")?;
        connection
            .flush()
            .map_err(|_| "X11 flush failed".to_string())
    });
}
