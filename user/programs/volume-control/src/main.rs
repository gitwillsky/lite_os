//! 音量控制（`sndvol`）：Windows 2000 的 sndvol32.exe 对应物，独立窗口的单列混音器：平衡、音量与全部静音。
//! 托盘的小面板由外壳使用同一份 [`sndvol::Volume`] 状态与几何绘制。
extern crate liteos_runtime as _;
use canvas::{Rect, Surface};
use classic_theme::{Edge, SystemColor as C, Theme};
use classic_ui::{
    Decoration, DecorationAction, Display, DropItem, DropMenu, EventKind, FrameKind, Ink, MenuBar,
    MenuEvent, Painter,
};
use sndvol::{Layout, Volume};
use std::{path::Path, process::ExitCode, time::Duration};
use typeface::Typeface;

const MENU_HEIGHT: i32 = 19;
const ESCAPE: u32 = 1;

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

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut display = Display::connect()?;
    let window = display.create_window("音量控制", "org.liteos.sndvol", (136, 330))?;
    display.fix_size(window)?;
    let mut decoration = Decoration::new(FrameKind::Fixed);
    decoration.load_icon("sound-volume");
    let theme = Theme::classic();
    let mut volume = Volume::open();
    let menus = menus();
    let mut menubar = MenuBar::new();
    let mut about = false;
    let mut font: Option<Typeface> = None;
    let mut scale = 0;
    let mut pointer: (f64, f64) = (0., 0.);
    let mut closing = false;
    let mut dirty = true;
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
        dirty |= volume.poll();
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
            let at = (pointer.0.floor() as i32, pointer.1.floor() as i32);
            let mut command = None;
            match event.kind {
                EventKind::Close => closing = true,
                EventKind::PointerEnter { x, y } => pointer = (x, y),
                EventKind::PointerMove { x, y } => {
                    pointer = (x, y);
                    let at = (x.floor() as i32, y.floor() as i32);
                    if !menubar.motion(&probe, bar, &menus, at) {
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
                    } else {
                        match menubar.press(&probe, bar, &menus, at) {
                            MenuEvent::Command(chosen) => command = Some(chosen),
                            MenuEvent::Handled => {}
                            MenuEvent::Ignored => {
                                volume.button(&layout, pointer.0, pointer.1, true)
                            }
                        }
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
                    } else {
                        match menubar.key(&probe, bar, &menus, &key) {
                            MenuEvent::Command(chosen) => command = Some(chosen),
                            MenuEvent::Handled => {}
                            MenuEvent::Ignored if key.code == ESCAPE => closing = true,
                            MenuEvent::Ignored => volume.key(key.code),
                        }
                    }
                }
                EventKind::Key(key) => {
                    menubar.key_released(&key);
                }
                _ => {}
            }
            match command {
                Some(Command::Exit) => closing = true,
                Some(Command::About) => about = true,
                None => {}
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
            volume.paint(&mut p, &layout);
            menubar.paint(
                &mut p,
                bar,
                &menus,
                (pointer.0.floor() as i32, pointer.1.floor() as i32),
            );
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
            dirty = false;
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
