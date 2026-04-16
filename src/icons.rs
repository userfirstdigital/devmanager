use gpui::{px, rgb, svg, IntoElement, Styled};

pub const BOT: &str = "icons/bot.svg";
pub const FOLDER: &str = "icons/folder.svg";
pub const SPARKLES: &str = "icons/sparkles.svg";
pub const TERMINAL: &str = "icons/terminal.svg";
pub const CHEVRON_RIGHT: &str = "icons/chevron-right.svg";
pub const CHEVRON_LEFT: &str = "icons/chevron-left.svg";
pub const CHEVRON_DOWN: &str = "icons/chevron-down.svg";
pub const PLUS: &str = "icons/plus.svg";
pub const MORE_HORIZONTAL: &str = "icons/more-horizontal.svg";
pub const SQUARE: &str = "icons/square.svg";
pub const PLAY: &str = "icons/play.svg";
pub const REFRESH_CW: &str = "icons/refresh-cw.svg";
pub const X: &str = "icons/x.svg";
pub const SETTINGS: &str = "icons/settings.svg";
pub const SERVER: &str = "icons/server.svg";
pub const GLOBE: &str = "icons/globe.svg";
pub const ACTIVITY: &str = "icons/activity.svg";
pub const GIT_BRANCH: &str = "icons/git-branch.svg";
pub const CHEVRON_UP: &str = "icons/chevron-up.svg";
pub const FILE_TEXT: &str = "icons/file-text.svg";

pub fn app_icon(path: &'static str, size_px: f32, color: u32) -> impl IntoElement {
    svg().path(path).size(px(size_px)).text_color(rgb(color))
}
