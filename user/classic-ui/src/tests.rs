use std::path::Path;
use std::sync::OnceLock;

use canvas::{Rect, Surface};
use classic_theme::{Edge, SystemColor, Theme};
use typeface::Typeface;

use crate::{
    ButtonState, CaptionButton, FrameKind, Ink, MenuItemState, Painter, ScrollbarPart, thumb_span,
};

const SIZE: i32 = 200;

fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(Theme::classic)
}

fn typeface(scale: u32) -> &'static Typeface {
    static FACES: [OnceLock<Typeface>; 2] = [OnceLock::new(), OnceLock::new()];
    FACES[scale as usize - 1].get_or_init(|| {
        Typeface::load(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts"),
            scale,
        )
        .unwrap()
    })
}

/// 渲染一个 200 × 200 逻辑像素的画布，背景为桌面色，返回物理像素。
struct Rendered {
    pixels: Vec<u32>,
    scale: i32,
}

impl Rendered {
    /// 逻辑坐标 `(x, y)` 处左上物理像素的 RGB。
    fn at(&self, x: i32, y: i32) -> u32 {
        let (px, py) = (x * self.scale, y * self.scale);
        self.pixels[(py * SIZE * self.scale + px) as usize] & 0x00FF_FFFF
    }

    /// 同一逻辑像素内的所有物理像素是否同色（立体线在 scale N 下是 N × N 的整块）。
    fn uniform(&self, x: i32, y: i32) -> bool {
        let first = self.at(x, y);
        (0..self.scale).all(|dy| {
            (0..self.scale).all(|dx| {
                let (px, py) = (x * self.scale + dx, y * self.scale + dy);
                self.pixels[(py * SIZE * self.scale + px) as usize] & 0x00FF_FFFF == first
            })
        })
    }
}

fn render(scale: i32, draw: impl FnOnce(&mut Painter<'_>)) -> Rendered {
    let side = SIZE * scale;
    let mut pixels = vec![0u32; (side * side) as usize];
    let surface = Surface::new(&mut pixels, side, side, side as usize).unwrap();
    let mut painter = Painter::new(surface, theme(), typeface(scale as u32), scale);
    painter.fill(Rect::new(0, 0, SIZE, SIZE), SystemColor::Background);
    draw(&mut painter);
    Rendered { pixels, scale }
}

fn rgb(color: SystemColor) -> u32 {
    let value = theme().color(color);
    u32::from(value.red) << 16 | u32::from(value.green) << 8 | u32::from(value.blue)
}

/// 对 1x 与 2x 各运行一次同一断言：几何比例必须不随 scale 改变。
fn at_both_scales(check: impl Fn(i32)) {
    for scale in [1, 2] {
        check(scale);
    }
}

#[test]
fn raised_edge_uses_documented_layers_and_scales_uniformly() {
    at_both_scales(|scale| {
        let image = render(scale, |p| {
            let inner = p.edge(Rect::new(10, 10, 40, 20), Edge::Raised);
            assert_eq!(inner, Rect::new(12, 12, 36, 16));
        });
        // 外层：左上 3DLIGHT、右下 3DDKSHADOW；内层：左上 BTNHILIGHT、右下 BTNSHADOW。
        assert_eq!(image.at(10, 10), rgb(SystemColor::ButtonLight));
        assert_eq!(image.at(49, 29), rgb(SystemColor::ButtonDkShadow));
        assert_eq!(image.at(11, 11), rgb(SystemColor::ButtonHilight));
        assert_eq!(image.at(48, 28), rgb(SystemColor::ButtonShadow));
        for (x, y) in [
            (10, 10),
            (49, 29),
            (11, 11),
            (48, 28),
            (30, 10),
            (10, 20),
            (49, 10),
            (10, 29),
        ] {
            assert!(image.uniform(x, y), "({x},{y}) at scale {scale}");
        }
        // 边框线 N 物理像素宽：内部第一像素之外仍是背景。
        assert_eq!(image.at(30, 20), rgb(SystemColor::Background));
    });
}

#[test]
fn single_layer_edges_shrink_by_one_pixel_and_sit_on_the_outside() {
    let image = render(1, |p| {
        assert_eq!(
            p.edge(Rect::new(10, 10, 20, 10), Edge::RaisedInner),
            Rect::new(11, 11, 18, 8)
        );
        assert_eq!(
            p.edge(Rect::new(50, 10, 20, 10), Edge::SunkenOuter),
            Rect::new(51, 11, 18, 8)
        );
    });
    assert_eq!(image.at(10, 10), rgb(SystemColor::ButtonHilight));
    assert_eq!(image.at(29, 19), rgb(SystemColor::ButtonShadow));
    assert_eq!(image.at(50, 10), rgb(SystemColor::ButtonShadow));
    assert_eq!(image.at(69, 19), rgb(SystemColor::ButtonHilight));
}

#[test]
fn push_button_states_match_the_specification() {
    at_both_scales(|scale| {
        let rect = Rect::new(20, 20, 75, 23);
        let normal = render(scale, |p| p.push_button(rect, "", ButtonState::default()));
        assert_eq!(normal.at(20, 20), rgb(SystemColor::ButtonLight));
        assert_eq!(normal.at(21, 21), rgb(SystemColor::ButtonHilight));
        assert_eq!(normal.at(94, 42), rgb(SystemColor::ButtonDkShadow));
        assert_eq!(normal.at(40, 30), rgb(SystemColor::ButtonFace));

        // 按下：外圈黑，内圈 BTNSHADOW。
        let pressed = render(scale, |p| {
            p.push_button(
                rect,
                "",
                ButtonState {
                    pressed: true,
                    ..Default::default()
                },
            )
        });
        assert_eq!(pressed.at(20, 20), rgb(SystemColor::WindowFrame));
        assert_eq!(pressed.at(94, 42), rgb(SystemColor::WindowFrame));
        assert_eq!(pressed.at(21, 21), rgb(SystemColor::ButtonShadow));
        assert_eq!(pressed.at(22, 22), rgb(SystemColor::ButtonFace));

        // 默认按钮：外加一圈黑框，立体边整体内缩 1 像素。
        let default = render(scale, |p| {
            p.push_button(
                rect,
                "",
                ButtonState {
                    default: true,
                    ..Default::default()
                },
            )
        });
        assert_eq!(default.at(20, 20), rgb(SystemColor::WindowFrame));
        assert_eq!(default.at(94, 42), rgb(SystemColor::WindowFrame));
        assert_eq!(default.at(21, 21), rgb(SystemColor::ButtonLight));
        assert_eq!(default.at(93, 41), rgb(SystemColor::ButtonDkShadow));
    });
}

#[test]
fn button_label_is_centered_and_disabled_text_is_embossed() {
    let rect = Rect::new(20, 20, 75, 23);
    let ink = |image: &Rendered, color: SystemColor| {
        (rect.x..rect.right())
            .flat_map(|x| (rect.y..rect.bottom()).map(move |y| (x, y)))
            .filter(|&(x, y)| image.at(x, y) == rgb(color))
            .count()
    };
    let enabled = render(1, |p| p.push_button(rect, "确定", ButtonState::default()));
    assert!(ink(&enabled, SystemColor::ButtonText) > 20);
    let disabled = render(1, |p| {
        p.push_button(
            rect,
            "确定",
            ButtonState {
                disabled: true,
                ..Default::default()
            },
        )
    });
    assert_eq!(ink(&disabled, SystemColor::ButtonText), 0);
    assert!(ink(&disabled, SystemColor::ButtonShadow) > 20);
    // 浮雕：高光副本在阴影文字的右下方；左上角没有任何高光墨迹早于阴影。
    let first_shadow = (rect.x + 2..rect.right() - 2)
        .find(|&x| {
            (rect.y + 3..rect.bottom() - 3)
                .any(|y| disabled.at(x, y) == rgb(SystemColor::ButtonShadow))
        })
        .unwrap();
    let first_highlight = (rect.x + 3..rect.right() - 2)
        .find(|&x| {
            (rect.y + 3..rect.bottom() - 3)
                .any(|y| disabled.at(x, y) == rgb(SystemColor::ButtonHilight))
        })
        .unwrap();
    assert_eq!(first_highlight, first_shadow + 1);
    // 文字水平居中（±1 像素）。
    let columns: Vec<i32> = (rect.x + 3..rect.right() - 3)
        .filter(|&x| {
            (rect.y + 3..rect.bottom() - 3)
                .any(|y| enabled.at(x, y) == rgb(SystemColor::ButtonText))
        })
        .collect();
    let (left, right) = (
        columns[0] - rect.x,
        rect.right() - 1 - columns[columns.len() - 1],
    );
    assert!((left - right).abs() <= 2, "{left} vs {right}");
}

#[test]
fn focus_rectangle_is_dotted_and_self_erasing() {
    let rect = Rect::new(20, 20, 40, 20);
    let image = render(1, |p| {
        p.fill(rect, SystemColor::ButtonFace);
        p.focus_rect(rect.inset(3));
    });
    let inverted = |c: SystemColor| rgb(c) ^ 0x00FF_FFFF;
    assert_eq!(image.at(23, 23), inverted(SystemColor::ButtonFace));
    assert_eq!(image.at(24, 23), rgb(SystemColor::ButtonFace));
    assert_eq!(image.at(25, 23), inverted(SystemColor::ButtonFace));
    let erased = render(1, |p| {
        p.fill(rect, SystemColor::ButtonFace);
        p.focus_rect(rect.inset(3));
        p.focus_rect(rect.inset(3));
    });
    assert!((20..60).all(|x| erased.at(x, 25) == rgb(SystemColor::ButtonFace)));
}

#[test]
fn checkbox_and_radio_are_13_pixels() {
    at_both_scales(|scale| {
        let image = render(scale, |p| {
            p.checkbox(20, 20, true, false);
            p.checkbox(50, 20, false, false);
            p.radio(80, 20, true, false);
            p.radio(110, 20, false, false);
        });
        assert_eq!(image.at(20, 20), rgb(SystemColor::ButtonShadow));
        assert_eq!(image.at(21, 21), rgb(SystemColor::ButtonDkShadow));
        assert_eq!(image.at(32, 32), rgb(SystemColor::ButtonHilight));
        assert_eq!(image.at(29, 24), rgb(SystemColor::WindowText), "check mark");
        assert_eq!(image.at(55, 25), rgb(SystemColor::Window));
        assert_eq!(
            image.at(33, 20),
            rgb(SystemColor::Background),
            "13 pixels wide"
        );
        // 单选框：圆心黑点仅在选中时出现，四角透明。
        assert_eq!(image.at(86, 26), rgb(SystemColor::WindowText));
        assert_eq!(image.at(116, 26), rgb(SystemColor::Window));
        assert_eq!(image.at(80, 20), rgb(SystemColor::Background));
        assert_eq!(image.at(92, 32), rgb(SystemColor::Background));
    });
}

#[test]
fn window_frame_widths_follow_the_specification() {
    at_both_scales(|scale| {
        let rect = Rect::new(10, 10, 120, 80);
        let mut resizable = None;
        let mut fixed = None;
        let image = render(scale, |p| {
            resizable = Some(p.window_frame(rect, FrameKind::Resizable));
            fixed = Some(p.window_frame(Rect::new(10, 100, 120, 80), FrameKind::Fixed));
        });
        assert_eq!(resizable.unwrap(), Rect::new(14, 14, 112, 72));
        assert_eq!(fixed.unwrap(), Rect::new(13, 103, 114, 74));
        assert_eq!(image.at(10, 10), rgb(SystemColor::ButtonLight));
        assert_eq!(image.at(12, 40), rgb(SystemColor::ActiveBorder));
        assert_eq!(image.at(13, 40), rgb(SystemColor::ActiveBorder));
        assert_eq!(image.at(129, 89), rgb(SystemColor::ButtonDkShadow));
    });
}

#[test]
fn caption_bar_gradient_separator_and_buttons() {
    at_both_scales(|scale| {
        let bar = Rect::new(20, 20, 160, 19);
        let image = render(scale, |p| {
            p.caption_bar(
                bar,
                "标题",
                true,
                &[
                    (CaptionButton::Minimize, true),
                    (CaptionButton::Maximize, true),
                    (CaptionButton::Close, true),
                ],
                None,
            );
        });
        assert_eq!(image.at(20, 20), rgb(SystemColor::ActiveTitle));
        assert_eq!(image.at(20, 37), rgb(SystemColor::ActiveTitle));
        // 分隔线在高 18 的渐变之下。
        assert_eq!(image.at(60, 38), rgb(SystemColor::ButtonFace));
        // 渐变向右变亮，右端（按钮左侧的最后一个未被覆盖的像素列）接近 GradientActiveTitle。
        let left = image.at(60, 21);
        let right = image.at(100, 21);
        let blue = |value: u32| value & 0xFF;
        assert!(blue(right) > blue(left), "{right:06x} vs {left:06x}");
        // 关闭按钮：右边距 2，16 × 14，凸起边框；最小化与最大化之间无缝，最大化与关闭之间隔 2。
        assert_eq!(image.at(180 - 2 - 16, 22), rgb(SystemColor::ButtonLight));
        assert_eq!(image.at(180 - 3, 22 + 13), rgb(SystemColor::ButtonDkShadow));
        // 最小化与最大化紧贴（交界处是前者的右下色与后者的左上色）；最大化与关闭之间留 2 像素渐变。
        assert_eq!(image.at(143, 22), rgb(SystemColor::ButtonDkShadow));
        assert_eq!(image.at(144, 22), rgb(SystemColor::ButtonLight));
        for gap in [160, 161] {
            let color = image.at(gap, 22);
            assert!(
                ![
                    rgb(SystemColor::ButtonLight),
                    rgb(SystemColor::ButtonDkShadow),
                    rgb(SystemColor::ButtonFace)
                ]
                .contains(&color),
                "{color:06x}"
            );
        }
    });
}

#[test]
fn inactive_caption_uses_inactive_colors() {
    let image = render(1, |p| {
        p.caption_bar(Rect::new(20, 20, 160, 19), "x", false, &[], None)
    });
    assert_eq!(image.at(20, 25), rgb(SystemColor::InactiveTitle));
    assert_eq!(image.at(179, 25), rgb(SystemColor::GradientInactiveTitle));
}

fn truncated(text: &str, limit: i32) -> (String, i32) {
    let mut result = (String::new(), 0);
    render(1, |p| {
        let shown = p.truncate_to_width(text, limit, true);
        result = (shown.clone(), p.text_width(&shown, true));
    });
    result
}

#[test]
fn long_titles_are_truncated_with_an_ellipsis() {
    let (shown, width) = truncated("一个非常非常非常非常长的标题", 60);
    assert!(
        shown.ends_with("...") && width <= 60 && shown.chars().count() > 3,
        "{shown:?} {width}"
    );
    assert_eq!(truncated("短", 60).0, "短");
    // 连省略号都放不下时只剩省略号，不会 panic。
    assert_eq!(truncated("标题", 1).0, "...");
}

#[test]
fn scrollbar_geometry_and_track_dither() {
    at_both_scales(|scale| {
        let image = render(scale, |p| {
            p.scrollbar(Rect::new(20, 20, 16, 100), true, (100, 20, 30), None);
        });
        // 箭头按钮 16 × 16，凸起；轨道是 SCROLLBAR/BTNHILIGHT 的 1 像素棋盘。
        assert_eq!(image.at(20, 20), rgb(SystemColor::ButtonLight));
        assert_eq!(image.at(35, 35), rgb(SystemColor::ButtonDkShadow));
        let track = [
            image.at(20, 100),
            image.at(21, 100),
            image.at(20, 101),
            image.at(21, 101),
        ];
        let (a, b) = (rgb(SystemColor::Scrollbar), rgb(SystemColor::ButtonHilight));
        assert!(track == [a, b, b, a] || track == [b, a, a, b], "{track:x?}");
        // 滑块凸起且在轨道范围内。
        let (offset, length) = thumb_span(100 - 32, 100, 20, 30).unwrap();
        assert_eq!(image.at(20, 36 + offset), rgb(SystemColor::ButtonLight));
        assert!(offset + length <= 68);
    });
}

#[test]
fn pressed_track_is_inverted_and_disabled_scrollbar_has_no_thumb() {
    let normal = render(1, |p| {
        p.scrollbar(Rect::new(20, 20, 16, 100), true, (100, 20, 0), None)
    });
    let pressed = render(1, |p| {
        p.scrollbar(
            Rect::new(20, 20, 16, 100),
            true,
            (100, 20, 0),
            Some(ScrollbarPart::TrackAfter),
        )
    });
    assert_eq!(pressed.at(25, 100), normal.at(25, 100) ^ 0x00FF_FFFF);
    assert_eq!(pressed.at(25, 40), normal.at(25, 40));
    let disabled = render(1, |p| {
        p.scrollbar(Rect::new(20, 20, 16, 100), true, (0, 0, 0), None)
    });
    let (a, b) = (rgb(SystemColor::Scrollbar), rgb(SystemColor::ButtonHilight));
    assert!([a, b].contains(&disabled.at(25, 60)));
}

#[test]
fn thumb_span_is_proportional_clamped_and_reaches_both_ends() {
    assert_eq!(thumb_span(100, 99, 100, 0), None);
    assert_eq!(thumb_span(100, 0, 1, 0), None);
    assert_eq!(thumb_span(0, 100, 10, 0), None);
    // 页占 1/4：滑块长 25，位置 0 贴起点，最大位置贴终点。
    assert_eq!(thumb_span(100, 99, 25, 0), Some((0, 25)));
    assert_eq!(thumb_span(100, 99, 25, 75), Some((75, 25)));
    assert_eq!(thumb_span(100, 99, 25, 1000), Some((75, 25)));
    assert_eq!(thumb_span(100, 99, 25, -5), Some((0, 25)));
    // 最小长度 8，且不超过轨道。
    assert_eq!(thumb_span(100, 9999, 10, 0).unwrap().1, 8);
    assert_eq!(thumb_span(5, 100, 1, 0).unwrap().1, 5);
    // 单调。
    let offsets: Vec<i32> = (0..=75)
        .map(|pos| thumb_span(100, 99, 25, pos).unwrap().0)
        .collect();
    assert!(offsets.windows(2).all(|pair| pair[0] <= pair[1]));
}

#[test]
fn tooltip_group_box_and_menus_use_theme_colors() {
    at_both_scales(|scale| {
        let mut tooltip = Rect::default();
        let image = render(scale, |p| {
            tooltip = p.tooltip(10, 10, "提示");
            let popup = p.menu_popup(Rect::new(60, 10, 100, 80));
            let row = |i: i32| Rect::new(popup.x, popup.y + i * 18, popup.width, 18);
            p.menu_item(
                row(0),
                "打开",
                "Ctrl+O",
                MenuItemState {
                    highlighted: true,
                    ..Default::default()
                },
            );
            p.menu_item(row(1), "保存", "", MenuItemState::default());
            p.menu_bar_item(Rect::new(10, 100, 40, 19), "文件", true, false);
            p.menu_bar_item(Rect::new(60, 100, 40, 19), "编辑", false, true);
        });
        assert_eq!(
            (tooltip.height, tooltip.width > 20),
            (self::tooltip_height(scale), true)
        );
        assert_eq!(image.at(10, 10), rgb(SystemColor::WindowFrame));
        assert_eq!(image.at(12, 12), rgb(SystemColor::InfoWindow));
        // 菜单：EDGE_RAISED；高亮行左右各留 2 像素。
        assert_eq!(image.at(60, 40), rgb(SystemColor::ButtonLight));
        assert_eq!(image.at(62, 14), rgb(SystemColor::Menu));
        assert_eq!(image.at(64, 14), rgb(SystemColor::Hilight));
        assert_eq!(image.at(155, 14), rgb(SystemColor::Hilight));
        assert_eq!(image.at(156, 14), rgb(SystemColor::Menu));
        assert_eq!(image.at(150, 34), rgb(SystemColor::Menu));
        // 菜单栏：悬停凸起，打开时凹陷。
        assert_eq!(image.at(10, 100), rgb(SystemColor::ButtonHilight));
        assert_eq!(image.at(60, 100), rgb(SystemColor::ButtonShadow));
    });
}

fn tooltip_height(scale: i32) -> i32 {
    let (ascent, descent) = typeface(scale as u32).metrics();
    (ascent + descent + scale - 1) / scale + 6
}

#[test]
fn disabled_menu_item_is_embossed_and_highlight_overrides_it() {
    let image = render(1, |p| {
        p.menu_item(
            Rect::new(10, 10, 100, 18),
            "另存为",
            "",
            MenuItemState {
                disabled: true,
                ..Default::default()
            },
        );
        p.menu_item(
            Rect::new(10, 40, 100, 18),
            "另存为",
            "",
            MenuItemState {
                disabled: true,
                highlighted: true,
                ..Default::default()
            },
        );
    });
    let count = |color: SystemColor, y0: i32| {
        (10..110)
            .flat_map(|x| (y0..y0 + 18).map(move |y| (x, y)))
            .filter(|&(x, y)| image.at(x, y) == rgb(color))
            .count()
    };
    assert!(
        count(SystemColor::ButtonShadow, 10) > 10 && count(SystemColor::ButtonHilight, 10) > 10
    );
    assert!(count(SystemColor::HilightText, 40) > 10);
}

#[test]
fn edit_frame_and_group_box_geometry() {
    let image = render(1, |p| {
        assert_eq!(
            p.edit_frame(Rect::new(20, 20, 60, 21), true),
            Rect::new(22, 22, 56, 17)
        );
        p.group_box(Rect::new(20, 60, 100, 60), "组");
    });
    assert_eq!(image.at(20, 20), rgb(SystemColor::ButtonShadow));
    assert_eq!(image.at(21, 21), rgb(SystemColor::ButtonDkShadow));
    assert_eq!(image.at(40, 30), rgb(SystemColor::Window));
    assert_eq!(image.at(79, 40), rgb(SystemColor::ButtonHilight));
    // 蚀刻框的上边在标题行盒中线：外层 BTNSHADOW、内层 BTNHILIGHT；标题区被面色覆盖。
    let middle = 60 + p_line_height() / 2;
    assert_eq!(image.at(100, middle), rgb(SystemColor::ButtonShadow));
    assert_eq!(image.at(100, middle + 1), rgb(SystemColor::ButtonHilight));
    assert_eq!(image.at(28, 60), rgb(SystemColor::ButtonFace));
}

#[test]
fn ink_variants_are_distinct() {
    assert_ne!(Ink::Disabled, Ink::Color(SystemColor::ButtonShadow));
}

#[test]
fn caption_glyph_geometry_stays_inside_the_button() {
    for button in [
        CaptionButton::Minimize,
        CaptionButton::Maximize,
        CaptionButton::Restore,
        CaptionButton::Close,
    ] {
        let image = render(1, |p| {
            p.caption_bar(
                Rect::new(20, 20, 100, 19),
                "",
                true,
                &[(button, true)],
                None,
            )
        });
        let area = Rect::new(100 - 2 - 16 + 20 - 2 + 2, 22, 16, 14);
        let black = rgb(SystemColor::ButtonText);
        let dark: Vec<(i32, i32)> = (area.x - 2..area.right() + 2)
            .flat_map(|x| (area.y..area.bottom()).map(move |y| (x, y)))
            .filter(|&(x, y)| image.at(x, y) == black)
            .collect();
        assert!(!dark.is_empty(), "{button:?}");
        assert!(
            dark.iter()
                .all(|&(x, y)| (area.x + 2..area.right() - 2).contains(&x)
                    && (area.y + 1..area.bottom() - 1).contains(&y)),
            "{button:?} {dark:?}"
        );
    }
}

fn p_line_height() -> i32 {
    let (ascent, descent) = typeface(1).metrics();
    ascent + descent
}

#[test]
fn password_edit_renders_the_same_as_its_mask_at_both_scales() {
    at_both_scales(|scale| {
        let password = crate::Edit::new("中Ab");
        let mask = crate::Edit::new("***");
        let hidden = render(scale, |p| {
            p.edit(Rect::new(20, 20, 100, 23), &password, true, true);
        });
        let plain = render(scale, |p| {
            p.edit(Rect::new(20, 20, 100, 23), &mask, true, false);
        });
        assert_eq!(hidden.pixels, plain.pixels);
    });
}

#[test]
fn edit_scroll_clips_text_and_restores_the_callers_clip() {
    at_both_scales(|scale| {
        let edit = crate::Edit::new(&"long text ".repeat(100));
        let rect = Rect::new(20, 20, 80, 23);
        let image = render(scale, |p| {
            p.edit(rect, &edit, true, false);
            p.fill(Rect::new(150, 150, 4, 4), SystemColor::WindowFrame);
        });
        assert_eq!(image.at(150, 150), rgb(SystemColor::WindowFrame));
        for (x, y) in [(19, 30), (100, 30), (50, 19), (50, 43)] {
            assert_eq!(image.at(x, y), rgb(SystemColor::Background));
        }
    });
}

#[test]
fn bitmap_region_paints_only_the_requested_strip() {
    let bitmap = crate::Bitmap::from_pixels(vec![0xff00_0001, 0xff00_0002, 0xff00_0003], 3, 1);
    let mut pixels = vec![0; 3];
    let mut painter = Painter::new(
        Surface::new(&mut pixels, 3, 1, 3).unwrap(),
        theme(),
        typeface(1),
        1,
    );
    bitmap.paint_region(&mut painter, 0, 0, 1, 1);
    bitmap.paint_region(&mut painter, 2, 0, 2, 2);
    painter.into_surface();
    assert_eq!(pixels, [0xff00_0002, 0, 0]);
}

#[test]
fn clipped_drawing_stays_inside_the_rectangle_and_restores_the_clip() {
    let mut pixels = vec![0; 8 * 4];
    let mut painter = Painter::new(
        Surface::new(&mut pixels, 8, 4, 8).unwrap(),
        theme(),
        typeface(1),
        1,
    );
    painter.clipped(Rect::new(2, 0, 2, 4), |p| {
        p.fill(Rect::new(0, 0, 8, 4), SystemColor::Window)
    });
    painter.fill(Rect::new(6, 0, 1, 1), SystemColor::Window);
    painter.into_surface();
    let lit: Vec<_> = (0..8).filter(|x| pixels[*x] != 0).collect();
    assert_eq!(lit, [2, 3, 6]);
}

#[test]
fn bitmap_corner_color_fills_the_requested_height() {
    let bitmap = crate::Bitmap::from_pixels(vec![0xff10_2030, 0xffff_ffff], 2, 1);
    let mut pixels = vec![0; 2 * 3];
    let mut painter = Painter::new(
        Surface::new(&mut pixels, 2, 3, 2).unwrap(),
        theme(),
        typeface(1),
        1,
    );
    bitmap.fill_with_corner(&mut painter, 0, 0, 3);
    painter.into_surface();
    assert!(pixels.iter().all(|p| p & 0x00ff_ffff == 0x0010_2030));
}
