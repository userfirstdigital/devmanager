use gpui::{px, rgb, svg, IntoElement, Styled};

pub const BOT: &str = "icons/bot.svg";
pub const FOLDER: &str = "icons/folder.svg";
pub const SPARKLES: &str = "icons/sparkles.svg";
pub const TERMINAL: &str = "icons/terminal.svg";

pub fn app_icon(path: &'static str, size_px: f32, color: u32) -> impl IntoElement {
    svg().path(path).size(px(size_px)).text_color(rgb(color))
}
