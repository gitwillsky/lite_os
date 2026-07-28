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
fn text_shadow_parses_offsets_and_color() {
    assert_eq!(
        text_shadow("1px 1px #123b66"),
        Some((SCALE as i32, SCALE as i32, 0xff12_3b66))
    );
}

#[test]
fn text_shadow_accepts_and_ignores_blur() {
    assert_eq!(
        text_shadow("0px 1px 2px #000000"),
        Some((0, SCALE as i32, 0xff00_0000))
    );
}

#[test]
fn text_shadow_rejects_missing_parts() {
    assert_eq!(text_shadow("1px #123b66"), None);
    assert_eq!(text_shadow("1px 1px"), None);
}


/// In-memory physical-pixel target for exercising the full draw path
/// (parley layout → glyph cache → A8 blit) on the host.
struct Buffer {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
}

impl Buffer {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            pixels: vec![0xffff_ffff; width * height],
        }
    }

    fn count_ink(&self) -> usize {
        self.pixels.iter().filter(|&&p| p != 0xffff_ffff).count()
    }
}

impl Raster for Buffer {
    fn width(&self) -> usize {
        self.width
    }

    fn height(&self) -> usize {
        self.height
    }

    fn row_mut(&mut self, row: usize) -> &mut [u32] {
        &mut self.pixels[row * self.width..(row + 1) * self.width]
    }
}

#[test]
fn draw_paints_glyph_ink_for_latin_and_cjk() {
    let font = test_font();
    let style = computed(".t { font-size: 11px; }");
    let mut target = Buffer::new(300, 60);
    let bounds = PhysicalRect {
        x1: 0,
        y1: 0,
        x2: 300,
        y2: 28,
    };
    font.draw(&mut target, bounds, None, &style, "Lite 中");
    assert!(target.count_ink() > 0, "glyphs paint ink");
    // A second identical draw hits the glyph cache and paints the same ink.
    let mut again = Buffer::new(300, 60);
    font.draw(&mut again, bounds, None, &style, "Lite 中");
    assert_eq!(target.pixels, again.pixels);
}

#[test]
fn draw_wraps_long_text_onto_a_second_line() {
    let font = test_font();
    let style = computed(".t { font-size: 11px; line-height: 14px; }");
    let text = "alpha beta gamma delta epsilon zeta eta theta iota";
    let width = (font.measure(&style, text) * 0.4 * SCALE).round() as usize;
    let mut target = Buffer::new(width, 120);
    let bounds = PhysicalRect {
        x1: 0,
        y1: 0,
        x2: width,
        y2: 120,
    };
    font.draw(&mut target, bounds, None, &style, text);
    // Ink past 1.5 physical line-heights can only come from a wrapped line:
    // first-line descenders end around baseline + descent, well above it.
    let beyond_first_line = (14.0 * 1.5 * SCALE) as usize;
    let lower_ink = target.pixels[beyond_first_line * width..]
        .iter()
        .filter(|&&p| p != 0xffff_ffff)
        .count();
    assert!(lower_ink > 0, "wrapped line paints below the first line");
}

#[test]
fn draw_ellipsizes_a_nowrap_overflowing_line() {
    let font = test_font();
    let style =
        computed(".t { font-size: 11px; white-space: nowrap; text-overflow: ellipsis; }");
    let text = "alpha beta gamma delta epsilon zeta eta theta iota";
    let full = (font.measure(&style, text) * SCALE).round() as usize;
    let width = full / 2;
    let mut target = Buffer::new(width, 60);
    let bounds = PhysicalRect {
        x1: 0,
        y1: 0,
        x2: width,
        y2: 60,
    };
    font.draw(&mut target, bounds, None, &style, text);
    // Default line-height is 1.25×11px; ink past 1.5 physical line-heights
    // would mean a wrapped second line (first-line descenders end higher).
    let beyond_first_line = (11.0 * 1.25 * 1.5 * SCALE) as usize;
    let lower_ink = target.pixels[beyond_first_line * width..]
        .iter()
        .filter(|&&p| p != 0xffff_ffff)
        .count();
    assert_eq!(lower_ink, 0, "nowrap text never wraps onto a second line");
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
