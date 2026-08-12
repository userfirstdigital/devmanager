use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use devmanager::theme;
use devmanager::ui::tokens::{
    contrast_ratio, srgb_luminance, theme as build_theme, Color, ContrastPair, Density, Scale,
    TerminalSlotRole, ThemeMode, ThemeTokens,
};
use serde::Deserialize;
use serde_json::Value;

const EXPECTED_COLOR_KEYS: &[&str] = &[
    "text_primary",
    "text_secondary",
    "text_muted",
    "text_disabled",
    "text_inverse",
    "text_on_accent",
    "text_on_selection",
    "surface_canvas",
    "surface_raised",
    "surface_overlay",
    "surface_sunken",
    "surface_hover",
    "surface_selected",
    "surface_disabled",
    "border_subtle",
    "border_default",
    "border_strong",
    "border_focus",
    "border_selection",
    "border_disabled",
    "action_primary_default",
    "action_primary_hover",
    "action_primary_focus",
    "action_primary_selected",
    "action_primary_disabled",
    "action_primary_foreground",
    "action_destructive_default",
    "action_destructive_hover",
    "action_destructive_focus",
    "action_destructive_selected",
    "action_destructive_disabled",
    "action_destructive_foreground",
    "status_external",
    "status_attention",
    "status_success",
    "status_warning",
    "status_destructive",
    "status_inactive",
    "status_external_surface",
    "status_external_foreground",
    "status_attention_surface",
    "status_attention_foreground",
    "status_success_surface",
    "status_success_foreground",
    "status_warning_surface",
    "status_warning_foreground",
    "status_destructive_surface",
    "status_destructive_foreground",
    "status_inactive_surface",
    "status_inactive_foreground",
    "terminal_background",
    "terminal_foreground",
    "terminal_cursor",
    "terminal_selection",
    "terminal_black",
    "terminal_red",
    "terminal_green",
    "terminal_yellow",
    "terminal_blue",
    "terminal_magenta",
    "terminal_cyan",
    "terminal_white",
    "terminal_bright_black",
    "terminal_bright_red",
    "terminal_bright_green",
    "terminal_bright_yellow",
    "terminal_bright_blue",
    "terminal_bright_magenta",
    "terminal_bright_cyan",
    "terminal_bright_white",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeMatrixFixture {
    schema_version: u16,
    content: MatrixContent,
    interaction_bindings: Vec<MatrixInteractionBinding>,
    status_bindings: Vec<MatrixStatusBinding>,
    cases: Vec<ThemeMatrixCase>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixContent {
    long_text: String,
    unicode: String,
    disabled_text: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixInteractionBinding {
    state: MatrixInteractionState,
    content_key: MatrixContentKey,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixStatusBinding {
    status: MatrixStatus,
    content_key: MatrixContentKey,
}

#[derive(Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
enum MatrixInteractionState {
    Default,
    Hover,
    Focus,
    Selected,
    Disabled,
}

#[derive(Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
enum MatrixStatus {
    External,
    Attention,
    Success,
    Warning,
    Destructive,
    Inactive,
}

#[derive(Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
enum MatrixContentKey {
    LongText,
    Unicode,
    DisabledText,
}

#[derive(Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
enum MatrixTheme {
    Dark,
    Light,
}

#[derive(Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
enum MatrixDensity {
    Compact,
    Comfortable,
}

#[derive(Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
enum MatrixScale {
    #[serde(rename = "100%")]
    Scale100,
    #[serde(rename = "125%")]
    Scale125,
    #[serde(rename = "150%")]
    Scale150,
    #[serde(rename = "200%")]
    Scale200,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeMatrixCase {
    theme: MatrixTheme,
    density: MatrixDensity,
    scale: MatrixScale,
}

fn load_theme_matrix() -> ThemeMatrixFixture {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ui/theme-matrix.json"
    )))
    .expect("typed theme matrix fixture")
}

fn matrix_case_tokens(case: &ThemeMatrixCase) -> ThemeTokens {
    let mode = match case.theme {
        MatrixTheme::Dark => ThemeMode::Dark,
        MatrixTheme::Light => ThemeMode::Light,
    };
    let density = match case.density {
        MatrixDensity::Compact => Density::Compact,
        MatrixDensity::Comfortable => Density::Comfortable,
    };
    let scale = match case.scale {
        MatrixScale::Scale100 => Scale::Scale100,
        MatrixScale::Scale125 => Scale::Scale125,
        MatrixScale::Scale150 => Scale::Scale150,
        MatrixScale::Scale200 => Scale::Scale200,
    };
    build_theme(mode, density, scale)
}

fn colors(tokens: &ThemeTokens) -> BTreeSet<&'static str> {
    tokens
        .semantic_color_tokens()
        .into_iter()
        .map(|token| token.name)
        .collect()
}

fn assert_color_contrast(tokens: &ThemeTokens) {
    for pair in tokens.normal_text_contrast_pairs() {
        assert!(
            pair.foreground.is_opaque(),
            "{} has a transparent foreground",
            pair.name
        );
        assert!(
            contrast_ratio(pair.foreground, pair.background) >= 4.5,
            "{} must provide 4.5:1 normal-text contrast, got {:.3}",
            pair.name,
            contrast_ratio(pair.foreground, pair.background)
        );
    }

    for pair in tokens.large_text_contrast_pairs() {
        assert!(
            pair.foreground.is_opaque(),
            "{} has a transparent foreground",
            pair.name
        );
        assert!(
            contrast_ratio(pair.foreground, pair.background) >= 3.0,
            "{} must provide 3:1 large-text contrast, got {:.3}",
            pair.name,
            contrast_ratio(pair.foreground, pair.background)
        );
    }

    for pair in tokens.ui_indicator_contrast_pairs() {
        assert!(
            pair.foreground.is_opaque(),
            "{} has a transparent foreground",
            pair.name
        );
        assert!(
            contrast_ratio(pair.foreground, pair.background) >= 3.0,
            "{} must provide 3:1 UI-indicator contrast, got {:.3}",
            pair.name,
            contrast_ratio(pair.foreground, pair.background)
        );
    }

    for pair in tokens.disabled_text_contrast_pairs() {
        assert!(
            pair.foreground.is_opaque(),
            "{} has a transparent foreground",
            pair.name
        );
        assert!(
            contrast_ratio(pair.foreground, pair.background) >= 4.5,
            "{} must keep disabled text readable at 4.5:1, got {:.3}",
            pair.name,
            contrast_ratio(pair.foreground, pair.background)
        );
    }

    for pair in tokens.interaction_state_contrast_pairs() {
        assert!(
            pair.foreground.is_opaque(),
            "{} has a transparent foreground",
            pair.name
        );
        assert!(
            contrast_ratio(pair.foreground, pair.background) >= 4.5,
            "{} must provide 4.5:1 normal-text contrast, got {:.3}",
            pair.name,
            contrast_ratio(pair.foreground, pair.background)
        );
    }

    for pair in tokens.status_surface_contrast_pairs() {
        assert!(
            pair.foreground.is_opaque(),
            "{} has a transparent foreground",
            pair.name
        );
        assert!(
            contrast_ratio(pair.foreground, pair.background) >= 4.5,
            "{} must provide 4.5:1 normal-text contrast, got {:.3}",
            pair.name,
            contrast_ratio(pair.foreground, pair.background)
        );
    }
}

fn minimum_contrast_pair(pairs: &[ContrastPair]) -> ContrastPair {
    *pairs
        .iter()
        .min_by(|left, right| {
            contrast_ratio(left.foreground, left.background)
                .partial_cmp(&contrast_ratio(right.foreground, right.background))
                .expect("finite contrast ratio")
        })
        .expect("non-empty contrast pair set")
}

#[test]
fn exhaustive_contrast_diagnostics_report_minimum_ratios() {
    let mut minima = [
        ("normal_text", f64::INFINITY, ""),
        ("large_text", f64::INFINITY, ""),
        ("ui_indicator", f64::INFINITY, ""),
        ("disabled_text", f64::INFINITY, ""),
        ("interaction_text", f64::INFINITY, ""),
        ("status_surface", f64::INFINITY, ""),
    ];

    for case in load_theme_matrix().cases {
        let tokens = matrix_case_tokens(&case);
        let categories = [
            ("normal_text", tokens.normal_text_contrast_pairs()),
            ("large_text", tokens.large_text_contrast_pairs()),
            ("ui_indicator", tokens.ui_indicator_contrast_pairs()),
            ("disabled_text", tokens.disabled_text_contrast_pairs()),
            (
                "interaction_text",
                tokens.interaction_state_contrast_pairs(),
            ),
            ("status_surface", tokens.status_surface_contrast_pairs()),
        ];
        for (category, pairs) in categories {
            let pair = minimum_contrast_pair(&pairs);
            let ratio = contrast_ratio(pair.foreground, pair.background);
            let (_, minimum, minimum_name) = minima
                .iter_mut()
                .find(|(name, _, _)| *name == category)
                .expect("diagnostic category");
            if ratio < *minimum {
                *minimum = ratio;
                *minimum_name = pair.name;
            }
        }
    }

    for (category, ratio, name) in minima {
        println!("minimum {category} contrast: {ratio:.3}:1 ({name})");
    }
}

fn assert_orange(color: Color) {
    assert!(
        color.red() > color.green() && color.green() > color.blue(),
        "expected orange-family status color, got {}",
        color.to_hex()
    );
}

fn assert_green(color: Color) {
    assert!(
        color.green() > color.red() && color.green() > color.blue(),
        "expected green-family status color, got {}",
        color.to_hex()
    );
}

fn assert_red(color: Color) {
    assert!(
        color.red() > color.green() && color.red() > color.blue(),
        "expected red-family status color, got {}",
        color.to_hex()
    );
}

fn assert_blue(color: Color) {
    assert!(
        color.blue() > color.red() && color.blue() > color.green(),
        "expected blue-family external status color, got {}",
        color.to_hex()
    );
}

fn assert_neutral(color: Color) {
    let max = color.red().max(color.green()).max(color.blue());
    let min = color.red().min(color.green()).min(color.blue());
    assert!(
        max.saturating_sub(min) <= 80,
        "expected neutral inactive status color, got {}",
        color.to_hex()
    );
}

#[test]
fn dark_and_light_themes_share_one_complete_semantic_color_contract() {
    let matrix = load_theme_matrix();
    let dark_case = matrix
        .cases
        .iter()
        .find(|case| case.theme == MatrixTheme::Dark)
        .expect("dark matrix case");
    let light_case = matrix
        .cases
        .iter()
        .find(|case| case.theme == MatrixTheme::Light)
        .expect("light matrix case");
    let dark = matrix_case_tokens(dark_case);
    let light = matrix_case_tokens(light_case);

    assert_eq!(colors(&dark), colors(&light));
    for key in EXPECTED_COLOR_KEYS {
        assert!(
            colors(&dark).contains(key),
            "missing semantic color key {key}"
        );
    }
    assert_eq!(colors(&dark).len(), EXPECTED_COLOR_KEYS.len());
    assert_ne!(dark.text.primary, light.text.primary);
    assert_ne!(dark.surfaces.canvas, light.surfaces.canvas);
}

#[test]
fn every_semantic_color_and_contrast_foreground_is_opaque_and_conforming() {
    for case in load_theme_matrix().cases {
        let tokens = matrix_case_tokens(&case);
        for token in tokens.semantic_color_tokens() {
            assert!(token.color.is_opaque(), "{} is not opaque", token.name);
        }
        assert_color_contrast(&tokens);
    }
}

#[test]
fn action_state_backgrounds_and_borders_are_visible_against_their_owning_surface() {
    for case in load_theme_matrix().cases {
        let tokens = matrix_case_tokens(&case);
        let action_states = [
            ("action_primary_default", tokens.actions.primary.default),
            ("action_primary_hover", tokens.actions.primary.hover),
            ("action_primary_focus", tokens.actions.primary.focus),
            ("action_primary_selected", tokens.actions.primary.selected),
            ("action_primary_disabled", tokens.actions.primary.disabled),
            (
                "action_destructive_default",
                tokens.actions.destructive.default,
            ),
            ("action_destructive_hover", tokens.actions.destructive.hover),
            ("action_destructive_focus", tokens.actions.destructive.focus),
            (
                "action_destructive_selected",
                tokens.actions.destructive.selected,
            ),
            (
                "action_destructive_disabled",
                tokens.actions.destructive.disabled,
            ),
        ];
        for (name, state) in action_states {
            for (edge, color) in [("background", state.background), ("border", state.border)] {
                let ratio = contrast_ratio(color, tokens.surfaces.canvas);
                assert!(
                    ratio >= 3.0,
                    "{name}_{edge}_on_canvas must provide 3:1 UI-indicator contrast, got {ratio:.3}"
                );
            }
        }
    }
}

#[test]
fn ui_indicator_pairs_expose_every_action_background_and_border() {
    let expected = [
        "action_primary_default_background_on_canvas",
        "action_primary_default_border_on_canvas",
        "action_primary_hover_background_on_canvas",
        "action_primary_hover_border_on_canvas",
        "action_primary_focus_background_on_canvas",
        "action_primary_focus_border_on_canvas",
        "action_primary_selected_background_on_canvas",
        "action_primary_selected_border_on_canvas",
        "action_primary_disabled_background_on_canvas",
        "action_primary_disabled_border_on_canvas",
        "action_destructive_default_background_on_canvas",
        "action_destructive_default_border_on_canvas",
        "action_destructive_hover_background_on_canvas",
        "action_destructive_hover_border_on_canvas",
        "action_destructive_focus_background_on_canvas",
        "action_destructive_focus_border_on_canvas",
        "action_destructive_selected_background_on_canvas",
        "action_destructive_selected_border_on_canvas",
        "action_destructive_disabled_background_on_canvas",
        "action_destructive_disabled_border_on_canvas",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();

    for case in load_theme_matrix().cases {
        let tokens = matrix_case_tokens(&case);
        let actual = tokens
            .ui_indicator_contrast_pairs()
            .into_iter()
            .map(|pair| pair.name)
            .filter(|name| name.starts_with("action_"))
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
    }
}

#[test]
fn every_terminal_foreground_slot_is_named_and_contrast_compliant() {
    let expected = [
        "terminal_foreground_on_background",
        "terminal_black_on_background",
        "terminal_red_on_background",
        "terminal_green_on_background",
        "terminal_yellow_on_background",
        "terminal_blue_on_background",
        "terminal_magenta_on_background",
        "terminal_cyan_on_background",
        "terminal_white_on_background",
        "terminal_bright_black_on_background",
        "terminal_bright_red_on_background",
        "terminal_bright_green_on_background",
        "terminal_bright_yellow_on_background",
        "terminal_bright_blue_on_background",
        "terminal_bright_magenta_on_background",
        "terminal_bright_cyan_on_background",
        "terminal_bright_white_on_background",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();

    for case in load_theme_matrix().cases {
        let tokens = matrix_case_tokens(&case);
        let pairs = tokens
            .normal_text_contrast_pairs()
            .into_iter()
            .filter(|pair| pair.name.starts_with("terminal_"))
            .collect::<Vec<_>>();
        let actual = pairs.iter().map(|pair| pair.name).collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
        for pair in pairs {
            let ratio = contrast_ratio(pair.foreground, pair.background);
            assert!(
                ratio >= 4.5,
                "{} must provide 4.5:1 terminal foreground contrast, got {ratio:.3}",
                pair.name
            );
        }
    }
}

#[test]
fn terminal_slots_explicitly_classify_foreground_and_non_foreground_roles() {
    let expected_foreground = [
        "terminal_foreground",
        "terminal_black",
        "terminal_red",
        "terminal_green",
        "terminal_yellow",
        "terminal_blue",
        "terminal_magenta",
        "terminal_cyan",
        "terminal_white",
        "terminal_bright_black",
        "terminal_bright_red",
        "terminal_bright_green",
        "terminal_bright_yellow",
        "terminal_bright_blue",
        "terminal_bright_magenta",
        "terminal_bright_cyan",
        "terminal_bright_white",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    let expected_non_foreground = [
        "terminal_background",
        "terminal_cursor",
        "terminal_selection",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();

    let tokens = build_theme(ThemeMode::Dark, Density::Comfortable, Scale::Scale100);
    let slots = tokens.terminal.slots();
    assert_eq!(slots.len(), 20);
    assert_eq!(
        slots
            .iter()
            .filter(|slot| slot.is_foreground_capable())
            .map(|slot| slot.name)
            .collect::<BTreeSet<_>>(),
        expected_foreground
    );
    assert_eq!(
        slots
            .iter()
            .filter(|slot| !slot.is_foreground_capable())
            .map(|slot| slot.name)
            .collect::<BTreeSet<_>>(),
        expected_non_foreground
    );
    assert!(slots.iter().any(|slot| {
        slot.name == "terminal_background" && slot.role == TerminalSlotRole::Background
    }));
    assert!(slots.iter().any(|slot| {
        slot.name == "terminal_cursor" && slot.role == TerminalSlotRole::CursorIndicator
    }));
    assert!(slots.iter().any(|slot| {
        slot.name == "terminal_selection" && slot.role == TerminalSlotRole::SelectionBackground
    }));
}

#[test]
fn luminance_and_contrast_use_deterministic_srgb_calculation() {
    assert!((srgb_luminance(Color::rgb(0, 0, 0)) - 0.0).abs() < f64::EPSILON);
    assert!((srgb_luminance(Color::rgb(255, 255, 255)) - 1.0).abs() < f64::EPSILON);
    assert!((contrast_ratio(Color::rgb(255, 255, 255), Color::rgb(0, 0, 0)) - 21.0).abs() < 1e-12);
    assert!((contrast_ratio(Color::rgb(0, 0, 0), Color::rgb(255, 255, 255)) - 21.0).abs() < 1e-12);
}

#[test]
fn status_colors_encode_meaning_without_relying_on_text_or_shape() {
    for case in load_theme_matrix().cases {
        let status = matrix_case_tokens(&case).status;
        assert_blue(status.external);
        assert_orange(status.attention);
        assert_green(status.success);
        assert_orange(status.warning);
        assert_red(status.destructive);
        assert_neutral(status.inactive);
    }
}

#[test]
fn semantic_action_and_status_surfaces_declare_every_exposed_state() {
    let expected_action_states = [
        "action_primary_default",
        "action_primary_hover",
        "action_primary_focus",
        "action_primary_selected",
        "action_primary_disabled",
        "action_destructive_default",
        "action_destructive_hover",
        "action_destructive_focus",
        "action_destructive_selected",
        "action_destructive_disabled",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    let expected_status_lights = [
        "status_external_surface",
        "status_attention_surface",
        "status_success_surface",
        "status_warning_surface",
        "status_destructive_surface",
        "status_inactive_surface",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();

    for case in load_theme_matrix().cases {
        let tokens = matrix_case_tokens(&case);
        let action_states = tokens
            .interaction_state_contrast_pairs()
            .into_iter()
            .filter_map(|pair| pair.name.strip_suffix("_on_surface"))
            .filter(|name| name.starts_with("action_"))
            .collect::<BTreeSet<_>>();
        assert_eq!(action_states, expected_action_states);

        let normal_pairs = tokens
            .normal_text_contrast_pairs()
            .into_iter()
            .map(|pair| pair.name)
            .collect::<BTreeSet<_>>();
        for name in [
            "text_inverse_on_terminal_background",
            "text_on_accent_on_action_primary_default",
            "text_on_accent_on_action_primary_hover",
            "text_on_accent_on_action_primary_focus",
            "text_on_accent_on_action_primary_selected",
            "text_on_accent_on_action_destructive_default",
            "text_on_accent_on_action_destructive_hover",
            "text_on_accent_on_action_destructive_focus",
            "text_on_accent_on_action_destructive_selected",
        ] {
            assert!(normal_pairs.contains(name), "missing normal pair {name}");
        }

        let status_surfaces = tokens
            .status_surface_contrast_pairs()
            .into_iter()
            .map(|pair| pair.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(status_surfaces, expected_status_lights);
    }
}

#[test]
fn high_contrast_theme_keeps_wcag_aa_and_visible_focus() {
    let tokens = build_theme(
        ThemeMode::HighContrast,
        Density::Comfortable,
        Scale::Scale100,
    );
    assert_eq!(tokens.mode, ThemeMode::HighContrast);
    assert_color_contrast(&tokens);
    assert!(
        contrast_ratio(tokens.text.primary, tokens.surfaces.canvas) >= 7.0,
        "high-contrast primary text must meet AAA 7:1, got {:.3}",
        contrast_ratio(tokens.text.primary, tokens.surfaces.canvas)
    );
    assert!(
        contrast_ratio(tokens.borders.focus, tokens.surfaces.canvas) >= 3.0,
        "high-contrast focus ring must stay visible"
    );
    assert_ne!(tokens.borders.focus, tokens.borders.default);
}

#[test]
fn focus_and_selection_are_distinguishable_and_visible() {
    for case in load_theme_matrix().cases {
        let tokens = matrix_case_tokens(&case);
        assert_ne!(tokens.borders.focus, tokens.borders.default);
        assert_ne!(tokens.borders.selection, tokens.borders.default);
        assert_ne!(tokens.surfaces.selection, tokens.surfaces.hover);
        assert!(
            contrast_ratio(tokens.borders.focus, tokens.surfaces.canvas) >= 3.0,
            "focus ring is not visible against the canvas"
        );
        assert!(
            contrast_ratio(tokens.borders.selection, tokens.surfaces.canvas) >= 3.0,
            "selection border is not visible against the canvas"
        );
        assert!(
            contrast_ratio(tokens.text.on_selection, tokens.surfaces.selection) >= 4.5,
            "selection text is not readable"
        );
    }
}

#[test]
fn density_metrics_remain_internally_valid_at_supported_windows_scales() {
    for case in load_theme_matrix().cases {
        let metrics = matrix_case_tokens(&case).density;
        let physical = metrics.physical();
        assert!(physical.control_height >= physical.icon_size + 2 * physical.control_padding);
        assert!(physical.row_height >= physical.body_line_height + 2 * physical.row_padding);
        assert!(physical.terminal_line_height >= physical.code_line_height);
        assert!(physical.focus_ring_width >= 1);
        assert!(physical.label_min_width >= physical.icon_size);
        assert!(metrics.spacing.xxs > 0.0);
        assert!(metrics.spacing.xxs < metrics.spacing.xs);
        assert!(metrics.spacing.xs < metrics.spacing.sm);
        assert!(metrics.spacing.sm < metrics.spacing.md);
        assert!(metrics.spacing.md < metrics.spacing.lg);
        assert!(metrics.spacing.lg < metrics.spacing.xl);
        assert!(metrics.spacing.xl < metrics.spacing.xxl);
        assert!(metrics.radii.none <= metrics.radii.sm);
        assert!(metrics.radii.sm < metrics.radii.md);
        assert!(metrics.radii.md < metrics.radii.lg);
        assert!(metrics.typography.body_line_height >= metrics.typography.body);
        assert!(metrics.typography.code_line_height >= metrics.typography.code);
        assert!(metrics.icons.xs < metrics.icons.sm);
        assert!(metrics.icons.sm < metrics.icons.md);
        assert!(metrics.icons.md < metrics.icons.lg);
        assert!(metrics.icons.lg < metrics.icons.xl);
        assert!(metrics.controls.input_height >= metrics.controls.control_height);
        assert!(metrics.controls.button_height <= metrics.controls.input_height);
        assert!(metrics.motion.fast_ms <= metrics.motion.normal_ms);
        assert!(metrics.motion.normal_ms <= metrics.motion.slow_ms);
        assert_eq!(metrics.motion.reduced_motion_ms, 0);
    }

    let compact = build_theme(ThemeMode::Dark, Density::Compact, Scale::Scale100).density;
    let comfortable = build_theme(ThemeMode::Dark, Density::Comfortable, Scale::Scale100).density;
    assert!(comfortable.controls.control_height >= compact.controls.control_height);
    assert!(comfortable.controls.row_height >= compact.controls.row_height);
}

#[test]
fn legacy_theme_aliases_preserve_byte_values_with_aa_safe_text_dim() {
    assert_eq!(theme::APP_BG, 0x18181b);
    assert_eq!(theme::SIDEBAR_BG, 0x27272a);
    assert_eq!(theme::PANEL_BG, 0x18181b);
    assert_eq!(theme::PANEL_HEADER_BG, 0x27272a);
    assert_eq!(theme::PANEL_CARD_BG, 0x18181b);
    assert_eq!(theme::EDITOR_CARD_BG, 0x202127);
    assert_eq!(theme::EDITOR_FIELD_BG, 0x121318);
    assert_eq!(theme::EDITOR_NOTICE_BG, 0x1a202a);
    assert_eq!(theme::TOPBAR_BG, 0x27272a);
    assert_eq!(theme::TAB_BAR_BG, 0x27272a);
    assert_eq!(theme::TAB_ACTIVE_BG, 0x18181b);
    assert_eq!(theme::TAB_HOVER_BG, 0x323238);
    assert_eq!(theme::STATUS_BAR_BG, 0x09090b);
    assert_eq!(theme::TERMINAL_BG, 0x09090b);
    assert_eq!(theme::PROJECT_ROW_BG, 0x3f3f46);
    assert_eq!(theme::AGENT_ROW_BG, 0x27272a);
    assert_eq!(theme::BORDER_PRIMARY, 0x3f3f46);
    assert_eq!(theme::BORDER_SECONDARY, 0x27272a);
    assert_eq!(theme::BORDER_ACCENT, 0x243040);
    assert_eq!(theme::TEXT_PRIMARY, 0xe4e4e7);
    assert_eq!(theme::TEXT_MUTED, 0xa1a1aa);
    assert_eq!(theme::TEXT_SUBTLE, 0x71717a);
    // TEXT_DIM was intentionally raised to the dark muted semantic (WCAG AA on PANEL_BG).
    assert_eq!(theme::TEXT_DIM, 0xc4c4cc);
    assert_eq!(theme::SELECTION_BG, 0x22364d);
    assert_eq!(theme::SELECTION_TEXT, 0xf8fafc);
    assert_eq!(theme::PROJECT_DOT, 0x6366f1);
    assert_eq!(theme::AI_DOT, 0xf59e0b);
    assert_eq!(theme::SSH_DOT, 0x06b6d4);
    assert_eq!(theme::SUCCESS_BG, 0x142117);
    assert_eq!(theme::SUCCESS_TEXT, 0x4ade80);
    assert_eq!(theme::WARNING_TEXT, 0xfacc15);
    assert_eq!(theme::EXTERNAL_TEXT, 0x60a5fa);
    assert_eq!(theme::DANGER_TEXT, 0xfb7185);
    assert_eq!(theme::DANGER_BG_SUBTLE, 0x2a1517);
    assert_eq!(theme::PRIMARY, 0x4f46e5);
    assert_eq!(theme::PRIMARY_HOVER, 0x4338ca);
    assert_eq!(theme::PRIMARY_MUTED, 0x2c266b);
    assert_eq!(theme::ROW_HOVER_BG, 0x323238);
    assert_eq!(theme::BUTTON_HOVER_BG, 0x52525b);
}

#[test]
fn theme_exports_the_library_token_module_without_a_shadow_source() {
    let lib = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"))
        .expect("read library root");
    let theme_source =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(r"src/theme/mod.rs"))
            .expect("read theme module");
    let ui_source =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(r"src/ui/mod.rs"))
            .expect("read UI module");

    assert!(lib.lines().any(|line| line.trim() == "pub mod ui;"));
    assert!(theme_source.contains("pub use crate::ui::tokens::*;"));
    assert!(!theme_source.contains("token_source"));
    assert!(!theme_source.contains("#[path"));
    assert!(ui_source.contains("pub mod tokens;"));
}

#[test]
fn ui_source_outside_tokens_contains_no_direct_color_literals() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let canonical_token_path = source_root
        .join(r"ui\tokens.rs")
        .canonicalize()
        .expect("canonical token module");
    let mut production_files = Vec::new();
    collect_rust_files(&source_root, &mut production_files);
    let token_modules = production_files
        .iter()
        .filter(|path| is_token_module(path))
        .collect::<Vec<_>>();
    assert_eq!(
        token_modules.len(),
        1,
        "expected exactly one production token module"
    );
    assert_eq!(
        token_modules[0]
            .canonicalize()
            .expect("canonical token module path"),
        canonical_token_path
    );

    let root = source_root.join(r"ui");
    let mut files = Vec::new();
    collect_rust_files(&root, &mut files);

    assert!(!files.is_empty(), "expected the new src/ui module to exist");
    for path in files {
        if path.canonicalize().expect("canonical UI source path") == canonical_token_path {
            continue;
        }
        let source = fs::read_to_string(&path).expect("read UI source");
        assert!(
            !contains_direct_color_literal(&source),
            "{} contains a direct hex/RGB(A) color literal",
            path.display()
        );
    }
}

fn collect_rust_files(path: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(path).expect("read UI source directory") {
        let entry = entry.expect("read UI source entry");
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            files.push(path);
        }
    }
}

fn is_token_module(path: &Path) -> bool {
    path.file_stem().and_then(|name| name.to_str()) == Some("tokens")
}

fn contains_direct_color_literal(source: &str) -> bool {
    source.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        if contains_numeric_rgb_call(&lower) {
            return true;
        }

        let bytes = line.as_bytes();
        for index in 0..bytes.len().saturating_sub(1) {
            let has_hash = bytes[index] == b'#';
            let has_hex_prefix = bytes[index] == b'0' && matches!(bytes[index + 1], b'x' | b'X');
            if !has_hash && !has_hex_prefix {
                continue;
            }
            let start = if has_hash { index + 1 } else { index + 2 };
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_hexdigit() {
                end += 1;
            }
            if matches!(end - start, 6 | 8) {
                return true;
            }
        }
        false
    })
}

fn contains_numeric_rgb_call(line: &str) -> bool {
    for function in ["rgba(", "rgb("] {
        let mut offset = 0;
        while let Some(relative) = line[offset..].find(function) {
            let start = offset + relative + function.len();
            let Some(end) = line[start..].find(')') else {
                break;
            };
            let arguments = line[start..start + end].trim_start();
            if arguments.starts_with("0x")
                || arguments
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_digit())
            {
                return true;
            }
            offset = start + end + 1;
        }
    }
    false
}

#[test]
fn direct_color_scan_allows_token_to_rgb_conversion_but_rejects_literals() {
    assert!(!contains_direct_color_literal(
        "let color = rgb(tokens.actions.primary.default.background.to_u32());"
    ));
    assert!(!contains_direct_color_literal(
        "let color = rgb(color_value);"
    ));
    assert!(contains_direct_color_literal(
        "let color = rgb(12, 34, 56);"
    ));
    assert!(contains_direct_color_literal("let color = rgb(0x123456);"));
    assert!(contains_direct_color_literal(
        "let color = rgba(12, 34, 56, 255);"
    ));
    assert!(contains_direct_color_literal("let color = 0x123456;"));
    assert!(contains_direct_color_literal("let color = #123456;"));
}

#[test]
fn direct_color_scan_uses_only_the_exact_canonical_token_module_exemption() {
    let test_source =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(r"tests\ui_tokens.rs"))
            .expect("read token tests");
    assert!(
        !test_source
            .contains("path.file_name().and_then(|name| name.to_str()) == Some(\"tokens.rs\")"),
        "the source scan must exempt only the exact canonical token path"
    );

    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rust_files(&source_root, &mut files);
    let token_modules = files
        .iter()
        .filter(|path| is_token_module(path))
        .collect::<Vec<_>>();
    assert_eq!(
        token_modules.len(),
        1,
        "expected exactly one production token module"
    );

    let canonical = source_root
        .join(r"ui\tokens.rs")
        .canonicalize()
        .expect("canonical token module");
    assert_eq!(
        token_modules[0]
            .canonicalize()
            .expect("canonical token module path"),
        canonical
    );
}

#[test]
fn theme_matrix_is_typed_independent_fixture_data_not_generated_expectations() {
    let test_source =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(r"tests\ui_tokens.rs"))
            .expect("read token tests");
    let generated_matrix_marker = ["serde_json", "::json!({"].concat();
    assert!(
        !test_source.contains(&generated_matrix_marker),
        "theme coverage must come from the checked-in typed matrix fixture"
    );

    let fixture = load_theme_matrix();
    assert_eq!(fixture.schema_version, 1);
    assert_eq!(
        fixture.cases.len(),
        16,
        "theme matrix must contain 2 x 2 x 4 cases"
    );
    let interaction_states = fixture
        .interaction_bindings
        .iter()
        .map(|binding| &binding.state)
        .collect::<BTreeSet<_>>();
    let status_states = fixture
        .status_bindings
        .iter()
        .map(|binding| &binding.status)
        .collect::<BTreeSet<_>>();
    assert_eq!(interaction_states.len(), 5);
    assert_eq!(status_states.len(), 6);
    assert!(fixture.content.long_text.chars().count() > 120);
    assert!(!fixture.content.unicode.is_ascii());
    assert!(!fixture.content.disabled_text.is_empty());

    let mut combinations = BTreeSet::new();
    for case in &fixture.cases {
        let _tokens = matrix_case_tokens(case);
        combinations.insert(format!(
            "{:?}:{:?}:{:?}",
            case.theme, case.density, case.scale
        ));
    }
    assert_eq!(combinations.len(), 16);
}

#[test]
fn theme_gallery_preview_fixture_and_token_matrix_cover_phase_5_2() {
    let preview_fixture: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ui/theme-gallery.json"
    )))
    .expect("preview theme gallery JSON");
    assert_eq!(preview_fixture["schema"], "devmanager.ui.preview/v1");
    assert_eq!(preview_fixture["id"], "theme-gallery");
    assert_eq!(preview_fixture["root"]["kind"], "minimal");

    // Task 5.1 deliberately owns the checked-in preview schema. The independent
    // typed matrix remains separate so this test never weakens that loader.
    let matrix = load_theme_matrix();
    assert_eq!(matrix.schema_version, 1);
    assert_eq!(matrix.interaction_bindings.len(), 5);
    assert_eq!(matrix.status_bindings.len(), 6);
    for binding in matrix
        .interaction_bindings
        .iter()
        .map(|binding| &binding.content_key)
        .chain(
            matrix
                .status_bindings
                .iter()
                .map(|binding| &binding.content_key),
        )
    {
        let content = match binding {
            MatrixContentKey::LongText => &matrix.content.long_text,
            MatrixContentKey::Unicode => &matrix.content.unicode,
            MatrixContentKey::DisabledText => &matrix.content.disabled_text,
        };
        assert!(!content.is_empty());
    }

    let mut combinations = BTreeSet::new();
    for case in &matrix.cases {
        let tokens = matrix_case_tokens(case);
        assert_color_contrast(&tokens);
        let physical = tokens.density.physical();
        assert!(physical.control_height >= physical.icon_size + 2 * physical.control_padding);
        assert!(physical.row_height >= physical.body_line_height + 2 * physical.row_padding);
        combinations.insert(format!(
            "{:?}:{:?}:{:?}",
            case.theme, case.density, case.scale
        ));
    }
    assert_eq!(
        combinations.len(),
        16,
        "one case per theme, density, and scale"
    );
}
