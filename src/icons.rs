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
pub const TRASH: &str = "icons/trash-2.svg";
pub const CHECK: &str = "icons/check.svg";
pub const ARCHIVE: &str = "icons/archive.svg";
pub const SETTINGS: &str = "icons/settings.svg";
pub const SERVER: &str = "icons/server.svg";
pub const GLOBE: &str = "icons/globe.svg";
pub const ACTIVITY: &str = "icons/activity.svg";
pub const GIT_BRANCH: &str = "icons/git-branch.svg";
pub const CHEVRON_UP: &str = "icons/chevron-up.svg";
pub const FILE_TEXT: &str = "icons/file-text.svg";
pub const SEARCH: &str = "icons/search.svg";
pub const PANEL_RIGHT: &str = "icons/panel-right.svg";
pub const PROVIDER_CLAUDE: &str = "icons/provider-claude.svg";
pub const PROVIDER_CODEX: &str = "icons/provider-codex.svg";
pub const PROVIDER_CURSOR: &str = "icons/provider-cursor.svg";
pub const PROVIDER_OTHER: &str = "icons/provider-other.svg";

pub fn app_icon(path: &'static str, size_px: f32, color: u32) -> impl IntoElement {
    svg().path(path).size(px(size_px)).text_color(rgb(color))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two ways a mark may declare its paint. Matching the whole attribute
    /// keeps a bare `currentColor` mention elsewhere in the file from passing.
    const CURRENT_COLOR_FILL: &str = "fill=\"currentColor\"";
    const CURRENT_COLOR_STROKE: &str = "stroke=\"currentColor\"";

    /// Every double-quoted value of `attribute` in `text`, so a paint can be
    /// inspected rather than matched as a substring anywhere in the file.
    fn attribute_values<'a>(text: &'a str, attribute: &str) -> Vec<&'a str> {
        let needle = format!("{attribute}=\"");
        let mut values = Vec::new();
        let mut rest = text;
        while let Some(start) = rest.find(&needle) {
            let after = &rest[start + needle.len()..];
            let end = after
                .find('"')
                .unwrap_or_else(|| panic!("unterminated {attribute} attribute"));
            values.push(&after[..end]);
            rest = &after[end..];
        }
        values
    }

    /// The first XML comment's text, or `None` when there is no terminated one.
    fn origin_comment(text: &str) -> Option<&str> {
        let start = text.find("<!--")?;
        let end = text[start..].find("-->")? + start;
        Some(&text[start..end])
    }

    #[test]
    fn provider_marks_exist_and_are_small_monochrome_svgs() {
        for path in [
            PROVIDER_CLAUDE,
            PROVIDER_CODEX,
            PROVIDER_CURSOR,
            PROVIDER_OTHER,
        ] {
            let full = crate::assets::asset_path(path);
            let bytes = std::fs::read(&full).unwrap_or_else(|e| panic!("{full:?}: {e}"));
            assert!(bytes.len() < 2048, "{path} must stay under 2 KB");
            let text = String::from_utf8(bytes).expect("utf-8 svg");
            assert!(text.contains("<svg"), "{path} is not an svg");
            assert!(
                text.contains(CURRENT_COLOR_FILL) || text.contains(CURRENT_COLOR_STROKE),
                "{path} must paint with {CURRENT_COLOR_FILL} or {CURRENT_COLOR_STROKE}"
            );
            for attribute in ["fill", "stroke"] {
                for value in attribute_values(&text, attribute) {
                    assert!(
                        !value.contains('#'),
                        "{path}: {attribute}={value:?} hard-codes a colour; every paint \
                         must resolve through currentColor"
                    );
                }
            }
            let comment = origin_comment(&text)
                .unwrap_or_else(|| panic!("{path} must state its origin in a comment"));
            assert!(
                comment.contains("stand-in") || comment.contains("licence"),
                "{path} origin comment must name a stand-in or state a licence: {comment:?}"
            );
        }
    }
}
