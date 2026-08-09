use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use devmanager::theme;
use devmanager::ui::tokens::{
    contrast_ratio, srgb_luminance, theme as build_theme, Color, Density, Scale, ThemeMode,
    ThemeTokens,
};
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

    for mode in ALL_MODES {
        let tokens = build_theme(*mode, Density::Comfortable, Scale::Scale100);
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
fn legacy_theme_aliases_preserve_existing_values() {
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
    assert_eq!(theme::TEXT_DIM, 0x52525b);
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
fn theme_gallery_preview_fixture_and_token_matrix_cover_phase_5_2() {
    let preview_fixture: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ui/theme-gallery.json"
    )))
    .expect("preview theme gallery JSON");
    assert_eq!(preview_fixture["schema"], "devmanager.ui.preview/v1");
    assert_eq!(preview_fixture["id"], "theme-gallery");
    assert_eq!(preview_fixture["root"]["kind"], "minimal");

    // Task 5.1 deliberately owns the checked-in file's strict preview schema.
    // Keep this matrix deterministic in the scoped token test until the
    // preview contract accepts extensions without weakening its validation.
    let themes = ["dark", "light"];
    let densities = ["compact", "comfortable"];
    let scales = [100, 125, 150, 200];
    let interaction_states = ["default", "hover", "focus", "selected", "disabled"];
    let status_lights = [
        "external",
        "attention",
        "success",
        "warning",
        "destructive",
        "inactive",
    ];
    let state_bindings = serde_json::json!({
        "default": "long_text",
        "hover": "long_text",
        "focus": "unicode",
        "selected": "long_text",
        "disabled": "disabled_text",
        "external": "unicode",
        "attention": "long_text",
        "success": "long_text",
        "warning": "disabled_text",
        "destructive": "long_text",
        "inactive": "disabled_text"
    });
    let content = serde_json::json!({
        "long_text": "A long task title demonstrates wrapping without clipping while the token gallery exercises status, controls, terminal text, and focus affordances at every approved Windows scale.",
        "unicode": "Résumé • 日本語 • العربية • Ελληνικά • 🚀",
        "disabled_text": "Unavailable until the host reconnects"
    });
    let cases = themes
        .into_iter()
        .flat_map(|theme| {
            densities.into_iter().flat_map(move |density| {
                scales.into_iter().map(move |scale| {
                    serde_json::json!({
                        "theme": theme,
                        "density": density,
                        "scale": scale,
                        "interaction_states": interaction_states,
                        "status_lights": status_lights,
                        "content_keys": ["long_text", "unicode", "disabled_text"]
                    })
                })
            })
        })
        .collect::<Vec<_>>();
    let fixture = serde_json::json!({
        "schema_version": 1,
        "themes": themes,
        "densities": densities,
        "scales": scales,
        "interaction_states": interaction_states,
        "status_lights": status_lights,
        "state_bindings": state_bindings,
        "content": content,
        "cases": cases
    });

    assert_eq!(fixture["schema_version"], 1);
    assert_eq!(fixture["themes"], serde_json::json!(["dark", "light"]));
    assert_eq!(
        fixture["densities"],
        serde_json::json!(["compact", "comfortable"])
    );
    assert_eq!(fixture["scales"], serde_json::json!([100, 125, 150, 200]));
    assert_eq!(
        fixture["interaction_states"],
        serde_json::json!(["default", "hover", "focus", "selected", "disabled"])
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

    let state_bindings = fixture["state_bindings"]
        .as_object()
        .expect("state/content bindings");
    assert_eq!(state_bindings.len(), 11);
    for (state, content_key) in state_bindings {
        let content_key = content_key.as_str().expect("binding content key");
        assert!(
            content.get(content_key).and_then(Value::as_str).is_some(),
            "{state} binding must point to fixture content"
        );
    }

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
        for content_key in state_bindings.values() {
            assert!(
                case["content_keys"]
                    .as_array()
                    .expect("case content keys")
                    .contains(content_key),
                "matrix case must bind every state content key"
            );
        }
    }

    let expected_combinations = themes
        .into_iter()
        .flat_map(|theme| {
            densities.into_iter().flat_map(move |density| {
                scales
                    .into_iter()
                    .map(move |scale| format!("{theme}:{density}:{scale}"))
            })
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(combinations, expected_combinations);
}
