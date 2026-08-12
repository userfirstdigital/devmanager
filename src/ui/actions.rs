//! GPUI key tokens for Task Cockpit presentation.
//!
//! These are not a second action catalog. Host mutations stay in
//! [`crate::client::action`]. Dock keypresses capture Task/binding/epochs
//! through [`crate::ui::task_cockpit::shell::TaskCockpitShell`].

use gpui::{App, KeyBinding};

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "dock.tool.changes")]
pub struct DockSelectChanges;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "dock.tool.files")]
pub struct DockSelectFiles;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "dock.tool.terminal")]
pub struct DockSelectTerminal;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "dock.tool.browser")]
pub struct DockSelectBrowser;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "dock.tool.services")]
pub struct DockSelectServices;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "dock.tool.artifacts")]
pub struct DockSelectArtifacts;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "dock.tool.review")]
pub struct DockSelectReview;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "dock.terminal.toggle")]
pub struct DockToggleRawTerminal;

pub fn register(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("alt-1", DockSelectChanges, None),
        KeyBinding::new("alt-2", DockSelectFiles, None),
        KeyBinding::new("alt-3", DockSelectTerminal, None),
        KeyBinding::new("alt-4", DockSelectBrowser, None),
        KeyBinding::new("alt-5", DockSelectServices, None),
        KeyBinding::new("alt-6", DockSelectArtifacts, None),
        KeyBinding::new("alt-7", DockSelectReview, None),
        KeyBinding::new("ctrl-`", DockToggleRawTerminal, None),
    ]);
}
