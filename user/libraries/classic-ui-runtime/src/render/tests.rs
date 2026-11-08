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

// 原有列表滚动用例不经过普通容器的排版/裁剪/Tab 顺序；此生产树路径捕获不可见表单项失联和滚轮被焦点拉回。
#[test]
fn scrolling_container_reveals_keyboard_focus_without_undoing_wheel_scroll() {
    for scale in [1, 2] {
        let theme = Theme::classic();
        let catalog = typeface::FontCatalog::load(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../assets/classic/fonts"),
        )
        .unwrap();
        let fonts = Fonts::new(Rc::new(catalog), &theme.fonts, scale as u32);
        let mut scratch = [0];
        let painter = Painter::new(
            Surface::new(&mut scratch, 1, 1, 1).unwrap(),
            &theme,
            &fonts,
            scale,
        );
        let mut tree = Tree::new();
        let spec = |kind, state, layout| classic_ui_abi::Widget {
            kind,
            state,
            layout,
            group: 0,
            text: classic_ui_abi::Text {
                data: std::ptr::null(),
                len: 0,
            },
        };
        let root = tree
            .insert(
                0,
                spec(
                    widget::COLUMN,
                    state::VSCROLL,
                    classic_ui_abi::Layout {
                        grow: 1,
                        padding: 2,
                        ..Default::default()
                    },
                ),
                String::new(),
            )
            .unwrap();
        let mut fields = Vec::new();
        for name in ["first", "second", "last"] {
            fields.push(
                tree.insert(
                    root,
                    spec(
                        widget::EDIT,
                        0,
                        classic_ui_abi::Layout {
                            height: 40,
                            ..Default::default()
                        },
                    ),
                    name.into(),
                )
                .unwrap(),
            );
        }
        let bounds = Rect::new(0, 0, 160, 70);
        tree.layout(bounds, &painter);
        assert!(tree.nodes[&root].scroll_bar().is_some());
        assert!(tree.nodes[&fields[2]].clip.is_empty());
        tree.focus_next(false);
        tree.layout(bounds, &painter);
        tree.focus_next(false);
        tree.layout(bounds, &painter);
        assert_eq!(tree.focus, Some(fields[2]));
        let page = tree.nodes[&root].viewport();
        let field = tree.nodes[&fields[2]].rect;
        assert!(field.y >= page.y && field.bottom() <= page.bottom());
        tree.pointer = (field.x + 1, field.y + 1);
        assert!(tree.scroll_input(
            &classic_ui::EventKind::Wheel {
                vertical: -20,
                horizontal: 0
            },
            painter.line_height()
        ));
        tree.layout(bounds, &painter);
        assert_eq!(tree.nodes[&root].position.position, 0);
        assert_eq!(tree.focus, Some(fields[2]));
        assert!(tree.nodes[&fields[2]].clip.is_empty());
        let bar = tree.nodes[&root].scroll_bar().unwrap();
        tree.pointer = (bar.x + 1, bar.y + 1);
        assert_eq!(
            tree.hit(),
            Some(root),
            "children cannot hit through the scrollbar gutter"
        );
        for field in &fields[..2] {
            tree.nodes.get_mut(field).unwrap().state = state::HIDDEN;
        }
        tree.layout_dirty = true;
        tree.layout(bounds, &painter);
        assert!(tree.nodes[&root].scroll_bar().is_none());
        assert_eq!(tree.nodes[&root].position.position, 0);
        assert!(tree.nodes[&fields[2]].clip.width > field.width);
    }
}

// 原有标签用例只覆盖单次绘制；这里用生产 Tree 损伤与整帧光栅逐像素对账，
// 捕获移除/移动控件或关闭覆盖层后残影，以及无变化定时推进导致全树失效的回归。
#[test]
fn retained_tree_damage_matches_full_raster_after_content_focus_geometry_and_popup_changes() {
    for scale in [1, 2] {
        let bounds = Rect::new(0, 0, 220, 130);
        let theme = Theme::classic();
        let catalog = typeface::FontCatalog::load(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../assets/classic/fonts"),
        )
        .unwrap();
        let fonts = Fonts::new(Rc::new(catalog), &theme.fonts, scale as u32);
        let pictures = PictureLoader::default();
        let now = std::time::Instant::now();
        let assets = CanvasAssets {
            mono: None,
            pictures: &pictures,
            screen: None,
            now,
        };
        let mut scratch = [0];
        let probe = Painter::new(
            Surface::new(&mut scratch, 1, 1, 1).unwrap(),
            &theme,
            &fonts,
            scale,
        );
        let spec = |kind| classic_ui_abi::Widget {
            kind,
            state: 0,
            group: 0,
            layout: Default::default(),
            text: classic_ui_abi::Text {
                data: std::ptr::null(),
                len: 0,
            },
        };
        let mut tree = Tree::new();
        tree.layout_bounds = Some((bounds, scale));
        let button = tree.insert(0, spec(widget::BUTTON), "确定".into()).unwrap();
        let label = tree
            .insert(0, spec(widget::LABEL), "原始文本".into())
            .unwrap();
        let popup = tree
            .insert(0, spec(widget::POPUP_MENU), String::new())
            .unwrap();
        for (id, rect) in [
            (button, Rect::new(8, 8, 70, 24)),
            (label, Rect::new(8, 50, 100, 22)),
        ] {
            let node = tree.nodes.get_mut(&id).unwrap();
            node.rect = rect;
            node.clip = rect;
        }
        tree.nodes
            .get_mut(&popup)
            .unwrap()
            .menu
            .as_mut()
            .unwrap()
            .entries = vec![classic_ui::DropMenu {
            title: String::new(),
            items: vec![classic_ui::DropItem::new("打开".into(), String::new(), 1)],
        }];
        let (width, height) = (bounds.width * scale, bounds.height * scale);
        let paint = |tree: &Tree, pixels: &mut [u32], regions: &[Rect]| {
            let mut painter = Painter::new(
                Surface::new(pixels, width, height, width as usize).unwrap(),
                &theme,
                &fonts,
                scale,
            );
            for region in regions {
                painter.clipped(*region, |painter| {
                    painter.fill(bounds, C::ButtonFace);
                    tree.paint(painter, true, &assets);
                });
            }
        };
        let mut retained = vec![0; (width * height) as usize];
        paint(&tree, &mut retained, &[bounds]);
        tree.presented(&probe, true, now);
        for step in 0..6 {
            match step {
                0 => tree.nodes.get_mut(&label).unwrap().text = "新的内容".into(),
                1 => tree.focus = Some(button),
                2 => {
                    let node = tree.nodes.get_mut(&button).unwrap();
                    node.rect = Rect::new(125, 8, 70, 24);
                    node.clip = node.rect;
                }
                3 => {
                    let menu = tree.nodes.get_mut(&popup).unwrap().menu.as_mut().unwrap();
                    menu.state
                        .show_popup(&probe, &menu.entries, (50, 40), bounds);
                }
                4 => tree
                    .nodes
                    .get_mut(&popup)
                    .unwrap()
                    .menu
                    .as_mut()
                    .unwrap()
                    .state
                    .close(),
                5 => tree.remove(label).unwrap(),
                _ => unreachable!(),
            }
            let damage = tree.damage(&probe, true, now);
            assert!(
                !damage.is_empty(),
                "missing damage: step={step}, scale={scale}"
            );
            if step == 0 {
                assert!(
                    damage
                        .iter()
                        .all(|rect| rect.intersect(Rect::new(8, 8, 70, 24)).is_empty()),
                    "text update invalidated unrelated button"
                );
            }
            paint(&tree, &mut retained, &damage);
            let mut reference = vec![0; retained.len()];
            paint(&tree, &mut reference, &[bounds]);
            assert_eq!(
                retained, reference,
                "incomplete damage: step={step}, scale={scale}"
            );
            tree.presented(&probe, true, now);
            assert!(tree.damage(&probe, true, now).is_empty());
            assert!(!tree.animate(now + std::time::Duration::from_millis(10)));
            assert!(
                tree.damage(&probe, true, now).is_empty(),
                "idle animation check dirtied nodes"
            );
        }
    }
}
