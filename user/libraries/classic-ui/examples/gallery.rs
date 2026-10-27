//! 把所有控件渲染到一张 PPM（P6）：`cargo run -p classic-ui --example gallery -- <scale> <out.ppm>`。
//! 供人工对照规格；自动化回归见 `tests.rs`。

use std::{fs, io::Write, path::Path};

use canvas::{Rect, Surface};
use classic_theme::{SystemColor, Theme};
use classic_ui::Fonts;
use classic_ui::{
    ButtonState, CaptionButton, FrameKind, Ink, MenuItemState, Painter, ScrollbarPart,
};
use typeface::FontCatalog;

const WIDTH: i32 = 420;
const HEIGHT: i32 = 330;

fn main() {
    let mut arguments = std::env::args().skip(1);
    let scale: i32 = arguments
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let output = arguments.next().unwrap_or_else(|| "gallery.ppm".into());
    let theme = Theme::classic();
    let fonts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../assets/classic/fonts");
    let typeface = Fonts::new(
        std::rc::Rc::new(FontCatalog::load(&fonts).unwrap()),
        &theme.fonts,
        scale as u32,
    );
    let (width, height) = ((WIDTH * scale) as usize, (HEIGHT * scale) as usize);
    let mut pixels = vec![0u32; width * height];
    let surface = Surface::new(&mut pixels, width as i32, height as i32, width).unwrap();
    let mut p = Painter::new(surface, &theme, &typeface, scale);

    p.fill(Rect::new(0, 0, WIDTH, HEIGHT), SystemColor::Background);

    // 活动与非活动窗口
    let window = Rect::new(8, 8, 300, 220);
    let content = p.window_frame(window, FrameKind::Resizable);
    p.caption_bar(
        Rect::new(content.x, content.y, content.width, 19),
        "登录到 LiteOS - Log On",
        None,
        true,
        &[
            (CaptionButton::Minimize, true),
            (CaptionButton::Maximize, true),
            (CaptionButton::Close, true),
        ],
        None,
    );
    let client = Rect::new(
        content.x,
        content.y + 19,
        content.width,
        content.height - 19,
    );
    p.fill(client, SystemColor::ButtonFace);
    p.menu_bar_item(
        Rect::new(client.x, client.y, 40, 19),
        "文件(F)",
        false,
        false,
    );
    p.menu_bar_item(
        Rect::new(client.x + 40, client.y, 40, 19),
        "编辑(E)",
        true,
        false,
    );
    p.menu_bar_item(
        Rect::new(client.x + 80, client.y, 40, 19),
        "查看(V)",
        false,
        true,
    );

    let body = Rect::new(client.x + 8, client.y + 28, 200, 120);
    p.group_box(body, "用户信息");
    p.text(
        body.x + 12,
        body.y + 18,
        "用户名(U):",
        Ink::Color(SystemColor::ButtonText),
        false,
    );
    let edit = Rect::new(body.x + 80, body.y + 14, 100, 21);
    let inner = p.edit_frame(edit, true);
    p.text(
        inner.x + 2,
        inner.y + 3,
        "Administrator",
        Ink::Color(SystemColor::WindowText),
        false,
    );
    p.text(
        body.x + 12,
        body.y + 44,
        "Password:",
        Ink::Color(SystemColor::ButtonText),
        false,
    );
    p.edit_frame(Rect::new(body.x + 80, body.y + 40, 100, 21), true);
    p.checkbox(body.x + 12, body.y + 70, true, false);
    p.text(
        body.x + 30,
        body.y + 70,
        "记住我",
        Ink::Color(SystemColor::ButtonText),
        false,
    );
    p.checkbox(body.x + 100, body.y + 70, false, false);
    p.checkbox(body.x + 120, body.y + 70, true, true);
    p.radio(body.x + 12, body.y + 92, true, false);
    p.radio(body.x + 40, body.y + 92, false, false);
    p.radio(body.x + 68, body.y + 92, true, true);

    let buttons_y = client.y + client.height - 30;
    p.push_button(
        Rect::new(client.x + 8, buttons_y, 75, 23),
        "确定",
        ButtonState {
            default: true,
            focused: true,
            ..Default::default()
        },
    );
    p.push_button(
        Rect::new(client.x + 91, buttons_y, 75, 23),
        "取消",
        ButtonState::default(),
    );
    p.push_button(
        Rect::new(client.x + 174, buttons_y, 75, 23),
        "按下",
        ButtonState {
            pressed: true,
            ..Default::default()
        },
    );
    p.push_button(
        Rect::new(client.x + 8, buttons_y - 28, 75, 23),
        "禁用",
        ButtonState {
            disabled: true,
            ..Default::default()
        },
    );

    // 滚动条
    p.scrollbar(
        Rect::new(client.right() - 16, client.y + 19, 16, 140),
        true,
        (100, 20, 30),
        None,
    );
    p.scrollbar(
        Rect::new(client.x + 120, client.y + 4, 120, 16),
        false,
        (50, 10, 20),
        Some(ScrollbarPart::TrackAfter),
    );
    p.scrollbar(
        Rect::new(client.x + 130, buttons_y - 28, 100, 16),
        false,
        (0, 0, 0),
        None,
    );

    // 菜单
    let menu = Rect::new(320, 8, 92, 114);
    let area = p.menu_popup(menu);
    let row = |index: i32| Rect::new(area.x, area.y + index * 18, area.width, 18);
    p.menu_item(
        row(0),
        "打开(O)",
        "Ctrl+O",
        MenuItemState {
            highlighted: true,
            ..Default::default()
        },
    );
    p.menu_item(
        row(1),
        "保存(S)",
        "Ctrl+S",
        MenuItemState {
            checked: true,
            ..Default::default()
        },
    );
    p.menu_item(
        row(2),
        "另存为",
        "",
        MenuItemState {
            disabled: true,
            ..Default::default()
        },
    );
    let separator = Rect::new(area.x, area.y + 3 * 18, area.width, 8);
    p.menu_separator(separator);
    p.menu_item(
        Rect::new(area.x, area.y + 3 * 18 + 8, area.width, 18),
        "最近",
        "",
        MenuItemState {
            has_submenu: true,
            ..Default::default()
        },
    );
    p.menu_item(
        Rect::new(area.x, area.y + 4 * 18 + 8, area.width, 18),
        "退出(X)",
        "Alt+F4",
        MenuItemState::default(),
    );

    // 非活动窗口与提示
    let second = Rect::new(8, 236, 220, 88);
    let content = p.window_frame(second, FrameKind::Fixed);
    p.caption_bar(
        Rect::new(content.x, content.y, content.width, 19),
        "一个标题特别特别长的非活动窗口 Very Long Title",
        None,
        false,
        &[
            (CaptionButton::Minimize, false),
            (CaptionButton::Restore, false),
            (CaptionButton::Close, true),
        ],
        None,
    );
    p.fill(
        Rect::new(
            content.x,
            content.y + 19,
            content.width,
            content.height - 19,
        ),
        SystemColor::ButtonFace,
    );
    p.tooltip(240, 250, "这是工具提示 Tooltip");

    let mut file = fs::File::create(&output).unwrap();
    write!(file, "P6\n{width} {height}\n255\n").unwrap();
    let mut rgb = Vec::with_capacity(width * height * 3);
    for pixel in &pixels {
        rgb.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, *pixel as u8]);
    }
    file.write_all(&rgb).unwrap();
}
