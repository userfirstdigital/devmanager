//! Shared compact, keyboard-focusable tabs with an overflow menu.
use crate::ui::tokens::ThemeTokens;
use gpui::{div, px, AnyElement, App, ElementId, IntoElement, Styled, Window};
use gpui_component::{
    tab::{Tab, TabBar},
    Sizable,
};

pub fn tab_strip(
    id: impl Into<ElementId>,
    labels: Vec<String>,
    selected: Option<usize>,
    tokens: ThemeTokens,
    on_select: impl Fn(&usize, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let mut tabs = TabBar::new(id)
        .underline()
        .small()
        .menu(true)
        .min_w(px(0.0))
        .flex_1()
        .text_color(tokens.text.secondary.to_gpui())
        .last_empty_space(div().w(px(0.0)))
        .children(labels.into_iter().map(|label| Tab::new().label(label)))
        .on_click(on_select);
    if let Some(index) = selected {
        tabs = tabs.selected_index(index);
    }
    tabs.into_any_element()
}
