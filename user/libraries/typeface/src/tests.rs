use super::*;

fn face() -> Typeface {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../assets");
    FontCatalog::load(&root.join("fonts"))
        .expect("Pango and Fontconfig")
        .resolve(
            &LogicalFont {
                face: "JetBrains Mono NL".into(),
                points: 12,
                weight: 400,
            },
            1,
            true,
        )
}

#[test]
fn shaped_combining_cluster_is_one_cursor_unit() {
    let face = face();
    let layout = face.layout("e\u{301}x", false, false, None);
    assert_eq!(layout.size().0, face.measure("éx", false));
    let advance = layout.caret_x("e\u{301}".len());
    for x in 0..advance {
        assert!(matches!(layout.hit(x, 0), 0 | 3));
    }
}

#[test]
fn layout_applies_bidi_to_selection_and_hit_testing() {
    let face = face();
    let text = "abc שלום def";
    let start = "abc ".len();
    let after_first = start + 'ש'.len_utf8();
    let after_second = after_first + 'ל'.len_utf8();
    let layout = face.layout(text, false, false, None);
    let first = &layout.selection(start..after_first)[0];
    let second = &layout.selection(after_first..after_second)[0];
    assert!(second.start < first.start);
    assert!(layout.selection(1..after_second).len() > 1);
}

#[test]
fn default_symbol_faces_cover_codex_markers() {
    let face = face();
    assert_eq!(face.layout("› • ⠋", false, false, None).missing_glyphs(), 0);
}

#[test]
fn shaped_text_draws_into_clip_without_writing_outside_it() {
    let face = face();
    let layout = face.layout("A›•⠋ 中文 e\u{301}", false, false, None);
    let mut pixels = vec![0_u32; 160 * 40];
    layout.draw(
        RenderTarget {
            pixels: &mut pixels,
            width: 160,
            height: 40,
            stride: 160,
            clip: (5, 3, 100, 25),
        },
        0,
        0,
        0xcbd5e1,
    );
    assert!(pixels.iter().any(|&pixel| pixel != 0));
    for (i, &pixel) in pixels.iter().enumerate() {
        if pixel != 0 {
            assert!((5..105).contains(&(i % 160)) && (3..28).contains(&(i / 160)));
        }
    }
}

#[test]
fn wrapped_lines_are_utf8_and_use_the_shaped_width() {
    let face = face();
    let text = "alpha beta 中文\nlast";
    let width = face.measure("alpha ", false);
    let ranges = face.wrap(text, width, false);
    assert!(ranges.len() > 2);
    assert_eq!(&text[ranges.last().unwrap().clone()], "last");
    assert!(ranges.iter().all(|range| text.get(range.clone()).is_some()));
}
