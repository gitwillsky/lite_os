use super::*;
use crate::style::Sheet;
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf};

fn test_font() -> Font {
    let fonts = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts");
    Font::from_faces(
        fs::read(fonts.join("liteos-ui-regular.otf")).expect("regular UI face"),
        fs::read(fonts.join("liteos-ui-bold.otf")).expect("bold UI face"),
    )
    .expect("UI faces register")
}

fn computed(css: &str) -> Computed {
    let sheet = Sheet::parse(css).expect("style parses");
    let node = crate::tree::Node {
        id: 1,
        kind: "span".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String("t".to_owned()))]),
        text: String::new(),
        children: Vec::new(),
    };
    sheet.compute(&node, &[])
}

#[test]
fn every_font_size_rasterizes_at_its_own_advance() {
    let font = test_font();
    let small = font.measure(&computed(".t { font-size: 9px; }"), "LiteOS");
    let medium = font.measure(&computed(".t { font-size: 13px; }"), "LiteOS");
    let large = font.measure(&computed(".t { font-size: 18px; }"), "LiteOS");
    assert!(small < medium, "9px {small} < 13px {medium}");
    assert!(medium < large, "13px {medium} < 18px {large}");
    // 13px is not snapped to the 11/12/14px presets: its advance scales
    // with the declared size rather than the nearest face.
    let ratio = medium / small;
    assert!((ratio - 13.0 / 9.0).abs() < 0.05, "ratio {ratio}");
}

#[test]
fn weight_maps_to_the_two_checked_faces() {
    let font = test_font();
    let text = "标题 LiteOS";
    let regular = font.measure(&computed(".t { font-weight: normal; }"), text);
    let medium = font.measure(&computed(".t { font-weight: 500; }"), text);
    let bold = font.measure(&computed(".t { font-weight: bold; }"), text);
    let heavy = font.measure(&computed(".t { font-weight: 600; }"), text);
    assert_eq!(regular, medium, "500 falls back to regular");
    assert_eq!(bold, heavy, "600 maps to bold");
    assert!(bold > regular, "bold {bold} wider than regular {regular}");
}

#[test]
fn wrapping_text_breaks_at_the_inline_constraint() {
    let font = test_font();
    let style = computed(".t { font-size: 11px; line-height: 14px; }");
    let text = "alpha beta gamma delta epsilon zeta";
    let one_line = font.measure_text(
        &style,
        text,
        Size::NONE,
        Size {
            width: AvailableSpace::MaxContent,
            height: AvailableSpace::MaxContent,
        },
    );
    assert_eq!(one_line.height, 14.0);
    let full = font.measure(&style, text);
    let wrapped = font.measure_text(
        &style,
        text,
        Size::NONE,
        Size {
            width: AvailableSpace::Definite(full / 2.0),
            height: AvailableSpace::MaxContent,
        },
    );
    assert!(
        wrapped.height >= 28.0 && wrapped.height % 14.0 == 0.0,
        "half width wraps to whole lines: {}",
        wrapped.height
    );
    assert!(wrapped.width <= full / 2.0 + 1.0);
}

#[test]
fn nowrap_text_keeps_one_line_under_a_definite_constraint() {
    let font = test_font();
    let style = computed(".t { font-size: 11px; line-height: 14px; white-space: nowrap; }");
    let text = "alpha beta gamma delta epsilon zeta";
    let measured = font.measure_text(
        &style,
        text,
        Size::NONE,
        Size {
            width: AvailableSpace::Definite(40.0),
            height: AvailableSpace::MaxContent,
        },
    );
    assert_eq!(measured.height, 14.0);
    assert!(measured.width > 40.0, "nowrap overflows the constraint");
}

#[test]
fn min_content_width_is_the_widest_unbreakable_run() {
    let font = test_font();
    let style = computed(".t { font-size: 11px; }");
    let measured = font.measure_text(
        &style,
        "aa bbbb cc",
        Size::NONE,
        Size {
            width: AvailableSpace::MinContent,
            height: AvailableSpace::MaxContent,
        },
    );
    let widest = font.measure(&style, "bbbb");
    assert!(
        (measured.width - widest).abs() < 1.0,
        "min-content {} ≈ widest word {}",
        measured.width,
        widest
    );
}

#[test]
fn line_height_accepts_multiplier_percentage_and_px() {
    let font = test_font();
    let text = "one two three four five six seven eight nine ten";
    let height_at = |css: &str| {
        font.measure_text(
            &computed(css),
            text,
            Size::NONE,
            Size {
                width: AvailableSpace::Definite(60.0),
                height: AvailableSpace::MaxContent,
            },
        )
        .height
    };
    let multiplier = height_at(".t { font-size: 10px; line-height: 1.4; }");
    let percent = height_at(".t { font-size: 10px; line-height: 140%; }");
    let pixels = height_at(".t { font-size: 10px; line-height: 14px; }");
    assert_eq!(multiplier, percent);
    assert_eq!(multiplier, pixels);
    let lines = multiplier / 14.0;
    assert!(lines >= 2.0, "constrained text wraps to {lines} lines");
}

#[test]
fn preformatted_text_keeps_hard_breaks() {
    let font = test_font();
    let style = computed(".t { white-space: pre; font-size: 11px; line-height: 16px; }");
    let measured = font.measure_text(
        &style,
        "one\ntwo\nthree",
        Size::NONE,
        Size {
            width: AvailableSpace::MaxContent,
            height: AvailableSpace::MaxContent,
        },
    );
    assert_eq!(measured.height, 48.0);
    let widest = ["one", "two", "three"]
        .map(|line| font.measure(&style, line))
        .into_iter()
        .fold(0.0, f32::max);
    assert!(
        (measured.width - widest).abs() < 1.0,
        "pre width {} is the widest line {}",
        measured.width,
        widest
    );
}

#[test]
fn gpu_text_emits_a8_glyph_geometry_for_latin_and_cjk() {
    let font = test_font();
    let style = computed(".t { color: #123b66; font-size: 11px; }");
    let bounds = crate::renderer::PhysicalRect {
        x1: 0,
        y1: 0,
        x2: 300,
        y2: 60,
    };
    let mut atlas = GlyphAtlas::new();
    let runs = font.gpu_text(&mut atlas, bounds, None, &style, "Lite 中");
    assert!(!runs.is_empty());
    assert!(runs.iter().all(|run| run.color == 0xff12_3b66));
    // Four Latin letters plus one CJK ideograph have visible masks; the CSS
    // space advances layout but intentionally contributes no atlas bitmap.
    assert_eq!(runs.iter().flat_map(|run| &run.glyphs).count(), 5);
    let (size, bytes) = atlas.upload().expect("glyph coverage atlas");
    assert_eq!(size.width, 2048);
    assert_eq!(bytes.len(), (size.width * size.height) as usize);
    assert!(bytes.iter().any(|coverage| *coverage != 0));
}

#[test]
fn glyph_atlas_keeps_the_tallest_row_when_a_shorter_glyph_follows() {
    let mut atlas = GlyphAtlas::new();
    atlas
        .insert(
            super::AtlasKey::Terminal {
                bold: false,
                glyph: 1,
            },
            2,
            4,
            &[1; 8],
        )
        .expect("tall glyph");
    atlas
        .insert(
            super::AtlasKey::Terminal {
                bold: false,
                glyph: 2,
            },
            2,
            1,
            &[2; 2],
        )
        .expect("short glyph");

    let (size, bytes) = atlas.upload().expect("atlas");
    assert_eq!(size.height, 4);
    assert_eq!(bytes.len(), (size.width * size.height) as usize);
    assert_eq!(
        &bytes[3 * size.width as usize..3 * size.width as usize + 2],
        &[1; 2]
    );
}

#[test]
fn glyph_atlas_reuses_stable_rasters_without_another_upload() {
    let mut atlas = GlyphAtlas::new();
    let key = super::AtlasKey::Terminal {
        bold: false,
        glyph: 7,
    };
    let first = atlas.insert(key, 2, 2, &[1; 4]).expect("first glyph");
    assert!(atlas.dirty());
    atlas.mark_clean();

    let reused = atlas.insert(key, 2, 2, &[9; 4]).expect("reused glyph");
    assert_eq!(reused, first);
    assert!(!atlas.dirty());
    let (size, bytes) = atlas.upload().expect("atlas");
    assert_eq!(&bytes[..2], &[1; 2]);
    assert_eq!(
        &bytes[size.width as usize..size.width as usize + 2],
        &[1; 2]
    );
}

#[test]
fn layout_cache_reuses_identical_inputs() {
    let font = test_font();
    let style = computed(".t { font-size: 11px; }");
    let first = font.measure(&style, "LiteOS desktop");
    let second = font.measure(&style, "LiteOS desktop");

    assert_eq!(first, second);
    // Two identical measures shape once: one cache entry, reused on the hit.
    assert_eq!(font.layouts.borrow().entries.len(), 1);
    // A different width constraint is a different input, not a stale hit.
    font.build_layout(&style, "LiteOS desktop", true, Some(120.0));
    assert_eq!(font.layouts.borrow().entries.len(), 2);
}
