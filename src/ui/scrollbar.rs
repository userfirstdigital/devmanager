//! One scrollbar look, expressed once.
//!
//! Two things paint a scrollbar in this app and there is no way to make it
//! one: every shell surface is ordinary GPUI layout, while the terminal's
//! gutter lives inside a `canvas` element and has to be painted by hand. What
//! *can* be one is the contract -- so all the geometry lives here as pure
//! functions over [`ScrollbarTokens`], both painters call them, and a test can
//! prove the two agree for the same viewport instead of two lists of constants
//! being eyeballed against each other.
//!
//! Why not `gpui_component::Scrollbar`, which the app already had at six
//! sites: its `THUMB_WIDTH` (6 px) and `THUMB_ACTIVE_WIDTH` (8 px) are private
//! module constants and its theme exposes only colours and a show-policy, so
//! the redesign's 4 px idle / 10 px active is unreachable through it. Its three
//! show-modes are also each wrong for the ruling: `Scrolling` fades the bar to
//! nothing after two seconds, `Hover` does the same when the pointer leaves,
//! and `Always` paints a constant 8 px that never responds to the pointer.
//! None of them is "thin and delicate when idle but still easily visible,
//! thicker and easy to grab when hovered". This element is that, and it drives
//! the same `ScrollbarHandle` trait the component defines, so every existing
//! `ScrollHandle`, `UniformListScrollHandle` and `ListState` works unchanged.

use std::rc::Rc;

use gpui::{
    canvas, div, prelude::FluentBuilder, px, App, Bounds, Div, Element, ElementId, Entity,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Point, RenderOnce, ScrollHandle, SharedString, Stateful,
    StatefulInteractiveElement, StyleRefinement, Styled, Window,
};
use gpui_component::scroll::ScrollbarHandle;
use gpui_component::StyledExt;

use crate::ui::tokens::{Color, ScrollbarColors, ScrollbarTokens, ThemeTokens};

/// Where the thumb lands inside its gutter, in gutter-local logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollbarThumb {
    pub left: f32,
    pub width: f32,
    pub top: f32,
    pub height: f32,
    pub radius: f32,
}

/// The groove the thumb runs in, in gutter-local logical pixels. It is painted
/// only in the active state; at rest the bar is the thumb alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollbarTrack {
    pub left: f32,
    pub width: f32,
    pub top: f32,
    pub height: f32,
    pub radius: f32,
}

/// The track is as wide as the widest thumb, centred in the gutter, so the
/// thumb never leaves it and the groove does not move when the thumb grows.
pub fn track_geometry(spec: ScrollbarTokens, gutter_height: f32) -> ScrollbarTrack {
    let width = spec.active_thumb_width;
    let height = (gutter_height - spec.track_inset_y * 2.0).max(0.0);
    ScrollbarTrack {
        left: ((spec.gutter_width - width) / 2.0).max(0.0),
        width,
        top: spec.track_inset_y,
        height,
        radius: width * spec.thumb_radius_ratio,
    }
}

/// The thumb for one scroll position, or `None` when the content fits.
///
/// `visible_fraction` is viewport / content and `scroll_fraction` is the
/// position of the viewport within the scrollable range, both 0..=1. Those two
/// ratios are all either painter has to supply, which is why the terminal --
/// whose model carries exactly `thumb_height_ratio` and `thumb_top_ratio` --
/// can call this without learning anything about pixels.
pub fn thumb_geometry(
    spec: ScrollbarTokens,
    gutter_height: f32,
    visible_fraction: f32,
    scroll_fraction: f32,
    active: bool,
) -> Option<ScrollbarThumb> {
    if !visible_fraction.is_finite() || visible_fraction >= 1.0 || visible_fraction <= 0.0 {
        return None;
    }
    let track = track_geometry(spec, gutter_height);
    if track.height <= 0.0 {
        return None;
    }
    let width = spec.thumb_width(active);
    let height = (track.height * visible_fraction)
        .max(spec.min_thumb_length)
        .min(track.height);
    let travel = (track.height - height).max(0.0);
    let scroll_fraction = if scroll_fraction.is_finite() {
        scroll_fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    Some(ScrollbarThumb {
        left: ((spec.gutter_width - width) / 2.0).max(0.0),
        width,
        top: track.top + travel * scroll_fraction,
        height,
        radius: width * spec.thumb_radius_ratio,
    })
}

/// The scroll fraction that puts the thumb's centre under `pointer_y`, given
/// in gutter-local coordinates. This is the inverse of [`thumb_geometry`] and
/// is what both click-to-position and drag use, so a drag cannot disagree with
/// the paint about where the thumb belongs.
pub fn scroll_fraction_for_pointer(
    spec: ScrollbarTokens,
    gutter_height: f32,
    visible_fraction: f32,
    pointer_y: f32,
) -> f32 {
    let Some(thumb) = thumb_geometry(spec, gutter_height, visible_fraction, 0.0, true) else {
        return 0.0;
    };
    let track = track_geometry(spec, gutter_height);
    let travel = track.height - thumb.height;
    if travel <= 0.0 {
        return 0.0;
    }
    ((pointer_y - track.top - thumb.height / 2.0) / travel).clamp(0.0, 1.0)
}

/// Whether the gutter should paint anything at all for this content.
pub fn has_overflow(visible_fraction: f32) -> bool {
    visible_fraction.is_finite() && visible_fraction > 0.0 && visible_fraction < 1.0
}

#[derive(Debug, Default, Clone, Copy)]
struct ScrollbarInteraction {
    /// Window-space bounds of the gutter, captured during prepaint. Mouse
    /// events arrive in window space and there is no other way to map one back
    /// onto the track.
    gutter: Bounds<Pixels>,
    dragging: bool,
}

/// The app's scrollbar. Absolutely positioned over the right edge of whatever
/// it is a child of, so it never takes layout space and expanding on hover
/// cannot reflow the content beside it.
#[derive(IntoElement)]
pub struct AppScrollbar {
    id: ElementId,
    group: SharedString,
    handle: Rc<dyn ScrollbarHandle>,
    spec: ScrollbarTokens,
    colors: ScrollbarColors,
}

impl AppScrollbar {
    /// A vertical scrollbar over `handle`, painted over `background`.
    ///
    /// `id` must be unique within the window: it keys both the retained
    /// interaction state and the hover group, so two scrollbars sharing an id
    /// would expand together. `background` is the surface the gutter sits on
    /// -- the colours are resolved against it rather than against the theme,
    /// because the app mixes polarities (see `ScrollbarTokens::colors_on`).
    pub fn vertical<H>(
        id: impl Into<ElementId>,
        handle: &H,
        spec: ScrollbarTokens,
        background: Color,
    ) -> Self
    where
        H: ScrollbarHandle + Clone,
    {
        let id = id.into();
        let group = SharedString::from(format!("app-scrollbar-group-{id:?}"));
        Self {
            id,
            group,
            handle: Rc::new(handle.clone()),
            spec,
            colors: spec.colors_on(background),
        }
    }
}

/// Viewport / content for a handle whose viewport is `viewport_height`.
fn visible_fraction_of(content_height: f32, viewport_height: f32) -> f32 {
    if content_height <= 0.0 || viewport_height <= 0.0 {
        return 1.0;
    }
    viewport_height / content_height
}

/// Position of the viewport within the scrollable range, 0..=1.
fn scroll_fraction_of(offset_y: f32, content_height: f32, viewport_height: f32) -> f32 {
    let travel = content_height - viewport_height;
    if travel <= 0.0 {
        return 0.0;
    }
    (-offset_y / travel).clamp(0.0, 1.0)
}

impl RenderOnce for AppScrollbar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let spec = self.spec;
        let colors = self.colors;
        let state: Entity<ScrollbarInteraction> =
            window.use_keyed_state(self.id.clone(), cx, |_, _| ScrollbarInteraction::default());
        let view = window.current_view();

        // Read the handle once per frame. `content_size` already includes the
        // viewport, so the viewport height is the gutter's own height -- the
        // gutter is stretched to the scroll container, which is the same box.
        let content_height: f32 = self.handle.content_size().height.into();
        let gutter_bounds = state.read(cx).gutter;
        let viewport_height: f32 = gutter_bounds.size.height.into();
        let offset_y: f32 = self.handle.offset().y.into();
        let visible_fraction = visible_fraction_of(content_height, viewport_height);
        let scroll_fraction = scroll_fraction_of(offset_y, content_height, viewport_height);

        let track = track_geometry(spec, viewport_height);
        let idle = thumb_geometry(
            spec,
            viewport_height,
            visible_fraction,
            scroll_fraction,
            false,
        );
        let hovered = thumb_geometry(
            spec,
            viewport_height,
            visible_fraction,
            scroll_fraction,
            true,
        );

        let bounds_probe = {
            let state = state.clone();
            canvas(
                move |bounds, _window, cx| {
                    state.update(cx, |interaction, _| interaction.gutter = bounds);
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full()
        };

        // Drag continuation has to be heard outside the gutter, so the move and
        // up listeners are global for the frame rather than element-scoped.
        let drag_listeners = {
            let state = state.clone();
            let handle = self.handle.clone();
            canvas(
                move |_, _, _| {},
                move |_, _, window, _| {
                    let move_state = state.clone();
                    let move_handle = handle.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, _, _, cx| {
                        let interaction = *move_state.read(cx);
                        if !interaction.dragging {
                            return;
                        }
                        apply_pointer(
                            &move_handle,
                            spec,
                            interaction.gutter,
                            event.position,
                            content_height,
                        );
                        cx.notify(view);
                    });

                    let up_state = state.clone();
                    let up_handle = handle.clone();
                    window.on_mouse_event(move |_: &MouseUpEvent, _, _, cx| {
                        let was_dragging = up_state.read(cx).dragging;
                        if !was_dragging {
                            return;
                        }
                        up_state.update(cx, |interaction, _| interaction.dragging = false);
                        up_handle.end_drag();
                        cx.notify(view);
                    });
                },
            )
            .absolute()
            .size_full()
        };

        let mouse_down = {
            let state = state.clone();
            let handle = self.handle.clone();
            move |event: &MouseDownEvent, _window: &mut Window, cx: &mut App| {
                let gutter = state.read(cx).gutter;
                state.update(cx, |interaction, _| interaction.dragging = true);
                handle.start_drag();
                apply_pointer(&handle, spec, gutter, event.position, content_height);
                cx.notify(view);
            }
        };

        let mut gutter = div()
            .id(self.id)
            .group(self.group.clone())
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .w(px(spec.gutter_width))
            .child(bounds_probe)
            .child(drag_listeners);

        if let (Some(idle), Some(hovered)) = (idle, hovered) {
            gutter = gutter
                .on_mouse_down(MouseButton::Left, mouse_down)
                .child(
                    div()
                        .absolute()
                        .left(px(track.left))
                        .top(px(track.top))
                        .w(px(track.width))
                        .h(px(track.height))
                        .rounded(px(track.radius))
                        .group_hover(self.group.clone(), |style| {
                            style.bg(colors.track_active.to_gpui())
                        }),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(idle.left))
                        .top(px(idle.top))
                        .w(px(idle.width))
                        .h(px(idle.height))
                        .rounded(px(idle.radius))
                        .bg(colors.thumb_idle.to_gpui())
                        // Hovering anywhere in the gutter widens the thumb, not
                        // only hovering the thumb itself -- that is what makes
                        // a 4 px bar grabbable.
                        .group_hover(self.group.clone(), |style| {
                            style
                                .left(px(hovered.left))
                                .w(px(hovered.width))
                                .rounded(px(hovered.radius))
                                .bg(colors.thumb_hover.to_gpui())
                        }),
                );
        }

        gutter
    }
}

/// Move the handle so the thumb centres on a window-space pointer position.
fn apply_pointer(
    handle: &Rc<dyn ScrollbarHandle>,
    spec: ScrollbarTokens,
    gutter: Bounds<Pixels>,
    position: Point<Pixels>,
    content_height: f32,
) {
    let gutter_height: f32 = gutter.size.height.into();
    let viewport_height = gutter_height;
    let visible_fraction = visible_fraction_of(content_height, viewport_height);
    if !has_overflow(visible_fraction) {
        return;
    }
    let local_y: f32 = (position.y - gutter.origin.y).into();
    let fraction = scroll_fraction_for_pointer(spec, gutter_height, visible_fraction, local_y);
    let travel = content_height - viewport_height;
    let mut offset = handle.offset();
    offset.y = px(-(fraction * travel));
    handle.set_offset(offset);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tokens::{dark, Density, Scale};

    fn spec() -> ScrollbarTokens {
        dark(Density::Comfortable, Scale::Scale100).scrollbar
    }

    #[test]
    fn a_surface_with_no_overflow_paints_no_thumb() {
        assert!(thumb_geometry(spec(), 400.0, 1.0, 0.0, false).is_none());
        assert!(thumb_geometry(spec(), 400.0, 1.5, 0.0, false).is_none());
        assert!(!has_overflow(1.0));
        assert!(has_overflow(0.5));
    }

    #[test]
    fn idle_and_hover_thumbs_differ_only_in_width_and_stay_centred() {
        let spec = spec();
        let idle = thumb_geometry(spec, 400.0, 0.5, 0.5, false).expect("idle thumb");
        let hovered = thumb_geometry(spec, 400.0, 0.5, 0.5, true).expect("hover thumb");
        assert_eq!(idle.width, spec.idle_thumb_width);
        assert_eq!(hovered.width, spec.active_thumb_width);
        assert!(hovered.width > idle.width);
        // Same vertical geometry: hovering must not appear to scroll the view.
        assert_eq!(idle.top, hovered.top);
        assert_eq!(idle.height, hovered.height);
        // Both centred in the same gutter, so growing is symmetric and the
        // content beside the bar never shifts.
        assert_eq!(
            idle.left + idle.width / 2.0,
            hovered.left + hovered.width / 2.0
        );
    }

    #[test]
    fn the_thumb_never_leaves_its_track() {
        let spec = spec();
        let track = track_geometry(spec, 300.0);
        for fraction in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let thumb = thumb_geometry(spec, 300.0, 0.2, fraction, false).expect("thumb");
            assert!(
                thumb.top >= track.top,
                "thumb above the track at {fraction}"
            );
            assert!(
                thumb.top + thumb.height <= track.top + track.height + f32::EPSILON,
                "thumb below the track at {fraction}"
            );
        }
    }

    #[test]
    fn a_long_document_still_gets_a_grabbable_thumb() {
        let spec = spec();
        let thumb = thumb_geometry(spec, 600.0, 0.001, 0.0, false).expect("thumb");
        assert_eq!(thumb.height, spec.min_thumb_length);
    }

    #[test]
    fn pointer_mapping_is_the_inverse_of_the_paint() {
        let spec = spec();
        let gutter_height = 500.0;
        let visible = 0.3;
        for fraction in [0.0, 0.2, 0.5, 0.9, 1.0] {
            let thumb =
                thumb_geometry(spec, gutter_height, visible, fraction, true).expect("thumb");
            let centre = thumb.top + thumb.height / 2.0;
            let recovered = scroll_fraction_for_pointer(spec, gutter_height, visible, centre);
            assert!(
                (recovered - fraction).abs() < 1e-4,
                "pointer at the thumb centre for {fraction} recovered {recovered}"
            );
        }
    }

    #[test]
    fn pointer_mapping_saturates_rather_than_running_off_the_ends() {
        let spec = spec();
        assert_eq!(scroll_fraction_for_pointer(spec, 500.0, 0.3, -400.0), 0.0);
        assert_eq!(scroll_fraction_for_pointer(spec, 500.0, 0.3, 9000.0), 1.0);
    }

    /// Sabotage guard: the geometry must be a function of the tokens, not of
    /// constants that happen to equal them. Changing a token has to move the
    /// answer, or nothing here is really reading the spec.
    #[test]
    fn geometry_follows_the_token_spec_rather_than_local_constants() {
        let mut spec = spec();
        let before = thumb_geometry(spec, 400.0, 0.5, 0.5, false).expect("thumb");
        spec.idle_thumb_width += 3.0;
        let after = thumb_geometry(spec, 400.0, 0.5, 0.5, false).expect("thumb");
        assert_eq!(after.width, before.width + 3.0);
        assert_eq!(after.radius, after.width / 2.0);

        let mut spec = super::super::tokens::dark(Density::Comfortable, Scale::Scale100).scrollbar;
        spec.min_thumb_length += 40.0;
        let long = thumb_geometry(spec, 600.0, 0.001, 0.0, false).expect("thumb");
        assert_eq!(long.height, spec.min_thumb_length);
    }

    #[test]
    fn handle_ratios_come_from_content_and_viewport() {
        assert_eq!(visible_fraction_of(1000.0, 250.0), 0.25);
        // No content measured yet: never paint a thumb rather than paint a
        // wrong one.
        assert_eq!(visible_fraction_of(0.0, 250.0), 1.0);
        assert_eq!(visible_fraction_of(1000.0, 0.0), 1.0);
        assert_eq!(scroll_fraction_of(0.0, 1000.0, 250.0), 0.0);
        assert_eq!(scroll_fraction_of(-750.0, 1000.0, 250.0), 1.0);
        assert_eq!(scroll_fraction_of(-375.0, 1000.0, 250.0), 0.5);
        // Offsets past the end saturate instead of pushing the thumb out.
        assert_eq!(scroll_fraction_of(-5000.0, 1000.0, 250.0), 1.0);
        assert_eq!(scroll_fraction_of(0.0, 100.0, 250.0), 0.0);
    }
}

/// A scrollable container with the app's scrollbar over it.
///
/// This is the shape `gpui_component`'s `.overflow_y_scrollbar()` has -- the
/// element keeps its own `ScrollHandle` in element state, so a call site does
/// not have to own one -- with our scrollbar in place of theirs. Existing call
/// sites change one method name and nothing else.
#[derive(IntoElement)]
pub struct AppScrollableY<E: InteractiveElement + Styled + ParentElement + Element> {
    id: ElementId,
    element: E,
    spec: ScrollbarTokens,
    background: Color,
}

/// `.app_scroll_y(tokens)` on any `Div` or `Stateful<Div>`.
pub trait AppScrollableElement: InteractiveElement + Styled + ParentElement + Element {
    /// Vertical scrolling with the app's scrollbar, coloured for the shell's
    /// canvas. Surfaces that are not the shell's polarity -- the terminal is
    /// the only one today -- must not use this: they resolve their own ground.
    #[track_caller]
    fn app_scroll_y(self, tokens: ThemeTokens) -> AppScrollableY<Self> {
        let caller = std::panic::Location::caller();
        AppScrollableY {
            id: ElementId::CodeLocation(*caller),
            element: self,
            spec: tokens.scrollbar,
            background: tokens.surfaces.canvas,
        }
    }
}

impl AppScrollableElement for Div {}
impl<E> AppScrollableElement for Stateful<E>
where
    E: ParentElement + Styled + Element,
    Self: InteractiveElement,
{
}

impl<E> Styled for AppScrollableY<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn style(&mut self) -> &mut StyleRefinement {
        self.element.style()
    }
}

impl<E> ParentElement for AppScrollableY<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn extend(&mut self, elements: impl IntoIterator<Item = gpui::AnyElement>) {
        self.element.extend(elements)
    }
}

impl<E> RenderOnce for AppScrollableY<E>
where
    E: InteractiveElement + Styled + ParentElement + Element + 'static,
{
    fn render(mut self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let scroll_handle = window
            .use_keyed_state(self.id.clone(), cx, |_, _| ScrollHandle::default())
            .read(cx)
            .clone();

        let style = self.element.style().clone();
        *self.element.style() = StyleRefinement::default();

        div()
            .id(self.id.clone())
            .size_full()
            .refine_style(&style)
            .relative()
            .child(
                div()
                    .id("app-scroll-area")
                    .flex()
                    .flex_col()
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&scroll_handle)
                    .child(self.element.flex_1()),
            )
            .child(AppScrollbar::vertical(
                self.id,
                &scroll_handle,
                self.spec,
                self.background,
            ))
    }
}
