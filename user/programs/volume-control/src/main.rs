//! 音量控制（`sndvol`）：Windows 2000 的 sndvol32.exe 对应物，独立窗口调整 Master 音量与静音。
//! 托盘的小面板由外壳使用同一份 [`sndvol::Volume`] 状态与布局绘制。
extern crate liteos_runtime as _;
use canvas::{Rect, Surface};
use classic_theme::{SystemColor as C, Theme};
use classic_ui::{
    ButtonState, Decoration, DecorationAction, Display, EventKind, FrameKind, Painter,
};
use sndvol::{Layout, Volume};
use std::{path::Path, process::ExitCode, time::Duration};
use typeface::Typeface;

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut display = Display::connect()?;
    let window = display.create_window("音量控制", "org.liteos.sndvol", (260, 320))?;
    let mut decoration = Decoration::new(FrameKind::Fixed);
    let theme = Theme::classic();
    let mut volume = Volume::open();
    let mut font: Option<Typeface> = None;
    let mut scale = 0;
    let mut pointer = (0., 0.);
    let mut armed = false;
    loop {
        let event = display.next_event_timeout(Duration::from_millis(100))?;
        let Some(c) = display.configuration(window) else {
            continue;
        };
        let mut dirty = volume.poll();
        if let Some(event) = event {
            if event.window != window {
                continue;
            }
            match decoration.event(&event.kind, c) {
                Some(DecorationAction::Request(request)) => display.request(window, request)?,
                Some(DecorationAction::Close) => break,
                None => {}
            }
            dirty |= !matches!(event.kind, EventKind::FramePresented { .. });
            let layout = Layout::dialog(c);
            match event.kind {
                EventKind::Close => break,
                EventKind::Configure(c) if scale != c.scale => {
                    font = Some(Typeface::load(
                        Path::new("/usr/share/liteos/fonts"),
                        c.scale as u32,
                    )?);
                    scale = c.scale;
                }
                EventKind::PointerEnter { x, y } => pointer = (x, y),
                EventKind::PointerMove { x, y } => {
                    pointer = (x, y);
                    volume.motion(&layout, y);
                }
                EventKind::PointerLeave | EventKind::Focus(false) => volume.cancel(),
                EventKind::Button {
                    code: 0x110,
                    pressed,
                    ..
                } => {
                    volume.button(&layout, pointer.0, pointer.1, pressed);
                    let ok = ok_button(c);
                    let inside = (f64::from(ok.x)..f64::from(ok.right())).contains(&pointer.0)
                        && (f64::from(ok.y)..f64::from(ok.bottom())).contains(&pointer.1);
                    if pressed {
                        armed = inside;
                    } else if std::mem::take(&mut armed) && inside {
                        break;
                    }
                }
                EventKind::Key(key) if key.pressed => {
                    if matches!(key.code, 1 | 28) {
                        break;
                    }
                    volume.key(key.code);
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
            volume.paint(&mut p, &Layout::dialog(c));
            p.push_button(
                ok_button(c),
                "确定",
                ButtonState {
                    default: true,
                    ..Default::default()
                },
            );
            display.present(window, &pixels)?;
        }
    }
    display.destroy_window(window)?;
    Ok(())
}

fn ok_button(c: classic_ui::Configuration) -> Rect {
    Rect::new(c.width - 115, c.height - 35, 100, 23)
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
