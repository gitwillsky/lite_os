//! classic-ui 的真实客户端消费探针，只注入 Wayland gate，不进入产品 rootfs。
//! 窗口装饰、两个编辑框、Tab/Shift+Tab 和默认按钮全部通过库的公开接口使用。

use canvas::{Rect, Surface};
use classic_theme::{SystemColor, Theme};
use classic_ui::{
    ButtonState, Configuration, Decoration, DecorationAction, Display, Edit, EventKind, FocusChain,
    FrameKind, Ink, Painter,
};
use std::{io, path::Path, process::ExitCode};
use typeface::Typeface;

const TITLE: &str = "经典控件 - Classic UI";
const EDITS: [Rect; 2] = [Rect::new(20, 60, 220, 23), Rect::new(20, 105, 220, 23)];
const OK: Rect = Rect::new(250, 180, 75, 23);

fn paint(
    configuration: Configuration,
    decoration: &Decoration,
    edits: &[Edit; 2],
    focus: &FocusChain,
    theme: &Theme,
    font: &Typeface,
    keyboard_focus: bool,
) -> io::Result<Vec<u32>> {
    let (width, height) = configuration.pixel_size()?;
    let mut pixels = vec![0; width as usize * height as usize];
    let surface = Surface::new(&mut pixels, width, height, width as usize)
        .ok_or_else(|| io::Error::other("invalid paint size"))?;
    let mut painter = Painter::new(surface, theme, font, configuration.scale);
    painter.fill(
        Rect::new(0, 0, configuration.width, configuration.height),
        SystemColor::ButtonFace,
    );
    decoration.paint(&mut painter, configuration, TITLE);
    painter.text(
        20,
        40,
        "用户名(U):",
        Ink::Color(SystemColor::ButtonText),
        false,
    );
    painter.text(
        20,
        85,
        "密码(P):",
        Ink::Color(SystemColor::ButtonText),
        false,
    );
    for (index, rect) in EDITS.iter().enumerate() {
        painter.edit(
            *rect,
            &edits[index],
            keyboard_focus && focus.current() == Some(index),
            index == 1,
        );
    }
    painter.push_button(
        OK,
        "确定",
        ButtonState {
            default: true,
            focused: keyboard_focus && focus.current() == Some(2),
            ..ButtonState::default()
        },
    );
    Ok(pixels)
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut display = Display::connect()?;
    let window = display.create_window(TITLE, "classic-ui-probe", (360, 240))?;
    let theme = Theme::classic();
    let mut font = None;
    let mut font_scale = 0;
    let mut decoration = Decoration::new(FrameKind::Resizable);
    let mut edits = [Edit::default(), Edit::default()];
    let mut focus = FocusChain::new(&[true, true, true]);
    let mut pointer = (0.0, 0.0);
    let mut dirty = false;
    let mut frames = 0;
    loop {
        let event = display.next_event()?;
        if event.window != window {
            continue;
        }
        let configuration = display
            .configuration(window)
            .ok_or("event before configure")?;
        match decoration.event(&event.kind, configuration) {
            Some(DecorationAction::Request(request)) => {
                display.request(window, request)?;
                println!("ui-probe: decoration {request:?}");
            }
            Some(DecorationAction::Close) => {
                display.destroy_window(window)?;
                println!("ui-probe: closed");
                return Ok(());
            }
            Some(DecorationAction::Cursor(shape)) => display.set_cursor_shape(shape)?,
            None => {}
        }
        match event.kind {
            EventKind::Configure(configuration) => {
                if font_scale != configuration.scale {
                    font = Some(Typeface::load(
                        Path::new("/var/tmp/ui-fonts"),
                        configuration.scale as u32,
                    )?);
                    font_scale = configuration.scale;
                }
                dirty = true;
                println!(
                    "ui-probe: configure {}x{} scale={}",
                    configuration.width, configuration.height, configuration.scale
                );
            }
            EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                pointer = (x, y);
                dirty = true;
            }
            EventKind::PointerLeave | EventKind::Focus(_) => dirty = true,
            EventKind::Button {
                code: 0x110,
                pressed,
                ..
            } => {
                println!(
                    "ui-probe: button pressed={pressed} at={},{}",
                    pointer.0, pointer.1
                );
                if pressed {
                    for (index, rect) in EDITS.iter().enumerate() {
                        if pointer.0 >= f64::from(rect.x)
                            && pointer.0 < f64::from(rect.right())
                            && pointer.1 >= f64::from(rect.y)
                            && pointer.1 < f64::from(rect.bottom())
                        {
                            focus.focus(index);
                            println!("ui-probe: focus {index}");
                        }
                    }
                }
                dirty = true;
            }
            EventKind::Key(key) if key.pressed => {
                if key.code == 15 {
                    focus.advance(key.modifiers.shift);
                    println!("ui-probe: focus {}", focus.current().unwrap());
                    dirty = true;
                } else if key.code == 28 {
                    println!("ui-probe: accepted");
                } else if let Some(index @ 0..=1) = focus.current() {
                    dirty |= edits[index].key(key);
                    // 只打印普通编辑框；密码仅报告掩码，探针也不输出原文。
                    println!(
                        "ui-probe: edit {index} {}{}",
                        if index == 1 {
                            edits[index].masked()
                        } else {
                            edits[index].text().into()
                        },
                        if key.repeat { " repeat" } else { "" }
                    );
                }
            }
            EventKind::FrameReady { .. } => {
                frames += 1;
                println!("ui-probe: frame {frames}");
            }
            EventKind::Close => {
                display.destroy_window(window)?;
                println!("ui-probe: closed");
                return Ok(());
            }
            _ => {}
        }
        if dirty && display.ready_to_draw(window) {
            let pixels = paint(
                configuration,
                &decoration,
                &edits,
                &focus,
                &theme,
                font.as_ref().ok_or("missing font")?,
                display.keyboard_focus() == Some(window),
            )?;
            match display.present(window, &pixels) {
                Ok(()) => dirty = false,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ui-probe: error: {error}");
            ExitCode::FAILURE
        }
    }
}
