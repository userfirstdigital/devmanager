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
use crate::ui::board::render::segments_element;
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
pub const TAB_ROW_HEIGHT: f32 = 26.0;
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
/// `.hdr .title { font-size: 13px; font-weight: 600 }`.
const TITLE_FONT_SIZE: f32 = 13.0;
/// `.hdr .inline { font-size: 11.5px }` -- the status folded into the title row.
const INLINE_STATUS_FONT_SIZE: f32 = 11.5;
/// `.st .s { gap: 5px }`.
const STATUS_GAP: f32 = 5.0;
/// The title floor at [`TIGHT_WIDTH`], where the row has nothing to spare.
///
/// A blocked panel at 250 px owes 165 px of controls and a 73 px status floor,
/// which is 238 of the 250, so 12 px is not chosen, it is what is left. See
/// [`title_floor`] for what happens above that width -- a fixed 12 px floor is
/// only honest at the one width that forces it, and would leave the title at
/// 12 px on a 470 px panel behind a long blocked cause.
const TITLE_MIN_WIDTH: f32 = 12.0;
/// The narrowest panel this chrome is built for, and the width at which the
/// title floor is at its minimum.
const TIGHT_WIDTH: f32 = 250.0;
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
///   width   title_floor   status text cap   controls + title + status floor
///     250            12                73     165 +  12 + 73 = 250  (exactly)
///     300            32               103     165 +  32 + 73 = 270
///     370            60               145     165 +  60 + 73 = 298
///     470           100               205     165 + 100 + 73 = 338
/// ```
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
/// The ⋯ glyph plus its padding: [`MENU_FONT_SIZE`] + 2 x [`MENU_PADDING_X`].
const MENU_GLYPH_WIDTH: f32 = MENU_FONT_SIZE + 2.0 * MENU_PADDING_X;
/// Everything in the title row whose width does not depend on the panel's:
///
/// ```text
///   13  ROW_PADDING_LEFT          (10 px padding + the 3 px stripe)
///   10  ROW_PADDING_X             (right padding)
///   11  PROVIDER_MARK_SIZE
///   12  ZOOM_AFFORDANCE_WIDTH
///   58  PRIMARY_BUTTON_MAX_WIDTH
///   21  MENU_GLYPH_WIDTH          (15 + 2 x 3)
///   40  5 x TITLE_ROW_GAP         (six children, five gaps)
///  ---
///  165
/// ```
///
/// This is what the status must yield to. Before this budget existed the status
/// was `flex_none` behind a fixed cap, so on a panel of roughly 260-370 px a
/// long doing-now string pushed Done and ⋯ off the right edge -- silently, with
/// no panic and nothing in any test to see it.
const CONTROLS_RESERVE: f32 = ROW_PADDING_LEFT
    + ROW_PADDING_X
    + PROVIDER_MARK_SIZE
    + ZOOM_AFFORDANCE_WIDTH
    + PRIMARY_BUTTON_MAX_WIDTH
    + MENU_GLYPH_WIDTH
    + 5.0 * TITLE_ROW_GAP;

/// The ceiling on the status *text*: what is left once the fixed controls and
/// the title's floor at this width have been paid.
///
/// `min_w(0)` plus `flex_shrink` on the status container is what keeps the
/// controls on screen; this is what keeps the *title* on screen, by stopping a
/// long doing-now line or a 60-character blocked cause from claiming room the
/// title needs. Same shape as the board's `row_content_width`: a rule about the
/// content, not about the panel.
fn status_text_max_width(width_px: f32) -> f32 {
    (width_px - CONTROLS_RESERVE - title_floor(width_px)).max(0.0)
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
        .flex_shrink()
        // A floor, and deliberately no ceiling: the container may shrink to
        // this and no further, so the icon, the age and Retry survive every
        // width while the text child above is the only part that yields.
        .min_w(px(status_floor(blocked)))
        .overflow_hidden()
        .items_center()
        .gap(px(STATUS_GAP))
        .text_size(px(INLINE_STATUS_FONT_SIZE))
        .text_color(tone.to_gpui())
        .child(div().flex_none().child(chrome.status.icon));

    if layout.show_text {
        row = row.child(
            div()
                .flex_shrink()
                .min_w(px(0.0))
                .max_w(px(status_text_max_width(width_px)))
                .truncate()
                .child(chrome.status.text.clone()),
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
        .child(
            div()
                .id(("devmanager-panel-title", element_key))
                .flex_1()
                // The floor, not zero, and it widens with the panel: the status
                // text yields first, and a title squeezed to nothing leaves an
                // anonymous panel at any width.
                .min_w(px(title_floor(width_px)))
                .truncate()
                .text_size(px(TITLE_FONT_SIZE))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(title_colour.to_gpui())
                .tooltip(move |window, app| {
                    gpui_component::tooltip::Tooltip::new(tooltip_text.clone()).build(window, app)
                })
                .child(chrome.title.clone()),
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
                .when(!active, |tab| {
                    tab.text_color(tokens.text.muted.to_gpui())
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
        assert_eq!(TAB_ROW_HEIGHT, 26.0);
        assert_eq!(MINIMISED_HEIGHT, 28.0);
        assert!(MINIMISED_HEIGHT < TITLE_ROW_HEIGHT + TAB_ROW_HEIGHT);
        // The stripe is the board's, on the same edge and at the same width,
        // and the chrome's left padding clears it.
        assert_eq!(PANEL_STRIPE_WIDTH, ROW_STRIPE_WIDTH);
        assert!(ROW_PADDING_LEFT > PANEL_STRIPE_WIDTH);
    }

    /// The status is the only part of the title row allowed to lose width, so
    /// at every width a panel can be given the fixed controls plus the title's
    /// floor still fit inside the panel. Pure arithmetic over the constants: no
    /// window, so a layout pass cannot quietly satisfy it.
    ///
    /// The regression it exists for: with the status `flex_none` behind a fixed
    /// 170 px cap, a long doing-now string on a 260-370 px panel pushed Done
    /// and the ⋯ menu off the right edge, silently and without panicking.
    #[test]
    fn the_status_text_yields_before_the_controls_the_title_and_the_status_floor() {
        assert_eq!(
            CONTROLS_RESERVE, 165.0,
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
        for (width, expected_floor, expected_cap) in [
            (250.0_f32, 12.0_f32, 73.0_f32),
            (300.0, 32.0, 103.0),
            (370.0, 60.0, 145.0),
            (470.0, 100.0, 205.0),
        ] {
            assert_eq!(
                title_floor(width),
                expected_floor,
                "the title floor at {width} px is not the documented one"
            );
            assert_eq!(
                status_text_max_width(width),
                expected_cap,
                "the status text cap at {width} px is not the documented one"
            );

            // The binding case: a blocked panel, which owes the widest status
            // floor. Rearranged, this is "the width left after the controls and
            // the title floor is at least what the status can never give up".
            assert!(
                CONTROLS_RESERVE + title_floor(width) + status_floor(true) <= width,
                "at {width} px a blocked panel cannot pay for its controls, its title floor and its status floor at once"
            );
            assert!(
                status_text_max_width(width) >= status_floor(true),
                "at {width} px the status text cap sits below the parts the status may never drop"
            );
        }

        // At the tight width the three add up exactly: nothing is spare, and
        // nothing is over-committed.
        assert_eq!(
            CONTROLS_RESERVE + title_floor(TIGHT_WIDTH) + status_floor(true),
            TIGHT_WIDTH
        );
        // Below the tight width the floor stops shrinking and the cap floors at
        // zero rather than handing a negative width to the layout.
        assert_eq!(title_floor(100.0), TITLE_MIN_WIDTH);
        assert_eq!(status_text_max_width(100.0), 0.0);
    }
}
