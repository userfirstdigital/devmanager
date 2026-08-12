//! Task cockpit surfaces owned by the native GPUI shell.

pub mod timeline;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeCockpitMount {
    HoldMissingShell,
}

pub const NATIVE_COCKPIT_MOUNT: NativeCockpitMount = NativeCockpitMount::HoldMissingShell;
