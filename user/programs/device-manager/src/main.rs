//! 设备管理器（`devmgmt`）：按类别列出系统里的硬件并查看属性，对应 Windows 2000 的 devmgmt.msc。
extern crate liteos_runtime as _;
mod devices;

use canvas::{Rect, Surface};
use classic_theme::{Edge, SystemColor as C, Theme};
use classic_ui::{
    Bitmap, ButtonState, CursorShape, Decoration, DecorationAction, Display, DropItem, DropMenu,
    EventKind, FrameKind, Ink, Key, MenuBar, MenuEvent, Painter, SCROLLBAR_WIDTH, Scroll,
};
use devices::{Category, Device};
use std::{
    path::Path,
    process::ExitCode,
    time::{Duration, Instant},
};
use typeface::Typeface;

const APP_ID: &str = "org.liteos.device-manager";
const MENU_HEIGHT: i32 = 19;
const ROW: i32 = 18;
const INDENT: i32 = 18;
const DOUBLE_CLICK: Duration = Duration::from_millis(500);
const ENTER: u32 = 28;
const ESCAPE: u32 = 1;
const UP: u32 = 103;
const DOWN: u32 = 108;
const LEFT: u32 = 105;
const RIGHT: u32 = 106;
const F5: u32 = 63;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Properties,
    Refresh,
    Exit,
    About,
}

fn menus() -> Vec<DropMenu<Command>> {
    vec![
        DropMenu {
            title: "操作(A)",
            items: vec![
                DropItem::new("属性(R)", "", Command::Properties),
                DropItem::new("刷新(F)", "F5", Command::Refresh).separated(),
                DropItem::new("退出(X)", "", Command::Exit).separated(),
            ],
        },
        DropMenu {
            title: "查看(V)",
            items: vec![DropItem::new("刷新(R)", "F5", Command::Refresh)],
        },
        DropMenu {
            title: "帮助(H)",
            items: vec![DropItem::new("关于设备管理器(A)", "", Command::About)],
        },
    ]
}

/// 树里的一行。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    Computer,
    Category(usize),
    Device(usize, usize),
}

enum Modal {
    Properties(usize, usize),
    About,
}

struct App {
    computer: String,
    categories: Vec<Category>,
    /// 与 `categories` 对应：该类别是否展开。刷新后保持用户已展开/折叠的状态。
    expanded: Vec<bool>,
    selected: usize,
    scroll: Scroll,
    menubar: MenuBar,
    modal: Option<Modal>,
    last_click: Option<(usize, Instant)>,
    pointer: (i32, i32),
    closing: bool,
}

/// 树里用的 16×16 图标；素材缺失时不画。
struct Icons {
    computer: Option<Bitmap>,
    category: Option<Bitmap>,
    device: Option<Bitmap>,
}

impl Icons {
    fn load() -> Self {
        let load = |name: &str| {
            Bitmap::load(&Path::new("/usr/share/liteos/ui").join(format!("{name}-16.bmp"))).ok()
        };
        Self {
            computer: load("my-computer"),
            category: load("device-manager"),
            device: load("device-manager"),
        }
    }
}

fn inside(rect: Rect, (x, y): (i32, i32)) -> bool {
    (rect.x..rect.right()).contains(&x) && (rect.y..rect.bottom()).contains(&y)
}

impl App {
    fn new() -> Self {
        let computer = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|name| name.trim().to_uppercase())
            .unwrap_or_else(|_| "LITEOS".into());
        let categories = devices::scan();
        Self {
            computer,
            expanded: vec![true; categories.len()],
            categories,
            selected: 0,
            scroll: Scroll::default(),
            menubar: MenuBar::new(),
            modal: None,
            last_click: None,
            pointer: (-1, -1),
            closing: false,
        }
    }

    fn refresh(&mut self) {
        let names: Vec<(&str, bool)> = self
            .categories
            .iter()
            .zip(&self.expanded)
            .map(|(category, open)| (category.name, *open))
            .collect();
        self.categories = devices::scan();
        self.expanded = self
            .categories
            .iter()
            .map(|category| {
                names
                    .iter()
                    .find(|(name, _)| *name == category.name)
                    .is_none_or(|(_, open)| *open)
            })
            .collect();
        self.selected = self.selected.min(self.rows().len() - 1);
    }

    /// 当前可见的行：电脑名、各类别，以及展开类别下的设备。
    fn rows(&self) -> Vec<Row> {
        let mut rows = vec![Row::Computer];
        for (index, category) in self.categories.iter().enumerate() {
            rows.push(Row::Category(index));
            if self.expanded[index] {
                rows.extend((0..category.devices.len()).map(|device| Row::Device(index, device)));
            }
        }
        rows
    }

    fn device(&self, category: usize, device: usize) -> &Device {
        &self.categories[category].devices[device]
    }

    fn tree_area(client: Rect) -> Rect {
        Rect::new(
            client.x,
            client.y + MENU_HEIGHT,
            client.width,
            client.height - MENU_HEIGHT,
        )
        .inset(2)
    }

    /// 树视图的行区与（内容超出时的）滚动条。
    fn view(&self, client: Rect) -> (Rect, Option<Rect>) {
        let area = Self::tree_area(client).inset(1);
        if self.rows().len() as i32 * ROW > area.height {
            (
                Rect::new(area.x, area.y, area.width - SCROLLBAR_WIDTH, area.height),
                Some(Rect::new(
                    area.right() - SCROLLBAR_WIDTH,
                    area.y,
                    SCROLLBAR_WIDTH,
                    area.height,
                )),
            )
        } else {
            (area, None)
        }
    }

    fn run(&mut self, command: Command) {
        match command {
            Command::Properties => {
                if let Some(&Row::Device(category, device)) = self.rows().get(self.selected) {
                    self.modal = Some(Modal::Properties(category, device));
                }
            }
            Command::Refresh => self.refresh(),
            Command::Exit => self.closing = true,
            Command::About => self.modal = Some(Modal::About),
        }
    }

    /// 切换选中行所在类别的展开状态（对类别行）；返回是否切换了。
    fn toggle(&mut self, row: Row) -> bool {
        if let Row::Category(index) = row {
            self.expanded[index] ^= true;
            return true;
        }
        false
    }

    fn press(&mut self, p: &Painter<'_>, client: Rect) {
        let point = self.pointer;
        if self.modal.is_some() {
            if inside(modal_rects(client).1, point) {
                self.modal = None;
            }
            return;
        }
        let menus = menus();
        let bar = Rect::new(client.x, client.y, client.width, MENU_HEIGHT);
        match self.menubar.press(p, bar, &menus, point) {
            MenuEvent::Command(command) => return self.run(command),
            MenuEvent::Handled => return,
            MenuEvent::Ignored => {}
        }
        let (rows_rect, scrollbar) = self.view(client);
        if let Some(scrollbar) = scrollbar.filter(|bar| inside(*bar, point)) {
            let content = (self.rows().len() as i32 * ROW, rows_rect.height, ROW);
            self.scroll.press(scrollbar, true, content, point);
            return;
        }
        if !inside(rows_rect, point) {
            return;
        }
        let index = ((point.1 - rows_rect.y + self.scroll.position) / ROW).max(0) as usize;
        let rows = self.rows();
        let Some(&row) = rows.get(index) else { return };
        self.selected = index;
        // 类别行的 +/- 方框：单击即切换。
        if let Row::Category(_) = row
            && point.0 < rows_rect.x + INDENT + 12
            && self.toggle(row)
        {
            self.last_click = None;
            return;
        }
        let double = self
            .last_click
            .is_some_and(|(last, at)| last == index && at.elapsed() < DOUBLE_CLICK);
        self.last_click = (!double).then(|| (index, Instant::now()));
        if double {
            match row {
                Row::Device(..) => self.run(Command::Properties),
                _ => {
                    self.toggle(row);
                }
            }
        }
    }

    /// 滚轮：向下滚 `notches` 档滚动设备树。
    fn wheel(&mut self, client: Rect, notches: i32) {
        if self.modal.is_some() {
            return;
        }
        let (rows_rect, _) = self.view(client);
        let content = self.rows().len() as i32 * ROW;
        self.scroll.wheel(notches, ROW, content - rows_rect.height);
    }

    fn pointer_moved(&mut self, p: &Painter<'_>, client: Rect) {
        let menus = menus();
        let bar = Rect::new(client.x, client.y, client.width, MENU_HEIGHT);
        if self.menubar.motion(p, bar, &menus, self.pointer) {
            return;
        }
        let (rows_rect, scrollbar) = self.view(client);
        if let Some(scrollbar) = scrollbar {
            self.scroll.drag(
                scrollbar,
                true,
                (self.rows().len() as i32 * ROW, rows_rect.height),
                self.pointer,
            );
        }
    }

    fn key(&mut self, p: &Painter<'_>, client: Rect, key: Key) {
        if self.modal.is_some() {
            if matches!(key.code, ENTER | ESCAPE) {
                self.modal = None;
            }
            return;
        }
        let menus = menus();
        let bar = Rect::new(client.x, client.y, client.width, MENU_HEIGHT);
        match self.menubar.key(p, bar, &menus, &key) {
            MenuEvent::Command(command) => return self.run(command),
            MenuEvent::Handled => return,
            MenuEvent::Ignored => {}
        }
        let rows = self.rows();
        let count = rows.len();
        let row = rows[self.selected.min(count - 1)];
        match key.code {
            UP => self.selected = self.selected.saturating_sub(1),
            DOWN => self.selected = (self.selected + 1).min(count - 1),
            RIGHT => {
                if let Row::Category(index) = row {
                    self.expanded[index] = true;
                }
            }
            LEFT => {
                if let Row::Category(index) = row {
                    self.expanded[index] = false;
                }
            }
            ENTER => match row {
                Row::Device(..) => self.run(Command::Properties),
                _ => {
                    self.toggle(row);
                }
            },
            F5 => self.refresh(),
            _ => {}
        }
        self.selected = self.selected.min(self.rows().len() - 1);
        // 选中行保持可见。
        let (rows_rect, _) = self.view(client);
        let top = self.selected as i32 * ROW;
        if top < self.scroll.position {
            self.scroll.position = top;
        } else if top + ROW > self.scroll.position + rows_rect.height {
            self.scroll.position = top + ROW - rows_rect.height;
        }
    }

    fn paint(&mut self, p: &mut Painter<'_>, client: Rect, icons: &Icons) {
        let menus = menus();
        let bar = Rect::new(client.x, client.y, client.width, MENU_HEIGHT);
        let frame = Self::tree_area(client);
        let inner = p.edge(frame, Edge::Sunken);
        p.fill(inner, C::Window);
        let (rows_rect, scrollbar) = self.view(client);
        let rows = self.rows();
        let content = rows.len() as i32 * ROW;
        self.scroll.clamp((content - rows_rect.height).max(0));
        let position = self.scroll.position;
        let first = (position / ROW) as usize;
        p.clipped(rows_rect, |p| {
            for (index, row) in rows
                .iter()
                .enumerate()
                .skip(first)
                .take((rows_rect.height / ROW + 2) as usize)
            {
                let y = rows_rect.y + index as i32 * ROW - position;
                let (level, text, icon) = match *row {
                    Row::Computer => (0, self.computer.clone(), &icons.computer),
                    Row::Category(category) => (
                        1,
                        self.categories[category].name.to_owned(),
                        &icons.category,
                    ),
                    Row::Device(category, device) => {
                        (2, self.device(category, device).name.clone(), &icons.device)
                    }
                };
                let x = rows_rect.x + 2 + level * INDENT;
                if let Row::Category(category) = *row {
                    // +/- 方框。
                    let boxed = Rect::new(x, y + (ROW - 9) / 2, 9, 9);
                    p.fill(boxed, C::Window);
                    p.outline(boxed, C::ButtonShadow);
                    p.fill(Rect::new(boxed.x + 2, boxed.y + 4, 5, 1), C::WindowText);
                    if !self.expanded[category] {
                        p.fill(Rect::new(boxed.x + 4, boxed.y + 2, 1, 5), C::WindowText);
                    }
                }
                let icon_x = x + if matches!(row, Row::Category(_)) {
                    14
                } else {
                    0
                };
                if let Some(icon) = icon {
                    icon.paint(p, icon_x, y + 1);
                }
                let text_x = icon_x + 20;
                let hot = index == self.selected;
                let width = (p.text_width(&text, false) + p.scale() - 1) / p.scale();
                if hot {
                    p.fill(Rect::new(text_x - 2, y, width + 4, ROW), C::Hilight);
                }
                p.text_fit(
                    text_x,
                    y + (ROW - p.line_height()) / 2,
                    rows_rect.right() - text_x - 4,
                    &text,
                    Ink::Color(if hot { C::HilightText } else { C::WindowText }),
                    false,
                );
            }
        });
        if let Some(scrollbar) = scrollbar {
            p.scrollbar(
                scrollbar,
                true,
                (content - 1, rows_rect.height, position),
                self.scroll.pressed(),
            );
        }
        self.menubar.paint(p, bar, &menus, self.pointer);
        match &self.modal {
            Some(Modal::Properties(category, device)) => {
                let device = self.device(*category, *device);
                paint_properties(p, client, device);
            }
            Some(Modal::About) => paint_about(p, client),
            None => {}
        }
    }
}

/// 模态对话框与它的确定按钮，居中于 `client`。
fn modal_rects(client: Rect) -> (Rect, Rect) {
    let dialog = Rect::new(
        client.x + (client.width - 330) / 2,
        client.y + (client.height - 260).max(0) / 3,
        330,
        260,
    );
    (
        dialog,
        Rect::new(
            dialog.x + (dialog.width - 75) / 2,
            dialog.bottom() - 34,
            75,
            23,
        ),
    )
}

fn dialog_frame(p: &mut Painter<'_>, dialog: Rect, title: &str) {
    let inner = p.window_frame(dialog, FrameKind::Fixed);
    p.fill(inner, C::ButtonFace);
    p.caption_bar(
        Rect::new(inner.x, inner.y, inner.width, 19),
        title,
        None,
        true,
        &[],
        None,
    );
}

fn ok_button(p: &mut Painter<'_>, rect: Rect) {
    p.push_button(
        rect,
        "确定",
        ButtonState {
            default: true,
            ..Default::default()
        },
    );
}

fn paint_properties(p: &mut Painter<'_>, client: Rect, device: &Device) {
    let (dialog, ok) = modal_rects(client);
    dialog_frame(p, dialog, &format!("{} 属性", device.name));
    let ink = Ink::Color(C::ButtonText);
    p.text_fit(
        dialog.x + 16,
        dialog.y + 34,
        dialog.width - 32,
        &device.name,
        ink,
        true,
    );
    p.fill(
        Rect::new(dialog.x + 16, dialog.y + 56, dialog.width - 32, 1),
        C::ButtonShadow,
    );
    let step = p.line_height() + 6;
    for (row, (label, value)) in device.details.iter().enumerate() {
        let y = dialog.y + 66 + row as i32 * step;
        p.text_fit(dialog.x + 16, y, 80, &format!("{label}："), ink, false);
        p.text_fit(dialog.x + 100, y, dialog.width - 116, value, ink, false);
    }
    p.text(
        dialog.x + 16,
        ok.y - 24,
        "设备状态：这个设备工作正常。",
        ink,
        false,
    );
    ok_button(p, ok);
}

fn paint_about(p: &mut Painter<'_>, client: Rect) {
    let (dialog, ok) = modal_rects(client);
    let dialog = Rect::new(dialog.x, dialog.y, dialog.width, 110);
    let ok = Rect::new(ok.x, dialog.bottom() - 34, ok.width, ok.height);
    dialog_frame(p, dialog, "关于设备管理器");
    let ink = Ink::Color(C::ButtonText);
    p.text(dialog.x + 20, dialog.y + 34, "LiteOS 设备管理器", ink, true);
    p.text(
        dialog.x + 20,
        dialog.y + 54,
        "按类别列出系统里能确认的硬件。",
        ink,
        false,
    );
    ok_button(p, ok);
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut display = Display::connect()?;
    let window = display.create_window("设备管理器", APP_ID, (440, 400))?;
    let mut decoration = Decoration::new(FrameKind::Resizable);
    decoration.load_icon("device-manager");
    let theme = Theme::classic();
    let icons = Icons::load();
    let mut app = App::new();
    let mut font: Option<Typeface> = None;
    let mut scale = 0;
    while !app.closing {
        let event = display.next_event_timeout(Duration::from_millis(500))?;
        let Some(c) = display.configuration(window) else {
            continue;
        };
        let client = decoration.client_rect(c);
        let mut dirty = false;
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
            match event.kind {
                EventKind::Close => break,
                EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                    app.pointer = (x.floor() as i32, y.floor() as i32);
                    app.pointer_moved(&probe, client);
                    if inside(client, app.pointer) {
                        display.set_cursor_shape(CursorShape::Default)?;
                    }
                }
                EventKind::PointerLeave => app.pointer = (-1, -1),
                EventKind::Wheel { vertical, .. } => app.wheel(client, vertical),
                EventKind::Button {
                    code: 0x110,
                    pressed: true,
                    ..
                } => app.press(&probe, client),
                EventKind::Button {
                    code: 0x110,
                    pressed: false,
                    ..
                } => app.scroll.release(),
                EventKind::Key(key) if key.pressed => app.key(&probe, client, key),
                EventKind::Key(key) => {
                    app.menubar.key_released(&key);
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
            decoration.paint(&mut p, c, "设备管理器");
            app.paint(&mut p, client, &icons);
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
            eprintln!("devmgmt: error: {error}");
            ExitCode::FAILURE
        }
    }
}
