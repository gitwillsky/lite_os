//! 资源管理器：浏览文件夹、"我的电脑"与"控制面板"；使用普通 Wayland 连接，没有外壳权限。
extern crate liteos_runtime as _;
mod browser;
mod chrome;
mod dialogs;
mod fileops;
mod grid;
mod model;
mod ui;

use browser::Browser;
use canvas::{Rect, Surface};
use chrome::{Command, Entry, MENU_TITLES, TOOLS};
use classic_theme::Theme;
use classic_ui::{
    Configuration, Decoration, DecorationAction, Display, EventKind, FrameKind, Key, Painter,
    WindowId,
};
use dialogs::{Confirm, Notice};
use grid::{Grid, inside};
use model::Location;
use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant},
};
use typeface::Typeface;
use ui::{Assets, Popup, Scene, Ui};

const LEFT_BUTTON: u32 = 0x110;
const RIGHT_BUTTON: u32 = 0x111;
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

/// Linux evdev 键码。
/// 左、右 Alt 的 evdev 键码。
const ALT_KEYS: [u32; 2] = [56, 100];

mod code {
    pub const ESCAPE: u32 = 1;
    pub const TAB: u32 = 15;
    pub const A: u32 = 30;
    pub const X: u32 = 45;
    pub const C: u32 = 46;
    pub const V: u32 = 47;
    pub const Y: u32 = 21;
    pub const N: u32 = 49;
    pub const F2: u32 = 60;
    pub const DELETE: u32 = 111;
    pub const BACKSPACE: u32 = 14;
    pub const ENTER: u32 = 28;
    pub const L: u32 = 38;
    pub const F4: u32 = 62;
    pub const F5: u32 = 63;
    pub const HOME: u32 = 102;
    pub const UP: u32 = 103;
    pub const PAGE_UP: u32 = 104;
    pub const LEFT: u32 = 105;
    pub const RIGHT: u32 = 106;
    pub const END: u32 = 107;
    pub const DOWN: u32 = 108;
    pub const PAGE_DOWN: u32 = 109;
}

struct App {
    browser: Browser,
    ui: Ui,
    /// 界面上次同步时显示的位置；与 `browser.location` 不同说明刚导航过。
    shown: Location,
    last_click: Option<(usize, Instant)>,
    closing: bool,
    /// 最近一次键盘事件里的 Ctrl 与 Shift：列表点击据此决定是单选、切换还是区间选择。
    control: bool,
    shift: bool,
}

impl App {
    fn client(decoration: &Decoration, c: Configuration) -> Rect {
        decoration.client_rect(c)
    }

    fn grid(&self, client: Rect) -> Grid {
        let l = ui::layout(client, &self.browser);
        Grid::new(self.browser.view, l.list_inner, self.browser.items.len())
    }

    /// 导航后让地址栏、滚动位置与标题回到新位置。
    fn sync(&mut self) {
        if self.shown != self.browser.location {
            self.shown = self.browser.location.clone();
            self.ui.address = classic_ui::Edit::new(&self.browser.location.address());
            self.ui.list_scroll.position = 0;
            self.ui.address_focus = false;
            self.ui.rename = None;
            if let Some(row) = self.browser.tree_index(&self.shown) {
                self.ui.tree_scroll.position = (row as i32 * 18 - 36).max(0);
            }
        }
    }

    fn start_rename(&mut self) {
        if let Some(index) = self.browser.selected
            && let Some(item) = self.browser.items.get(index)
        {
            self.ui.rename = Some((index, classic_ui::Edit::new(&item.name)));
        }
    }

    /// 提交原地改名；名称非法时留在编辑状态，错误显示在状态栏。
    fn commit_rename(&mut self) {
        if let Some((_, edit)) = &self.ui.rename
            && self.browser.rename_selected(edit.text())
        {
            self.ui.rename = None;
        }
    }

    /// 删除类操作先问用户：放入回收站、永久删除、清空回收站的措辞不同。
    fn ask(&mut self, command: Command) {
        let name = self
            .browser
            .selected
            .and_then(|i| self.browser.items.get(i))
            .map(|item| item.name.clone())
            .unwrap_or_default();
        let text = match command {
            Command::EmptyTrash => {
                "确实要删除回收站中的所有项目吗？\n这些项目将被永久删除。".to_owned()
            }
            _ if self.browser.selection().len() > 1 => {
                let count = self.browser.selection().len();
                if self.browser.in_trash() {
                    format!("确实要删除这 {count} 项吗？\n这些项目将被永久删除，无法还原。")
                } else {
                    format!("确实要把这 {count} 项放入回收站吗？")
                }
            }
            _ if self.browser.in_trash() => {
                format!("确实要删除“{name}”吗？\n该项目将被永久删除，无法还原。")
            }
            _ => format!("确实要把“{name}”放入回收站吗？"),
        };
        self.ui.confirm = Some(Confirm { text, command });
        let _ = sound::play(ALERT_SOUND);
    }

    fn run(&mut self, command: Command, client: Rect) {
        match command {
            Command::Delete | Command::EmptyTrash => return self.ask(command),
            Command::Cut => self.browser.copy_selected(true),
            Command::Copy => self.browser.copy_selected(false),
            Command::Paste => self.browser.paste(),
            Command::Rename => self.start_rename(),
            Command::NewFolder => {
                if self.browser.new_folder().is_some() {
                    self.start_rename();
                }
            }
            Command::Restore => self.browser.restore_selected(),
            Command::Properties => self.ui.properties = self.browser.properties(),
            Command::Open => self.browser.open_selected(),
            Command::Close => self.closing = true,
            Command::Refresh => self.browser.refresh(),
            Command::Back => self.browser.back(),
            Command::Forward => self.browser.forward(),
            Command::Up => self.browser.up(),
            Command::ToggleToolbar => self.browser.show_toolbar ^= true,
            Command::ToggleStatusBar => self.browser.show_status_bar ^= true,
            Command::ToggleFolders => self.browser.show_folders ^= true,
            Command::SetView(view) => {
                self.browser.view = view;
                self.ui.list_scroll.position = 0;
            }
            Command::ViewMenu => {
                let l = ui::layout(client, &self.browser);
                if let Some(bar) = l.toolbar {
                    let rect = chrome::tool_rects(bar)[TOOLS.len() - 1];
                    self.ui.popup = Some(Popup {
                        origin: (rect.x, rect.bottom()),
                        entries: chrome::view_entries(self.browser.view),
                        hover: None,
                        menu: None,
                        from_toolbar: true,
                    });
                }
            }
        }
        self.sync();
    }

    /// 把失败原因从状态栏提升为错误提示框并响铃；框关闭前不接受其他输入，
    /// 否则连续失败的原因会被后一次操作悄悄覆盖。
    fn raise_notice(&mut self) {
        if self.ui.notice.is_none()
            && let Some(text) = self.browser.message.take()
        {
            self.ui.notice = Some(Notice { text });
            let _ = sound::play(ALERT_SOUND);
        }
    }

    fn answer(&mut self, yes: bool) {
        let Some(confirm) = self.ui.confirm.take() else {
            return;
        };
        if yes {
            match confirm.command {
                Command::EmptyTrash => {
                    self.browser.empty_trash();
                    let _ = sound::play("recycle.wav");
                }
                _ => self.browser.delete_selected(),
            }
        }
        self.sync();
    }

    fn open_menu(&mut self, p: &Painter<'_>, index: usize, client: Rect) {
        let l = ui::layout(client, &self.browser);
        let rect = ui::menu_title_rects(p, l.menu)[index];
        self.ui.popup = Some(Popup {
            origin: (rect.x, rect.bottom()),
            entries: chrome::menu(index, &self.browser),
            hover: None,
            menu: Some(index),
            from_toolbar: false,
        });
    }

    /// 地址栏回车：绝对路径、相对当前目录的路径或虚拟文件夹名。
    fn go_to_address(&mut self) {
        let text = self.ui.address.text().trim().to_owned();
        let location = match text.as_str() {
            "我的电脑" | "::my-computer" => Location::MyComputer,
            "控制面板" | "::control-panel" => Location::ControlPanel,
            _ => {
                let path = PathBuf::from(&text);
                Location::Directory(match &self.browser.location {
                    Location::Directory(current) if !path.is_absolute() => current.join(path),
                    _ => path,
                })
            }
        };
        if matches!(&location, Location::Directory(p) if !p.is_dir()) {
            self.browser.message = Some(format!("找不到 “{text}”。请检查名称是否正确，然后重试。"));
            return;
        }
        self.browser.go(location);
        self.sync();
    }

    fn press(&mut self, p: &Painter<'_>, client: Rect) {
        let point = self.ui.pointer;
        self.ui.tooltip.dismiss();
        if self.ui.properties.is_some() {
            if inside(dialogs::properties_rects(client).1, point) {
                self.ui.properties = None;
            }
            return;
        }
        if self.ui.notice.is_some() {
            if inside(dialogs::notice_rects(client).1, point) {
                self.ui.notice = None;
            }
            return;
        }
        if self.ui.confirm.is_some() {
            let (_, yes, no) = dialogs::confirm_rects(client);
            if inside(yes, point) {
                self.answer(true);
            } else if inside(no, point) {
                self.answer(false);
            }
            return;
        }
        if self.ui.rename.is_some() {
            self.commit_rename();
            if self.ui.rename.is_some() {
                return;
            }
        }
        let l = ui::layout(client, &self.browser);
        if let Some(popup) = self.ui.popup.take() {
            if let Some(Entry::Item {
                command: Some(command),
                ..
            }) = popup.row_at(point).and_then(|row| popup.entries.get(row))
            {
                let command = *command;
                self.run(command, client);
                return;
            }
            // 点在其他顶层菜单标题上则切换到它；点在别处只关闭菜单。
            if inside(popup.rect(), point) {
                return;
            }
            if let Some(index) = ui::menu_title_rects(p, l.menu)
                .iter()
                .position(|r| inside(*r, point))
            {
                if popup.menu != Some(index) {
                    self.open_menu(p, index, client);
                }
                return;
            }
            if l.toolbar.is_none_or(|bar| !inside(bar, point)) {
                return;
            }
        }
        self.ui.address_focus = inside(l.combo, point);
        if let Some(index) = ui::menu_title_rects(p, l.menu)
            .iter()
            .position(|r| inside(*r, point))
        {
            self.open_menu(p, index, client);
        } else if let Some(bar) = l.toolbar.filter(|bar| inside(*bar, point)) {
            self.ui.pressed_tool = chrome::tool_rects(bar)
                .iter()
                .position(|r| inside(*r, point))
                .filter(|&i| chrome::tool_enabled(&TOOLS[i], &self.browser));
        } else if inside(l.go, point) {
            self.ui.pressed_tool = Some(usize::MAX);
        } else if let Some(pane) = l.pane.filter(|pane| inside(*pane, point)) {
            if self.browser.show_folders {
                self.press_tree(pane, point);
            }
        } else if inside(l.list, point) {
            self.press_list(client, point);
        }
    }

    /// 右键：在项目上先选中再弹出项目菜单，在空白处弹出列表菜单。
    fn press_context(&mut self, client: Rect) {
        let point = self.ui.pointer;
        self.ui.popup = None;
        if self.ui.confirm.is_some()
            || self.ui.notice.is_some()
            || !inside(ui::layout(client, &self.browser).list, point)
        {
            return;
        }
        let hit = self.grid(client).at(point, self.ui.list_scroll.position);
        if hit.is_some() {
            self.browser.selected = hit;
        }
        self.ui.popup = Some(Popup {
            origin: point,
            entries: chrome::context(&self.browser, hit.is_some()),
            hover: None,
            menu: None,
            from_toolbar: false,
        });
    }

    fn press_tree(&mut self, pane: Rect, point: (i32, i32)) {
        if inside(ui::tree_close_rect(pane), point) {
            self.browser.show_folders = false;
            return;
        }
        let (view, bar, content) = ui::tree_scroll_geometry(pane, self.browser.rows.len());
        if let Some(bar) = bar.filter(|bar| inside(*bar, point)) {
            self.ui
                .tree_scroll
                .press(bar, true, (content, view.height, 18), point);
            return;
        }
        let scroll = self.ui.tree_scroll.position;
        let hit = self
            .browser
            .rows
            .iter()
            .enumerate()
            .find_map(|(index, row)| {
                let rect = ui::tree_row_rect(view, index, scroll);
                inside(view, point)
                    .then_some(())
                    .filter(|_| inside(rect, point))
                    .map(|_| {
                        (
                            row.location.clone(),
                            row.expandable,
                            inside(ui::expander_rect(rect, row.depth).inset(-2), point),
                        )
                    })
            });
        if let Some((location, expandable, on_expander)) = hit {
            if expandable && on_expander {
                self.browser.toggle_expanded(&location);
            } else {
                self.browser.go(location);
                self.sync();
            }
        }
    }

    fn press_list(&mut self, client: Rect, point: (i32, i32)) {
        let grid = self.grid(client);
        if let Some(bar) = grid.bar.filter(|bar| inside(*bar, point)) {
            let content = (grid.extent, grid.page(), grid.line());
            self.ui
                .list_scroll
                .press(bar, grid.vertical(), content, point);
            return;
        }
        match grid.at(point, self.ui.list_scroll.position) {
            Some(index) if self.control => {
                self.browser.toggle(index);
                self.last_click = None;
            }
            Some(index) if self.shift => {
                self.browser.select_range(index);
                self.last_click = None;
            }
            Some(index) => {
                self.browser.select_only(index);
                let double = self
                    .last_click
                    .is_some_and(|(last, at)| last == index && at.elapsed() < DOUBLE_CLICK);
                self.last_click = (!double).then(|| (index, Instant::now()));
                if double {
                    self.browser.open_selected();
                    self.sync();
                }
            }
            // Ctrl 或 Shift 按着时点空白不丢掉已有选择，与 Windows 一致。
            None if self.control || self.shift => {}
            None => {
                self.browser.selected = None;
                self.browser.extra.clear();
                self.last_click = None;
            }
        }
    }

    fn release(&mut self, client: Rect) {
        self.ui.list_scroll.release();
        self.ui.tree_scroll.release();
        let l = ui::layout(client, &self.browser);
        let point = self.ui.pointer;
        match self.ui.pressed_tool.take() {
            Some(usize::MAX) if inside(l.go, point) => self.go_to_address(),
            Some(index) if index < TOOLS.len() => {
                let hit = l
                    .toolbar
                    .is_some_and(|bar| inside(chrome::tool_rects(bar)[index], point));
                if let (true, Some(command)) = (hit, TOOLS[index].command) {
                    self.run(command, client);
                }
            }
            _ => {}
        }
    }

    fn pointer_moved(&mut self, client: Rect) {
        let point = self.ui.pointer;
        self.ui
            .tooltip
            .hover(ui::layout(client, &self.browser).toolbar.and_then(|bar| {
                chrome::tool_rects(bar)
                    .iter()
                    .position(|rect| inside(*rect, point))
            }));
        if let Some(popup) = &mut self.ui.popup {
            popup.hover = popup.row_at(point);
        }
        let l = ui::layout(client, &self.browser);
        let grid = self.grid(client);
        if let Some(bar) = grid.bar {
            self.ui
                .list_scroll
                .drag(bar, grid.vertical(), (grid.extent, grid.page()), point);
        }
        if let Some(pane) = l.pane.filter(|_| self.browser.show_folders) {
            let (view, bar, content) = ui::tree_scroll_geometry(pane, self.browser.rows.len());
            if let Some(bar) = bar {
                self.ui
                    .tree_scroll
                    .drag(bar, true, (content, view.height), point);
            }
        }
    }

    /// 记下 Ctrl 与 Shift 的状态；按下与释放事件都要调用。
    fn track_modifiers(&mut self, key: &Key) {
        self.control = key.modifiers.control;
        self.shift = key.modifiers.shift;
    }

    /// 记录 Alt/Tab 对键盘提示的影响：任何按键序列里只要出现 Alt 或 Tab 就显示下划线；
    /// Alt 之后没有其他键直接松开才进入菜单栏（见 [`Self::alt_released`]）。
    fn track_cues(&mut self, key: &Key) {
        let alt = ALT_KEYS.contains(&key.code);
        self.ui.alt_alone = alt;
        if alt || key.code == code::TAB {
            self.ui.cues = true;
        }
    }

    /// 单独的 Alt 松开：进入或退出菜单栏的键盘选择（K11）。
    fn alt_released(&mut self, key: &Key) {
        if ALT_KEYS.contains(&key.code) && std::mem::take(&mut self.ui.alt_alone) {
            self.ui.menu_focus = if self.ui.menu_focus.is_some() {
                None
            } else {
                Some(0)
            };
        }
    }

    /// 菜单栏处于键盘选择状态时的按键；返回是否已处理。
    fn menu_bar_key(&mut self, p: &Painter<'_>, key: &Key, client: Rect) -> bool {
        let Some(index) = self.ui.menu_focus else {
            return false;
        };
        let count = MENU_TITLES.len();
        match key.code {
            code::LEFT => self.ui.menu_focus = Some((index + count - 1) % count),
            code::RIGHT => self.ui.menu_focus = Some((index + 1) % count),
            code::ENTER | code::DOWN => {
                self.ui.menu_focus = None;
                self.open_menu(p, index, client);
            }
            _ => self.ui.menu_focus = None,
        }
        true
    }

    fn key(&mut self, p: &Painter<'_>, key: Key, client: Rect) {
        self.track_cues(&key);
        self.track_modifiers(&key);
        if self.ui.properties.is_some() {
            if matches!(key.code, code::ENTER | code::ESCAPE) {
                self.ui.properties = None;
            }
            return;
        }
        if self.ui.notice.is_some() {
            if matches!(key.code, code::ENTER | code::ESCAPE) {
                self.ui.notice = None;
            }
            return;
        }
        if self.ui.confirm.is_some() {
            match key.code {
                code::Y | code::ENTER => self.answer(true),
                code::N | code::ESCAPE => self.answer(false),
                _ => {}
            }
            return;
        }
        if let Some((_, edit)) = &mut self.ui.rename {
            match key.code {
                code::ENTER => self.commit_rename(),
                code::ESCAPE => self.ui.rename = None,
                _ => {
                    edit.key(key);
                }
            }
            return;
        }
        if let Some(popup) = &mut self.ui.popup {
            match key.code {
                code::ESCAPE => self.ui.popup = None,
                code::UP | code::DOWN => {
                    let step: isize = if key.code == code::UP { -1 } else { 1 };
                    let count = popup.entries.len() as isize;
                    let mut at = popup
                        .hover
                        .map_or(if step > 0 { -1 } else { count }, |h| h as isize);
                    for _ in 0..count {
                        at = (at + step).rem_euclid(count);
                        if matches!(
                            popup.entries[at as usize],
                            Entry::Item {
                                command: Some(_),
                                ..
                            }
                        ) {
                            popup.hover = Some(at as usize);
                            break;
                        }
                    }
                }
                code::ENTER => {
                    let command = popup.hover.and_then(|h| match popup.entries[h] {
                        Entry::Item { command, .. } => command,
                        Entry::Separator => None,
                    });
                    self.ui.popup = None;
                    if let Some(command) = command {
                        self.run(command, client);
                    }
                }
                _ => {}
            }
            return;
        }
        if self.menu_bar_key(p, &key, client) {
            return;
        }
        if key.modifiers.alt {
            // Alt+字母：打开助记键匹配的菜单。
            if let Some(letter) = key.letter()
                && let Some(index) = MENU_TITLES.iter().position(|title| {
                    title
                        .split_once('(')
                        .and_then(|(_, rest)| rest.chars().next())
                        .is_some_and(|mark| mark.to_ascii_lowercase() == letter)
                })
            {
                self.open_menu(p, index, client);
                return;
            }
            match key.code {
                code::LEFT => self.run(Command::Back, client),
                code::RIGHT => self.run(Command::Forward, client),
                code::UP => self.run(Command::Up, client),
                code::ENTER => self.run(Command::Properties, client),
                _ => {}
            }
            return;
        }
        if self.ui.address_focus {
            match key.code {
                code::ENTER => self.go_to_address(),
                code::ESCAPE => {
                    self.ui.address = classic_ui::Edit::new(&self.browser.location.address());
                    self.ui.address_focus = false;
                }
                _ => {
                    self.ui.address.key(key);
                }
            }
            return;
        }
        match key.code {
            code::F4 => self.ui.address_focus = true,
            code::L if key.modifiers.control => self.ui.address_focus = true,
            code::F5 => self.run(Command::Refresh, client),
            code::BACKSPACE => self.run(Command::Up, client),
            code::ENTER => self.run(Command::Open, client),
            code::DELETE => self.run(Command::Delete, client),
            code::F2 => self.run(Command::Rename, client),
            code::A if key.modifiers.control => self.browser.select_all(),
            code::C if key.modifiers.control => self.run(Command::Copy, client),
            code::X if key.modifiers.control => self.run(Command::Cut, client),
            code::V if key.modifiers.control => self.run(Command::Paste, client),
            _ => self.select_by_key(key, client),
        }
    }

    /// 方向键、翻页键与首字母查找；改变选择后滚动到可见。
    fn select_by_key(&mut self, key: Key, client: Rect) {
        let count = self.browser.items.len();
        if count == 0 {
            return;
        }
        let grid = self.grid(client);
        let current = self.browser.selected;
        let page_rows = (grid.page() / grid.line()).max(1);
        let target = match (key.code, current) {
            (code::HOME, _) => Some(0),
            (code::END, _) => Some(count - 1),
            (
                code::UP | code::DOWN | code::LEFT | code::RIGHT | code::PAGE_UP | code::PAGE_DOWN,
                None,
            ) => Some(0),
            (code::UP, Some(i)) => Some(grid.step(i, 0, -1)),
            (code::DOWN, Some(i)) => Some(grid.step(i, 0, 1)),
            (code::LEFT, Some(i)) => Some(grid.step(i, -1, 0)),
            (code::RIGHT, Some(i)) => Some(grid.step(i, 1, 0)),
            (code::PAGE_UP, Some(i)) => Some(grid.step(i, 0, -page_rows)),
            (code::PAGE_DOWN, Some(i)) => Some(grid.step(i, 0, page_rows)),
            _ => key.text.and_then(|typed| {
                let typed = typed.to_lowercase().next()?;
                let start = current.map_or(0, |i| i + 1);
                (0..count)
                    .map(|offset| (start + offset) % count)
                    .find(|&i| self.browser.items[i].name.to_lowercase().starts_with(typed))
            }),
        };
        if let Some(index) = target {
            self.browser.selected = Some(index);
            self.ui.list_scroll.position = grid.reveal(index, self.ui.list_scroll.position);
        }
    }

    /// 绘制前把滚动位置夹在内容范围内（窗口缩小、项目变少都可能让旧位置越界）。
    fn clamp_scrolls(&mut self, client: Rect) {
        let grid = self.grid(client);
        self.ui.list_scroll.clamp(grid.max_scroll());
        if let Some(pane) = ui::layout(client, &self.browser).pane {
            let (view, _, content) = ui::tree_scroll_geometry(pane, self.browser.rows.len());
            self.ui.tree_scroll.clamp(content - view.height);
        }
    }
}

/// 确认框与错误提示框弹出时播放的系统声音（Windows 2000 的消息框事件都映射到它）。
const ALERT_SOUND: &str = "chord.wav";

/// 画一帧并提交：窗口装饰加资源管理器内容。
#[allow(clippy::too_many_arguments)]
fn redraw(
    display: &mut Display,
    window: WindowId,
    c: Configuration,
    theme: &Theme,
    font: &Typeface,
    decoration: &Decoration,
    app: &mut App,
    assets: &Assets,
    title: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = App::client(decoration, c);
    app.clamp_scrolls(client);
    let (w, h) = c.pixel_size()?;
    let mut pixels = vec![0; w as usize * h as usize];
    let surface = Surface::new(&mut pixels, w, h, w as usize).ok_or("invalid explorer window")?;
    let mut painter = Painter::new(surface, theme, font, c.scale);
    decoration.paint(&mut painter, c, title);
    ui::paint(
        &mut painter,
        &Scene {
            browser: &app.browser,
            ui: &app.ui,
            assets,
            active: c.active,
        },
        client,
    );
    display.present(window, &pixels)?;
    Ok(())
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let location = std::env::args()
        .nth(1)
        .map(|argument| Location::parse(&argument))
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| Location::Directory(home.into()))
        })
        .unwrap_or_else(|| Location::Directory("/".into()));
    let mut app = App {
        ui: Ui::new(&location.address()),
        shown: location.clone(),
        browser: Browser::new(location),
        last_click: None,
        closing: false,
        control: false,
        shift: false,
    };
    app.browser.show_folders = false;
    let mut display = Display::connect()?;
    let window: WindowId = display.create_window(
        &app.browser.location.title(),
        "org.liteos.file-explorer",
        (640, 480),
    )?;
    let mut decoration = Decoration::new(FrameKind::Resizable);
    decoration.load_icon("folder-closed");
    let theme = Theme::classic();
    let assets = Assets::load(Path::new("/usr/share/liteos/ui"));
    let mut font: Option<Typeface> = None;
    let mut scale = 0;
    let mut title = app.browser.location.title();
    let mut dirty = true;
    while !app.closing {
        let event = match app.ui.tooltip.pending() {
            Some(wait) => display.next_event_timeout(wait)?,
            None => Some(display.next_event()?),
        };
        // 超时没有事件：提示到点，只需要重画。
        let Some(event) = event else {
            if display.ready_to_draw(window)
                && let Some(c) = display.configuration(window)
                && let Some(font) = font.as_ref()
            {
                redraw(
                    &mut display,
                    window,
                    c,
                    &theme,
                    font,
                    &decoration,
                    &mut app,
                    &assets,
                    &title,
                )?;
            }
            continue;
        };
        if event.window != window {
            continue;
        }
        let Some(c) = display.configuration(window) else {
            continue;
        };
        match decoration.event(&event.kind, c) {
            Some(DecorationAction::Request(request)) => display.request(window, request)?,
            Some(DecorationAction::Close) => break,
            Some(DecorationAction::Cursor(shape)) => display.set_cursor_shape(shape)?,
            None => {}
        }
        let client = App::client(&decoration, c);
        if let EventKind::Configure(c) = &event.kind
            && scale != c.scale
        {
            font = Some(Typeface::load(
                Path::new("/usr/share/liteos/fonts"),
                c.scale as u32,
            )?);
            scale = c.scale;
        }
        let Some(font) = font.as_ref() else {
            continue;
        };
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
                app.ui.pointer = (x.floor() as i32, y.floor() as i32);
                app.pointer_moved(client);
            }
            EventKind::PointerLeave => {
                app.ui.pointer = (-1, -1);
                app.ui.tooltip.hover(None);
            }
            EventKind::Button {
                code: LEFT_BUTTON,
                pressed,
                ..
            } => {
                if pressed {
                    app.press(&probe, client);
                } else {
                    app.release(client);
                }
            }
            EventKind::Button {
                code: RIGHT_BUTTON,
                pressed: true,
                ..
            } => app.press_context(client),
            EventKind::Key(key) if key.pressed => app.key(&probe, key, client),
            EventKind::Key(key) => {
                app.track_modifiers(&key);
                app.alt_released(&key);
            }
            _ => {}
        }
        app.raise_notice();
        dirty |= !matches!(event.kind, EventKind::FramePresented { .. });
        let current = app.browser.location.title();
        if current != title {
            display.set_title(window, &current)?;
            title = current;
        }
        if dirty && display.ready_to_draw(window) {
            redraw(
                &mut display,
                window,
                c,
                &theme,
                font,
                &decoration,
                &mut app,
                &assets,
                &title,
            )?;
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
            eprintln!("file-explorer: error: {error}");
            ExitCode::FAILURE
        }
    }
}
