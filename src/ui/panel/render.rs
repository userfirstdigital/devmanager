//! The panel chrome painter. Every colour comes from [`ThemeTokens`] or the
//! project palette; the amber and red states are the only saturated colours.
//!
//! Geometry and typography are copied from the approved mockup
//! `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/02-panel-chrome-2.html`
//! (chosen option 2: the status folded into the title row, so a panel spends
//! two rows of chrome rather than three and the stream gets the line back),
//! read together with `01-composition-A.png`, which is where the same chrome is
//! seen at the width it actually gets as one of eight.
//!
//! The numbers live in the constants below rather than inline, so the mockup
//! can be re-measured against them instead of against a painter's memory.

use std::rc::Rc;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, AnyElement, App, ElementId, FontWeight, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ParentElement, Pixels, Point, StatefulInteractiveElement, Styled,
    Window,
};
use sha2::{Digest, Sha256};

use crate::client::HostTaskKey;
use crate::ui::board::layout::{PROVIDER_MARK_SIZE, ROW_STRIPE_WIDTH};
use crate::ui::board::render::{ordinal_chip, segments_element};
use crate::ui::board::ProjectColourBook;
use crate::ui::panel::model::{
    status_layout, NeedsYou, PanelChrome, PrimaryAction, StatusLayout, StatusTone,
};
use crate::ui::task_workspace::PaneView;
use crate::ui::tokens::{Color, ThemeTokens};

/// `.hdr` in the mockup: 8 px of top padding over a 13 px/1.4 title line and a
/// 20 px action button, which measures 30 px including the button's border.
pub const TITLE_ROW_HEIGHT: f32 = 30.0;
/// `.tabs`: 6 px of top padding over an 11.5 px tab with 3/5 padding, sitting
/// on its own 1 px bottom rule.
///
/// Pinned at the spec's 28 rather than the 26 those paddings sum to: the tab is
/// a POINTER target as well as a label, and 26 px left the selected pill
/// looking squeezed against the rule under it at the width a panel gets as one
/// of eight (fix wave 1, F11). The two extra pixels go under the tab, between
/// its bottom padding and the rule.
pub const TAB_ROW_HEIGHT: f32 = 28.0;
/// A minimised panel is the title row alone, two pixels tighter because it has
/// no tab row beneath it to align to.
pub const MINIMISED_HEIGHT: f32 = 28.0;

/// `.hdr` / `.tabs` horizontal padding.
const ROW_PADDING_X: f32 = 10.0;
/// The project stripe is painted over the very left edge of the panel, so the
/// left padding is the mockup's 10 px plus the stripe. Without this the
/// provider mark sits under the stripe at the narrowest widths.
const ROW_PADDING_LEFT: f32 = ROW_PADDING_X + PANEL_STRIPE_WIDTH;
/// `.hdr { gap: 8px }`.
const TITLE_ROW_GAP: f32 = 8.0;
/// `.hdr .title { font-weight: 600 }`, at the redesign's title size.
///
/// The mockup CSS says 13 px, and 13 px is what shipped -- against the PNGs
/// the row then read one to two pixels large everywhere, because the mockup's
/// browser and GPUI do not measure "13 px" the same way. The spec's type scale
/// is the authority over the CSS literal (design language rule 2: 12 for
/// titles, 13 for the ONE heading a surface gets, and the panel title is not
/// that heading), so the constant is pinned at 12 (fix wave 1, F12).
const TITLE_FONT_SIZE: f32 = 12.0;
/// `.hdr .inline` -- the status folded into the title row, at the scale's
/// "secondary rows" size. 11.5 is the BODY size in rule 2; the status is a
/// secondary row beside a 12 px title, and at 11.5 it read as a second title.
const INLINE_STATUS_FONT_SIZE: f32 = 11.0;
/// `.st .s { gap: 5px }`.
const STATUS_GAP: f32 = 5.0;
/// The title floor at [`TIGHT_WIDTH`], where the row has nothing to spare.
///
/// A blocked panel at 280 px owes 195 px of controls and a 73 px status floor,
/// which is 268 of the 280, so 12 px is not chosen, it is what is left. See
/// [`title_floor`] for what happens above that width -- a fixed 12 px floor is
/// only honest at the one width that forces it, and would leave the title at
/// 12 px on a 470 px panel behind a long blocked cause.
const TITLE_MIN_WIDTH: f32 = 12.0;
/// The narrowest panel this chrome is built for, and the width at which the
/// title floor is at its minimum. Below the production pane minimum of 320 px
/// (`AllocationMetrics::production`), so the budget holds at every width a
/// panel is ever allocated.
const TIGHT_WIDTH: f32 = 280.0;
/// The share of every pixel above [`TIGHT_WIDTH`] that the title's floor
/// claims. Below 0.5 so the status text still gains room as the panel widens;
/// high enough that the title is legible well before the design width.
const TITLE_GROWTH: f32 = 0.4;

/// The width below which the title may never be squeezed, as a function of the
/// panel's own width.
///
/// The title is the panel's identity: the stripe says which project and the
/// mark says which provider, but only the title says which task. A fixed floor
/// cannot serve both ends of the range -- 12 px is all a 250 px panel has, and
/// 12 px on a 470 px panel is an anonymous panel:
///
/// ```text
///   width   title_floor   status_budget   controls + title floor + budget
///     280            12              73    195 +  12 +  73 = 280
///     300            20              85    195 +  20 +  85 = 300
///     370            48             127    195 +  48 + 127 = 370
///     470            88             187    195 +  88 + 187 = 470
/// ```
///
/// The row is exactly paid for at every width, which is what makes the title's
/// ellipsis land somewhere rather than being clipped flush against the status.
fn title_floor(width_px: f32) -> f32 {
    TITLE_MIN_WIDTH + TITLE_GROWTH * (width_px - TIGHT_WIDTH).max(0.0)
}
/// `.act { padding: 2px 9px; border-radius: 6px; font-size: 11.5px }`.
const ACTION_FONT_SIZE: f32 = 11.5;
const ACTION_PADDING_X: f32 = 9.0;
const ACTION_PADDING_Y: f32 = 2.0;
const ACTION_RADIUS: f32 = 6.0;
/// `.menu { font-size: 15px; padding: 0 3px }`.
const MENU_FONT_SIZE: f32 = 15.0;
const MENU_PADDING_X: f32 = 3.0;
/// `.ic { font-size: 12px }` -- the zoom affordance.
const ZOOM_ICON_FONT_SIZE: f32 = 12.0;
/// `.tabs { font-size: 11.5px; gap: 2px; padding: 6px 10px 0 }` and
/// `.tabs span { padding: 3px 9px 5px; border-radius: 6px 6px 0 0 }`.
const TAB_FONT_SIZE: f32 = 11.5;
const TAB_GAP: f32 = 2.0;
const TAB_PADDING_X: f32 = 9.0;
const TAB_PADDING_TOP: f32 = 3.0;
const TAB_PADDING_BOTTOM: f32 = 5.0;
const TAB_RADIUS: f32 = 6.0;
const TABS_PADDING_TOP: f32 = 6.0;
/// `.pane { border: 1px }`; a focused panel doubles it (the shell's current
/// pane frame already does, and the two must not disagree).
const PANEL_BORDER_WIDTH: f32 = 1.0;
const PANEL_FOCUS_BORDER_WIDTH: f32 = 2.0;
/// The same 3 px project stripe the board row carries, on the same edge.
const PANEL_STRIPE_WIDTH: f32 = ROW_STRIPE_WIDTH;
/// A panel that wants a person is bordered in its state colour rather than in
/// the neutral frame, with a fainter ring outside it so the panel reads as lit
/// rather than merely outlined.
///
/// The mockup's needs-you pane rule over the pane fill solves to roughly this
/// per channel. Named `PANEL_` rather than sharing the board's
/// `NEEDS_YOU_BORDER_ALPHA`: the board's row rule sits at a different alpha over
/// a different ground, and two constants of the same name in one crate is how a
/// painter ends up importing the wrong one.
const PANEL_NEEDS_YOU_BORDER_ALPHA: f32 = 0.45;
const NEEDS_YOU_GLOW_ALPHA: f32 = 0.25;
const NEEDS_YOU_GLOW_WIDTH: f32 = 1.0;

/// The zoom affordance's glyph box at [`ZOOM_ICON_FONT_SIZE`]. Zoomed it reads
/// "⤡ Esc" and is wider, but a zoomed panel owns the whole window, so the
/// budget below is written for the crowded case.
const ZOOM_AFFORDANCE_WIDTH: f32 = 12.0;
/// The primary button at its widest label ("Reopen", six characters at
/// [`ACTION_FONT_SIZE`]) plus its padding and its two border pixels.
const PRIMARY_BUTTON_MAX_WIDTH: f32 = 58.0;
/// The ordinal chip at two digits: the glyph pair at
/// `ORDINAL_CHIP_FONT_SIZE`, its padding on both sides and its 1 px rule.
/// A panel numbered above 99 cannot be supervised and does not get a chip at
/// all, so this is the real maximum rather than an estimate.
const ORDINAL_CHIP_MAX_WIDTH: f32 = 22.0;
/// The ⋯ glyph plus its padding: [`MENU_FONT_SIZE`] + 2 x [`MENU_PADDING_X`].
const MENU_GLYPH_WIDTH: f32 = MENU_FONT_SIZE + 2.0 * MENU_PADDING_X;
/// Everything in the title row whose width does not depend on the panel's:
///
/// ```text
///   13  ROW_PADDING_LEFT          (10 px padding + the 3 px stripe)
///   10  ROW_PADDING_X             (right padding)
///   11  PROVIDER_MARK_SIZE
///   22  ORDINAL_CHIP_MAX_WIDTH    (two digits, padding and border)
///   12  ZOOM_AFFORDANCE_WIDTH
///   58  PRIMARY_BUTTON_MAX_WIDTH
///   21  MENU_GLYPH_WIDTH          (15 + 2 x 3)
///   48  6 x TITLE_ROW_GAP         (seven children, six gaps)
///  ---
///  195
/// ```
///
/// This is what the status must yield to. Before this budget existed the status
/// was `flex_none` behind a fixed cap, so on a panel of roughly 260-370 px a
/// long doing-now string pushed Done and ⋯ off the right edge -- silently, with
/// no panic and nothing in any test to see it.
const CONTROLS_RESERVE: f32 = ROW_PADDING_LEFT
    + ROW_PADDING_X
    + PROVIDER_MARK_SIZE
    + ORDINAL_CHIP_MAX_WIDTH
    + ZOOM_AFFORDANCE_WIDTH
    + PRIMARY_BUTTON_MAX_WIDTH
    + MENU_GLYPH_WIDTH
    + 6.0 * TITLE_ROW_GAP;

/// Every pixel the status GROUP may occupy: what is left once the fixed
/// controls and the title's floor at this width have been paid, and never less
/// than the parts the status may not drop.
///
/// The group is `flex_none` behind this as a `max_w`, which is what makes "the
/// title never runs under the status" a fact of the arithmetic rather than a
/// hope about how the flex line resolves: by construction
/// `CONTROLS_RESERVE + title_floor(w) + status_budget(w) == w` at every width
/// down to [`TIGHT_WIDTH`], so the row is exactly paid for and the title's
/// ellipsis has somewhere to land.
fn status_budget(width_px: f32, blocked: bool) -> f32 {
    (width_px - CONTROLS_RESERVE - title_floor(width_px)).max(status_floor(blocked))
}

/// The ceiling on the status *text*: the group's budget less the parts of the
/// group that are present at every width.
///
/// This is what keeps the *title* on screen, by stopping a long doing-now line
/// or a 60-character blocked cause from claiming room the title needs. Same
/// shape as the board's `row_content_width`: a rule about the content, not
/// about the panel.
///
/// It used to be the budget itself, which under-counted the icon, the age and
/// a blocked panel's Retry -- 187 px of text plus a 40 px floor on a 470 px
/// panel is 227 px for a group that only has 187 to spend.
fn status_text_max_width(width_px: f32, blocked: bool) -> f32 {
    (status_budget(width_px, blocked) - status_floor(blocked)).max(0.0)
}

/// One status glyph at [`INLINE_STATUS_FONT_SIZE`]. The widest of the five is
/// the working triangle.
const STATUS_ICON_WIDTH: f32 = 10.0;
/// `format_age` is at most four characters ("59s", "23h", and days for a task
/// nobody has touched in a year), so this is its box at the status font size.
const STATUS_AGE_MAX_WIDTH: f32 = 25.0;
/// The five characters of "Retry".
const STATUS_RETRY_WIDTH: f32 = 28.0;

/// The width the status may never be squeezed below, because these parts are
/// present at every width and are `flex_none`: the state icon, the age, and on
/// a blocked panel the Retry link.
///
/// This exists because the previous cut capped the status *container* at a
/// derived maximum. A `max_w` on a container of `flex_none` children does not
/// make them yield -- it clips them mid-element -- so a blocked panel below
/// roughly 320 px silently lost the Retry affordance that the comment three
/// lines above the Retry element promises it keeps at every width. The floor
/// is the opposite instruction and the one that actually holds.
/// The icon is counted even for the states that have none (`PanelStatus::icon`
/// is `None` for idle): a floor that over-reserves by ten pixels only ever
/// leaves the status a little narrower than it could be, while one that
/// under-reserves is the bug this function exists for.
fn status_floor(blocked: bool) -> f32 {
    let base = STATUS_ICON_WIDTH + STATUS_GAP + STATUS_AGE_MAX_WIDTH;
    if blocked {
        base + STATUS_RETRY_WIDTH + STATUS_GAP
    } else {
        base
    }
}

/// What the shell does when the chrome is clicked or typed into. The painter
/// owns no state: it hands the panel's key back and the shell decides.
pub struct PanelHandlers {
    pub on_focus: Rc<dyn Fn(&HostTaskKey, &mut Window, &mut App)>,
    pub on_select_view: Rc<dyn Fn(&HostTaskKey, PaneView, &mut Window, &mut App)>,
    pub on_primary: Rc<dyn Fn(&HostTaskKey, PrimaryAction, &mut Window, &mut App)>,
    pub on_zoom: Rc<dyn Fn(&HostTaskKey, &mut Window, &mut App)>,
    /// The ⋯ menu opens at the pointer, so the shell is handed the position it
    /// has to anchor the popover to.
    pub on_menu: Rc<dyn Fn(&HostTaskKey, Point<Pixels>, &mut Window, &mut App)>,
    pub on_retry: Rc<dyn Fn(&HostTaskKey, &mut Window, &mut App)>,
    pub on_key: Rc<dyn Fn(&HostTaskKey, &KeyDownEvent, &mut Window, &mut App)>,
}

/// Hash host + task into the panel's element identity.
///
/// This is deliberately byte-for-byte the digest
/// `native_shell::stable_host_task_element_key(key, "pane")` already computes,
/// so the panel painter and the shell name the same element and the
/// accessibility tree keeps the ids it publishes today. That function is
/// private to the shell module, so this is a copy rather than a call; the test
/// below pins the byte layout so a change on either side is a failure rather
/// than a silent second identity.
fn panel_element_key(key: &HostTaskKey) -> u64 {
    let mut digest = Sha256::new();
    digest.update(b"native-host-task");
    digest.update([0]);
    digest.update(crate::ui::native_shell::host_identity_digest_bytes(
        &key.host,
    ));
    digest.update([0]);
    digest.update(key.task_id.as_bytes());
    digest.update([0]);
    digest.update(b"pane");
    let bytes: [u8; 8] = digest.finalize()[..8]
        .try_into()
        .expect("sha256 prefixes are always eight bytes");
    u64::from_le_bytes(bytes)
}

/// The panel's element id, built on [`panel_element_key`].
pub fn panel_element_id(key: &HostTaskKey) -> ElementId {
    ElementId::from(("devmanager-panel", panel_element_key(key)))
}

/// The state colour a needs-you panel is framed in, or `None` for a panel that
/// is merely working.
fn needs_you_colour(needs_you: Option<&NeedsYou>, tokens: ThemeTokens) -> Option<Color> {
    match needs_you {
        Some(NeedsYou::Question { .. }) | Some(NeedsYou::Permission { .. }) => {
            Some(tokens.status.attention)
        }
        Some(NeedsYou::Blocked { .. }) => Some(tokens.status.destructive),
        None => None,
    }
}

fn status_colour(tone: StatusTone, tokens: ThemeTokens) -> Color {
    match tone {
        // `.hdr .inline` in the mockup, one step above the tab row's grey.
        StatusTone::Neutral => tokens.text.secondary,
        StatusTone::Attention => tokens.status.attention,
        StatusTone::Blocked => tokens.status.destructive,
    }
}

/// The mockup's "·" between the status text, the age and the strip.
fn status_separator(tokens: ThemeTokens) -> AnyElement {
    div()
        .flex_none()
        .text_color(tokens.borders.strong.to_gpui())
        .child("·")
        .into_any_element()
}

/// The status folded into the title row: the state icon, the doing-now text,
/// the age and the plan strip, each dropping out at the width where it stops
/// fitting (spec 6.3, [`status_layout`]).
///
/// The container shrinks and clips rather than pushing: it is the one part of
/// the title row that may lose width, because the title carries the identity
/// and the three controls on the right are how the panel is operated at all.
fn inline_status_element(
    chrome: &PanelChrome,
    tokens: ThemeTokens,
    layout: StatusLayout,
    width_px: f32,
    element_key: u64,
    handlers: &PanelHandlers,
) -> AnyElement {
    let tone = status_colour(chrome.status.tone, tokens);
    let blocked = matches!(chrome.needs_you, Some(NeedsYou::Blocked { .. }));
    let mut row = div()
        .flex()
        // `flex_none` inside a budget, not `flex_shrink`: a shrinking group
        // still resolves against its CONTENT first, so a long doing-now line
        // took the width and left the title clipped hard against it with no
        // gap -- which is what a clipped path title running into "Idle" was in
        // the capture. Bounded above by `status_budget` and below by
        // `status_floor`, the group can neither eat the title's floor nor be
        // squeezed out of its own icon, age and Retry.
        .flex_none()
        .min_w(px(status_floor(blocked)))
        .max_w(px(status_budget(width_px, blocked)))
        // The plan strip is `flex_none` and as wide as the plan is long, so the
        // group still clips rather than pushing when a plan outgrows the panel.
        .overflow_hidden()
        .items_center()
        .gap(px(STATUS_GAP))
        .text_size(px(INLINE_STATUS_FONT_SIZE))
        .text_color(tone.to_gpui());

    // Idle has no verb and therefore no glyph: its old middle dot was the same
    // character as the separator below, so the row opened on a separator.
    if let Some(icon) = chrome.status.icon {
        row = row.child(div().flex_none().child(icon));
    }

    if layout.show_text {
        row = row.child(
            div()
                .flex_shrink()
                .min_w(px(0.0))
                .max_w(px(status_text_max_width(width_px, blocked)))
                .overflow_hidden()
                // The ellipsis needs a DEFINITE width to be measured against:
                // GPUI truncates in the text element's measure pass, which only
                // sees one when `known_dimensions.width` is set. `w_full` on
                // the inner child resolves against the bounded box above, which
                // is why the same two-div shape is used for every truncating
                // label in this app (`task_cockpit::panel::panel_list_row`).
                .child(div().w_full().truncate().child(chrome.status.text.clone())),
        );
    }

    // A blocked panel keeps its Retry at every width: the cause can be dropped
    // and still leave the panel usable, but a blocked panel with no way to
    // retry is a dead panel, and the narrow widths are exactly where a person
    // would otherwise have to zoom just to find the affordance. `status_floor`
    // is what makes that true rather than merely intended -- Retry is
    // `flex_none`, so without the floor it is clipped, not moved.
    if blocked {
        let retry_key = chrome.key.clone();
        let on_retry = handlers.on_retry.clone();
        row = row.child(
            div()
                .id(("devmanager-panel-retry", element_key))
                .tab_stop(true)
                .flex_none()
                .cursor_pointer()
                .text_color(tokens.status.destructive.to_gpui())
                .hover(|style| style.text_color(tokens.status.destructive_foreground.to_gpui()))
                .on_mouse_down(
                    MouseButton::Left,
                    move |_event: &MouseDownEvent, window, app| {
                        (on_retry)(&retry_key, window, app);
                    },
                )
                .child("Retry"),
        );
    }

    if layout.show_text {
        row = row.child(status_separator(tokens));
    }
    row = row.child(
        div()
            .flex_none()
            .text_color(tokens.text.muted.to_gpui())
            .child(chrome.status.age.clone()),
    );

    if let Some(progress) = chrome.status.progress.filter(|_| layout.show_segments) {
        row = row
            .child(status_separator(tokens))
            .child(segments_element(progress, tokens, true));
    }

    row.into_any_element()
}

/// The title row: the provider mark, the title (and, zoomed, the crumb), the
/// inline status, the zoom affordance, the one primary button and the ⋯ menu.
fn title_row_element(
    chrome: &PanelChrome,
    tokens: ThemeTokens,
    layout: StatusLayout,
    width_px: f32,
    element_key: u64,
    handlers: &PanelHandlers,
) -> AnyElement {
    // Spec 5.1 as amended: a white title is reserved for the two rows that are
    // waiting on a person, so it keeps meaning something.
    let title_colour = match chrome.needs_you {
        Some(NeedsYou::Question { .. }) | Some(NeedsYou::Permission { .. }) => tokens.text.emphasis,
        _ => tokens.text.primary,
    };
    let tooltip_text = if chrome.crumb.is_empty() {
        chrome.title.clone()
    } else {
        format!("{} · {}", chrome.title, chrome.crumb)
    };

    let zoom_key = chrome.key.clone();
    let on_zoom = handlers.on_zoom.clone();
    let primary_key = chrome.key.clone();
    let primary = chrome.primary;
    let on_primary = handlers.on_primary.clone();
    let menu_key = chrome.key.clone();
    let on_menu = handlers.on_menu.clone();

    let mut row = div()
        .flex()
        .items_center()
        .flex_none()
        .w_full()
        .h(px(if chrome.minimised {
            MINIMISED_HEIGHT
        } else {
            TITLE_ROW_HEIGHT
        }))
        .gap(px(TITLE_ROW_GAP))
        .pl(px(ROW_PADDING_LEFT))
        .pr(px(ROW_PADDING_X))
        .child(div().flex_none().child(crate::icons::app_icon(
            chrome.provider.glyph_path(),
            PROVIDER_MARK_SIZE,
            tokens.text.muted.to_u32(),
        )))
        // Spec 4.2: the panel number sits before the title, and it is the same
        // chip the board row carries -- solid on the focused panel. Task 12's
        // rule, kept inside the new chrome rather than left on a header that
        // no longer exists.
        .children(
            chrome
                .ordinal
                .map(|ordinal| ordinal_chip(ordinal, chrome.focused, tokens)),
        )
        .child(
            div()
                .id(("devmanager-panel-title", element_key))
                .flex_1()
                // The floor, not zero, and it widens with the panel: the status
                // text yields first, and a title squeezed to nothing leaves an
                // anonymous panel at any width.
                .min_w(px(title_floor(width_px)))
                .overflow_hidden()
                .text_size(px(TITLE_FONT_SIZE))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(title_colour.to_gpui())
                .tooltip(move |window, app| {
                    gpui_component::tooltip::Tooltip::new(tooltip_text.clone()).build(window, app)
                })
                // `truncate()` on the flex item itself gave a HARD clip, not an
                // ellipsis: a `flex-basis: 0` item is measured with unbounded
                // available space, so the text element never learns the width
                // it has to fit and lays out at its full length inside a box
                // that is `overflow_hidden`. `w_full` on an inner child
                // resolves against the item's settled width, which is the one
                // number the measure pass will accept.
                .child(div().w_full().truncate().child(chrome.title.clone())),
        );

    // The crumb only earns its width when the panel is zoomed; at one-of-eight
    // width the board's stripe and provider mark already say project and
    // provider, and the title is what the eye is scanning for.
    if chrome.zoomed && !chrome.crumb.is_empty() {
        row = row.child(
            div()
                .flex_none()
                .text_size(px(TITLE_FONT_SIZE))
                .text_color(tokens.text.muted.to_gpui())
                .child(format!("· {}", chrome.crumb)),
        );
    }

    row = row.child(inline_status_element(
        chrome,
        tokens,
        layout,
        width_px,
        element_key,
        handlers,
    ));

    row = row.child(
        div()
            .id(("devmanager-panel-zoom", element_key))
            .tab_stop(true)
            .flex_none()
            .cursor_pointer()
            .text_size(px(ZOOM_ICON_FONT_SIZE))
            .text_color(tokens.text.muted.to_gpui())
            .on_mouse_down(
                MouseButton::Left,
                move |_event: &MouseDownEvent, window, app| {
                    (on_zoom)(&zoom_key, window, app);
                },
            )
            .child(if chrome.zoomed { "⤡ Esc" } else { "⤢" }),
    );

    // A minimised panel is the title row alone: it keeps the status, which is
    // the whole reason to leave a panel minimised, and drops the two controls
    // that need the panel open to be useful.
    if !chrome.minimised {
        row = row
            .child(
                div()
                    .id(("devmanager-panel-primary", element_key))
                    .tab_stop(true)
                    .flex_none()
                    .px(px(ACTION_PADDING_X))
                    .py(px(ACTION_PADDING_Y))
                    .rounded(px(ACTION_RADIUS))
                    .border(px(PANEL_BORDER_WIDTH))
                    .border_color(tokens.borders.default.to_gpui())
                    .text_size(px(ACTION_FONT_SIZE))
                    .text_color(tokens.text.primary.to_gpui())
                    .cursor_pointer()
                    .hover(|style| style.bg(tokens.surfaces.hover.to_gpui()))
                    .on_mouse_down(
                        MouseButton::Left,
                        move |_event: &MouseDownEvent, window, app| {
                            (on_primary)(&primary_key, primary, window, app);
                        },
                    )
                    .child(match primary {
                        PrimaryAction::Done => "Done",
                        PrimaryAction::Reopen => "Reopen",
                    }),
            )
            .child(
                div()
                    .id(("devmanager-panel-menu", element_key))
                    .tab_stop(true)
                    .flex_none()
                    .px(px(MENU_PADDING_X))
                    .text_size(px(MENU_FONT_SIZE))
                    .text_color(tokens.text.muted.to_gpui())
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        move |event: &MouseDownEvent, window, app| {
                            (on_menu)(&menu_key, event.position, window, app);
                        },
                    )
                    .child("⋯"),
            );
    }

    row.into_any_element()
}

/// The five tabs, the active one filled. The three views behind the menu
/// ([`PaneView::MORE`]) deliberately have no tab: five is what fits at the
/// width a panel gets as one of eight.
fn tab_row_element(
    chrome: &PanelChrome,
    tokens: ThemeTokens,
    element_key: u64,
    handlers: &PanelHandlers,
) -> AnyElement {
    let mut row = div()
        .flex()
        .items_end()
        .flex_none()
        .w_full()
        .h(px(TAB_ROW_HEIGHT))
        .gap(px(TAB_GAP))
        .pt(px(TABS_PADDING_TOP))
        .pl(px(ROW_PADDING_LEFT))
        .pr(px(ROW_PADDING_X))
        .border_b(px(PANEL_BORDER_WIDTH))
        .border_color(tokens.borders.subtle.to_gpui())
        .text_size(px(TAB_FONT_SIZE));

    for view in PaneView::TABS {
        let active = view == chrome.view;
        let select_key = chrome.key.clone();
        let on_select = handlers.on_select_view.clone();
        row = row.child(
            div()
                .id((view.label(), element_key))
                .tab_stop(true)
                .flex_none()
                .px(px(TAB_PADDING_X))
                .pt(px(TAB_PADDING_TOP))
                .pb(px(TAB_PADDING_BOTTOM))
                .rounded_tl(px(TAB_RADIUS))
                .rounded_tr(px(TAB_RADIUS))
                .cursor_pointer()
                .when(active, |tab| {
                    tab.bg(tokens.surfaces.selection.to_gpui())
                        .text_color(tokens.text.primary.to_gpui())
                })
                // The unselected tabs are `text.secondary`, one step above the
                // muted grey they used to be: at 11.5 px on `surfaces.raised`
                // the muted token read as disabled rather than as "another
                // view you can go to", which is what 02's tab row shows.
                .when(!active, |tab| {
                    tab.text_color(tokens.text.secondary.to_gpui())
                        .hover(|style| style.bg(tokens.surfaces.hover.to_gpui()))
                })
                .on_mouse_down(
                    MouseButton::Left,
                    move |_event: &MouseDownEvent, window, app| {
                        (on_select)(&select_key, view, window, app);
                    },
                )
                .child(view.label()),
        );
    }

    row.into_any_element()
}

/// The two chrome rows above a panel's body.
///
/// `_colours` is unused here and kept so the two painters in this module take
/// the same four arguments at the shell's call site; the project colour reaches
/// the panel through [`panel_frame`]'s stripe, which is the only place the
/// mockup spends it.
pub fn panel_chrome_element(
    chrome: &PanelChrome,
    _colours: &ProjectColourBook,
    tokens: ThemeTokens,
    width_px: f32,
    handlers: &PanelHandlers,
) -> AnyElement {
    let element_key = panel_element_key(&chrome.key);
    let layout = status_layout(width_px);
    let focus_key = chrome.key.clone();
    let key_key = chrome.key.clone();
    let on_focus = handlers.on_focus.clone();
    let on_key = handlers.on_key.clone();

    let mut column = div()
        .id(panel_element_id(&chrome.key))
        .tab_stop(true)
        .flex()
        .flex_col()
        .flex_none()
        .w_full()
        .on_mouse_down(
            MouseButton::Left,
            move |_event: &MouseDownEvent, window, app| {
                (on_focus)(&focus_key, window, app);
            },
        )
        .on_key_down(move |event: &KeyDownEvent, window, app| {
            (on_key)(&key_key, event, window, app);
        })
        .child(title_row_element(
            chrome,
            tokens,
            layout,
            width_px,
            element_key,
            handlers,
        ));

    if !chrome.minimised {
        column = column.child(tab_row_element(chrome, tokens, element_key, handlers));
    }

    column.into_any_element()
}

/// The panel box: the frame, the project stripe, the chrome and the body.
pub fn panel_frame(
    chrome: &PanelChrome,
    colours: &ProjectColourBook,
    tokens: ThemeTokens,
    chrome_element: AnyElement,
    body: AnyElement,
) -> AnyElement {
    let stripe = colours.colour(chrome.project_colour);
    let attention = needs_you_colour(chrome.needs_you.as_ref(), tokens);
    let border_width = if chrome.focused {
        PANEL_FOCUS_BORDER_WIDTH
    } else {
        PANEL_BORDER_WIDTH
    };
    // The focused frame is `text.primary`, not `borders.focus`: the shell's
    // current pane frame already draws it that way, and two panels drawing the
    // same state in two greys is the drift this painter exists to remove.
    let border = match attention {
        Some(colour) => colour.with_alpha(PANEL_NEEDS_YOU_BORDER_ALPHA),
        None if chrome.focused => tokens.text.primary,
        None => tokens.borders.subtle,
    };
    let radius = tokens.density.radii.md;

    let panel = div()
        .flex()
        .flex_col()
        .relative()
        .size_full()
        .overflow_hidden()
        .rounded(px(radius))
        .bg(tokens.surfaces.raised.to_gpui())
        .border(px(border_width))
        .border_color(border.to_gpui())
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(PANEL_STRIPE_WIDTH))
                .bg(stripe.to_gpui()),
        )
        .child(chrome_element)
        .child(div().flex_1().min_h(px(0.0)).overflow_hidden().child(body));

    match attention {
        // The glow is a second, fainter ring outside the frame rather than a
        // shadow: GPUI's shadow does not follow a rounded border cleanly at
        // 1 px, and a ring is the thing the mockup actually shows.
        Some(colour) => div()
            .flex()
            .flex_col()
            .size_full()
            .rounded(px(radius + NEEDS_YOU_GLOW_WIDTH))
            .border(px(NEEDS_YOU_GLOW_WIDTH))
            .border_color(colour.with_alpha(NEEDS_YOU_GLOW_ALPHA).to_gpui())
            .child(panel)
            .into_any_element(),
        None => panel.into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::HostId;
    use crate::domain::id::TaskId;
    use crate::ui::board::{BoardProgress, BoardRow, BoardState};
    use crate::ui::panel::model::panel_chrome;
    use crate::ui::task_cockpit::inbox::PrimaryProviderIcon;
    use crate::ui::tokens::{Density, Scale};

    /// A fixed, valid UUID v7: the panel's element identity is a digest of it,
    /// so a random id would make that identity untestable.
    const TASK_ID_BYTES: [u8; 16] = [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x01,
    ];

    fn task_key(profile: &str, tail: u8) -> HostTaskKey {
        let mut bytes = TASK_ID_BYTES;
        bytes[15] = tail;
        HostTaskKey::new(
            HostId::LocalProfile(profile.into()),
            TaskId::from_bytes(bytes).expect("task id"),
        )
    }

    fn row(state: BoardState) -> BoardRow {
        BoardRow {
            key: task_key("p", 1),
            title: "Snake Frontend".into(),
            state,
            why: "cargo test --lib ui::".into(),
            state_age_ms: 12_000,
            progress: Some(BoardProgress {
                completed: 5,
                total: 6,
            }),
            provider: PrimaryProviderIcon::Claude,
            project_colour: 0,
            project_id: None,
            project_label: "Snake Game".into(),
            branch: "main".into(),
            last_activity_ms: 0,
            open: None,
            active: false,
        }
    }

    fn noop_handlers() -> PanelHandlers {
        PanelHandlers {
            on_focus: Rc::new(|_, _, _| {}),
            on_select_view: Rc::new(|_, _, _, _| {}),
            on_primary: Rc::new(|_, _, _, _| {}),
            on_zoom: Rc::new(|_, _, _| {}),
            on_menu: Rc::new(|_, _, _, _| {}),
            on_retry: Rc::new(|_, _, _| {}),
            on_key: Rc::new(|_, _, _, _| {}),
        }
    }

    /// The panel and the shell must name the same element, or the panel picks
    /// up a second accessibility identity for the same task.
    ///
    /// `native_shell::stable_host_task_element_key` is private to that module,
    /// so this restates the digest and then asserts, as a canary, that the
    /// shell still builds it from the same pieces. The canary reads source
    /// text and is only a canary: it goes red on a real change to the
    /// algorithm, and it would also go red on a rename, which is the cheap
    /// direction to be wrong in.
    #[test]
    fn the_panel_element_key_is_the_shells_pane_digest() {
        let key = task_key("p", 1);
        let mut digest = Sha256::new();
        digest.update(b"native-host-task");
        digest.update([0]);
        digest.update(crate::ui::native_shell::host_identity_digest_bytes(
            &key.host,
        ));
        digest.update([0]);
        digest.update(key.task_id.as_bytes());
        digest.update([0]);
        digest.update(b"pane");
        let expected = u64::from_le_bytes(
            digest.finalize()[..8]
                .try_into()
                .expect("sha256 prefix is eight bytes"),
        );
        assert_eq!(panel_element_key(&key), expected);

        // Two tasks, and the same task on two hosts, never share an identity.
        assert_ne!(
            panel_element_key(&key),
            panel_element_key(&task_key("p", 2))
        );
        assert_ne!(
            panel_element_key(&key),
            panel_element_key(&task_key("q", 1))
        );

        let shell = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui/native_shell.rs"),
        )
        .expect("shell source");
        let start = shell
            .find("fn stable_host_task_element_key")
            .expect("the shell still defines the pane digest");
        let body = &shell[start..(start + 800).min(shell.len())];
        for fragment in [
            "b\"native-host-task\"",
            "host_identity_digest_bytes(&key.host)",
            "key.task_id.as_bytes()",
            "suffix.as_bytes()",
            "u64::from_le_bytes",
        ] {
            assert!(
                body.contains(fragment),
                "the shell's pane digest changed: {fragment} is gone"
            );
        }
        assert!(
            shell.contains("stable_host_task_element_key(&task_key, \"pane\")"),
            "the shell no longer asks for the pane suffix"
        );
    }

    /// The painter is the only panel code that needs a GPUI app, so this is the
    /// one headless test: every needs-you state, both status breakpoints, the
    /// minimised form and the zoomed form have to build an element tree without
    /// panicking. It builds elements only -- painting a window headlessly
    /// crashes, and building is what a painter regression breaks.
    #[test]
    fn panel_chrome_builds_for_every_needs_you_state_and_width() {
        if crate::ui::native_shell::tests::rerun_headless_shell_test_in_child(
            "ui::panel::render::tests::panel_chrome_builds_for_every_needs_you_state_and_width",
        ) {
            return;
        }
        let _guard = crate::ui::native_shell::tests::headless_shell_test_lock();
        gpui::Application::headless().run(|cx| {
            crate::ui::init(cx);
            let tokens = crate::ui::tokens::dark(Density::Comfortable, Scale::Scale100);
            let colours = ProjectColourBook::default();
            let handlers = noop_handlers();
            let cases = [
                (None, BoardState::Working),
                (
                    Some(NeedsYou::Question { choices: 3 }),
                    BoardState::Question,
                ),
                (
                    Some(NeedsYou::Permission { names_a_file: true }),
                    BoardState::Permission,
                ),
                (
                    Some(NeedsYou::Blocked {
                        cause: "x".repeat(200),
                    }),
                    BoardState::Blocked,
                ),
            ];
            for (needs_you, state) in cases {
                // The design width of one of eight; the width where the status
                // has lost both its strip and its text; the minimised form; and
                // the zoomed form, which is the only one carrying a crumb.
                for (width, focused, zoomed, minimised) in [
                    (470.0_f32, true, false, false),
                    (370.0_f32, false, false, false),
                    (300.0_f32, false, false, false),
                    (250.0_f32, false, false, false),
                    (470.0_f32, false, false, true),
                    (1_200.0_f32, true, true, false),
                ] {
                    let chrome = panel_chrome(
                        &row(state),
                        PaneView::Conversation,
                        focused,
                        zoomed,
                        minimised,
                        needs_you.clone(),
                        false,
                        "Snake Game · Claude · main".into(),
                    );
                    let element = panel_chrome_element(&chrome, &colours, tokens, width, &handlers);
                    let _ = panel_frame(
                        &chrome,
                        &colours,
                        tokens,
                        element,
                        div()
                            .id("devmanager-panel-test-body")
                            .child("stream")
                            .into_any_element(),
                    );
                }
            }
            // A reopened task paints the other primary label, and every tab
            // other than Conversation can be the active one.
            for view in PaneView::TABS {
                let done = panel_chrome(
                    &row(BoardState::Done),
                    view,
                    false,
                    false,
                    false,
                    None,
                    true,
                    String::new(),
                );
                let _ = panel_chrome_element(&done, &colours, tokens, 470.0, &handlers);
            }
            cx.quit();
        });
    }

    /// The chrome rows are the numbers the mockup was measured at, and the
    /// minimised row is shorter than the open one or minimising buys nothing.
    #[test]
    fn the_chrome_rows_keep_the_mockups_heights() {
        assert_eq!(TITLE_ROW_HEIGHT, 30.0);
        assert_eq!(TAB_ROW_HEIGHT, 28.0);
        assert_eq!(MINIMISED_HEIGHT, 28.0);
        assert!(MINIMISED_HEIGHT < TITLE_ROW_HEIGHT + TAB_ROW_HEIGHT);
        // The stripe is the board's, on the same edge and at the same width,
        // and the chrome's left padding clears it.
        assert_eq!(PANEL_STRIPE_WIDTH, ROW_STRIPE_WIDTH);
        assert!(ROW_PADDING_LEFT > PANEL_STRIPE_WIDTH);
    }

    /// The title row is exactly paid for at every width: the fixed controls,
    /// the title's floor and the status group's budget sum to the panel, so a
    /// long doing-now line cannot claim room the title needs and the title's
    /// ellipsis always has somewhere to land. Pure arithmetic over the
    /// constants: no window, so a layout pass cannot quietly satisfy it.
    ///
    /// The regression it exists for, twice over: with the status `flex_none`
    /// behind a fixed 170 px cap, a long doing-now string on a 260-370 px panel
    /// pushed Done and the menu off the right edge; with the status shrinking
    /// behind a cap that counted only the TEXT, the group could still ask for
    /// its floor on top of that cap -- 227 px of demand on a 470 px panel that
    /// had budgeted 187 -- and the title was clipped flush against it.
    #[test]
    fn the_status_group_fits_inside_what_the_controls_and_the_title_leave_it() {
        assert_eq!(
            CONTROLS_RESERVE, 195.0,
            "the documented budget and the summed constants disagree"
        );
        assert_eq!(status_floor(false), 40.0);
        assert_eq!(status_floor(true), 73.0);
        assert!(
            status_floor(true) > status_floor(false),
            "a blocked panel owes a Retry the others do not"
        );

        // The title floor widens with the panel, so the title is never
        // anonymous at a width that could afford to name it. These four are the
        // doc comment's table: if it and the formula ever disagree, this is
        // where it shows.
        for (width, expected_floor, expected_budget) in [
            (280.0_f32, 12.0_f32, 73.0_f32),
            (300.0, 20.0, 85.0),
            (370.0, 48.0, 127.0),
            (470.0, 88.0, 187.0),
        ] {
            assert_eq!(
                title_floor(width),
                expected_floor,
                "the title floor at {width} px is not the documented one"
            );
            for blocked in [false, true] {
                assert_eq!(
                    status_budget(width, blocked),
                    expected_budget,
                    "the status budget at {width} px is not the documented one"
                );
                // The whole point: the row is exactly paid for. Nothing is
                // spare and nothing is over-committed, in either state.
                assert_eq!(
                    CONTROLS_RESERVE + title_floor(width) + status_budget(width, blocked),
                    width,
                    "at {width} px the title row does not add up"
                );
                // The parts the status may never drop always fit inside the
                // budget, so the text is the only thing that yields.
                assert!(
                    status_budget(width, blocked) >= status_floor(blocked),
                    "at {width} px the status budget sits below its own floor"
                );
                assert_eq!(
                    status_text_max_width(width, blocked),
                    status_budget(width, blocked) - status_floor(blocked),
                    "the text cap is not the budget less the floor"
                );
            }
        }

        // A blocked panel at the tight width has spent everything on its icon,
        // its age and its Retry: the cause is the part that goes.
        assert_eq!(status_text_max_width(TIGHT_WIDTH, true), 0.0);
        assert_eq!(status_text_max_width(TIGHT_WIDTH, false), 33.0);
        // Below the tight width the floor stops shrinking and the budget floors
        // at what the status may never give up rather than going negative.
        assert_eq!(title_floor(100.0), TITLE_MIN_WIDTH);
        assert_eq!(status_budget(100.0, true), status_floor(true));
        assert_eq!(status_text_max_width(100.0, true), 0.0);
    }

    /// F8: idle has no verb, so it has no glyph -- otherwise the status opens
    /// on the same middle dot it uses to separate its own parts and reads as a
    /// separator with nothing before it ("· Idle · 4d" rather than "Idle · 4d").
    #[test]
    fn only_a_state_with_a_verb_carries_a_glyph() {
        let idle = panel_chrome(
            &row(BoardState::Idle),
            PaneView::Conversation,
            false,
            false,
            false,
            None,
            false,
            String::new(),
        );
        assert_eq!(idle.status.icon, None);

        let cases: [(BoardState, Option<NeedsYou>); 3] = [
            (BoardState::Working, None),
            (
                BoardState::Question,
                Some(NeedsYou::Question { choices: 1 }),
            ),
            (
                BoardState::Blocked,
                Some(NeedsYou::Blocked {
                    cause: "x".to_string(),
                }),
            ),
        ];
        for (state, needs_you) in cases {
            let chrome = panel_chrome(
                &row(state),
                PaneView::Conversation,
                false,
                false,
                false,
                needs_you,
                false,
                String::new(),
            );
            assert!(
                chrome.status.icon.is_some(),
                "{state:?} names a verb and must carry its glyph"
            );
        }
    }

    /// Both truncating labels in the title row hang the text off an inner
    /// `w_full` child. GPUI measures an ellipsis against a DEFINITE width, and
    /// a `flex-basis: 0` item is measured with unbounded available space, so
    /// `truncate()` applied to the flex item itself clips hard instead --
    /// which is what put "C:/Code/userfir" flush against the status in the
    /// capture. A source scan because the failure is a layout one: no pure
    /// assertion over the constants can see it.
    #[test]
    fn the_title_and_the_status_text_truncate_against_a_definite_width() {
        let source = include_str!("render.rs");
        let painter = source
            .split("#[cfg(test)]")
            .next()
            .expect("the painter is everything above its tests");
        // Whitespace-stripped so `cargo fmt` breaking the builder chain over
        // three lines cannot quietly turn either assertion vacuous.
        let compact: String = painter.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(
            compact.matches(".truncate()").count(),
            2,
            "the title row has exactly two truncating labels: the title and the status text"
        );
        assert_eq!(
            compact.matches(".w_full().truncate()").count(),
            2,
            "every truncating label must resolve its width through an inner w_full child"
        );
    }

    /// F9: the mark before the title is the board's own 11 px monochrome
    /// provider mark, painted in `text.muted` and sitting BEFORE the title, per
    /// mockup 05 (chosen option 1, "grey mark, second line" -- and the same
    /// mark on the panel) and design language rule 10, which forbids a brand
    /// tint outright.
    ///
    /// A source scan because there is nothing else to read: the size is a
    /// constant the board owns and the colour is a token, so the only thing
    /// that can go wrong is the painter passing different ones. Ordering is
    /// checked by position, which is what "before the title" means.
    #[test]
    fn the_provider_mark_is_the_boards_eleven_pixel_grey_one_before_the_title() {
        // Shared with the board row's meta line rather than restated, so the
        // row and the panel cannot draw two different marks.
        assert_eq!(PROVIDER_MARK_SIZE, 11.0);

        let source = include_str!("render.rs");
        let painter = source
            .split("#[cfg(test)]")
            .next()
            .expect("the painter is everything above its tests");
        let title_row = painter
            .split("fn title_row_element(")
            .nth(1)
            .expect("the title row painter");

        let mark = title_row
            .find("crate::icons::app_icon(")
            .expect("the title row paints the provider mark through app_icon");
        let title = title_row
            .find("devmanager-panel-title")
            .expect("the title row paints the title");
        assert!(
            mark < title,
            "the provider mark must be painted before the title"
        );

        let call = &title_row[mark..title];
        assert!(
            call.contains("chrome.provider.glyph_path()"),
            "the mark is the panel's own provider glyph"
        );
        assert!(
            call.contains("PROVIDER_MARK_SIZE"),
            "the mark is sized by the board's shared constant, not by a literal"
        );
        assert!(
            call.contains("tokens.text.muted.to_u32()"),
            "the mark is grey: design language rule 10 allows no brand tint"
        );
    }

    /// F11: the tab row per mockup 02 -- 28 px, 11.5 px labels, the selected
    /// tab a pill on `surfaces.selection` in `text.primary` and every other
    /// tab in `text.secondary`. The five tabs are all that fit at the width a
    /// panel gets as one of eight, so there is no "More" affordance to build:
    /// `PaneView::TABS` IS the five, and the three views behind it reach the
    /// panel through the menu.
    #[test]
    fn the_tab_row_is_the_mockups_and_five_tabs_are_all_of_them() {
        assert_eq!(TAB_ROW_HEIGHT, 28.0);
        assert_eq!(TAB_FONT_SIZE, 11.5);
        assert_eq!(
            PaneView::TABS.len(),
            5,
            "more than five tabs and the row would owe a More menu"
        );
        assert!(
            TABS_PADDING_TOP + TAB_PADDING_TOP + TAB_FONT_SIZE + TAB_PADDING_BOTTOM
                <= TAB_ROW_HEIGHT,
            "the tab and its paddings must fit inside the row"
        );

        let source = include_str!("render.rs");
        let painter = source
            .split("#[cfg(test)]")
            .next()
            .expect("the painter is everything above its tests");
        let tabs = painter
            .split("fn tab_row_element(")
            .nth(1)
            .expect("the tab row painter");
        let tabs = tabs
            .split("\n/// ")
            .next()
            .expect("everything up to the next item's doc comment");
        assert!(
            tabs.contains("tokens.surfaces.selection.to_gpui()")
                && tabs.contains("tokens.text.primary.to_gpui()"),
            "the selected tab is a pill on surfaces.selection in text.primary"
        );
        assert!(
            tabs.contains("tokens.text.secondary.to_gpui()"),
            "the unselected tabs are text.secondary"
        );
        assert!(
            !tabs.contains("tokens.text.muted.to_gpui()"),
            "no tab is painted in the muted grey, which reads as disabled"
        );
    }

    /// F12: the redesign's type scale, pinned at the density the shell is
    /// actually running, and shown not to move with it.
    ///
    /// Where the density comes from: `NativeShell::theme_tokens` builds its
    /// tokens from `self.preferences`, and the window is opened with
    /// `RuntimePreferencesSnapshot::from_system(appearance, scale_factor,
    /// RuntimePreferencesSnapshot::default().density())` -- so the shipped
    /// default is `Density::Comfortable` with the display's own `Scale`, and
    /// only the board's density toggle ever changes it.
    ///
    /// Whether it reaches these numbers: it does not. `density_metrics` gives
    /// Comfortable a 12 px caption and a 14 px body against Compact's 11 and
    /// 13, but this painter never reads `tokens.density.typography` -- every
    /// size here is a literal measured off the mockup. So "the redesign
    /// numbers ARE the comfortable numbers" holds by the painter ignoring the
    /// density, and the scan below is what keeps it true.
    #[test]
    fn the_chrome_type_scale_is_the_specs_and_no_density_moves_it() {
        assert_eq!(TITLE_FONT_SIZE, 12.0, "spec: panel title 12 semibold");
        assert_eq!(INLINE_STATUS_FONT_SIZE, 11.0, "spec: status 11");
        assert_eq!(TAB_FONT_SIZE, 11.5, "spec: tabs 11.5");
        // Nothing in the chrome is larger than the 12 px title: rule 2 keeps 13
        // for the one heading a surface gets, and a panel's heading is its
        // title. The two exceptions are glyph boxes, not text.
        assert!(ACTION_FONT_SIZE <= TAB_FONT_SIZE);
        assert!(INLINE_STATUS_FONT_SIZE < TITLE_FONT_SIZE);

        // The four sizes the shell can hand the painter, at both densities.
        // The metrics differ; the chrome does not.
        let comfortable = crate::ui::tokens::dark(Density::Comfortable, Scale::Scale100);
        let compact = crate::ui::tokens::dark(Density::Compact, Scale::Scale100);
        assert_eq!(comfortable.density.density, Density::Comfortable);
        assert_ne!(
            comfortable.density.typography.body, compact.density.typography.body,
            "the two densities must really differ, or this test proves nothing"
        );

        let source = include_str!("render.rs");
        let painter = source
            .split("#[cfg(test)]")
            .next()
            .expect("the painter is everything above its tests");
        assert!(
            !painter.contains("density.typography"),
            "the panel chrome must not take its type scale from the density metrics"
        );
        // Every text size in the painter comes from a named constant, so a
        // literal cannot creep back in beside them.
        assert!(
            !painter.contains(".text_size(px(1"),
            "text sizes belong in the constants above, not inline"
        );
    }
}
