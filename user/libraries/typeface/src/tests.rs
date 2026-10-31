use std::path::{Path, PathBuf};

use crate::raster::rasterize;
use crate::sfnt::{Outline, Point};
use crate::{BdfFont, FontCatalog, Glyph, LogicalFont, Sfnt, Typeface, overprint, wrap};

fn fonts() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../assets/classic/fonts")
}

fn sfnt(name: &str) -> Sfnt {
    Sfnt::parse(std::fs::read(fonts().join(name)).unwrap()).unwrap()
}

fn point(x: f32, y: f32, on_curve: bool) -> Point {
    Point { x, y, on_curve }
}

fn area(coverage: &[u8]) -> f32 {
    coverage.iter().map(|&value| f32::from(value)).sum::<f32>() / 255.0
}

const SMALL_BDF: &str = "STARTFONT 2.1
FONTBOUNDINGBOX 8 10 0 -2
STARTPROPERTIES 2
FONT_ASCENT 8
FONT_DESCENT 2
ENDPROPERTIES
CHARS 3
STARTCHAR a
ENCODING 97
DWIDTH 6 0
BBX 5 3 1 0
BITMAP
F8
88
A8
ENDCHAR
STARTCHAR nocode
ENCODING -1
DWIDTH 6 0
BBX 8 1 0 0
BITMAP
FF
ENDCHAR
STARTCHAR wide
ENCODING 20013
DWIDTH 12 0
BBX 10 1 0 -1
BITMAP
FFC0
ENDCHAR
ENDFONT
";

#[test]
fn bdf_decodes_bits_offsets_and_drops_unaddressable_glyphs() {
    let font = BdfFont::parse(SMALL_BDF).unwrap();
    assert_eq!((font.ascent(), font.descent()), (8, 2));
    let glyph = font.glyph('a').unwrap();
    assert_eq!(
        (
            glyph.width,
            glyph.height,
            glyph.left,
            glyph.top,
            glyph.advance
        ),
        (5, 3, 1, 3, 6)
    );
    assert_eq!(
        glyph.coverage,
        [
            255, 255, 255, 255, 255, 255, 0, 0, 0, 255, 255, 0, 255, 0, 255
        ]
    );
    let wide = font.glyph('中').unwrap();
    assert_eq!((wide.width, wide.top, wide.advance), (10, 0, 12));
    assert!(wide.coverage.iter().all(|&value| value == 255));
    assert!(font.glyph('b').is_none());
}

#[test]
fn bdf_rejects_malformed_input() {
    let broken = |text: &str| BdfFont::parse(text).is_err();
    assert!(broken(""));
    assert!(broken("STARTFONT 2.1\nENDFONT\n"));
    assert!(broken(&SMALL_BDF.replace("88\n", "8\n")));
    assert!(broken(&SMALL_BDF.replace("A8", "ZZ")));
    assert!(broken(&SMALL_BDF.replace("BBX 5 3 1 0", "BBX 5 300 1 0")));
    assert!(broken(
        &SMALL_BDF.replace("FONTBOUNDINGBOX 8 10 0 -2\n", "")
    ));
    assert!(broken("FONTBOUNDINGBOX 1 1 0 0\nBBX 1 1 0 0\n"));
}

#[test]
fn rasterizer_fills_axis_aligned_square_exactly() {
    let square = Outline {
        contours: vec![vec![
            point(0.0, 0.0, true),
            point(10.0, 0.0, true),
            point(10.0, 10.0, true),
            point(0.0, 10.0, true),
        ]],
    };
    let raster = rasterize(&square, 1.0, 0.0);
    assert_eq!(
        (raster.width, raster.height, raster.left, raster.top),
        (10, 10, 0, 10)
    );
    assert!(raster.coverage.iter().all(|&value| value == 255));
}

#[test]
fn rasterizer_is_independent_of_winding_and_matches_triangle_area() {
    let triangle = vec![
        point(0.0, 0.0, true),
        point(20.0, 0.0, true),
        point(0.0, 12.0, true),
    ];
    let mut reversed = triangle.clone();
    reversed.reverse();
    let forward = rasterize(
        &Outline {
            contours: vec![triangle],
        },
        1.0,
        0.0,
    );
    let backward = rasterize(
        &Outline {
            contours: vec![reversed],
        },
        1.0,
        0.0,
    );
    assert_eq!(forward.coverage, backward.coverage);
    assert!(
        (area(&forward.coverage) - 120.0).abs() < 0.5,
        "area {}",
        area(&forward.coverage)
    );
}

#[test]
fn rasterizer_handles_quadratic_curves_and_fractional_offsets() {
    // 四段二次曲线近似的圆：半径 10 的圆面积约 314。
    let k = 10.0;
    let circle = vec![
        point(k, 0.0, true),
        point(k, k, false),
        point(0.0, k, true),
        point(-k, k, false),
        point(-k, 0.0, true),
        point(-k, -k, false),
        point(0.0, -k, true),
        point(k, -k, false),
    ];
    let raster = rasterize(
        &Outline {
            contours: vec![circle],
        },
        1.0,
        0.0,
    );
    // 每段二次曲线取控制点在角上，面积大于圆；只验证落在合理范围并且对称。
    let total = area(&raster.coverage);
    assert!((300.0..=400.0).contains(&total), "area {total}");
    let (w, h) = (raster.width as usize, raster.height as usize);
    for row in 0..h {
        for column in 0..w / 2 {
            let (left, right) = (
                raster.coverage[row * w + column],
                raster.coverage[row * w + w - 1 - column],
            );
            assert!(left.abs_diff(right) <= 1, "asymmetric at {row},{column}");
        }
    }
    let shifted = rasterize(
        &Outline {
            contours: vec![vec![
                point(0.5, 0.0, true),
                point(10.5, 0.0, true),
                point(10.5, 4.0, true),
                point(0.5, 4.0, true),
            ]],
        },
        1.0,
        0.0,
    );
    assert_eq!(shifted.width, 11);
    assert_eq!(shifted.coverage[0], 128);
    assert_eq!(shifted.coverage[5], 255);
    assert_eq!(shifted.coverage[10], 128);
}

#[test]
fn rasterizer_accepts_contours_without_on_curve_points_and_empty_outlines() {
    let ring = vec![
        point(0.0, 0.0, false),
        point(8.0, 0.0, false),
        point(8.0, 8.0, false),
        point(0.0, 8.0, false),
    ];
    let raster = rasterize(
        &Outline {
            contours: vec![ring],
        },
        1.0,
        0.0,
    );
    assert!(area(&raster.coverage) > 40.0);
    let empty = rasterize(&Outline::default(), 1.0, 0.0);
    assert_eq!((empty.width, empty.height, empty.coverage.len()), (0, 0, 0));
}

#[test]
fn original_tahoma_cmap_metrics_and_outlines() {
    let tahoma = sfnt("tahoma.ttf");
    assert_eq!(tahoma.units_per_em(), 2048);
    let a = tahoma.glyph_id('A').unwrap();
    assert!(tahoma.glyph_id('中').is_none());
    assert!(tahoma.advance_units(a).unwrap() > 0);
    // 原版 Tahoma 没有 11 ppem 内嵌点阵；西文走原版轮廓与 hinting。
    assert!(!tahoma.has_strike(11));
    assert!(tahoma.embedded_bitmap(a, 11).is_none());
    assert!(!tahoma.outline(a).unwrap().contours.is_empty());
}

#[test]
fn original_marlett_symbol_cmap_exposes_caption_glyphs_without_text_fallback() {
    let font = sfnt("marlett.ttf");
    for symbol in ['\u{f030}', '\u{f031}', '\u{f032}', '\u{f072}'] {
        let id = font
            .glyph_id(symbol)
            .expect("original Marlett caption symbol");
        assert!(!font.outline(id).unwrap().contours.is_empty());
    }
    assert!(font.glyph_id('中').is_none());
}

#[test]
fn outlines_reproduce_simple_and_composite_glyphs() {
    let tahoma = sfnt("tahoma.ttf");
    let bar = tahoma.outline(tahoma.glyph_id('I').unwrap()).unwrap();
    assert_eq!(bar.contours.len(), 1);
    let raster = rasterize(&bar, 22.0 / 2048.0, 0.0);
    assert!(
        (14..=17).contains(&raster.height) && raster.width <= 10,
        "{}x{}",
        raster.width,
        raster.height
    );
    // 带附加符号的拉丁字母是复合字形，必须包含基字形与符号两部分轮廓。
    let accented = tahoma.outline(tahoma.glyph_id('é').unwrap()).unwrap();
    let plain = tahoma.outline(tahoma.glyph_id('e').unwrap()).unwrap();
    assert!(accented.contours.len() > plain.contours.len());
}

#[test]
fn simsun_collection_preserves_faces_cjk_and_native_strikes() {
    let faces = Sfnt::parse_collection(std::fs::read(fonts().join("simsun.ttc")).unwrap()).unwrap();
    assert_eq!(faces.len(), 2);
    let simsun = &faces[0];
    for ppem in [12, 13, 14, 15, 16, 18] {
        assert!(simsun.has_strike(ppem), "missing strike {ppem}");
        let glyph = simsun
            .embedded_bitmap(simsun.glyph_id('中').unwrap(), ppem)
            .unwrap();
        assert!(glyph.coverage.contains(&255));
        assert!(
            glyph
                .coverage
                .iter()
                .all(|&value| value == 0 || value == 255)
        );
    }
    for character in "中文登录到关机取消确定".chars() {
        let id = simsun
            .glyph_id(character)
            .unwrap_or_else(|| panic!("missing {character}"));
        let outline = simsun.outline(id).unwrap();
        assert!(!outline.contours.is_empty(), "{character}");
    }
    assert!(simsun.glyph_id('\u{1F600}').is_none());
}

fn song(points: u16, scale: u32, smoothing: bool) -> Typeface {
    FontCatalog::load(&fonts()).unwrap().resolve(
        &LogicalFont {
            face: "宋体".into(),
            points,
            weight: 400,
        },
        scale,
        smoothing,
    )
}

#[test]
fn original_song_9pt_at_1x_uses_embedded_bitmaps() {
    // 原版宋体的 12 ppem 点阵直接用于 1x；不再依赖可分发替代字体。
    let face = song(9, 1, false);
    assert_eq!(face.ppem(), 12);
    assert_eq!(face.glyph('中', false).advance, 12);
    assert_eq!(face.glyph('“', false).advance, 12);
    let (ascent, descent) = face.metrics();
    assert!(
        ascent >= 9 && descent >= 1 && ascent + descent <= 16,
        "{ascent}+{descent}"
    );
    for character in "Hello World 123 LiteOS 中文".chars() {
        assert!(
            face.glyph(character, false)
                .coverage
                .iter()
                .all(|&v| v == 0 || v == 255),
            "{character}"
        );
    }
    // 链中都没有的字符退化为半个 em 宽的空白，而不是失败。
    let missing = face.glyph('\u{0378}', false);
    assert_eq!((missing.width, missing.advance), (0, 6));
}

#[test]
fn outline_rendering_follows_the_font_smoothing_setting() {
    let smooth = song(9, 2, true);
    assert_eq!(smooth.ppem(), 24);
    let glyph = smooth.glyph('中', false);
    assert!(
        glyph.coverage.iter().any(|&v| v != 0 && v != 255),
        "expected anti-aliased edges"
    );
    let mono = song(9, 2, false).glyph('中', false);
    assert!(mono.coverage.iter().all(|&v| v == 0 || v == 255));
    assert!(mono.coverage.contains(&255));
}

#[test]
fn bold_cjk_overprints_one_scaled_pixel() {
    let glyph = Glyph {
        coverage: vec![255, 0, 128, 0],
        width: 2,
        height: 2,
        left: 1,
        top: 2,
        advance: 12,
    };
    let bold = overprint(glyph, 1);
    assert_eq!(
        (bold.width, bold.advance, bold.left, bold.top),
        (3, 13, 1, 2)
    );
    assert_eq!(bold.coverage, [255, 255, 0, 128, 128, 0]);
    let blank = overprint(
        Glyph {
            coverage: vec![],
            width: 0,
            height: 0,
            left: 0,
            top: 0,
            advance: 6,
        },
        2,
    );
    assert_eq!(blank.advance, 8);
}

fn half_width(character: char) -> i32 {
    if character.is_ascii() { 6 } else { 12 }
}

fn lines(text: &str, width: i32) -> Vec<&str> {
    wrap(text, width, half_width)
        .into_iter()
        .map(|range| &text[range])
        .collect()
}

#[test]
fn wrap_breaks_latin_at_spaces_and_cjk_anywhere() {
    assert_eq!(lines("hello world", 36), ["hello", "world"]);
    assert_eq!(lines("hello world", 66), ["hello world"]);
    assert_eq!(lines("登录到系统", 36), ["登录到", "系统"]);
    assert_eq!(lines("a\nb\n\nc", 100), ["a", "b", "", "c"]);
    assert_eq!(lines("", 10), [""]);
}

#[test]
fn wrap_respects_punctuation_rules_and_always_progresses() {
    // 句号不能在行首：放不下时把前一个字一起带到下一行。
    assert_eq!(lines("登录。系统", 36), ["登录。", "系统"]);
    assert_eq!(lines("登录。系统", 24), ["登", "录。", "系统"]);
    // 开括号不能留在行尾。
    assert_eq!(lines("甲乙（丙丁）", 48), ["甲乙（丙", "丁）"]);
    assert_eq!(lines("甲乙（丙丁）", 36), ["甲乙", "（丙", "丁）"]);
    // 比行宽还长的单词在字符处硬断，不丢字符。
    assert_eq!(
        lines("supercalifragilistic ok", 30),
        ["super", "calif", "ragil", "istic", "ok"]
    );
    // 宽度小于一个字时每行一个字，仍然前进。
    assert_eq!(lines("中文", 1), ["中", "文"]);
    // 悬挂的空格不进入行内容。
    assert_eq!(lines("ab  cd", 12), ["ab", "cd"]);
}

#[test]
fn malformed_fonts_never_panic() {
    let original = std::fs::read(fonts().join("tahoma.ttf")).unwrap();
    for cut in [
        0,
        3,
        11,
        12,
        100,
        4000,
        original.len() / 2,
        original.len() - 1,
    ] {
        if let Ok(font) = Sfnt::parse(original[..cut].to_vec()) {
            exercise(&font);
        }
    }
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..40 {
        let mut data = original.clone();
        for _ in 0..64 {
            let position = next() as usize % data.len();
            data[position] = next() as u8;
        }
        if let Ok(font) = Sfnt::parse(data) {
            exercise(&font);
        }
    }
}

fn exercise(font: &Sfnt) {
    for character in "AéI中 ".chars() {
        if let Some(id) = font.glyph_id(character) {
            let _ = font.advance_units(id);
            let _ = font.embedded_bitmap(id, 11);
            if let Some(outline) = font.outline(id) {
                let _ = rasterize(&outline, 0.01, 0.0);
                let _ = rasterize(&outline, 0.02, 0.0);
            }
        }
    }
}
