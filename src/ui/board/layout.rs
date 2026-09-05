//! Width-dependent row decisions (spec 4.3): the count goes first, then the
//! segments. Pure so the rule is testable without a window.
//!
//! Every number here is read from the approved mockup
//! `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/03-board-rows-boxed-A.html`
//! (chosen option A) and `05-provider-mark-1.html` (chosen option 1), so the
//! painter never approximates a size from memory.

use super::model::BoardProgress;

/// Two lines: 6 top padding + 17.5 title line + 1 row gap + 14.7 meta line + 7
/// bottom padding, rounded to the spec's 46 px.
pub const BOARD_ROW_HEIGHT: f32 = 46.0;
/// Density "Compact": 2 px less padding per side.
pub const BOARD_ROW_HEIGHT_COMPACT: f32 = 42.0;
/// Done rows are one line: title and age only (spec 4.1).
pub const BOARD_DONE_ROW_HEIGHT: f32 = 28.0;
pub const BOARD_ROW_GAP: f32 = 3.0;
/// The board column a fresh profile opens at, and the width the row's
/// breakpoints are reasoned about at. Still resizable: it is only the default
/// `inbox_width`, clamped to
/// [`crate::ui::workspace_layout::INBOX_MIN`]..`INBOX_MAX` (220..560).
///
/// 236 was `01-composition-A.html`'s column and the number its option card
/// costed. 05 -- the chosen provider-mark composition, and the one that added
/// a mark to the meta line -- draws the same board 300 px wide, and at 236 the
/// real titles ("Build the Snake backend in C:/Code/userfirst/snake-game")
/// were ellipsed to a few words (fix wave 1, F10).
pub const BOARD_COLUMN_WIDTH: f32 = 300.0;
pub const BOARD_RAIL_WIDTH: f32 = 36.0;

/// The 1 px rule above and below each row (`border-top` / `border-bottom` on
/// `.a .r`). GPUI lays out border-box, so these two pixels are spent inside
/// [`BOARD_ROW_HEIGHT`] and the paddings below are the mockup's 6/7 less the
/// one pixel each rule takes.
pub const ROW_BORDER_WIDTH: f32 = 1.0;
/// `.r` padding in the mockup is `6px 10px 7px` around a border-less box; the
/// painter spends one of those pixels per side on the rule instead, so the
/// outer row measures exactly [`BOARD_ROW_HEIGHT`].
pub const ROW_PADDING_TOP: f32 = 5.0;
pub const ROW_PADDING_BOTTOM: f32 = 6.0;
pub const ROW_PADDING_X: f32 = 10.0;
/// Compact takes 2 px off the vertical padding per side.
pub const ROW_COMPACT_PADDING_DELTA: f32 = 2.0;
/// `.r` on a Done row keeps the horizontal padding but loses the second line.
pub const DONE_ROW_PADDING_TOP: f32 = 4.0;
pub const DONE_ROW_PADDING_BOTTOM: f32 = 5.0;

/// `.r::before`: the project stripe on the very left edge, full row height.
pub const ROW_STRIPE_WIDTH: f32 = 3.0;
/// A row whose task is OPEN as a panel widens the same stripe so the set of
/// open tasks is legible from the edge of the eye. The stripe is absolutely
/// positioned over the row, so this never reaches [`row_content_width`]: the
/// meta-line breakpoints stay a rule about the column, not about which rows
/// happen to be open.
pub const ROW_STRIPE_WIDTH_OPEN: f32 = 5.0;

/// The "open" marker: a small bordered chip at the right end of the title line
/// carrying the panel's ordinal, so a row and its panel share one number. The
/// active row inverts it to a solid light tag.
pub const ORDINAL_CHIP_FONT_SIZE: f32 = 10.0;
pub const ORDINAL_CHIP_RADIUS: f32 = 4.0;
pub const ORDINAL_CHIP_PADDING_X: f32 = 4.0;
/// `.r` grid is `8px 1fr auto` with an 8 px column gap: the dot cell, then the
/// title. The meta line starts at grid column 2, i.e. 8 + 8 px in.
pub const DOT_CELL_WIDTH: f32 = 8.0;
pub const DOT_CELL_GAP: f32 = 8.0;
pub const SECOND_LINE_INDENT: f32 = DOT_CELL_WIDTH + DOT_CELL_GAP;
/// `.r` grid `gap: 1px 8px` — the row gap between the two lines.
pub const LINE_GAP: f32 = 1.0;
/// `.dot` is 7 px; `.dot.you` adds a 3 px shadow spread, so the halo is 13 px.
pub const STATE_DOT_SIZE: f32 = 7.0;
pub const STATE_DOT_HALO_SIZE: f32 = 13.0;
pub const STATE_DOT_HALO_ALPHA: f32 = 0.18;
/// The mockup's needs-you and blocked row rules are the state colour at roughly
/// a third opacity over `surfaces.raised`; solving its two border colours per
/// channel gives 0.31/0.33/0.20 and 0.33/0.31/0.30.
pub const NEEDS_YOU_BORDER_ALPHA: f32 = 0.32;

/// `.t` / `.r` font-size, `.age` and `.m` font-size, `.segn` font-size.
///
/// The title is pinned at the spec's 12 rather than the mockup CSS's 12.5:
/// design language rule 2 sets titles at 12, and against the reference PNGs
/// the rows read a pixel large (fix wave 1, F12). The meta line's 10.5 is both
/// the CSS's and the spec's, so it is unchanged.
pub const TITLE_FONT_SIZE: f32 = 12.0;
pub const META_FONT_SIZE: f32 = 10.5;
pub const COUNT_FONT_SIZE: f32 = 10.0;
/// `font: 13px/1.4` — the mockup's single line-height ratio, pinned to whole
/// pixels so the two-line row adds up to the spec's 46 (6 + 17 + 1 + 15 + 7).
pub const LINE_HEIGHT_RATIO: f32 = 1.4;
pub const TITLE_LINE_HEIGHT: f32 = 17.0;
pub const META_LINE_HEIGHT: f32 = 15.0;
/// `.mk` is an 11 px monochrome provider mark at the end of the meta line.
pub const PROVIDER_MARK_SIZE: f32 = 11.0;

/// `.seg i` and `.seg { gap: 2px }`; `.m { gap: 6px }` separates the meta text,
/// the strip, the count and the mark.
pub const SEGMENT_WIDTH: f32 = 9.0;
pub const SEGMENT_HEIGHT: f32 = 4.0;
pub const SEGMENT_GAP: f32 = 2.0;
pub const SEGMENT_RADIUS: f32 = 1.5;
pub const META_GAP: f32 = 6.0;

/// The one-line top bar across the window. `.dm-top { height: 34px }` in
/// `01-composition-A.html`, which is the only source for this bar: the spec's
/// section 3 chooses composition A but gives the bar no geometry of its own,
/// so every number here is the mockup's and none of them is the spec's.
pub const TOP_BAR_HEIGHT: f32 = 34.0;
/// `.dm-top { gap: 14px; padding: 0 12px }`.
pub const TOP_BAR_GAP: f32 = 14.0;
pub const TOP_BAR_PADDING_X: f32 = 12.0;
/// `.dm-top .brand` sets a weight and a colour and no size at all, so the
/// brand is the bar's own 11.5 px in semibold. It
/// reads as the product name by weight, not by being a size nothing else is.
/// Kept equal to [`TOP_BAR_SCOPE_FONT_SIZE`] by the test beneath, so the two
/// literals cannot drift apart.
pub const TOP_BAR_BRAND_FONT_SIZE: f32 = 11.5;
/// `.dm-top { font-size: 11.5px }` -- the scope label inherits the bar's size.
pub const TOP_BAR_SCOPE_FONT_SIZE: f32 = 11.5;
/// `.dm-top .kbd { border: 1px solid; border-radius: 4px; padding: 1px 6px;
/// font-size: 10.5px }`.
pub const KBD_FONT_SIZE: f32 = 10.5;
pub const KBD_RADIUS: f32 = 4.0;
pub const KBD_PADDING_X: f32 = 6.0;
pub const KBD_PADDING_Y: f32 = 1.0;
/// The needs-you chip is a `kbd` chip repainted amber: composition C gives it
/// the attention amber for its text and a much darker olive for its border,
/// which is that same amber at roughly a third alpha over the mockup's ground.
/// Solving the two colours per channel gives 0.333/0.361/0.327; the spec pins
/// the shipped value at 0.35, a hair above the solved mean of 0.340.
pub const NEEDS_YOU_CHIP_BORDER_ALPHA: f32 = 0.35;
/// The settings glyph at the right end of the bar, the same 14 px the footer
/// strip's icons were.
pub const TOP_BAR_SETTINGS_ICON_SIZE: f32 = 14.0;
/// The connection chip's ceiling. `host_status_headline` is three " · "
/// segments of boot and connection truth and can run well past a hundred
/// characters; unbounded it would push the needs-you chip and the hints off
/// the right of the bar, which is the one thing the bar must never do. Wide
/// enough for the leading segment that states the connection, which is the
/// part a person reads.
pub const TOP_BAR_CONNECTION_MAX_WIDTH: f32 = 320.0;

/// `.hd { padding: 9px 10px 7px; gap: 8px }` with a 13 px title and an
/// 11.5 px `+ New` button in a 6 px-radius 1 px box.
pub const HEADER_PADDING_TOP: f32 = 9.0;
pub const HEADER_PADDING_BOTTOM: f32 = 7.0;
pub const HEADER_GAP: f32 = 8.0;
pub const HEADER_TITLE_FONT_SIZE: f32 = 13.0;
pub const HEADER_BUTTON_FONT_SIZE: f32 = 11.5;
pub const HEADER_BUTTON_PADDING_X: f32 = 8.0;
pub const HEADER_BUTTON_PADDING_Y: f32 = 2.0;
pub const HEADER_BUTTON_RADIUS: f32 = 6.0;
/// The `+ New` button's own text line, its 11.5 px at the mockup's 1.4 ratio
/// rounded to a whole pixel, and the 1 px box around it.
pub const HEADER_BUTTON_LINE_HEIGHT: f32 = 16.0;
pub const HEADER_BUTTON_BORDER: f32 = 1.0;
/// The board header's whole box: its 9/7 padding around the tallest control in
/// the row, which is the `+ New` button rather than the 13 px title.
///
/// A constant because the board's `...` menu drops from under this header, and
/// a menu placed off a number only the painter knows drifts the moment the
/// header changes. Asserted against its parts below.
pub const HEADER_HEIGHT: f32 = 38.0;

/// `.g { padding: 8px 10px 3px; gap: 6px; font-size: 10.5px }`.
pub const GROUP_LABEL_PADDING_TOP: f32 = 8.0;
pub const GROUP_LABEL_PADDING_BOTTOM: f32 = 3.0;
pub const GROUP_LABEL_GAP: f32 = 6.0;
pub const GROUP_LABEL_FONT_SIZE: f32 = 10.5;

/// Rail mode draws one group dot per group with its count beneath.
pub const RAIL_DOT_SIZE: f32 = 13.0;
pub const RAIL_COUNT_FONT_SIZE: f32 = 10.0;
pub const RAIL_GROUP_GAP: f32 = 10.0;
/// Between a rail group's dot and the count beneath it.
pub const RAIL_DOT_COUNT_GAP: f32 = 2.0;
pub const RAIL_PADDING_TOP: f32 = 10.0;

/// Below this the "3/5" count is dropped; below [`SEGMENTS_MIN_WIDTH`] the
/// strip goes too and the meta text keeps the whole line. Both are measured
/// against the row's CONTENT width -- see [`row_content_width`] -- not against
/// the column, or the clamp on the column width puts them out of reach.
pub const COUNT_MIN_WIDTH: f32 = 200.0;
pub const SEGMENTS_MIN_WIDTH: f32 = 160.0;

/// The width the meta line actually has to lay out in: the column less the
/// project stripe on the left edge and the row's horizontal padding on both
/// sides. [`row_layout`] is a rule about the content, so handing it the column
/// width overstates the space by 23 px at every width -- enough, at the
/// narrowest legal column, to keep painting a count that does not fit.
pub fn row_content_width(column_width_px: f32) -> f32 {
    (column_width_px - ROW_STRIPE_WIDTH - 2.0 * ROW_PADDING_X).max(0.0)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoardRowLayout {
    pub show_segments: bool,
    pub show_count: bool,
}

/// The count is the first thing to go, then the segments. A row with no plan
/// shows neither at any width: the strip means "there is a plan", so an empty
/// strip would be a lie rather than a smaller version of the truth.
///
/// `width_px` is the row's content width, i.e. [`row_content_width`] of the
/// column the row paints in.
pub fn row_layout(width_px: f32, progress: Option<BoardProgress>) -> BoardRowLayout {
    let Some(_) = progress else {
        return BoardRowLayout {
            show_segments: false,
            show_count: false,
        };
    };
    BoardRowLayout {
        show_segments: width_px >= SEGMENTS_MIN_WIDTH,
        show_count: width_px >= COUNT_MIN_WIDTH,
    }
}

/// The painted height of one row at the current density.
pub fn row_height(one_line: bool, compact: bool) -> f32 {
    if one_line {
        BOARD_DONE_ROW_HEIGHT
    } else if compact {
        BOARD_ROW_HEIGHT_COMPACT
    } else {
        BOARD_ROW_HEIGHT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::board::model::BoardProgress;

    #[test]
    fn wide_rows_show_segments_and_count_when_a_plan_exists() {
        let l = row_layout(
            236.0,
            Some(BoardProgress {
                completed: 1,
                total: 4,
            }),
        );
        assert!(l.show_segments && l.show_count);
    }

    #[test]
    fn narrow_rows_drop_the_count_first_then_the_segments() {
        let p = Some(BoardProgress {
            completed: 1,
            total: 4,
        });
        let at_199 = row_layout(199.0, p);
        assert!(at_199.show_segments && !at_199.show_count);
        let at_150 = row_layout(150.0, p);
        assert!(!at_150.show_segments && !at_150.show_count);
    }

    /// The painter is handed the COLUMN width, and the column is clamped to
    /// [`crate::ui::workspace_layout::INBOX_MIN`]..`INBOX_MAX` (220..560), so a
    /// breakpoint compared against the column can never fire: 220 is already
    /// above both thresholds. The rule is about the space the meta line has,
    /// which is the column less the stripe and the two paddings -- 197 px at
    /// the narrowest legal column, where the count must go and the strip must
    /// stay.
    #[test]
    fn the_breakpoints_are_reachable_at_the_narrowest_legal_column() {
        let p = Some(BoardProgress {
            completed: 1,
            total: 4,
        });
        let narrow = row_layout(row_content_width(crate::ui::workspace_layout::INBOX_MIN), p);
        assert!(
            narrow.show_segments,
            "the strip is the last thing to go and 197 px still has room for it"
        );
        assert!(
            !narrow.show_count,
            "the count must drop at the narrowest legal column, or the breakpoint is dead code"
        );
        let wide = row_layout(row_content_width(BOARD_COLUMN_WIDTH), p);
        assert!(
            wide.show_segments && wide.show_count,
            "the spec column width shows both"
        );
    }

    #[test]
    fn no_plan_means_nothing_regardless_of_width() {
        let l = row_layout(400.0, None);
        assert!(!l.show_segments && !l.show_count);
    }

    #[test]
    fn row_heights_follow_the_spec() {
        assert_eq!(BOARD_ROW_HEIGHT, 46.0);
        assert_eq!(BOARD_ROW_HEIGHT_COMPACT, 42.0);
        assert_eq!(BOARD_DONE_ROW_HEIGHT, 28.0, "done rows are one line");
    }

    #[test]
    fn row_height_picks_the_done_then_compact_then_comfortable_height() {
        assert_eq!(row_height(true, false), BOARD_DONE_ROW_HEIGHT);
        assert_eq!(row_height(true, true), BOARD_DONE_ROW_HEIGHT);
        assert_eq!(row_height(false, true), BOARD_ROW_HEIGHT_COMPACT);
        assert_eq!(row_height(false, false), BOARD_ROW_HEIGHT);
    }

    /// Every painted row is border-box, so its rules, paddings and line heights
    /// must add up to the spec height exactly. A later padding edit that no
    /// longer adds up fails here rather than as a 2 px overflow nobody
    /// attributes.
    #[test]
    fn each_row_height_is_exactly_its_rules_padding_and_lines() {
        let rules = 2.0 * ROW_BORDER_WIDTH;
        let two_line = rules
            + ROW_PADDING_TOP
            + TITLE_LINE_HEIGHT
            + LINE_GAP
            + META_LINE_HEIGHT
            + ROW_PADDING_BOTTOM;
        assert_eq!(two_line, BOARD_ROW_HEIGHT, "comfortable row");
        let compact = two_line - 2.0 * ROW_COMPACT_PADDING_DELTA;
        assert_eq!(compact, BOARD_ROW_HEIGHT_COMPACT, "compact row");
        let one_line = rules + DONE_ROW_PADDING_TOP + TITLE_LINE_HEIGHT + DONE_ROW_PADDING_BOTTOM;
        assert_eq!(one_line, BOARD_DONE_ROW_HEIGHT, "done row");
    }

    /// The whole-pixel line heights stay within half a pixel of the mockup's
    /// `1.4` ratio, so rounding them never becomes a silent type-size change.
    #[test]
    fn pinned_line_heights_match_the_mockup_ratio() {
        assert!((TITLE_LINE_HEIGHT - TITLE_FONT_SIZE * LINE_HEIGHT_RATIO).abs() <= 0.55);
        assert!((META_LINE_HEIGHT - META_FONT_SIZE * LINE_HEIGHT_RATIO).abs() <= 0.55);
    }

    /// The bar is the mockup's `.dm-top`, and that CSS is its only source:
    /// `height: 34px`, `font-size: 11.5px`, and a `.brand` rule that changes the
    /// weight and the colour but never the size. A bar pinned shorter, or a
    /// brand sized on its own, is a deviation from the approved composition
    /// rather than a reading of it.
    #[test]
    fn the_top_bar_is_the_composition_bar() {
        assert_eq!(TOP_BAR_HEIGHT, 34.0);
        assert_eq!(TOP_BAR_SCOPE_FONT_SIZE, 11.5);
        assert_eq!(
            TOP_BAR_BRAND_FONT_SIZE, TOP_BAR_SCOPE_FONT_SIZE,
            "the brand inherits the bar's font-size; only its weight differs"
        );
    }

    /// The header is border-box like every row, so its padding around its
    /// tallest control must add up to [`HEADER_HEIGHT`] exactly. The `+ New`
    /// button is that control, not the title -- a later change that makes the
    /// title taller fails here rather than silently leaving the board's menu
    /// hanging over the header it drops from.
    #[test]
    fn the_header_height_is_its_padding_around_the_tallest_control() {
        let button =
            2.0 * HEADER_BUTTON_BORDER + 2.0 * HEADER_BUTTON_PADDING_Y + HEADER_BUTTON_LINE_HEIGHT;
        assert_eq!(
            HEADER_PADDING_TOP + button + HEADER_PADDING_BOTTOM,
            HEADER_HEIGHT
        );
        assert!(
            button > HEADER_TITLE_FONT_SIZE * LINE_HEIGHT_RATIO,
            "the + New button sets the header height, not the title"
        );
        assert!(
            (HEADER_BUTTON_LINE_HEIGHT - HEADER_BUTTON_FONT_SIZE * LINE_HEIGHT_RATIO).abs() <= 0.55,
            "the button's line stays within half a pixel of the mockup ratio"
        );
    }

    /// The meta line starts under the title, not under the dot.
    #[test]
    fn the_second_line_starts_at_the_title_column() {
        assert_eq!(SECOND_LINE_INDENT, DOT_CELL_WIDTH + DOT_CELL_GAP);
        assert_eq!(SECOND_LINE_INDENT, 16.0);
    }
}
