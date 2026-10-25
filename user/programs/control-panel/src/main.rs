//! 控制面板小程序：`control-panel system`（系统属性）与 `control-panel datetime`（日期/时间属性）。
//! 使用普通 Wayland 连接，没有外壳权限；两个小程序都是只读信息页（系统没有可写的时钟接口与时区库）。
extern crate liteos_runtime as _;
mod datetime;
mod system;
mod winver;

use canvas::{Rect, Surface};
use classic_theme::{Edge, SystemColor as C, Theme};
use classic_ui::{
    Bitmap, ButtonState, Decoration, DecorationAction, Display, EventKind, FrameKind, Ink, Painter,
};
use std::{
    path::Path,
    process::ExitCode,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use typeface::Typeface;

const OK_KEY: u32 = 28;
const ESCAPE_KEY: u32 = 1;

/// 小程序页的内容：画在标签页内部，并可处理页内点击。
pub trait Page {
    fn title(&self) -> &'static str;
    fn tab(&self) -> &'static str;
    fn size(&self) -> (i32, i32);
    /// 画页内容；`page` 是标签页内部矩形，`now` 是当前 Unix 秒。
    fn paint(&self, p: &mut Painter<'_>, page: Rect, now: i64, icons: &Icons);
    /// 页内左键按下；返回是否需要重绘。
    fn click(&mut self, page: Rect, point: (i32, i32)) -> bool;
}

/// 页内用到的位图；缺失时按"无图标"绘制。
pub struct Icons {
    pub logo: Option<Bitmap>,
}

fn inside(rect: Rect, (x, y): (i32, i32)) -> bool {
    (rect.x..rect.right()).contains(&x) && (rect.y..rect.bottom()).contains(&y)
}

/// 标签页外框：一个标签头加其下的页面；返回页面内部矩形。
fn tab_page(p: &mut Painter<'_>, client: Rect, label: &str) -> Rect {
    let page = Rect::new(
        client.x + 6,
        client.y + 25,
        client.width - 12,
        client.height - 25 - 42,
    );
    let inner = p.edge(page, Edge::Raised);
    p.fill(inner, C::ButtonFace);
    let tab = Rect::new(client.x + 6, client.y + 4, 84, 22);
    let face = p.edge(tab, Edge::Raised);
    p.fill(face, C::ButtonFace);
    p.text_centered(tab, label, Ink::Color(C::ButtonText), false);
    // 标签头与页面之间不留分隔线。
    p.fill(
        Rect::new(tab.x + 2, page.y, tab.width - 4, 2),
        C::ButtonFace,
    );
    inner
}

fn buttons(client: Rect) -> [Rect; 3] {
    let y = client.bottom() - 32;
    [
        Rect::new(client.right() - 247, y, 75, 23),
        Rect::new(client.right() - 166, y, 75, 23),
        Rect::new(client.right() - 85, y, 75, 23),
    ]
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut page: Box<dyn Page> = match std::env::args().nth(1).as_deref() {
        Some("system") => Box::new(system::System::read()),
        Some("datetime") => Box::new(datetime::DateTime::now()),
        Some("winver") => Box::new(winver::Winver),
        other => {
            return Err(
                format!("unknown applet {other:?}; use `system`, `datetime` or `winver`").into(),
            );
        }
    };
    let mut display = Display::connect()?;
    let window = display.create_window(page.title(), "org.liteos.control-panel", page.size())?;
    let mut decoration = Decoration::new(FrameKind::Fixed);
    let theme = Theme::classic();
    let icons = Icons {
        logo: Bitmap::load(Path::new("/usr/share/liteos/ui/liteos-logo-32.bmp")).ok(),
    };
    let mut font: Option<Typeface> = None;
    let mut scale = 0;
    let mut pointer = (-1, -1);
    let mut pressed: Option<usize> = None;
    loop {
        let event = display.next_event_timeout(Duration::from_millis(500))?;
        let Some(c) = display.configuration(window) else {
            continue;
        };
        let client = decoration.client_rect(c);
        let page_rect = Rect::new(
            client.x + 6,
            client.y + 25,
            client.width - 12,
            client.height - 67,
        )
        .inset(2);
        let mut dirty = event.is_none();
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
            match event.kind {
                EventKind::Close => break,
                EventKind::Configure(c) if scale != c.scale => {
                    font = Some(Typeface::load(
                        Path::new("/usr/share/liteos/fonts"),
                        c.scale as u32,
                    )?);
                    scale = c.scale;
                }
                EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                    pointer = (x.floor() as i32, y.floor() as i32)
                }
                EventKind::PointerLeave => pointer = (-1, -1),
                EventKind::Button {
                    code: 0x110,
                    pressed: down,
                    ..
                } => {
                    if down {
                        pressed = buttons(client).iter().position(|r| inside(*r, pointer));
                        page.click(page_rect, pointer);
                    } else if let Some(index) = pressed.take()
                        && inside(buttons(client)[index], pointer)
                        && index < 2
                    {
                        break;
                    }
                }
                EventKind::Key(key) if key.pressed && matches!(key.code, OK_KEY | ESCAPE_KEY) => {
                    break;
                }
                _ => {}
            }
        }
        let Some(font) = font.as_ref() else { continue };
        if dirty && display.ready_to_draw(window) {
            let (w, h) = c.pixel_size()?;
            let mut pixels = vec![0; w as usize * h as usize];
            let surface =
                Surface::new(&mut pixels, w, h, w as usize).ok_or("invalid applet surface")?;
            let mut p = Painter::new(surface, &theme, font, c.scale);
            p.fill(Rect::new(0, 0, c.width, c.height), C::ButtonFace);
            decoration.paint(&mut p, c, page.title());
            let inner = tab_page(&mut p, client, page.tab());
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            page.paint(&mut p, inner, now, &icons);
            for (index, (rect, label)) in buttons(client)
                .iter()
                .zip(["确定", "取消", "应用(A)"])
                .enumerate()
            {
                p.push_button(
                    *rect,
                    label,
                    ButtonState {
                        pressed: pressed == Some(index) && inside(*rect, pointer),
                        default: index == 0,
                        disabled: index == 2,
                        ..Default::default()
                    },
                );
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
            eprintln!("control-panel: error: {error}");
            ExitCode::FAILURE
        }
    }
}
