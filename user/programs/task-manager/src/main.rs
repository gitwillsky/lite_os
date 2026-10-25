//! 任务管理器（`taskmgr`）：应用程序与进程两个标签页，独立于桌面外壳运行，与 Windows 2000 的 taskmgr.exe 一致。
//! 应用列表与控制走标准 foreign-toplevel 协议，进程列表读 procfs。
extern crate liteos_runtime as _;
mod processes;

use canvas::{Rect, Surface};
use classic_theme::{Edge, SystemColor as C, Theme};
use classic_ui::{
    Bitmap, ButtonState, Decoration, DecorationAction, Display, EventKind, FrameKind, Ink, Painter,
    ToplevelId, ToplevelRequest,
};
use std::{
    collections::HashMap,
    path::Path,
    process::ExitCode,
    time::{Duration, Instant},
};
use typeface::Typeface;

const TABS: [&str; 2] = ["应用程序", "进程"];
const ROW: i32 = 17;
/// 进程页各列：映像名称、PID、内存使用。
const PROCESS_COLUMNS: [i32; 3] = [170, 60, 100];
const OWN_APP_ID: &str = "org.liteos.task-manager";

fn inside(rect: Rect, (x, y): (i32, i32)) -> bool {
    (rect.x..rect.right()).contains(&x) && (rect.y..rect.bottom()).contains(&y)
}

fn tab_rects(client: Rect) -> [Rect; 2] {
    [
        Rect::new(client.x + 8, client.y + 6, 80, 22),
        Rect::new(client.x + 88, client.y + 6, 60, 22),
    ]
}

fn page_rect(client: Rect) -> Rect {
    Rect::new(
        client.x + 8,
        client.y + 27,
        client.width - 16,
        (client.height - 27 - 70).max(0),
    )
}

/// 底部按钮：应用页是"结束任务""切换至"，进程页是"结束进程"。
fn buttons(client: Rect, tab: usize) -> Vec<(Rect, &'static str)> {
    let y = client.bottom() - 32;
    if tab == 1 {
        vec![(Rect::new(client.right() - 98, y, 90, 23), "结束进程(E)")]
    } else {
        vec![
            (Rect::new(client.right() - 270, y, 85, 23), "结束任务(E)"),
            (Rect::new(client.right() - 180, y, 85, 23), "切换至(S)"),
        ]
    }
}

fn icon_for(app_id: &str) -> &'static str {
    match app_id {
        "org.liteos.file-explorer" => "folder-closed-16",
        "org.liteos.control-panel" => "control-panel-16",
        _ => "file-app-16",
    }
}

struct App {
    tab: usize,
    selected: [usize; 2],
    processes: Vec<processes::Process>,
    next_refresh: Instant,
    pointer: (i32, i32),
    pressed: Option<usize>,
}

impl App {
    /// 应用页的任务：除任务管理器自己以外的全部顶层窗口。
    fn tasks(display: &Display) -> Vec<(ToplevelId, String, bool, &'static str)> {
        display
            .toplevels()
            .filter(|(_, info)| info.app_id != OWN_APP_ID)
            .map(|(id, info)| {
                (
                    id,
                    info.title.clone(),
                    info.minimized,
                    icon_for(&info.app_id),
                )
            })
            .collect()
    }

    fn refresh_processes(&mut self) -> bool {
        let now = Instant::now();
        if now < self.next_refresh {
            return false;
        }
        self.next_refresh = now + Duration::from_secs(1);
        self.processes = processes::list();
        self.selected[1] = self.selected[1].min(self.processes.len().saturating_sub(1));
        true
    }

    fn row_count(&self, display: &Display) -> usize {
        if self.tab == 1 {
            self.processes.len()
        } else {
            Self::tasks(display).len()
        }
    }

    fn activate(&mut self, display: &mut Display, button: usize) {
        if self.tab == 1 {
            if let Some(process) = self.processes.get(self.selected[1]) {
                processes::terminate(process.pid);
                self.next_refresh = Instant::now();
            }
            return;
        }
        let tasks = Self::tasks(display);
        let Some((id, ..)) = tasks.get(self.selected[0]) else {
            return;
        };
        let request = if button == 0 {
            ToplevelRequest::Close
        } else {
            ToplevelRequest::Activate
        };
        let _ = display.control_toplevel(*id, request);
    }

    fn press(&mut self, display: &mut Display, client: Rect) {
        let point = self.pointer;
        if let Some(tab) = tab_rects(client).iter().position(|r| inside(*r, point)) {
            self.tab = tab;
            if tab == 1 {
                self.next_refresh = Instant::now();
                self.refresh_processes();
            }
            return;
        }
        self.pressed = buttons(client, self.tab)
            .iter()
            .position(|(r, _)| inside(*r, point));
        let page = page_rect(client);
        if inside(page, point) {
            let row = ((point.1 - page.y - 8 - ROW) / ROW).max(0) as usize;
            let count = self.row_count(display);
            self.selected[self.tab] = row.min(count.saturating_sub(1));
        }
    }

    fn key(&mut self, display: &mut Display, code: u32) -> bool {
        let count = self.row_count(display);
        let selected = &mut self.selected[self.tab];
        match code {
            1 => return false,
            103 => *selected = selected.saturating_sub(1),
            108 => *selected = (*selected + 1).min(count.saturating_sub(1)),
            111 => self.activate(display, 0),
            28 => self.activate(display, 1),
            _ => {}
        }
        true
    }

    fn paint(
        &self,
        p: &mut Painter<'_>,
        display: &Display,
        client: Rect,
        icons: &HashMap<&'static str, Bitmap>,
    ) {
        let page = page_rect(client);
        let face = p.edge(page, Edge::Raised);
        p.fill(face, C::ButtonFace);
        for (index, (rect, title)) in tab_rects(client).into_iter().zip(TABS).enumerate() {
            let active = self.tab == index;
            let rect = if active {
                rect
            } else {
                Rect::new(rect.x, rect.y + 2, rect.width, rect.height - 2)
            };
            let inner = p.edge(rect, Edge::Raised);
            p.fill(inner, C::ButtonFace);
            p.text_centered(rect, title, Ink::Color(C::ButtonText), false);
            if active {
                p.fill(
                    Rect::new(rect.x + 2, page.y, rect.width - 4, 2),
                    C::ButtonFace,
                );
            }
        }
        let list = Rect::new(page.x + 6, page.y + 6, page.width - 12, page.height - 12);
        let inner = p.edge(list, Edge::Sunken);
        p.fill(inner, C::Window);
        let columns: Vec<(&str, i32)> = if self.tab == 1 {
            vec![
                ("映像名称", PROCESS_COLUMNS[0]),
                ("PID", PROCESS_COLUMNS[1]),
                ("内存使用", PROCESS_COLUMNS[2]),
            ]
        } else {
            vec![("任务", inner.width - 90), ("状态", 90)]
        };
        let mut x = inner.x;
        for (title, width) in &columns {
            let cell = Rect::new(x, inner.y, *width, ROW);
            let face = p.edge(cell, Edge::Raised);
            p.fill(face, C::ButtonFace);
            let y = cell.y + (ROW - p.line_height()) / 2;
            p.text(cell.x + 5, y, title, Ink::Color(C::ButtonText), false);
            x += width;
        }
        let rows = Rect::new(inner.x, inner.y + ROW, inner.width, inner.height - ROW);
        let capacity = (rows.height / ROW).max(0) as usize;
        let count = self.row_count(display);
        p.clipped(rows, |p| {
            if self.tab == 1 {
                self.paint_processes(p, rows, capacity);
            } else {
                for (i, (_, title, minimized, icon)) in
                    Self::tasks(display).iter().take(capacity).enumerate()
                {
                    let y = rows.y + i as i32 * ROW;
                    let selected = self.selected[0] == i;
                    if selected {
                        p.fill(Rect::new(rows.x + 20, y, rows.width - 20, ROW), C::Hilight);
                    }
                    if let Some(bitmap) = icons.get(icon) {
                        bitmap.paint(p, rows.x + 3, y + 1);
                    }
                    let ink = Ink::Color(if selected {
                        C::HilightText
                    } else {
                        C::WindowText
                    });
                    let text_y = y + (ROW - p.line_height()) / 2;
                    p.text(
                        rows.x + 24,
                        text_y,
                        &fit(p, title, rows.width - 116),
                        ink,
                        false,
                    );
                    let state = if *minimized {
                        "已最小化"
                    } else {
                        "正在运行"
                    };
                    p.text(rows.x + rows.width - 85, text_y, state, ink, false);
                }
            }
        });
        let label = if self.tab == 1 {
            "进程数"
        } else {
            "任务数"
        };
        p.text(
            client.x + 12,
            page.bottom() + 8,
            &format!("{label}: {count}"),
            Ink::Color(C::ButtonText),
            false,
        );
        for (index, (rect, label)) in buttons(client, self.tab).into_iter().enumerate() {
            p.push_button(
                rect,
                label,
                ButtonState {
                    pressed: self.pressed == Some(index) && inside(rect, self.pointer),
                    ..Default::default()
                },
            );
        }
    }

    fn paint_processes(&self, p: &mut Painter<'_>, rows: Rect, capacity: usize) {
        let right = |p: &Painter<'_>, text: &str, edge: i32| {
            edge - 6 - (p.text_width(text, false) + p.scale() - 1) / p.scale()
        };
        for (i, process) in self.processes.iter().take(capacity).enumerate() {
            let y = rows.y + i as i32 * ROW;
            let selected = self.selected[1] == i;
            if selected {
                p.fill(Rect::new(rows.x, y, rows.width, ROW), C::Hilight);
            }
            let ink = Ink::Color(if selected {
                C::HilightText
            } else {
                C::WindowText
            });
            let text_y = y + (ROW - p.line_height()) / 2;
            p.text(rows.x + 5, text_y, &process.name, ink, false);
            let pid = process.pid.to_string();
            let pid_edge = rows.x + PROCESS_COLUMNS[0] + PROCESS_COLUMNS[1];
            p.text(right(p, &pid, pid_edge), text_y, &pid, ink, false);
            let memory = processes::memory_text(process.memory_kib);
            let memory_edge = pid_edge + PROCESS_COLUMNS[2];
            p.text(right(p, &memory, memory_edge), text_y, &memory, ink, false);
        }
    }
}

fn fit(p: &Painter<'_>, text: &str, width: i32) -> String {
    let wide = |s: &str| (p.text_width(s, false) + p.scale() - 1) / p.scale();
    if wide(text) <= width {
        return text.to_owned();
    }
    let mut shortened = text.to_owned();
    while !shortened.is_empty() && wide(&format!("{shortened}…")) > width {
        shortened.pop();
    }
    format!("{shortened}…")
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut display = Display::connect()?;
    let window = display.create_window("任务管理器", OWN_APP_ID, (460, 400))?;
    let mut decoration = Decoration::new(FrameKind::Resizable);
    let theme = Theme::classic();
    let icons: HashMap<&'static str, Bitmap> =
        ["folder-closed-16", "control-panel-16", "file-app-16"]
            .into_iter()
            .filter_map(|name| {
                let path = Path::new("/usr/share/liteos/ui").join(format!("{name}.bmp"));
                Some((name, Bitmap::load(&path).ok()?))
            })
            .collect();
    let mut app = App {
        tab: 0,
        selected: [0, 0],
        processes: Vec::new(),
        next_refresh: Instant::now(),
        pointer: (-1, -1),
        pressed: None,
    };
    let mut font: Option<Typeface> = None;
    let mut scale = 0;
    let mut revision = 0;
    loop {
        let event = display.next_event_timeout(Duration::from_millis(250))?;
        let Some(c) = display.configuration(window) else {
            continue;
        };
        let client = decoration.client_rect(c);
        let mut dirty = app.tab == 1 && app.refresh_processes();
        if display.toplevel_revision() != revision {
            revision = display.toplevel_revision();
            dirty = true;
        }
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
                    app.pointer = (x.floor() as i32, y.floor() as i32)
                }
                EventKind::PointerLeave => app.pointer = (-1, -1),
                EventKind::Button {
                    code: 0x110,
                    pressed: true,
                    ..
                } => app.press(&mut display, client),
                EventKind::Button {
                    code: 0x110,
                    pressed: false,
                    ..
                } => {
                    if let Some(index) = app.pressed.take()
                        && buttons(client, app.tab)
                            .get(index)
                            .is_some_and(|(r, _)| inside(*r, app.pointer))
                    {
                        app.activate(&mut display, index);
                    }
                }
                EventKind::Key(key) if key.pressed => {
                    if !app.key(&mut display, key.code) {
                        break;
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
            decoration.paint(&mut p, c, "任务管理器");
            app.paint(&mut p, &display, client, &icons);
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
            eprintln!("taskmgr: error: {error}");
            ExitCode::FAILURE
        }
    }
}
