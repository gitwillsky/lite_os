//! 控制面板小程序：`control-panel system`（系统属性）与 `control-panel datetime`（日期/时间属性）。
//! 使用普通 Wayland 连接，没有外壳权限；各小程序共享属性页布局、输入路由与内容区裁剪。
extern crate liteos_runtime as _;
mod datetime;
mod display;
mod fileprops;
mod network;
mod sounds;
mod system;
mod taskbar;
mod winver;

use canvas::{Rect, Surface};
use classic_theme::{Edge, SystemColor as C, Theme};
use classic_ui::{
    Bitmap, ButtonState, Decoration, DecorationAction, Display, EventKind, FrameKind, Painter,
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
    /// 随对象变化的标题（如"foo.txt 属性"）；为 `None` 时用 [`Self::title`]。
    fn heading_title(&self) -> Option<String> {
        None
    }
    /// 标签页标题，从左到右；多于一个时窗口显示标签条。
    fn tabs(&self) -> &'static [&'static str];
    /// 标题栏图标的素材名（不含 `-16.bmp`）。
    fn icon(&self) -> &'static str;
    fn size(&self) -> (i32, i32);
    /// 画第 `tab` 页的内容；`page` 是标签页内部矩形，`now` 是当前 Unix 秒。
    fn paint(&self, p: &mut Painter<'_>, page: Rect, tab: usize, now: i64, icons: &Icons);
    /// 第 `tab` 页内左键按下；返回是否需要重绘。
    fn click(&mut self, page: Rect, tab: usize, point: (i32, i32)) -> bool;
    /// 滚轮：向下滚 `notches` 档（负数向上）；`point` 是指针位置，页内有多个可滚动区域时据此选择。
    fn wheel(&mut self, _page: Rect, _tab: usize, _point: (i32, i32), _notches: i32) {}
    /// 指针移动（拖动滚动条滑块等）。
    fn motion(&mut self, _page: Rect, _tab: usize, _point: (i32, i32)) {}
    /// 左键释放（结束滑块拖动）。
    fn release(&mut self) {}
    /// 有尚未应用的修改；"应用"按钮只在这时可用。
    fn dirty(&self) -> bool {
        false
    }
    /// 应用修改（"确定"与"应用"）。
    fn apply(&mut self) {}
}

/// 页内用到的位图；缺失时按"无图标"绘制。
pub struct Icons {
    pub logo: Option<Bitmap>,
    pub computer: Option<Bitmap>,
}

fn inside(rect: Rect, (x, y): (i32, i32)) -> bool {
    (rect.x..rect.right()).contains(&x) && (rect.y..rect.bottom()).contains(&y)
}

/// 标签头宽度；各页的标题都较短，统一宽度与 Windows 2000 的属性页一致。
const TAB_WIDTH: i32 = 70;

/// 属性页内容矩形是绘制与输入命中的唯一几何来源。
fn page_rect(client: Rect) -> Rect {
    Rect::new(
        client.x + 6,
        client.y + 25,
        client.width - 12,
        client.height - 67,
    )
    .inset(2)
}

/// 各标签头的矩形（未选中形态）。
fn tab_rects(client: Rect, count: usize) -> Vec<Rect> {
    (0..count as i32)
        .map(|index| {
            Rect::new(
                client.x + 6 + index * TAB_WIDTH,
                client.y + 4,
                TAB_WIDTH,
                22,
            )
        })
        .collect()
}

/// 标签页外框：标签条加其下的页面；返回页面内部矩形。
///
/// 选中的标签头比页面高出 2 像素并向两侧多出 2 像素（第一个不越过页面左边框），未选中的矮 2 像素。
fn tab_page(p: &mut Painter<'_>, client: Rect, tabs: &[&str], selected: usize) -> Rect {
    let page = page_rect(client).inset(-2);
    let inner = p.edge(page, Edge::Raised);
    p.fill(inner, C::ButtonFace);
    let rects = tab_rects(client, tabs.len());
    for (index, (rect, label)) in rects.iter().zip(tabs).enumerate() {
        if index != selected {
            p.tab(
                Rect::new(rect.x, rect.y + 2, rect.width, page.y - rect.y - 2),
                label,
                false,
            );
        }
    }
    let chosen = rects[selected];
    let left = if selected == 0 {
        chosen.x
    } else {
        chosen.x - 2
    };
    p.tab(
        Rect::new(
            left,
            chosen.y - 2,
            chosen.right() + 2 - left,
            page.y + 2 - (chosen.y - 2),
        ),
        tabs[selected],
        true,
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
        Some("sounds") => Box::new(sounds::Sounds::new()),
        Some("taskbar") => Box::new(taskbar::Taskbar::new()),
        Some("network") => Box::new(network::Network::new()),
        Some("properties") => Box::new(fileprops::FileProperties::from_args(
            std::env::args().skip(2),
        )?),
        Some("display") => Box::new(display::Background::new()),
        Some("winver") => Box::new(winver::Winver),
        other => {
            return Err(format!(
                "unknown applet {other:?}; use `system`, `datetime`, `sounds`, `taskbar`, `display`, `network` or `winver`"
            )
            .into());
        }
    };
    let mut display = Display::connect()?;
    // app_id 带上小程序名：任务栏与 Alt+Tab 据此给每个小程序自己的图标。
    let app_id = format!(
        "org.liteos.control-panel.{}",
        std::env::args().nth(1).unwrap_or_default()
    );
    let title = page
        .heading_title()
        .unwrap_or_else(|| page.title().to_owned());
    let window = display.create_window(&title, &app_id, page.size())?;
    display.fix_size(window)?;
    let mut decoration = Decoration::new(FrameKind::Fixed);
    decoration.load_icon(page.icon());
    let theme = Theme::classic();
    let icons = Icons {
        logo: Bitmap::load(Path::new("/usr/share/liteos/ui/liteos-logo-32.bmp")).ok(),
        computer: Bitmap::load(Path::new("/usr/share/liteos/ui/my-computer-32.bmp")).ok(),
    };
    let mut font: Option<Typeface> = None;
    let mut scale = 0;
    let mut pointer = (-1, -1);
    let mut pressed: Option<usize> = None;
    let mut tab = 0;
    loop {
        let event = display.next_event_timeout(Duration::from_millis(500))?;
        let Some(c) = display.configuration(window) else {
            continue;
        };
        let client = decoration.client_rect(c);
        let page_rect = page_rect(client);
        let mut dirty = event.is_none();
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
                    pointer = (x.floor() as i32, y.floor() as i32);
                    page.motion(page_rect, tab, pointer);
                }
                EventKind::Wheel { vertical, .. } => {
                    page.wheel(page_rect, tab, pointer, vertical);
                }
                EventKind::PointerLeave => pointer = (-1, -1),
                EventKind::Button {
                    code: 0x110,
                    pressed: down,
                    ..
                } => {
                    if !down {
                        page.release();
                    }
                    if down {
                        pressed = buttons(client).iter().position(|r| inside(*r, pointer));
                        if let Some(index) = tab_rects(client, page.tabs().len())
                            .iter()
                            .position(|r| inside(*r, pointer))
                        {
                            tab = index;
                        } else {
                            page.click(page_rect, tab, pointer);
                        }
                    } else if let Some(index) = pressed.take()
                        && inside(buttons(client)[index], pointer)
                    {
                        match index {
                            // 确定：应用后关闭；取消：直接关闭；应用：留在窗口里。
                            0 => {
                                page.apply();
                                break;
                            }
                            1 => break,
                            _ if page.dirty() => page.apply(),
                            _ => {}
                        }
                    }
                }
                EventKind::Key(key) if key.pressed && matches!(key.code, OK_KEY | ESCAPE_KEY) => {
                    if key.code == OK_KEY {
                        page.apply();
                    }
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
            decoration.paint(&mut p, c, &title);
            let inner = tab_page(&mut p, client, page.tabs(), tab);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            p.clipped(inner, |p| page.paint(p, inner, tab, now, &icons));
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
                        disabled: index == 2 && !page.dirty(),
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
