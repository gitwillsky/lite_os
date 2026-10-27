use super::*;
use crate::label_edit::LabelEdit;
use canvas::{Color, Surface};
use classic_theme::Theme;
use classic_ui::Fonts;
use std::{path::Path, rc::Rc};

fn paint_labels(mode: u32, editing: Option<bool>, scale: i32) -> Vec<u32> {
    let mut node = Node::new(
        0,
        classic_ui_abi::Widget {
            kind: widget::LIST,
            state: state::BORDERLESS,
            group: 0,
            layout: Default::default(),
            text: classic_ui_abi::Text {
                data: std::ptr::null(),
                len: 0,
            },
        },
        String::new(),
    )
    .unwrap();
    node.rect = Rect::new(0, 0, 150, 100);
    node.clip = node.rect;
    node.rows.push((1, "新建文本文档.txt".into()));
    node.selected = Some(1);
    node.table.as_mut().unwrap().mode = mode;
    node.table.as_mut().unwrap().cells.1 = 120;
    node.label_edit = editing.map(|pending| LabelEdit { row: 1, pending });
    let theme = Theme::classic();
    let catalog = typeface::FontCatalog::load(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../assets/classic/fonts"),
    )
    .unwrap();
    let fonts = Fonts::new(Rc::new(catalog), &theme.fonts, scale as u32);
    let (width, height) = (150 * scale, 100 * scale);
    let mut pixels = vec![0xffffffff; (width * height) as usize];
    let mut surface = Surface::new(&mut pixels, width, height, width as usize).unwrap();
    surface.fill_rect(Rect::new(0, 0, width, height), Color::rgb(255, 255, 255));
    let mut painter = Painter::new(surface, &theme, &fonts, scale);
    paint_list(&node, &mut painter, true);
    pixels
}

#[test]
fn active_label_editor_replaces_the_label_in_each_icon_view_at_both_scales() {
    for scale in [1, 2] {
        for mode in [1, 2, 3] {
            let pixels = paint_labels(mode, Some(false), scale);
            assert!(
                pixels.iter().all(|pixel| pixel & 0xffffff == 0xffffff),
                "original label still drawn: mode={mode}, scale={scale}"
            );
        }
    }
}

#[test]
fn pending_or_cancelled_label_editor_restores_the_original_label() {
    for mode in [1, 2, 3] {
        let original = paint_labels(mode, None, 1);
        assert!(original.iter().any(|pixel| pixel & 0xffffff != 0xffffff));
        assert_eq!(paint_labels(mode, Some(true), 1), original);
    }
}
