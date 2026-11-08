//! 窗口内的菜单栏状态机：鼠标、方向键、Alt/F10 与助记键共用同一组命令。命令类型由调用者决定。

use canvas::Rect;
use std::time::{Duration, Instant};

use crate::{DropItem, DropMenu, Key, Painter, Popup, title_rects};

const ESCAPE: u32 = 1;
const ENTER: u32 = 28;
const TAB: u32 = 15;
const F10: u32 = 68;
const UP: u32 = 103;
const LEFT: u32 = 105;
const RIGHT: u32 = 106;
const DOWN: u32 = 108;
/// 左、右 Alt 的 evdev 键码。
const ALT_KEYS: [u32; 2] = [56, 100];
/// 级联子菜单的最大嵌套层数（含顶层下拉菜单）；运行库按此校验应用提供的菜单。
pub const MENU_DEPTH: usize = 4;
/// 指针停在子菜单父条目上到展开子菜单的延时：`SPI_GETMENUSHOWDELAY` 的 Windows 2000 默认值 400 毫秒。
const SHOW_DELAY: Duration = Duration::from_millis(400);

/// 菜单栏对一次输入的处理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuEvent<T> {
    /// 与菜单栏无关，调用者继续处理这次输入。
    Ignored,
    /// 菜单栏消费了这次输入（打开、移动高亮、关闭），调用者需要重画。
    Handled,
    /// 用户选中了一个命令。
    Command(T),
}

#[derive(Default)]
pub struct MenuBar {
    /// 打开的弹出层：第一个是下拉/上下文菜单，其后依次是级联子菜单。
    popups: Vec<Popup>,
    /// 指针悬停在子菜单父条目上、等待 [`SHOW_DELAY`] 后展开的子菜单（几何已在悬停时算好）。
    pending: Option<(Instant, Popup)>,
    /// 子菜单翻转与平移的边界（客户区）；调用者每次输入前更新。
    bounds: Option<Rect>,
    /// 键盘选中的菜单栏标题（单按 Alt 或 F10 进入）。
    focus: Option<usize>,
    /// 是否显示当前窗口的键盘助记提示。
    cues: bool,
    /// Alt 按下后还没有别的按键：此时松开 Alt 才算"单独的 Alt"。
    alt_alone: bool,
}

/// 菜单实际绘制状态的无分配投影；仅用于 owner 比较输入前后是否需要重绘。
#[derive(PartialEq, Eq)]
pub struct MenuVisual {
    menu: Option<usize>,
    popups: [Option<(Rect, Option<usize>)>; MENU_DEPTH],
    focus: Option<usize>,
    cues: bool,
}

/// 使用绘制下划线的同一字符作为助记键。
fn mnemonic(label: &str) -> Option<char> {
    let span = Painter::mnemonic_span(label)?;
    Some(char::from(
        label.as_bytes()[span.start].to_ascii_lowercase(),
    ))
}

fn inside(rect: Rect, (x, y): (i32, i32)) -> bool {
    (rect.x..rect.right()).contains(&x) && (rect.y..rect.bottom()).contains(&y)
}

impl MenuBar {
    pub fn new() -> Self {
        Self::default()
    }

    /// 返回实际影响像素的状态快照，不包含未显示的 Alt 按下时序。
    pub fn visual(&self) -> MenuVisual {
        let mut popups = [None; MENU_DEPTH];
        for (slot, popup) in popups.iter_mut().zip(&self.popups) {
            *slot = Some((popup.rect, popup.hover));
        }
        MenuVisual {
            menu: self.open_menu(),
            popups,
            focus: self.focus,
            cues: self.cues,
        }
    }

    /// 当前实际绘制的弹出层矩形；用于窗口失效，不暴露可变菜单状态。
    pub fn popup_rects(&self) -> impl Iterator<Item = Rect> + '_ {
        self.popups.iter().map(|popup| popup.rect)
    }

    /// 设置子菜单定位边界（窗口客户区）。
    pub fn set_bounds(&mut self, bounds: Rect) {
        self.bounds = Some(bounds);
    }

    /// 在客户区指定点显示共享上下文菜单；超出边界时向内平移，不新建协议 surface。
    pub fn show_popup<T, S: AsRef<str>>(
        &mut self,
        p: &Painter<'_>,
        menus: &[DropMenu<T, S>],
        point: (i32, i32),
        bounds: Rect,
    ) {
        if menus.is_empty() {
            return;
        }
        let mut popup = Popup::open(p, menus, 0, Rect::new(point.0, point.1, 0, 0));
        popup.rect.x = popup
            .rect
            .x
            .min(bounds.right() - popup.rect.width)
            .max(bounds.x);
        popup.rect.y = popup
            .rect
            .y
            .min(bounds.bottom() - popup.rect.height)
            .max(bounds.y);
        self.bounds = Some(bounds);
        self.popups = vec![popup];
        self.pending = None;
        self.focus = None;
    }
    /// 菜单是否正占着输入（下拉菜单打开或在键盘选择标题）：此时调用者不应再把键盘交给别处。
    pub fn active(&self) -> bool {
        !self.popups.is_empty() || self.focus.is_some()
    }

    /// 当前是否显示键盘助记下划线，供同一窗口内的其他标准控件使用。
    pub fn cues_visible(&self) -> bool {
        self.cues
    }

    /// 打开菜单后第几个菜单；用来给标题画"按下"状态。
    pub fn open_menu(&self) -> Option<usize> {
        self.popups.first().map(|popup| popup.menu)
    }

    /// 强制关闭下拉菜单与键盘选择（例如弹出模态对话框时）。
    pub fn close(&mut self) {
        self.popups.clear();
        self.pending = None;
        self.focus = None;
    }

    /// 等待展开的子菜单的截止时刻。
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|(due, _)| *due)
    }

    /// 到期则展开等待中的子菜单；返回是否需要重绘。
    pub fn tick(&mut self, now: Instant) -> bool {
        if self.pending.as_ref().is_none_or(|(due, _)| *due > now) {
            return false;
        }
        let (_, popup) = self.pending.take().expect("pending submenu");
        self.popups.push(popup);
        true
    }

    fn open<T, S: AsRef<str>>(
        &mut self,
        p: &Painter<'_>,
        bar: Rect,
        menus: &[DropMenu<T, S>],
        index: usize,
        keyboard: bool,
    ) {
        let titles = title_rects(p, bar, menus);
        let mut popup = Popup::open(p, menus, index, titles[index]);
        if keyboard {
            popup.step(&menus[index].items, 1);
        }
        self.popups = vec![popup];
        self.pending = None;
        self.focus = None;
    }

    /// 展开最深一层第 `index` 项的子菜单；`keyboard` 时高亮子菜单第一项。
    fn expand<T, S: AsRef<str>>(
        &mut self,
        p: &Painter<'_>,
        menus: &[DropMenu<T, S>],
        index: usize,
        keyboard: bool,
    ) {
        self.pending = None;
        let Some(parent) = self.popups.last() else {
            return;
        };
        if self.popups.len() >= MENU_DEPTH {
            return;
        }
        let mut popup = Popup::submenu(p, menus, parent, index, self.bounds);
        if keyboard {
            popup.step(popup.items(menus), 1);
        }
        self.popups.push(popup);
    }

    /// 指针所在的最深弹出层。
    fn layer_at(&self, point: (i32, i32)) -> Option<usize> {
        self.popups
            .iter()
            .rposition(|popup| inside(popup.rect, point))
    }

    fn item<'a, T, S>(
        &self,
        menus: &'a [DropMenu<T, S>],
        layer: usize,
        index: usize,
    ) -> &'a DropItem<T, S> {
        &self.popups[layer].items(menus)[index]
    }

    /// 激活最深层的可用条目。子菜单保留父项高亮，普通命令结束菜单会话。
    fn activate<T: Copy, S: AsRef<str>>(
        &mut self,
        p: &Painter<'_>,
        menus: &[DropMenu<T, S>],
        index: usize,
        keyboard: bool,
    ) -> MenuEvent<T> {
        let popup = self.popups.last_mut().expect("active popup");
        popup.hover = Some(index);
        let item = &popup.items(menus)[index];
        if item.has_submenu() {
            self.expand(p, menus, index, keyboard);
            return MenuEvent::Handled;
        }
        let command = item.command;
        self.close();
        MenuEvent::Command(command)
    }

    /// 左键按下：打开或切换菜单、执行点中的条目、点在菜单外则关闭（这次点击被消费）。
    pub fn press<T: Copy, S: AsRef<str>>(
        &mut self,
        p: &Painter<'_>,
        bar: Rect,
        menus: &[DropMenu<T, S>],
        point: (i32, i32),
    ) -> MenuEvent<T> {
        self.focus = None;
        let titles = title_rects(p, bar, menus);
        if !self.popups.is_empty() {
            if let Some(layer) = self.layer_at(point) {
                let popup = &self.popups[layer];
                if let Some(index) = popup.item_at(popup.items(menus), point) {
                    let item = self.item(menus, layer, index);
                    if item.disabled {
                        return MenuEvent::Handled;
                    }
                    self.popups.truncate(layer + 1);
                    return self.activate(p, menus, index, false);
                }
                return MenuEvent::Handled;
            }
            let current = self.open_menu();
            self.close();
            if let Some(index) = titles.iter().position(|rect| inside(*rect, point))
                && Some(index) != current
            {
                self.open(p, bar, menus, index, false);
            }
            return MenuEvent::Handled;
        }
        match titles.iter().position(|rect| inside(*rect, point)) {
            Some(index) => {
                self.open(p, bar, menus, index, false);
                MenuEvent::Handled
            }
            None => MenuEvent::Ignored,
        }
    }

    /// 指针移动：菜单打开时在标题间切换、更新高亮，并在子菜单父条目上排定延时展开。返回是否需要重画。
    pub fn motion<T, S: AsRef<str>>(
        &mut self,
        p: &Painter<'_>,
        bar: Rect,
        menus: &[DropMenu<T, S>],
        point: (i32, i32),
    ) -> bool {
        let Some(first) = self.popups.first() else {
            return false;
        };
        let titles = title_rects(p, bar, menus);
        if self.layer_at(point).is_none()
            && let Some(index) = titles.iter().position(|rect| inside(*rect, point))
            && index != first.menu
        {
            self.open(p, bar, menus, index, false);
        }
        let Some(layer) = self.layer_at(point) else {
            // 指针离开全部弹出层：最深一层取消高亮，父层保持指向已展开子菜单的条目。
            if let Some(last) = self.popups.last_mut() {
                last.hover = None;
            }
            self.pending = None;
            return true;
        };
        let popup = &self.popups[layer];
        let hover = popup.item_at(popup.items(menus), point);
        // 指针回到某层时更深的孙级菜单全部收起；直接子菜单只在仍指向其父条目时保留。
        self.popups.truncate(layer + 2);
        let child_parent = self
            .popups
            .get(layer + 1)
            .and_then(|child| child.trail.last().copied());
        match hover {
            Some(index) => {
                if child_parent.is_some_and(|parent| parent != index) {
                    self.popups.truncate(layer + 1);
                }
                self.popups[layer].hover = Some(index);
            }
            // 分隔线或边框上：保留指向已展开子菜单的高亮，否则取消高亮。
            None if child_parent.is_none() => self.popups[layer].hover = None,
            None => {}
        }
        match hover {
            Some(index)
                if child_parent != Some(index)
                    && self.item(menus, layer, index).has_submenu()
                    && !self.item(menus, layer, index).disabled =>
            {
                let parent = &self.popups[layer];
                let pending = self
                    .pending
                    .as_ref()
                    .map(|(_, popup)| popup.trail.as_slice());
                let mut trail = parent.trail.clone();
                trail.push(index);
                if pending != Some(trail.as_slice()) && self.popups.len() < MENU_DEPTH {
                    let popup = Popup::submenu(p, menus, parent, index, self.bounds);
                    self.pending = Some((Instant::now() + SHOW_DELAY, popup));
                }
            }
            _ => self.pending = None,
        }
        true
    }

    /// 按键按下。菜单打开时处理方向键与助记键；F10 或 Alt+标题字母可从窗口进入菜单。
    pub fn key<T: Copy, S: AsRef<str>>(
        &mut self,
        p: &Painter<'_>,
        bar: Rect,
        menus: &[DropMenu<T, S>],
        key: &Key,
    ) -> MenuEvent<T> {
        let alt = ALT_KEYS.contains(&key.code);
        self.alt_alone = alt;
        if alt || key.code == TAB {
            self.cues = true;
        }
        let count = menus.len();
        if count == 0 {
            self.close();
            return MenuEvent::Ignored;
        }
        let modifiers = key.modifiers;
        if key.code == F10
            && bar.height > 0
            && !modifiers.shift
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.super_key
        {
            if !key.repeat {
                let active = self.active();
                self.close();
                self.focus = (!active).then_some(0);
                self.cues = true;
            }
            return MenuEvent::Handled;
        }
        let letter = if !key.repeat && !modifiers.control && !modifiers.super_key {
            key.letter()
        } else {
            None
        };
        let depth = self.popups.len();
        if let Some(popup) = self.popups.last_mut() {
            self.pending = None;
            let items = popup.items(menus);
            let hovered = popup.hover.filter(|index| !items[*index].disabled);
            let submenu = hovered.is_some_and(|index| items[index].has_submenu());
            match key.code {
                ESCAPE => {
                    self.popups.pop();
                }
                UP => popup.step(items, -1),
                DOWN => popup.step(items, 1),
                RIGHT if submenu => {
                    self.expand(p, menus, hovered.expect("submenu parent"), true);
                }
                LEFT if depth > 1 => {
                    self.popups.pop();
                }
                LEFT | RIGHT => {
                    // 只有菜单栏的下拉菜单在标题间切换；上下文菜单没有相邻菜单。
                    if count == 1 || bar.height == 0 {
                        return MenuEvent::Handled;
                    }
                    let current = self.popups[0].menu;
                    let menu = if key.code == LEFT {
                        (current + count - 1) % count
                    } else {
                        (current + 1) % count
                    };
                    self.open(p, bar, menus, menu, true);
                }
                ENTER => {
                    if let Some(index) = hovered {
                        return self.activate(p, menus, index, true);
                    }
                }
                _ => {
                    if let Some(letter) = letter {
                        // 从高亮项之后开始查找。多个匹配只移动高亮，用户用 Enter 确认。
                        let start = popup.hover.map_or(0, |index| index + 1);
                        let mut matches = (start..items.len()).chain(0..start).filter(|&index| {
                            let item = &items[index];
                            !item.disabled && mnemonic(item.label.as_ref()) == Some(letter)
                        });
                        if let Some(index) = matches.next() {
                            if matches.next().is_none() {
                                return self.activate(p, menus, index, true);
                            }
                            popup.hover = Some(index);
                        }
                    }
                }
            }
            return MenuEvent::Handled;
        }
        if (modifiers.alt || self.focus.is_some())
            && let Some(letter) = letter
            && let Some(index) = menus
                .iter()
                .position(|menu| mnemonic(menu.title.as_ref()) == Some(letter))
        {
            self.open(p, bar, menus, index, true);
            return MenuEvent::Handled;
        }
        if let Some(index) = self.focus {
            match key.code {
                LEFT => self.focus = Some((index + count - 1) % count),
                RIGHT => self.focus = Some((index + 1) % count),
                ENTER | DOWN => self.open(p, bar, menus, index, true),
                _ => self.focus = None,
            }
            return MenuEvent::Handled;
        }
        MenuEvent::Ignored
    }

    /// 按键释放：单独的 Alt 松开时进入或退出键盘选择。返回是否需要重画。
    pub fn key_released(&mut self, key: &Key) -> bool {
        if ALT_KEYS.contains(&key.code) && std::mem::take(&mut self.alt_alone) {
            self.focus = if self.focus.is_some() || !self.popups.is_empty() {
                None
            } else {
                Some(0)
            };
            self.popups.clear();
            self.pending = None;
            return true;
        }
        false
    }

    /// 画菜单栏与（打开的）下拉菜单；下拉菜单盖在其他内容之上，所以应在一帧的最后调用。
    pub fn paint<T, S: AsRef<str>>(
        &self,
        p: &mut Painter<'_>,
        bar: Rect,
        menus: &[DropMenu<T, S>],
        pointer: (i32, i32),
    ) {
        p.set_cues(self.cues);
        let open = self.open_menu();
        for (index, (menu, rect)) in menus
            .iter()
            .zip(title_rects(p, bar, menus))
            .enumerate()
            .filter(|_| bar.height > 0)
        {
            let hovered = inside(rect, pointer) && open.is_none();
            p.menu_bar_item(
                rect,
                menu.title.as_ref(),
                hovered || open == Some(index) || self.focus == Some(index),
                open == Some(index),
            );
        }
        for popup in &self.popups {
            popup.paint(p, popup.items(menus));
        }
    }
}
