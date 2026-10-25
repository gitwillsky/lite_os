//! 窗口内的菜单栏状态机：鼠标打开与切换、Alt 激活、方向键导航、Alt+字母直达，以及助记键下划线的显示时机
//! （Windows 2000 在按过 Alt 或 Tab 之前隐藏下划线，见 `interaction.md` N1/N2、K11）。命令类型由调用者决定。

use canvas::Rect;

use crate::{DropMenu, Key, Painter, Popup, title_rects};

const ESCAPE: u32 = 1;
const ENTER: u32 = 28;
const TAB: u32 = 15;
const UP: u32 = 103;
const LEFT: u32 = 105;
const RIGHT: u32 = 106;
const DOWN: u32 = 108;
/// 左、右 Alt 的 evdev 键码。
const ALT_KEYS: [u32; 2] = [56, 100];

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
    popup: Option<Popup>,
    /// 键盘选中的菜单栏标题（单按 Alt 进入）。
    focus: Option<usize>,
    /// 是否显示助记键下划线：按过 Alt 或 Tab 之后一直显示。
    cues: bool,
    /// Alt 按下后还没有别的按键：此时松开 Alt 才算"单独的 Alt"。
    alt_alone: bool,
}

/// 菜单实际绘制状态的无分配投影；仅用于 owner 比较输入前后是否需要重绘。
#[derive(PartialEq, Eq)]
pub struct MenuVisual {
    popup: Option<(usize, Rect, Option<usize>)>,
    focus: Option<usize>,
    cues: bool,
}

/// 标题 `(X)` 里的助记字母，小写。
fn mnemonic(title: &str) -> Option<char> {
    let (_, rest) = title.split_once('(')?;
    let letter = rest.chars().next()?;
    (rest.chars().nth(1) == Some(')') && letter.is_ascii_alphabetic())
        .then(|| letter.to_ascii_lowercase())
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
        MenuVisual {
            popup: self
                .popup
                .as_ref()
                .map(|popup| (popup.menu, popup.rect, popup.hover)),
            focus: self.focus,
            cues: self.cues,
        }
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
        self.popup = Some(popup);
        self.focus = None;
    }
    /// 菜单是否正占着输入（下拉菜单打开或在键盘选择标题）：此时调用者不应再把键盘交给别处。
    pub fn active(&self) -> bool {
        self.popup.is_some() || self.focus.is_some()
    }

    /// 当前是否显示键盘助记下划线，供同一窗口内的其他标准控件使用。
    pub fn cues_visible(&self) -> bool {
        self.cues
    }

    /// 打开菜单后第几个菜单；用来给标题画"按下"状态。
    pub fn open_menu(&self) -> Option<usize> {
        self.popup.as_ref().map(|popup| popup.menu)
    }

    /// 强制关闭下拉菜单与键盘选择（例如弹出模态对话框时）。
    pub fn close(&mut self) {
        self.popup = None;
        self.focus = None;
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
        self.popup = Some(popup);
        self.focus = None;
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
        if let Some(popup) = self.popup.take() {
            if let Some(item) = popup.item_at(&menus[popup.menu].items, point) {
                if menus[popup.menu].items[item].disabled {
                    self.popup = Some(popup);
                    return MenuEvent::Handled;
                }
                return MenuEvent::Command(menus[popup.menu].items[item].command);
            }
            if inside(popup.rect, point) {
                self.popup = Some(popup);
            } else if let Some(index) = titles.iter().position(|rect| inside(*rect, point))
                && index != popup.menu
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

    /// 指针移动：菜单打开时在标题间切换并更新高亮。返回是否需要重画。
    pub fn motion<T, S: AsRef<str>>(
        &mut self,
        p: &Painter<'_>,
        bar: Rect,
        menus: &[DropMenu<T, S>],
        point: (i32, i32),
    ) -> bool {
        let Some(popup) = &self.popup else {
            return false;
        };
        let titles = title_rects(p, bar, menus);
        if let Some(index) = titles.iter().position(|rect| inside(*rect, point))
            && index != popup.menu
        {
            self.open(p, bar, menus, index, false);
        }
        if let Some(popup) = &mut self.popup {
            popup.hover = popup.item_at(&menus[popup.menu].items, point);
        }
        true
    }

    /// 按键按下。菜单打开或键盘选择期间吞掉全部按键；否则只在 Alt+字母命中标题时处理。
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
        if let Some(popup) = &mut self.popup {
            let items = &menus[popup.menu].items;
            match key.code {
                ESCAPE => self.popup = None,
                UP => popup.step(items, -1),
                DOWN => popup.step(items, 1),
                LEFT | RIGHT => {
                    if count == 1 {
                        return MenuEvent::Handled;
                    }
                    let menu = if key.code == LEFT {
                        (popup.menu + count - 1) % count
                    } else {
                        (popup.menu + 1) % count
                    };
                    self.open(p, bar, menus, menu, true);
                }
                ENTER => {
                    if let Some(index) = popup.hover.filter(|index| !items[*index].disabled) {
                        let command = items[index].command;
                        self.popup = None;
                        return MenuEvent::Command(command);
                    }
                }
                _ => {}
            }
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
        if key.modifiers.alt
            && let Some(letter) = key.letter()
            && let Some(index) = menus
                .iter()
                .position(|menu| mnemonic(menu.title.as_ref()) == Some(letter))
        {
            self.open(p, bar, menus, index, true);
            return MenuEvent::Handled;
        }
        MenuEvent::Ignored
    }

    /// 按键释放：单独的 Alt 松开时进入或退出键盘选择。返回是否需要重画。
    pub fn key_released(&mut self, key: &Key) -> bool {
        if ALT_KEYS.contains(&key.code) && std::mem::take(&mut self.alt_alone) {
            self.focus = if self.focus.is_some() || self.popup.is_some() {
                None
            } else {
                Some(0)
            };
            self.popup = None;
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
        if let Some(popup) = &self.popup {
            popup.paint(p, &menus[popup.menu].items);
        }
    }
}
