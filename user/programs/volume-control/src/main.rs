//! 音量控制（`sndvol`）：Windows 2000 的 sndvol32.exe 对应物，独立窗口的单列混音器：平衡、音量与全部静音。
//! 托盘的小面板由外壳使用同一份 [`sndvol::Volume`] 状态与几何绘制。
extern crate liteos_runtime as _;
use canvas::{Rect, Surface};
use classic_theme::{Edge, SystemColor as C, Theme};
use classic_ui::{
    Decoration, DecorationAction, Display, DropItem, DropMenu, EventKind, FrameKind, Ink, Painter,
    Popup, title_rects,
};
use sndvol::{Layout, Volume};
use std::{path::Path, process::ExitCode, time::Duration};
use typeface::Typeface;

const MENU_HEIGHT: i32 = 19;
const ESCAPE: u32 = 1;
const ENTER: u32 = 28;
const UP: u32 = 103;
const DOWN: u32 = 108;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Exit,
    About,
}

fn menus() -> Vec<DropMenu<Command>> {
    vec![
        DropMenu {
            title: "选项(P)",
            items: vec![DropItem::new("退出(X)", "", Command::Exit)],
        },
        DropMenu {
            title: "帮助(H)",
            items: vec![DropItem::new("关于音量控制(A)", "", Command::About)],
        },
    ]
}

fn inside(rect: Rect, (x, y): (f64, f64)) -> bool {
    x >= f64::from(rect.x)
        && x < f64::from(rect.right())
        && y >= f64::from(rect.y)
        && y < f64::from(rect.bottom())
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut display = Display::connect()?;
    let window = display.create_window("音量控制", "org.liteos.sndvol", (136, 330))?;
    let mut decoration = Decoration::new(FrameKind::Fixed);
    decoration.load_icon("sound-volume");
    let theme = Theme::classic();
    let mut volume = Volume::open();
    let menus = menus();
    let mut popup: Option<Popup> = None;
    let mut about = false;
    let mut font: Option<Typeface> = None;
    let mut scale = 0;
    let mut pointer: (f64, f64) = (0., 0.);
    let mut closing = false;
    while !closing {
        let event = display.next_event_timeout(Duration::from_millis(100))?;
        let Some(c) = display.configuration(window) else {
            continue;
        };
        let client = decoration.client_rect(c);
        let bar = Rect::new(client.x, client.y, client.width, MENU_HEIGHT);
        let body = Rect::new(
            client.x,
            client.y + MENU_HEIGHT,
            client.width,
            client.height - MENU_HEIGHT,
        );
        let layout = Layout::dialog(body);
        let mut dirty = volume.poll();
        if let Some(event) = event {
            if event.window != window {
                continue;
            }
            match decoration.event(&event.kind, c) {
                Some(DecorationAction::Request(request)) => display.request(window, request)?,
                Some(DecorationAction::Close) => break,
                Some(DecorationAction::Cursor(shape)) => display.set_cursor_shape(shape)?,
                None => {}
            }
            dirty |= !matches!(event.kind, EventKind::FramePresented { .. });
            if let EventKind::Configure(c) = &event.kind
                && scale != c.scale
            {
                font = Some(Typeface::load(
                    Path::new("/usr/share/liteos/fonts"),
                    c.scale as u32,
                )?);
                scale = c.scale;
            }
            let Some(font) = font.as_ref() else { continue };
            let mut scratch = [0u32; 1];
            let probe = Painter::new(
                Surface::new(&mut scratch, 1, 1, 1).ok_or("probe surface")?,
                &theme,
                font,
                c.scale,
            );
            let titles = title_rects(&probe, bar, &menus);
            let at = (pointer.0.floor() as i32, pointer.1.floor() as i32);
            match event.kind {
                EventKind::Close => closing = true,
                EventKind::PointerEnter { x, y } => pointer = (x, y),
                EventKind::PointerMove { x, y } => {
                    pointer = (x, y);
                    let at = (x.floor() as i32, y.floor() as i32);
                    if let Some(open) = &mut popup {
                        if let Some(index) = titles.iter().position(|r| inside(*r, (x, y)))
                            && index != open.menu
                        {
                            *open = Popup::open(&probe, &menus, index, titles[index]);
                        }
                        open.hover = open.item_at(&menus[open.menu].items, at);
                    } else {
                        volume.motion(&layout, x, y);
                    }
                }
                EventKind::PointerLeave | EventKind::Focus(false) => volume.cancel(),
                EventKind::Button {
                    code: 0x110,
                    pressed: true,
                    ..
                } => {
                    if about {
                        about = false;
                    } else if let Some(open) = popup.take() {
                        if let Some(item) = open.item_at(&menus[open.menu].items, at) {
                            match menus[open.menu].items[item].command {
                                Command::Exit => closing = true,
                                Command::About => about = true,
                            }
                        } else if inside(open.rect, pointer) {
                            popup = Some(open);
                        }
                    } else if let Some(index) = titles.iter().position(|r| inside(*r, pointer)) {
                        popup = Some(Popup::open(&probe, &menus, index, titles[index]));
                    } else {
                        volume.button(&layout, pointer.0, pointer.1, true);
                    }
                }
                EventKind::Button {
                    code: 0x110,
                    pressed: false,
                    ..
                } => volume.cancel(),
                EventKind::Key(key) if key.pressed => {
                    if about {
                        about = false;
                    } else if let Some(open) = &mut popup {
                        let items = &menus[open.menu].items;
                        match key.code {
                            ESCAPE => popup = None,
                            UP => open.step(items.len(), -1),
                            DOWN => open.step(items.len(), 1),
                            ENTER => {
                                if let Some(index) = open.hover {
                                    let command = items[index].command;
                                    popup = None;
                                    match command {
                                        Command::Exit => closing = true,
                                        Command::About => about = true,
                                    }
                                }
                            }
                            _ => {}
                        }
                    } else if key.code == ESCAPE {
                        closing = true;
                    } else {
                        volume.key(key.code);
                    }
                }
                _ => {}
            }
        }
        let Some(font) = font.as_ref() else { continue };
        if dirty && display.ready_to_draw(window) {
            let (w, h) = c.pixel_size()?;
            let mut pixels = vec![0; w as usize * h as usize];
            let surface = Surface::new(&mut pixels, w, h, w as usize).ok_or("invalid surface")?;
            let mut p = Painter::new(surface, &theme, font, c.scale);
            p.fill(Rect::new(0, 0, c.width, c.height), C::ButtonFace);
            decoration.paint(&mut p, c, "音量控制");
            let titles = title_rects(&p, bar, &menus);
            let open = popup.as_ref().map(|open| open.menu);
            for (index, (menu, rect)) in menus.iter().zip(&titles).enumerate() {
                let hovered = inside(*rect, pointer) && open.is_none();
                p.menu_bar_item(
                    *rect,
                    menu.title,
                    hovered || open == Some(index),
                    open == Some(index),
                );
            }
            volume.paint(&mut p, &layout);
            if let Some(open) = &popup {
                open.paint(&mut p, &menus[open.menu].items);
            }
            if about {
                let dialog = Rect::new(client.x + 10, client.y + 90, client.width - 20, 90);
                let inner = p.edge(dialog, Edge::Raised);
                p.fill(inner, C::ButtonFace);
                let ink = Ink::Color(C::ButtonText);
                p.text(dialog.x + 10, dialog.y + 14, "LiteOS 音量控制", ink, true);
                p.text(
                    dialog.x + 10,
                    dialog.y + 36,
                    "调整主音量与左右平衡。",
                    ink,
                    false,
                );
                p.text(dialog.x + 10, dialog.y + 62, "点击任意处关闭。", ink, false);
            }
            display.present(window, &pixels)?;
        }
    }
    display.destroy_window(window)?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sndvol: error: {error}");
            ExitCode::FAILURE
        }
    }
}
