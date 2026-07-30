use std::collections::BTreeMap;

use serde_json::Value;
use taffy::{
    AvailableSpace,
    prelude::{
        Dimension, Display, FlexDirection, LengthPercentage, LengthPercentageAuto, Position,
        Rect as TaffyRect, Size, Style, TaffyTree,
    },
};

use crate::tree::Node;

use super::{DocumentNode, PhysicalRect, Raster, collect_local_paint_changes, excludes_window};

fn node(window_id: Option<u32>) -> Node {
    let mut props = BTreeMap::new();
    if let Some(window_id) = window_id {
        props.insert("data-lite-window".to_owned(), Value::from(window_id));
    }
    Node {
        id: window_id.map_or(1, u64::from),
        kind: "div".to_owned(),
        props,
        text: String::new(),
        children: Vec::new(),
    }
}

#[test]
fn normal_render_never_excludes_nodes() {
    assert!(!excludes_window(&node(None), None));
    assert!(!excludes_window(&node(Some(7)), None));
}

#[test]
fn underlay_excludes_only_the_selected_window() {
    assert!(!excludes_window(&node(None), Some(7)));
    assert!(!excludes_window(&node(Some(6)), Some(7)));
    assert!(excludes_window(&node(Some(7)), Some(7)));
}

#[test]
fn fixed_damage_covers_appearance_and_removal_without_full_document_damage() {
    let full = display_proto::Rect {
        x: 0,
        y: 0,
        width: 3008,
        height: 1692,
    };
    let topbar = display_proto::Rect {
        x: 20,
        y: 18,
        width: 2960,
        height: 80,
    };
    let panel = display_proto::Rect {
        x: 772,
        y: 184,
        width: 1464,
        height: 1208,
    };
    let current = [crate::display::Overlay {
        rect: panel,
        clip_mask: display_proto::ClipMask {
            rect: panel,
            radii: [display_proto::CornerRadius { x: 40, y: 40 }; 4],
        },
        z_index: 950,
    }];

    assert_eq!(
        super::paint_damage(&super::DocumentPaint::Full, &[topbar], &current, full),
        [full]
    );
    assert_eq!(
        super::paint_damage(&super::DocumentPaint::Reuse, &[topbar], &current, full),
        [topbar, panel]
    );
    assert_eq!(
        super::paint_damage(&super::DocumentPaint::Reuse, &[panel], &[], full),
        [panel]
    );
}

/// Proves that the window border-box overflow clip retains the titlebar and
/// absolute edge grips used for move and resize input.
#[test]
fn window_overflow_clip_vs_titlebar_and_grips() {
    const WIN_W: f32 = 400.0;
    const WIN_H: f32 = 300.0;

    let mut tree = TaffyTree::<()>::new();
    let titlebar = tree
        .new_leaf(Style {
            position: Position::Relative,
            size: Size {
                width: Dimension::auto(),
                height: Dimension::length(21.0),
            },
            ..Style::default()
        })
        .unwrap();
    let grip =
        |tree: &mut TaffyTree<()>, inset: TaffyRect<LengthPercentageAuto>, w: f32, h: f32| {
            tree.new_leaf(Style {
                position: Position::Absolute,
                inset,
                size: Size {
                    width: Dimension::length(w),
                    height: Dimension::length(h),
                },
                ..Style::default()
            })
            .unwrap()
        };
    let auto = LengthPercentageAuto::auto;
    let len = LengthPercentageAuto::length;
    let nw = grip(
        &mut tree,
        TaffyRect {
            top: len(0.0),
            left: len(0.0),
            right: auto(),
            bottom: auto(),
        },
        10.0,
        10.0,
    );
    let se = grip(
        &mut tree,
        TaffyRect {
            top: auto(),
            left: auto(),
            right: len(0.0),
            bottom: len(0.0),
        },
        10.0,
        10.0,
    );
    let n = tree
        .new_leaf(Style {
            position: Position::Absolute,
            inset: TaffyRect {
                top: len(0.0),
                left: len(8.0),
                right: len(8.0),
                bottom: auto(),
            },
            size: Size {
                width: Dimension::auto(),
                height: Dimension::length(5.0),
            },
            ..Style::default()
        })
        .unwrap();
    let e = tree
        .new_leaf(Style {
            position: Position::Absolute,
            inset: TaffyRect {
                top: len(8.0),
                left: auto(),
                right: len(0.0),
                bottom: len(8.0),
            },
            size: Size {
                width: Dimension::length(5.0),
                height: Dimension::auto(),
            },
            ..Style::default()
        })
        .unwrap();
    let window = tree
        .new_with_children(
            Style {
                display: Display::Flex,
                flex_direction: FlexDirection::Column,
                position: Position::Absolute,
                size: Size {
                    width: Dimension::length(WIN_W),
                    height: Dimension::length(WIN_H),
                },
                padding: TaffyRect {
                    top: LengthPercentage::length(3.0),
                    right: LengthPercentage::length(3.0),
                    bottom: LengthPercentage::length(3.0),
                    left: LengthPercentage::length(3.0),
                },
                border: TaffyRect {
                    top: LengthPercentage::length(2.0),
                    right: LengthPercentage::length(2.0),
                    bottom: LengthPercentage::length(2.0),
                    left: LengthPercentage::length(2.0),
                },
                ..Style::default()
            },
            &[titlebar, n, e, nw, se],
        )
        .unwrap();

    tree.compute_layout(
        window,
        Size {
            width: AvailableSpace::Definite(WIN_W),
            height: AvailableSpace::Definite(WIN_H),
        },
    )
    .unwrap();

    let win = tree.layout(window).unwrap();
    let clip = PhysicalRect::new(0.0, 0.0, win.size.width, win.size.height, 4000, 4000);
    for (name, id) in [
        ("titlebar", titlebar),
        ("n", n),
        ("e", e),
        ("nw", nw),
        ("se", se),
    ] {
        let layout = tree.layout(id).unwrap();
        let bounds = PhysicalRect::new(
            layout.location.x,
            layout.location.y,
            layout.size.width,
            layout.size.height,
            4000,
            4000,
        );
        assert!(
            !bounds.intersect(clip).is_empty(),
            "{name} would be suppressed by the overflow clip"
        );
    }
}

#[test]
fn opacity_layer_zeroes_rows_lazily_within_the_dirty_span() {
    let mut layer = super::OpacityLayer::new(2, 8);
    // Pre-fill with stale pixels to prove first writes re-zero their rows.
    layer.pixels.fill(0xdead_beef);

    layer.row_mut(5)[0] = 0xffff_0000;
    layer.row_mut(2)[1] = 0xff00_ff00;

    assert_eq!(layer.dirty, Some((2, 5)));
    // Written cells keep their value; untouched cells in the span are zero.
    assert_eq!(layer.row(2), &[0, 0xff00_ff00]);
    assert_eq!(layer.row(3), &[0, 0]);
    assert_eq!(layer.row(4), &[0, 0]);
    assert_eq!(layer.row(5), &[0xffff_0000, 0]);
    // Rows outside the span are untouched (stale, never composited).
    assert_eq!(layer.row(1), &[0xdead_beef, 0xdead_beef]);

    layer.reset();
    assert_eq!(layer.dirty, None);
    // After reset the next write re-zeroes its row.
    layer.row_mut(5)[0] = 1;
    assert_eq!(layer.row(5), &[1, 0]);
}

#[test]
fn retained_damage_admits_only_text_and_controlled_value_paint_changes() {
    let document = |id: u64, kind: &str, text: &str, value: Option<&str>| {
        let mut source = Node {
            id,
            kind: kind.to_owned(),
            props: BTreeMap::new(),
            text: String::new(),
            children: Vec::new(),
        };
        if let Some(value) = value {
            source.props.insert("value".to_owned(), Value::from(value));
        }
        DocumentNode {
            source,
            paint_text: text.to_owned(),
            computed: crate::style::Computed::default(),
            children: Vec::new(),
        }
    };

    let previous = [document(7, "span", "0:01", None)];
    let current = [document(7, "span", "0:02", None)];
    let mut changed = Vec::new();
    assert!(collect_local_paint_changes(
        &previous,
        &current,
        &mut changed
    ));
    assert_eq!(changed, [7]);

    let previous = [document(8, "input", "", Some("1.0"))];
    let current = [document(8, "input", "", Some("1.1"))];
    changed.clear();
    assert!(collect_local_paint_changes(
        &previous,
        &current,
        &mut changed
    ));
    assert_eq!(changed, [8]);

    let previous = [document(9, "span", "same", None)];
    let mut current = document(9, "span", "same", None);
    current.computed.set("color", "#ffffff");
    changed.clear();
    assert!(!collect_local_paint_changes(
        &previous,
        &[current],
        &mut changed
    ));
}

#[test]
fn damage_raster_persists_only_the_scissored_span() {
    struct Target {
        width: usize,
        height: usize,
        pixels: Vec<u32>,
    }

    impl Raster for Target {
        fn width(&self) -> usize {
            self.width
        }

        fn height(&self) -> usize {
            self.height
        }

        fn row(&self, row: usize) -> &[u32] {
            &self.pixels[row * self.width..(row + 1) * self.width]
        }

        fn row_mut(&mut self, row: usize) -> &mut [u32] {
            &mut self.pixels[row * self.width..(row + 1) * self.width]
        }
    }

    let mut target = Target {
        width: 5,
        height: 3,
        pixels: vec![1; 15],
    };
    {
        let mut damaged = super::DamageRaster::new(
            &mut target,
            PhysicalRect {
                x1: 1,
                y1: 1,
                x2: 4,
                y2: 3,
            },
        );
        damaged.row_mut(0).fill(2);
        damaged.row_mut(1).fill(3);
        damaged.row_mut(2).fill(4);
    }

    assert_eq!(target.row(0), &[1, 1, 1, 1, 1]);
    assert_eq!(target.row(1), &[1, 3, 3, 3, 1]);
    assert_eq!(target.row(2), &[1, 4, 4, 4, 1]);
}

/// Loads the checked UI faces from the repository assets, like the font unit
/// tests do, so the taffy measure callback shapes real text.
fn test_font() -> crate::font::Font {
    let fonts = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts");
    crate::font::Font::from_faces(
        std::fs::read(fonts.join("liteos-ui-regular.otf")).expect("regular UI face"),
        std::fs::read(fonts.join("liteos-ui-bold.otf")).expect("bold UI face"),
    )
    .expect("UI faces register")
}

fn text_node(class: &str, text: &str) -> Node {
    Node {
        id: 1,
        kind: "span".to_owned(),
        props: BTreeMap::from([("className".to_owned(), Value::String(class.to_owned()))]),
        text: String::new(),
        children: vec![Node {
            id: 2,
            kind: "#text".to_owned(),
            props: BTreeMap::new(),
            text: text.to_owned(),
            children: Vec::new(),
        }],
    }
}

/// A proportional text leaf carries no definite size in its taffy style; the
/// measure callback sizes it from a parley layout under the real inline
/// constraint, so wrapping text grows its box in whole line-height steps.
#[test]
fn proportional_text_leaf_wraps_via_the_measure_callback() {
    let font = test_font();
    let sheet = crate::style::Sheet::parse(".t { font-size: 11px; line-height: 14px; }").unwrap();
    let node = text_node("t", "alpha beta gamma delta epsilon zeta eta theta");
    let computed = sheet.compute(&node, &[]);
    let style = super::layout::to_taffy(&node, &computed);
    assert_eq!(style.size.width, Dimension::auto());
    assert_eq!(style.size.height, Dimension::auto());

    let mut tree = TaffyTree::<super::layout::TextMeasure>::new();
    let leaf = tree
        .new_leaf_with_context(
            style,
            super::layout::TextMeasure {
                text: super::layout::text_content(&node),
                computed: computed.clone(),
            },
        )
        .unwrap();
    let root = tree
        .new_with_children(
            Style {
                display: Display::Block,
                size: Size {
                    width: Dimension::length(100.0),
                    height: Dimension::length(400.0),
                },
                ..Style::default()
            },
            &[leaf],
        )
        .unwrap();
    tree.compute_layout_with_measure(
        root,
        Size {
            width: AvailableSpace::Definite(100.0),
            height: AvailableSpace::Definite(400.0),
        },
        |known, available, _node, context, _style| match context {
            Some(measure) => font.measure_text(&measure.computed, &measure.text, known, available),
            None => Size::ZERO,
        },
    )
    .unwrap();

    let laid_out = tree.layout(leaf).unwrap();
    // The block child stretches to the 100px container width and wraps to
    // whole 14px lines well past one line.
    assert_eq!(laid_out.size.width, 100.0);
    assert!(laid_out.size.height >= 28.0);
    assert_eq!(laid_out.size.height % 14.0, 0.0);
}

/// `white-space: nowrap` text keeps one line: the measure callback reports the
/// overflowing advance and one line-height, exactly what `text-overflow:
/// ellipsis` needs at paint time.
#[test]
fn nowrap_text_leaf_overflows_via_the_measure_callback() {
    let font = test_font();
    let sheet = crate::style::Sheet::parse(
        ".t { font-size: 11px; line-height: 14px; white-space: nowrap; }",
    )
    .unwrap();
    let node = text_node("t", "alpha beta gamma delta epsilon zeta eta theta");
    let computed = sheet.compute(&node, &[]);
    let style = super::layout::to_taffy(&node, &computed);

    let mut tree = TaffyTree::<super::layout::TextMeasure>::new();
    let leaf = tree
        .new_leaf_with_context(
            style,
            super::layout::TextMeasure {
                text: super::layout::text_content(&node),
                computed: computed.clone(),
            },
        )
        .unwrap();
    tree.compute_layout_with_measure(
        leaf,
        Size {
            width: AvailableSpace::Definite(60.0),
            height: AvailableSpace::Definite(400.0),
        },
        |known, available, _node, context, _style| match context {
            Some(measure) => font.measure_text(&measure.computed, &measure.text, known, available),
            None => Size::ZERO,
        },
    )
    .unwrap();

    let laid_out = tree.layout(leaf).unwrap();
    assert_eq!(laid_out.size.height, 14.0);
}
