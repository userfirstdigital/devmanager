pub mod session;
pub mod view;

use gpui::{font, Font, FontFallbacks};

pub fn terminal_font() -> Font {
    let primary_family = if cfg!(target_os = "windows") {
        "Cascadia Mono"
    } else if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "DejaVu Sans Mono"
    };
    let mut font = font(primary_family);
    font.fallbacks = Some(FontFallbacks::from_fonts(vec![
        "Cascadia Mono".to_string(),
        "Consolas".to_string(),
        "Menlo".to_string(),
        "Monaco".to_string(),
        "DejaVu Sans Mono".to_string(),
        "Liberation Mono".to_string(),
    ]));
    font
}
