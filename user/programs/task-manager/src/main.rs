//! 任务管理器（`taskmgr`）：应用程序、进程、性能三个标签页，独立于桌面外壳运行，与 Windows 2000 的 taskmgr.exe 一致。
//! 应用列表与控制走标准 foreign-toplevel 协议，进程与系统总量读 procfs。
extern crate liteos_runtime as _;
mod menus;
mod performance;
mod processes;
mod system;

use canvas::{Rect, Surface};
use classic_theme::{Edge, SystemColor as C, Theme};
use classic_ui::{
    AppIcons, ButtonState, Decoration, DecorationAction, Display, DropMenu, EventKind, FrameKind,
    Ink, MenuBar, MenuEvent, Painter, Prompt, SCROLLBAR_WIDTH, Scroll, ToplevelId, ToplevelRequest,
};
use menus::{Command as MenuCommand, Speed};
use processes::{Column, Process, Sort};
use std::{
    collections::HashMap,
    path::Path,
    process::{Child, Command, ExitCode, Stdio},
    time::{Duration, Instant},
};
use system::{Monitor, Totals};
use typeface::Typeface;

const TABS: [&str; 3] = ["应用程序", "进程", "性能"];
const ROW: i32 = 17;
const MENU_HEIGHT: i32 = 19;
const STATUS_HEIGHT: i32 = 20;
const OWN_APP_ID: &str = "org.liteos.task-manager";
const ENTER: u32 = 28;
const ESCAPE: u32 = 1;
const UP: u32 = 103;
const DOWN: u32 = 108;
const DELETE: u32 = 111;
const F5: u32 = 63;

fn inside(rect: Rect, (x, y): (i32, i32)) -> bool {
    (rect.x..rect.right()).contains(&x) && (rect.y..rect.bottom()).contains(&y)
}

fn menu_rect(client: Rect) -> Rect {
    Rect::new(client.x, client.y, client.width, MENU_HEIGHT)
}

fn tab_rects(client: Rect) -> [Rect; 3] {
    let y = client.y + MENU_HEIGHT + 6;
    [
        Rect::new(client.x + 8, y, 78, 22),
        Rect::new(client.x + 86, y, 54, 22),
        Rect::new(client.x + 140, y, 54, 22),
    ]
}

/// 标签页的页面：应用与进程页下方留一排按钮，性能页直接到状态栏上方。
fn page_rect(client: Rect, tab: usize) -> Rect {
    let top = client.y + MENU_HEIGHT + 6 + 21;
    let bottom = client.bottom() - STATUS_HEIGHT - 8 - if tab == 2 { 0 } else { 31 };
    Rect::new(client.x + 8, top, client.width - 16, (bottom - top).max(0))
}

fn status_rect(client: Rect) -> Rect {
    Rect::new(
        client.x,
        client.bottom() - STATUS_HEIGHT,
        client.width,
        STATUS_HEIGHT,
    )
}

/// 页面下方的按钮：应用页"结束任务""切换至""新任务"，进程页"结束进程"。
fn buttons(client: Rect, tab: usize) -> Vec<(Rect, &'static str)> {
    let page = page_rect(client, tab);
    let labels: &[&str] = match tab {
        0 => &["结束任务(E)", "切换至(S)", "新任务(N)..."],
        1 => &["结束进程(E)"],
        _ => &[],
    };
    let mut x = page.right();
    let mut rects: Vec<(Rect, &'static str)> = labels
        .iter()
        .rev()
        .map(|label| {
            x -= 81;
            (Rect::new(x, page.bottom() + 8, 75, 23), *label)
        })
        .collect();
    rects.reverse();
    rects
}

/// 列表区：页面内缩，除去 6 像素边距；返回（外框，内部）。
fn list_rect(page: Rect) -> Rect {
    Rect::new(
        page.x + 6,
        page.y + 6,
        page.width - 12,
        (page.height - 12).max(0),
    )
}

/// 模态对话框：创建新任务与关于。
enum Modal {
    NewTask(Prompt),
    About,
}

struct App {
    tab: usize,
    selected_app: usize,
    selected_pid: Option<u32>,
    processes: Vec<Process>,
    previous: HashMap<u32, u64>,
    sort: Sort,
    monitor: Monitor,
    totals: Totals,
    speed: Speed,
    next_sample: Instant,
    menubar: MenuBar,
    modal: Option<Modal>,
    scroll: [Scroll; 2],
    children: Vec<Child>,
    pointer: (i32, i32),
    pressed: Option<usize>,
    closing: bool,
}

impl App {
    fn new() -> Self {
        let mut app = Self {
            tab: 0,
            selected_app: 0,
            selected_pid: None,
            processes: Vec::new(),
            previous: HashMap::new(),
            sort: Sort::default(),
            monitor: Monitor::new(),
            totals: Totals::default(),
            speed: Speed::Normal,
            next_sample: Instant::now(),
            menubar: MenuBar::new(),
            modal: None,
            scroll: [Scroll::default(), Scroll::default()],
            children: Vec::new(),
            pointer: (-1, -1),
            pressed: None,
            closing: false,
        };
        // 第一次采样只建立基线；紧接着很快再采一次，CPU 占比才有第一个有意义的值。
        app.sample();
        app.next_sample = Instant::now() + Duration::from_millis(300);
        app
    }

    /// 应用页的任务：除任务管理器自己以外的全部顶层窗口。
    fn tasks(display: &Display) -> Vec<(ToplevelId, String, bool, String)> {
        display
            .toplevels()
            .filter(|(_, info)| info.app_id != OWN_APP_ID)
            .map(|(id, info)| (id, info.title.clone(), info.minimized, info.app_id.clone()))
            .collect()
    }

    /// 采样系统总量并重读进程表。
    fn sample(&mut self) {
        self.monitor.sample();
        self.processes = processes::list(&self.previous, &self.monitor);
        self.previous = self
            .processes
            .iter()
            .map(|p| (p.pid, p.cpu_ticks))
            .collect();
        self.sort.apply(&mut self.processes);
        self.totals = Totals {
            handles: self.processes.iter().map(|p| u64::from(p.handles)).sum(),
            threads: self.processes.iter().map(|p| u64::from(p.threads)).sum(),
            processes: self.processes.len() as u64,
        };
    }

    /// 到点则采样；返回是否需要重画。
    fn tick(&mut self) -> bool {
        self.children
            .retain_mut(|child| child.try_wait().ok().flatten().is_none());
        let Some(interval) = self.speed.interval_ms() else {
            return false;
        };
        if Instant::now() < self.next_sample {
            return false;
        }
        self.next_sample = Instant::now() + Duration::from_millis(interval);
        self.sample();
        true
    }

    fn menus(&self) -> Vec<DropMenu<MenuCommand>> {
        menus::menu_bar(self.tab == 0, self.speed)
    }

    fn row_count(&self, display: &Display) -> usize {
        if self.tab == 1 {
            self.processes.len()
        } else {
            Self::tasks(display).len()
        }
    }

    /// 选中进程在列表里的序号。
    fn selected_process_index(&self) -> Option<usize> {
        self.processes
            .iter()
            .position(|p| Some(p.pid) == self.selected_pid)
    }

    fn selected_index(&self) -> usize {
        if self.tab == 1 {
            self.selected_process_index().unwrap_or(0)
        } else {
            self.selected_app
        }
    }

    fn select(&mut self, display: &Display, index: usize) {
        let count = self.row_count(display);
        let index = index.min(count.saturating_sub(1));
        if self.tab == 1 {
            self.selected_pid = self.processes.get(index).map(|p| p.pid);
        } else {
            self.selected_app = index;
        }
    }

    fn selected_task(&self, display: &Display) -> Option<ToplevelId> {
        Self::tasks(display)
            .get(self.selected_app)
            .map(|(id, ..)| *id)
    }

    fn end_selected(&mut self, display: &mut Display) {
        if self.tab == 1 {
            if let Some(pid) = self.selected_pid {
                processes::terminate(pid);
                self.next_sample = Instant::now();
            }
        } else if let Some(id) = self.selected_task(display) {
            let _ = display.control_toplevel(id, ToplevelRequest::Close);
        }
    }

    fn run(&mut self, display: &mut Display, command: MenuCommand) {
        let task = self.selected_task(display);
        let mut control = |request| {
            if let Some(id) = task {
                let _ = display.control_toplevel(id, request);
            }
        };
        match command {
            MenuCommand::NewTask => {
                self.modal = Some(Modal::NewTask(Prompt::new(
                    "创建新任务",
                    "键入程序、文件夹或文档的名称，\nLiteOS 将为您打开它。",
                    "打开(O):",
                    "",
                )))
            }
            MenuCommand::Exit => self.closing = true,
            MenuCommand::Refresh => self.sample(),
            MenuCommand::SetSpeed(speed) => {
                self.speed = speed;
                self.next_sample = Instant::now();
            }
            MenuCommand::Minimize => control(ToplevelRequest::Minimize(true)),
            MenuCommand::Maximize => control(ToplevelRequest::Maximize(true)),
            MenuCommand::BringToFront => control(ToplevelRequest::Activate),
            MenuCommand::About => self.modal = Some(Modal::About),
        }
    }

    /// 以当前用户执行 `text`（与桌面的"运行"一致），子进程由 [`Self::tick`] 回收。
    fn spawn(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let spawned = Command::new("/bin/sh")
            .args(["-c", text])
            .env_remove("WAYLAND_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => self.children.push(child),
            Err(error) => eprintln!("taskmgr: cannot start `{text}`: {error}"),
        }
    }

    /// 进程页表头每列的矩形。
    fn header_cells(list: Rect) -> Vec<(Column, Rect)> {
        let mut x = list.x + 2;
        Column::ALL
            .iter()
            .map(|column| {
                let rect = Rect::new(x, list.y + 2, column.width(), ROW);
                x += column.width();
                (*column, rect)
            })
            .collect()
    }

    /// 列表的行区（表头之下），有滚动条时已让出右侧。
    fn rows_rect(&self, list: Rect, count: usize) -> (Rect, Option<Rect>) {
        let inner = list.inset(2);
        let rows = Rect::new(inner.x, inner.y + ROW, inner.width, inner.height - ROW);
        if count as i32 * ROW > rows.height {
            let bar = Rect::new(
                rows.right() - SCROLLBAR_WIDTH,
                rows.y,
                SCROLLBAR_WIDTH,
                rows.height,
            );
            (
                Rect::new(rows.x, rows.y, rows.width - SCROLLBAR_WIDTH, rows.height),
                Some(bar),
            )
        } else {
            (rows, None)
        }
    }

    fn press(&mut self, p: &Painter<'_>, display: &mut Display, client: Rect) {
        let point = self.pointer;
        if let Some(modal) = &self.modal {
            let outcome = match modal {
                Modal::NewTask(prompt) => prompt.press(client, point),
                Modal::About => {
                    let (_, ok) = about_rects(client);
                    inside(ok, point).then_some(true)
                }
            };
            if let Some(accepted) = outcome {
                self.finish_modal(accepted);
            }
            return;
        }
        // 菜单：点标题打开/切换，点条目执行，点别处只关闭。
        let menus = self.menus();
        match self.menubar.press(p, menu_rect(client), &menus, point) {
            MenuEvent::Command(command) => {
                self.run(display, command);
                return;
            }
            MenuEvent::Handled => return,
            MenuEvent::Ignored => {}
        }
        if let Some(tab) = tab_rects(client).iter().position(|r| inside(*r, point)) {
            // 菜单栏随标签页变化（"窗口"只在应用页），先关掉打开的菜单。
            self.menubar.close();
            self.tab = tab;
            return;
        }
        self.pressed = buttons(client, self.tab)
            .iter()
            .position(|(r, _)| inside(*r, point));
        if self.tab == 2 {
            return;
        }
        let list = list_rect(page_rect(client, self.tab));
        let count = self.row_count(display);
        let (rows, bar) = self.rows_rect(list, count);
        if let Some(bar) = bar.filter(|bar| inside(*bar, point)) {
            let content = (count as i32 * ROW, rows.height, ROW);
            self.scroll[self.tab].press(bar, true, content, point);
            return;
        }
        if self.tab == 1
            && let Some((column, _)) = Self::header_cells(list)
                .into_iter()
                .find(|(_, rect)| inside(*rect, point))
        {
            self.sort.click(column);
            self.sort.apply(&mut self.processes);
            return;
        }
        if inside(rows, point) {
            let row = (point.1 - rows.y + self.scroll[self.tab].position) / ROW;
            self.select(display, row.max(0) as usize);
        }
    }

    fn finish_modal(&mut self, accepted: bool) {
        if let Some(Modal::NewTask(prompt)) = &self.modal
            && accepted
        {
            let text = prompt.text().to_owned();
            self.spawn(&text);
        }
        self.modal = None;
    }

    /// 滚轮：向下滚 `notches` 档滚动当前标签的列表（性能页没有列表）。
    fn wheel(&mut self, display: &Display, client: Rect, notches: i32) {
        if self.modal.is_some() || self.tab == 2 {
            return;
        }
        let list = list_rect(page_rect(client, self.tab));
        let count = self.row_count(display);
        let (rows, _) = self.rows_rect(list, count);
        self.scroll[self.tab].wheel(notches, ROW, count as i32 * ROW - rows.height);
    }

    fn pointer_moved(&mut self, p: &Painter<'_>, display: &Display, client: Rect) {
        let point = self.pointer;
        let menus = self.menus();
        if self.menubar.motion(p, menu_rect(client), &menus, point) {
            return;
        }
        if self.tab != 2 {
            let list = list_rect(page_rect(client, self.tab));
            let count = self.row_count(display);
            let (rows, bar) = self.rows_rect(list, count);
            if let Some(bar) = bar {
                self.scroll[self.tab].drag(bar, true, (count as i32 * ROW, rows.height), point);
            }
        }
    }

    fn key(&mut self, p: &Painter<'_>, display: &mut Display, key: classic_ui::Key, client: Rect) {
        if let Some(modal) = &mut self.modal {
            let outcome = match modal {
                Modal::NewTask(prompt) => prompt.key(key),
                Modal::About => matches!(key.code, ENTER | ESCAPE).then_some(true),
            };
            if let Some(accepted) = outcome {
                self.finish_modal(accepted);
            }
            return;
        }
        let menus = self.menus();
        match self.menubar.key(p, menu_rect(client), &menus, &key) {
            MenuEvent::Command(command) => {
                self.run(display, command);
                return;
            }
            MenuEvent::Handled => return,
            MenuEvent::Ignored => {}
        }
        let count = self.row_count(display);
        let index = self.selected_index();
        match key.code {
            UP if self.tab != 2 => self.select(display, index.saturating_sub(1)),
            DOWN if self.tab != 2 => self.select(display, index + 1),
            DELETE if self.tab != 2 => self.end_selected(display),
            ENTER if self.tab == 0 => {
                if let Some(id) = self.selected_task(display) {
                    let _ = display.control_toplevel(id, ToplevelRequest::Activate);
                }
            }
            F5 => self.sample(),
            _ => {}
        }
        // 键盘移动选择后让它留在可见范围内。
        if self.tab != 2 {
            let list = list_rect(page_rect(client, self.tab));
            let (rows, _) = self.rows_rect(list, count);
            let top = self.selected_index() as i32 * ROW;
            let scroll = &mut self.scroll[self.tab];
            if top < scroll.position {
                scroll.position = top;
            } else if top + ROW > scroll.position + rows.height {
                scroll.position = top + ROW - rows.height;
            }
        }
    }

    fn paint(&mut self, p: &mut Painter<'_>, display: &Display, client: Rect, icons: &AppIcons) {
        self.paint_tabs(p, client);
        let page = page_rect(client, self.tab);
        match self.tab {
            2 => performance::paint(p, page, &self.monitor, self.totals),
            _ => self.paint_list(p, display, client, icons),
        }
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
        self.paint_status(p, client);
        let menus = self.menus();
        self.menubar
            .paint(p, menu_rect(client), &menus, self.pointer);
        if let Some(modal) = &self.modal {
            paint_modal(p, client, modal);
        }
    }

    fn paint_tabs(&self, p: &mut Painter<'_>, client: Rect) {
        let page = page_rect(client, self.tab);
        let face = p.edge(page, Edge::Raised);
        p.fill(face, C::ButtonFace);
        // 先画未选中的，最后画选中的：选中标签头向两侧多出 2 像素并压住邻居的边线；
        // 第一个标签头不能越过页面左边框，否则左边线与页面对不齐。
        let tabs = tab_rects(client);
        for (index, (rect, title)) in tabs.into_iter().zip(TABS).enumerate() {
            if self.tab != index {
                let rect = Rect::new(rect.x, rect.y + 2, rect.width, page.y - rect.y - 2);
                p.tab(rect, title, false);
            }
        }
        let selected = tabs[self.tab];
        let left = if self.tab == 0 {
            selected.x
        } else {
            selected.x - 2
        };
        let rect = Rect::new(
            left,
            selected.y - 2,
            selected.right() + 2 - left,
            page.y + 2 - (selected.y - 2),
        );
        p.tab(rect, TABS[self.tab], true);
    }

    fn paint_list(
        &mut self,
        p: &mut Painter<'_>,
        display: &Display,
        client: Rect,
        icons: &AppIcons,
    ) {
        let list = list_rect(page_rect(client, self.tab));
        let inner = p.edge(list, Edge::Sunken);
        p.fill(inner, C::Window);
        let count = self.row_count(display);
        let (rows, bar) = self.rows_rect(list, count);
        let scroll = &mut self.scroll[self.tab];
        scroll.clamp((count as i32 * ROW - rows.height).max(0));
        let position = scroll.position;
        // 表头。
        let cells: Vec<(&str, Rect, bool)> = if self.tab == 1 {
            Self::header_cells(list)
                .into_iter()
                .map(|(column, rect)| (column.title(), rect, column.right_aligned()))
                .collect()
        } else {
            let status = 90;
            vec![
                (
                    "任务",
                    Rect::new(inner.x, inner.y, inner.width - status, ROW),
                    false,
                ),
                (
                    "状态",
                    Rect::new(inner.right() - status, inner.y, status, ROW),
                    false,
                ),
            ]
        };
        for (title, cell, _) in &cells {
            let face = p.edge(*cell, Edge::Raised);
            p.fill(face, C::ButtonFace);
            let y = cell.y + (ROW - p.line_height()) / 2;
            p.text(cell.x + 5, y, title, Ink::Color(C::ButtonText), false);
        }
        if self.tab == 1 {
            // 当前排序列在标题右侧画一个小三角。
            if let Some((_, cell)) = Self::header_cells(list)
                .into_iter()
                .find(|(column, _)| *column == self.sort.column)
            {
                let tip = cell.right() - 12;
                let mid = cell.y + ROW / 2;
                let rows_art: &[&str] = if self.sort.descending {
                    &["#####", ".###.", "..#.."]
                } else {
                    &["..#..", ".###.", "#####"]
                };
                for (row, art) in rows_art.iter().enumerate() {
                    for (col, ch) in art.chars().enumerate() {
                        if ch == '#' {
                            p.fill(
                                Rect::new(tip + col as i32, mid - 1 + row as i32, 1, 1),
                                C::ButtonText,
                            );
                        }
                    }
                }
            }
        }
        let selected = self.selected_index();
        p.clipped(rows, |p| {
            let first = (position / ROW) as usize;
            let visible = (rows.height / ROW + 2) as usize;
            if self.tab == 1 {
                for (i, process) in self.processes.iter().enumerate().skip(first).take(visible) {
                    let y = rows.y + i as i32 * ROW - position;
                    paint_process(p, rows, y, process, selected == i);
                }
            } else {
                for (i, (_, title, minimized, icon)) in Self::tasks(display)
                    .iter()
                    .enumerate()
                    .skip(first)
                    .take(visible)
                {
                    let y = rows.y + i as i32 * ROW - position;
                    let hot = selected == i;
                    if hot {
                        p.fill(Rect::new(rows.x + 20, y, rows.width - 20, ROW), C::Hilight);
                    }
                    icons.paint(p, icon, rows.x + 3, y + 1);
                    let ink = Ink::Color(if hot { C::HilightText } else { C::WindowText });
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
                    p.text(rows.right() - 85, text_y, state, ink, false);
                }
            }
        });
        if let Some(bar) = bar {
            let content = (
                count as i32 * ROW - 1,
                rows.height,
                self.scroll[self.tab].position,
            );
            p.scrollbar(bar, true, content, self.scroll[self.tab].pressed());
        }
    }

    fn paint_status(&self, p: &mut Painter<'_>, client: Rect) {
        let status = status_rect(client);
        let (used, limit) = self.monitor.memory.commit();
        let panes = [
            format!("进程数: {}", self.totals.processes),
            format!("CPU 使用: {}%", self.monitor.cpu_percent),
            format!(
                "内存使用: {}K / {}K",
                processes::grouped(used),
                processes::grouped(limit)
            ),
        ];
        let widths = [100, 110, status.width - 100 - 110];
        let mut x = status.x;
        for (text, width) in panes.iter().zip(widths) {
            let pane = Rect::new(x + 2, status.y + 2, width - 3, status.height - 3);
            p.edge(pane, Edge::SunkenOuter);
            let y = pane.y + (pane.height - p.line_height()) / 2;
            p.text(pane.x + 4, y, text, Ink::Color(C::ButtonText), false);
            x += width;
        }
    }
}

fn paint_process(p: &mut Painter<'_>, rows: Rect, y: i32, process: &Process, selected: bool) {
    if selected {
        p.fill(Rect::new(rows.x, y, rows.width, ROW), C::Hilight);
    }
    let ink = Ink::Color(if selected {
        C::HilightText
    } else {
        C::WindowText
    });
    let text_y = y + (ROW - p.line_height()) / 2;
    let mut x = rows.x;
    for column in Column::ALL {
        let text = column.text(process);
        if column.right_aligned() {
            let width = (p.text_width(&text, false) + p.scale() - 1) / p.scale();
            p.text(x + column.width() - 6 - width, text_y, &text, ink, false);
        } else {
            p.text(x + 5, text_y, &text, ink, false);
        }
        x += column.width();
    }
}

/// "关于"对话框与它的确定按钮，居中偏上于 `client`。
fn about_rects(client: Rect) -> (Rect, Rect) {
    let dialog = Rect::new(
        client.x + (client.width - 300) / 2,
        client.y + (client.height - 110) / 3,
        300,
        110,
    );
    let ok = Rect::new(
        dialog.x + (dialog.width - 75) / 2,
        dialog.bottom() - 34,
        75,
        23,
    );
    (dialog, ok)
}

fn paint_modal(p: &mut Painter<'_>, client: Rect, modal: &Modal) {
    match modal {
        Modal::NewTask(prompt) => prompt.paint(p, client),
        Modal::About => {
            let (dialog, ok) = about_rects(client);
            let inner = p.window_frame(dialog, FrameKind::Fixed);
            p.fill(inner, C::ButtonFace);
            p.caption_bar(
                Rect::new(inner.x, inner.y, inner.width, 19),
                "关于任务管理器",
                None,
                true,
                &[],
                None,
            );
            let ink = Ink::Color(C::ButtonText);
            p.text(dialog.x + 20, dialog.y + 34, "LiteOS 任务管理器", ink, true);
            p.text(
                dialog.x + 20,
                dialog.y + 54,
                "显示应用程序、进程与系统性能。",
                ink,
                false,
            );
            p.push_button(
                ok,
                "确定",
                ButtonState {
                    default: true,
                    ..Default::default()
                },
            );
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
    let window = display.create_window("任务管理器", OWN_APP_ID, (372, 450))?;
    let mut decoration = Decoration::new(FrameKind::Resizable);
    decoration.load_icon("task-manager");
    let theme = Theme::classic();
    let icons = AppIcons::load(16);
    let mut app = App::new();
    let mut font: Option<Typeface> = None;
    let mut scale = 0;
    let mut revision = 0;
    while !app.closing {
        let event = display.next_event_timeout(Duration::from_millis(250))?;
        let Some(c) = display.configuration(window) else {
            continue;
        };
        let client = decoration.client_rect(c);
        let mut dirty = app.tick();
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
            // 命中测试需要字体度量（菜单标题宽度），所以用一个不落笔的 Painter。
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
                    app.pointer_moved(&probe, &display, client);
                }
                EventKind::PointerLeave => app.pointer = (-1, -1),
                EventKind::Wheel { vertical, .. } => app.wheel(&display, client, vertical),
                EventKind::Button {
                    code: 0x110,
                    pressed: true,
                    ..
                } => app.press(&probe, &mut display, client),
                EventKind::Button {
                    code: 0x110,
                    pressed: false,
                    ..
                } => {
                    app.scroll[0].release();
                    app.scroll[1].release();
                    if let Some(index) = app.pressed.take()
                        && buttons(client, app.tab)
                            .get(index)
                            .is_some_and(|(r, _)| inside(*r, app.pointer))
                    {
                        match (app.tab, index) {
                            (0, 0) | (1, 0) => app.end_selected(&mut display),
                            (0, 1) => {
                                if let Some(id) = app.selected_task(&display) {
                                    let _ = display.control_toplevel(id, ToplevelRequest::Activate);
                                }
                            }
                            (0, 2) => app.run(&mut display, MenuCommand::NewTask),
                            _ => {}
                        }
                    }
                }
                EventKind::Key(key) if key.pressed => app.key(&probe, &mut display, key, client),
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
