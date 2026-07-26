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

use super::{PhysicalRect, excludes_window};

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
