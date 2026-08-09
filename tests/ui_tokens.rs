#[path = "../src/ui/mod.rs"]
#[allow(dead_code, unused_imports)]
mod ui;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use devmanager::theme;
use serde_json::Value;
use ui::tokens::{
    contrast_ratio, srgb_luminance, theme as build_theme, Color, Density, Scale, ThemeMode,
    ThemeTokens,
};

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
    "status_external",
    "status_attention",
    "status_success",
    "status_warning",
    "status_destructive",
    "status_inactive",
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

const ALL_MODES: &[ThemeMode] = &[ThemeMode::Dark, ThemeMode::Light];
const ALL_DENSITIES: &[Density] = &[Density::Compact, Density::Comfortable];
const ALL_SCALES: &[Scale] = &[
    Scale::Scale100,
    Scale::Scale125,
    Scale::Scale150,
    Scale::Scale200,
];

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
            contrast_ratio(pair.foreground, pair.background) >= 3.0,
            "{} must keep disabled text readable at 3:1, got {:.3}",
            pair.name,
            contrast_ratio(pair.foreground, pair.background)
        );
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
    let dark = build_theme(ThemeMode::Dark, Density::Comfortable, Scale::Scale100);
    let light = build_theme(ThemeMode::Light, Density::Comfortable, Scale::Scale100);

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
    for mode in ALL_MODES {
        let tokens = build_theme(*mode, Density::Comfortable, Scale::Scale100);
        for token in tokens.semantic_color_tokens() {
            assert!(token.color.is_opaque(), "{} is not opaque", token.name);
        }
        assert_color_contrast(&tokens);
    }
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
    for mode in ALL_MODES {
        let status = build_theme(*mode, Density::Comfortable, Scale::Scale100).status;
        assert_blue(status.external);
        assert_orange(status.attention);
        assert_green(status.success);
        assert_orange(status.warning);
        assert_red(status.destructive);
        assert_neutral(status.inactive);
    }
}

#[test]
fn focus_and_selection_are_distinguishable_and_visible() {
    for mode in ALL_MODES {
        let tokens = build_theme(*mode, Density::Comfortable, Scale::Scale100);
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
    for mode in ALL_MODES {
        for density in ALL_DENSITIES {
            for scale in ALL_SCALES {
                let metrics = build_theme(*mode, *density, *scale).density;
                let physical = metrics.physical();
                assert!(
                    physical.control_height >= physical.icon_size + 2 * physical.control_padding
                );
                assert!(
                    physical.row_height >= physical.body_line_height + 2 * physical.row_padding
                );
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
        }
    }

    let compact = build_theme(ThemeMode::Dark, Density::Compact, Scale::Scale100).density;
    let comfortable = build_theme(ThemeMode::Dark, Density::Comfortable, Scale::Scale100).density;
    assert!(comfortable.controls.control_height >= compact.controls.control_height);
    assert!(comfortable.controls.row_height >= compact.controls.row_height);
}

#[test]
fn legacy_theme_adapter_reads_the_new_token_source() {
    let dark = build_theme(ThemeMode::Dark, Density::Comfortable, Scale::Scale100);
    assert_eq!(theme::APP_BG, dark.surfaces.canvas.to_u32());
    assert_eq!(theme::PANEL_BG, dark.surfaces.canvas.to_u32());
    assert_eq!(theme::TEXT_PRIMARY, dark.text.primary.to_u32());
    assert_eq!(theme::TEXT_MUTED, dark.text.muted.to_u32());
    assert_eq!(theme::BORDER_PRIMARY, dark.borders.default.to_u32());
    assert_eq!(theme::TERMINAL_BG, dark.terminal.background.to_u32());
}

#[test]
fn ui_source_outside_tokens_contains_no_direct_color_literals() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(r"src\ui");
    let mut files = Vec::new();
    collect_rust_files(&root, &mut files);

    assert!(!files.is_empty(), "expected the new src/ui module to exist");
    for path in files {
        if path.file_name().and_then(|name| name.to_str()) == Some("tokens.rs") {
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

fn contains_direct_color_literal(source: &str) -> bool {
    source.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        if lower.contains("rgb(") || lower.contains("rgba(") {
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

#[test]
fn theme_gallery_fixture_covers_the_phase_5_2_matrix() {
    let fixture: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ui/theme-gallery.json"
    )))
    .expect("theme gallery JSON");

    assert_eq!(fixture["schema_version"], 1);
    assert_eq!(fixture["themes"], serde_json::json!(["dark", "light"]));
    assert_eq!(
        fixture["densities"],
        serde_json::json!(["compact", "comfortable"])
    );
    assert_eq!(fixture["scales"], serde_json::json!([100, 125, 150, 200]));
    assert_eq!(
        fixture["interaction_states"],
        serde_json::json!(["default", "hover", "focus", "selection", "disabled"])
    );
    assert_eq!(
        fixture["status_lights"],
        serde_json::json!([
            "external",
            "attention",
            "success",
            "warning",
            "destructive",
            "inactive"
        ])
    );

    let content = &fixture["content"];
    assert!(content["long_text"]
        .as_str()
        .is_some_and(|text| text.chars().count() > 120));
    assert!(content["unicode"]
        .as_str()
        .is_some_and(|text| !text.is_ascii()));
    assert!(content["disabled_text"]
        .as_str()
        .is_some_and(|text| !text.is_empty()));

    let cases = fixture["cases"].as_array().expect("gallery cases");
    assert_eq!(cases.len(), 16, "one case per theme, density, and scale");
    let mut combinations = BTreeSet::new();
    for case in cases {
        let theme_name = case["theme"].as_str().expect("case theme");
        let density_name = case["density"].as_str().expect("case density");
        let scale = case["scale"].as_u64().expect("case scale");
        assert!(combinations.insert(format!("{theme_name}:{density_name}:{scale}")));
        assert_eq!(case["interaction_states"], fixture["interaction_states"]);
        assert_eq!(case["status_lights"], fixture["status_lights"]);
        assert_eq!(
            case["content_keys"],
            serde_json::json!(["long_text", "unicode", "disabled_text"])
        );
    }

    let expected_combinations = [
        "dark:compact:100",
        "dark:compact:125",
        "dark:compact:150",
        "dark:compact:200",
        "dark:comfortable:100",
        "dark:comfortable:125",
        "dark:comfortable:150",
        "dark:comfortable:200",
        "light:compact:100",
        "light:compact:125",
        "light:compact:150",
        "light:compact:200",
        "light:comfortable:100",
        "light:comfortable:125",
        "light:comfortable:150",
        "light:comfortable:200",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();
    assert_eq!(combinations, expected_combinations);
}
