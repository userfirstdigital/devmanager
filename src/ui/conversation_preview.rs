//! THROWAWAY look prototype for the conversation-first redesign.
//!
//! This module renders a fabricated conversation using the treatments recorded
//! in `docs/superpowers/specs/2026-08-20-conversation-derivation-single-source-design.md`
//! under "Target UX". It exists to answer one question before any plan is
//! written: can GPUI land the look?
//!
//! It is deliberately not wired to the journal, the host, or any real model.
//! Every string here is fake. Delete this module once the real derivation and
//! renderers land.

use gpui::{
    div, px, size, AnyElement, AppContext, Bounds, Context, FontWeight, IntoElement,
    ParentElement, Render, Styled, Window, WindowBounds, WindowOptions,
};

use crate::ui::task_cockpit::timeline::CONVERSATION_CONTENT_MAX_WIDTH;
use crate::ui::tokens::{Color, RuntimePreferencesSnapshot, ThemeTokens};

/// Target: the user bubble caps at 80 percent of the readable measure.
const USER_BUBBLE_FRACTION: f32 = 0.80;
/// Target: 16 px radius on the user bubble.
const USER_BUBBLE_RADIUS: f32 = 16.0;
/// Target: 22 px outer / 20 px inner, with the 1 px difference drawn as padding.
const COMPOSER_OUTER_RADIUS: f32 = 22.0;
const COMPOSER_INNER_RADIUS: f32 = 20.0;
/// Target: the primary send control is a 32 px circle.
const SEND_DIAMETER: f32 = 32.0;
/// Target: work-entry icons occupy a 20 px slot.
const ICON_SLOT: f32 = 20.0;
/// Target: the working indicator uses three 4 px dots.
const WORKING_DOT: f32 = 4.0;

/// Blend two token colors. The target palette leans on fractional surfaces
/// (`--accent/20`, `--border/60`) that the token set does not carry directly,
/// so the prototype mixes them rather than inventing new literals.
pub(crate) fn mix(base: Color, other: Color, amount: f32) -> Color {
    let amount = amount.clamp(0.0, 1.0);
    let blend = |a: u8, b: u8| -> u8 {
        let a = f32::from(a);
        let b = f32::from(b);
        (a + (b - a) * amount).round().clamp(0.0, 255.0) as u8
    };
    Color::rgb(
        blend(base.red(), other.red()),
        blend(base.green(), other.green()),
        blend(base.blue(), other.blue()),
    )
}

/// The centered readable column every conversation row shares.
fn column() -> gpui::Div {
    // A definite width, not `w_full().max_w(..)`. The max_w form did not clamp
    // here -- the column grew past the measure and overflowed the window.
    div().w(px(CONVERSATION_CONTENT_MAX_WIDTH))
}

/// Centers a fixed-measure column. `mx_auto` collapses the child to zero width
/// in this layout, so centering is done by the parent instead of auto margins.
fn centered(child: AnyElement) -> AnyElement {
    div()
        .w_full()
        .flex()
        .justify_center()
        .child(child)
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// Turn fold. The only hairline anywhere in the transcript.
fn turn_fold_row(label: &str, tokens: ThemeTokens) -> AnyElement {
    div()
        .w_full()
        .pt(px(4.0))
        .pb(px(8.0))
        .border_b(px(1.0))
        .border_color(mix(tokens.surfaces.canvas, tokens.borders.subtle, 0.60).to_gpui())
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .px(px(4.0))
                .rounded(px(tokens.density.radii.sm))
                .text_size(px(tokens.density.typography.caption))
                .text_color(tokens.text.muted.to_gpui())
                .child(label.to_string())
                .child("v"),
        )
        .into_any_element()
}

/// Assistant turn. No surface, no border, no avatar, no role label.
fn assistant_row(paragraphs: &[&str], tokens: ThemeTokens) -> AnyElement {
    let mut block = div()
        .w_full()
        .px(px(4.0))
        .py(px(2.0))
        .flex()
        .flex_col()
        .gap(px(tokens.density.spacing.sm))
        .text_color(tokens.text.primary.to_gpui())
        .line_height(px(tokens.density.typography.body_line_height));
    for paragraph in paragraphs {
        block = block.child(div().w_full().child((*paragraph).to_string()));
    }
    block.into_any_element()
}

/// Assistant heading, to show markdown hierarchy inside a bare block.
fn assistant_heading(text: &str, tokens: ThemeTokens) -> AnyElement {
    div()
        .w_full()
        .px(px(4.0))
        .pt(px(tokens.density.spacing.md))
        .pb(px(tokens.density.spacing.xs))
        .text_size(px(tokens.density.typography.title))
        .line_height(px(tokens.density.typography.title_line_height))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(tokens.text.primary.to_gpui())
        .child(text.to_string())
        .into_any_element()
}

/// User turn. Right-aligned pill, the only surfaced message in the transcript.
fn user_row(text: &str, tokens: ThemeTokens) -> AnyElement {
    // Right alignment uses justify_end on a row. Aligning with items_end on a
    // column collapses its children to zero width in GPUI and they never paint
    // -- measured, not assumed: that probe rendered nothing while an otherwise
    // identical probe without it rendered fine. timeline.rs already uses the
    // justify_end form.
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div().w_full().flex().justify_end().child(
                div()
                    .max_w(px(CONVERSATION_CONTENT_MAX_WIDTH * USER_BUBBLE_FRACTION))
                    .p(px(12.0))
                    .rounded(px(USER_BUBBLE_RADIUS))
                    .bg(tokens.surfaces.raised.to_gpui())
                    .text_color(tokens.text.primary.to_gpui())
                    .child(text.to_string()),
            ),
        )
        // Meta row. Painted here at rest only because a still capture cannot
        // hover; in the real surface this is hidden until hover or focus.
        .child(
            div()
                .w_full()
                .flex()
                .justify_end()
                .items_center()
                .gap(px(8.0))
                .pr(px(4.0))
                .text_size(px(tokens.density.typography.caption))
                .text_color(mix(tokens.surfaces.canvas, tokens.text.muted, 0.55).to_gpui())
                .child("14:32")
                .child("Copy")
                .child("Revert"),
        )
        .into_any_element()
}

/// One work / tool entry. Quiet by default; failure recolors the heading only.
fn work_row(icon: &str, heading: &str, detail: &str, tone: WorkTone, tokens: ThemeTokens) -> AnyElement {
    let heading_color = match tone {
        WorkTone::Normal => tokens.text.primary,
        WorkTone::Warning => tokens.status.warning,
        WorkTone::Failed => tokens.status.destructive,
    };
    let background = match tone {
        // Shown in its hover state so the wash is visible in a still capture.
        WorkTone::Warning => mix(tokens.surfaces.canvas, tokens.surfaces.hover, 0.20),
        _ => tokens.surfaces.canvas,
    };
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(6.0))
        .px(px(2.0))
        .py(px(2.0))
        .rounded(px(tokens.density.radii.sm))
        .bg(background.to_gpui())
        .child(
            div()
                .flex_none()
                .size(px(ICON_SLOT))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(tokens.density.typography.caption))
                .text_color(tokens.text.muted.to_gpui())
                .child(icon.to_string()),
        )
        .child(
            div()
                .text_size(px(tokens.density.typography.caption))
                .font_weight(FontWeight::MEDIUM)
                .text_color(heading_color.to_gpui())
                .child(heading.to_string()),
        )
        .child(
            div()
                .text_size(px(tokens.density.typography.caption))
                .text_color(tokens.text.muted.to_gpui())
                .child(format!("- {detail}")),
        )
        .into_any_element()
}

#[derive(Clone, Copy)]
enum WorkTone {
    Normal,
    Warning,
    Failed,
}

/// Collapsed work group. Copy is count- and kind-aware in the real derivation.
fn work_toggle_row(hidden: usize, tokens: ThemeTokens) -> AnyElement {
    let noun = if hidden == 1 {
        "previous tool call"
    } else {
        "previous tool calls"
    };
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(6.0))
        .px(px(2.0))
        .py(px(2.0))
        .rounded(px(tokens.density.radii.sm))
        .child(
            div()
                .flex_none()
                .size(px(ICON_SLOT))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(tokens.density.typography.caption))
                .text_color(tokens.text.muted.to_gpui())
                .child("v"),
        )
        .child(
            div()
                .text_size(px(tokens.density.typography.caption))
                .font_weight(FontWeight::MEDIUM)
                .text_color(tokens.text.primary.to_gpui())
                .child(format!("+{hidden} {noun}")),
        )
        .into_any_element()
}

/// Working indicator. Three dots plus a self-ticking elapsed label.
fn working_row(elapsed: &str, step: &str, tokens: ThemeTokens) -> AnyElement {
    let dot = || {
        div()
            .flex_none()
            .size(px(WORKING_DOT))
            .rounded_full()
            .bg(mix(tokens.surfaces.canvas, tokens.text.muted, 0.30).to_gpui())
    };
    div()
        .w_full()
        .pl(px(6.0))
        .py(px(2.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(px(11.0))
        .text_color(tokens.text.secondary.to_gpui())
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(3.0))
                .child(dot())
                .child(dot())
                .child(dot()),
        )
        .child(div().child(format!("Working for {elapsed}")))
        .child(
            div()
                .text_color(mix(tokens.surfaces.canvas, tokens.text.muted, 0.55).to_gpui())
                .child(format!("- {step}")),
        )
        .into_any_element()
}

/// Changed-files summary attached beneath an assistant turn.
fn changed_files_row(tokens: ThemeTokens, files: usize, added: u32, removed: u32) -> AnyElement {
    div()
        .w_full()
        .mt(px(tokens.density.spacing.sm))
        .p(px(8.0))
        .rounded(px(tokens.density.radii.lg))
        .bg(tokens.surfaces.sunken.to_gpui())
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(px(tokens.density.typography.caption))
        .child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .text_color(tokens.text.primary.to_gpui())
                .child(format!("{files} changed files")),
        )
        .child(
            div()
                .text_color(tokens.status.success.to_gpui())
                .child(format!("+{added}")),
        )
        .child(
            div()
                .text_color(tokens.status.destructive.to_gpui())
                .child(format!("-{removed}")),
        )
        .child(
            div()
                .text_color(tokens.text.muted.to_gpui())
                .child("Show files"),
        )
        .into_any_element()
}

/// Approval callout. One of the two surfaces that is allowed to be prominent.
fn approval_card(command: &str, tokens: ThemeTokens) -> AnyElement {
    div()
        .w_full()
        .px(px(16.0))
        .py(px(14.0))
        .rounded(px(tokens.density.radii.lg))
        .bg(tokens.surfaces.raised.to_gpui())
        .flex()
        .flex_col()
        .gap(px(tokens.density.spacing.sm))
        .child(
            div()
                .text_size(px(tokens.density.typography.caption))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tokens.text.secondary.to_gpui())
                .child("A P P R O V A L   R E Q U E S T E D"),
        )
        .child(
            div()
                .w_full()
                .p(px(12.0))
                .rounded(px(tokens.density.radii.md))
                .bg(tokens.surfaces.sunken.to_gpui())
                .text_size(px(tokens.density.typography.code))
                .line_height(px(tokens.density.typography.code_line_height))
                .text_color(tokens.text.primary.to_gpui())
                .child(command.to_string()),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(pill("Allow once", true, tokens))
                .child(pill("Allow always", false, tokens))
                .child(pill("Deny", false, tokens)),
        )
        .into_any_element()
}

/// A composer control pill, also reused for approval actions.
fn pill(label: &str, primary: bool, tokens: ThemeTokens) -> AnyElement {
    let (background, foreground) = if primary {
        (
            tokens.actions.primary.default.background,
            tokens.actions.primary.default.foreground,
        )
    } else {
        (tokens.surfaces.sunken, tokens.text.secondary)
    };
    div()
        .flex()
        .items_center()
        .gap(px(4.0))
        .px(px(10.0))
        .py(px(5.0))
        .rounded(px(tokens.density.radii.pill))
        .bg(background.to_gpui())
        .text_size(px(tokens.density.typography.caption))
        .text_color(foreground.to_gpui())
        .child(label.to_string())
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Composer
// ---------------------------------------------------------------------------

fn composer(tokens: ThemeTokens) -> AnyElement {
    let hairline = mix(tokens.surfaces.canvas, tokens.borders.subtle, 0.65);
    div()
        .w(px(CONVERSATION_CONTENT_MAX_WIDTH))
        // Outer ring: 1 px of padding is the border.
        .p(px(1.0))
        .rounded(px(COMPOSER_OUTER_RADIUS))
        .bg(hairline.to_gpui())
        .child(
            div()
                .w_full()
                .rounded(px(COMPOSER_INNER_RADIUS))
                .bg(tokens.surfaces.raised.to_gpui())
                .flex()
                .flex_col()
                .child(
                    div()
                        .w_full()
                        .px(px(16.0))
                        .pt(px(14.0))
                        .pb(px(28.0))
                        .text_color(tokens.text.muted.to_gpui())
                        .child("Ask anything, @tag files/folders, $use skills, or / for commands"),
                )
                .child(
                    div()
                        .w_full()
                        .px(px(12.0))
                        .pb(px(12.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(pill("Claude Opus 5", false, tokens))
                        .child(pill("High - 1M", false, tokens))
                        .child(pill("Full access", false, tokens))
                        .child(div().flex_1())
                        .child(
                            div()
                                .flex_none()
                                .size(px(SEND_DIAMETER))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(tokens.actions.primary.default.background.to_gpui())
                                .text_color(
                                    tokens.actions.primary.default.foreground.to_gpui(),
                                )
                                .child("^"),
                        ),
                ),
        )
        .into_any_element()
}

/// Context strip beneath the composer. In T3 Code this is one continuous glass
/// shape with the composer; GPUI cannot clip an arbitrary path, so this is the
/// two-surface approximation the spec names under "Deliberate divergences".
fn composer_context_strip(tokens: ThemeTokens) -> AnyElement {
    div()
        .w(px(CONVERSATION_CONTENT_MAX_WIDTH - 44.0))
        .px(px(12.0))
        .py(px(6.0))
        .rounded(px(tokens.density.radii.lg))
        .bg(tokens.surfaces.sunken.to_gpui())
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(px(tokens.density.typography.caption))
        .text_color(tokens.text.muted.to_gpui())
        .child(div().child("Local checkout"))
        .child(div().flex_1())
        .child(div().child("master"))
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Root
// ---------------------------------------------------------------------------

/// Renders the whole fabricated conversation plus composer.
pub fn conversation_preview_element(tokens: ThemeTokens) -> AnyElement {
    let transcript = column()
        .flex()
        .flex_col()
        .gap(px(tokens.density.spacing.lg))
        .child(turn_fold_row("Earlier turn", tokens))
        .child(user_row(
            "the alerting suite is flaky again - can you find out why",
            tokens,
        ))
        .child(work_row(
            "*",
            "Read",
            "src/alerting/ladder.rs",
            WorkTone::Normal,
            tokens,
        ))
        .child(work_row(
            "!",
            "Bash",
            "cargo test --lib alerting (2 failed)",
            WorkTone::Warning,
            tokens,
        ))
        .child(work_row(
            "x",
            "Bash",
            "cargo test --lib snooze (exit 101)",
            WorkTone::Failed,
            tokens,
        ))
        .child(work_toggle_row(3, tokens))
        .child(assistant_row(
            &[
                "Two of the three failures share a cause. The snooze ladder reuses one clock \
                 for both the escalation deadline and the suppression window, so a test that \
                 advances time to assert escalation also silently expires the snooze.",
                "The third is unrelated and was already failing before this branch.",
            ],
            tokens,
        ))
        .child(assistant_heading("What I checked and left alone", tokens))
        .child(assistant_row(
            &[
                "I did not touch the Redis path. It is reachable from the same module but \
                 nothing in the failing suites exercises it, and changing it would widen the \
                 diff without evidence.",
            ],
            tokens,
        ))
        .child(changed_files_row(tokens, 10, 76, 16))
        .child(approval_card(
            "cargo test --lib -- --test-threads=1",
            tokens,
        ))
        .child(working_row("1m 12s", "running the alerting suites", tokens));

    // Content-height flow, not `size_full`. A full-height root inside the
    // preview harness's own full-height column consumed the frame and pushed
    // every child (including the harness sentinel) out of the capture.
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(tokens.density.spacing.lg))
        .bg(tokens.surfaces.canvas.to_gpui())
        .text_size(px(tokens.density.typography.body))
        .text_color(tokens.text.primary.to_gpui())
        .child(centered(transcript.into_any_element()))
        .child(centered(
            column()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(composer(tokens))
                .child(composer_context_strip(tokens))
                .into_any_element(),
        ))
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Live window
// ---------------------------------------------------------------------------

/// Root entity for the throwaway `--ui-proto` window.
pub struct PrototypeWindow;

impl Render for PrototypeWindow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = RuntimePreferencesSnapshot::default().tokens();
        div()
            .size_full()
            .bg(tokens.surfaces.canvas.to_gpui())
            .p(px(24.0))
            .child(conversation_preview_element(tokens))
    }
}

/// Opens a resizable window rendering the prototype at a realistic size.
///
/// The `--ui-preview` capture harness is locked to a 640x360 contract, which
/// cannot show an 860 px conversation column. This is debug-only and throwaway.
pub fn run_prototype_window() {
    let application = gpui::Application::new().with_assets(crate::assets::AppAssets::new());
    application.run(move |cx| {
        crate::ui::init(cx);
        let _ = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1000.0), px(1300.0)),
                    cx,
                ))),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| PrototypeWindow),
        );
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tokens::{theme, Density, Scale, ThemeMode};

    #[test]
    fn mix_returns_endpoints_unchanged() {
        let a = Color::rgb(0, 0, 0);
        let b = Color::rgb(255, 255, 255);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
    }

    #[test]
    fn mix_clamps_out_of_range_amounts() {
        let a = Color::rgb(10, 20, 30);
        let b = Color::rgb(200, 210, 220);
        assert_eq!(mix(a, b, -1.0), a);
        assert_eq!(mix(a, b, 2.0), b);
    }

    #[test]
    fn user_bubble_never_exceeds_the_readable_measure() {
        assert!(CONVERSATION_CONTENT_MAX_WIDTH * USER_BUBBLE_FRACTION < CONVERSATION_CONTENT_MAX_WIDTH);
    }

    #[test]
    fn composer_inner_radius_accounts_for_the_one_pixel_ring_on_both_sides() {
        // The ring is drawn as 1 px of padding, so the inner radius is 2 px
        // smaller than the outer one, not 1 px.
        assert_eq!(COMPOSER_OUTER_RADIUS - COMPOSER_INNER_RADIUS, 2.0);
    }

    #[test]
    fn preview_element_builds_for_dark_and_light() {
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            let tokens = theme(mode, Density::Comfortable, Scale::Scale100);
            let _ = conversation_preview_element(tokens);
        }
    }
}
