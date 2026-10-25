//! 窗口内的菜单栏与下拉菜单：标题排布、下拉框的几何、命中与绘制；命令类型由调用者决定。

use canvas::Rect;

use crate::{MenuItemState, Painter};

const ITEM_HEIGHT: i32 = 20;
const SEPARATOR_HEIGHT: i32 = 8;

/// 下拉菜单的一个条目。
#[derive(PartialEq, Eq)]
pub struct DropItem<T, S = &'static str> {
    pub label: S,
    pub shortcut: S,
    pub command: T,
    pub checked: bool,
    /// 禁用条目保留可见外观，但不参与键盘导航或命令激活。
    pub disabled: bool,
    /// 此条目上方带分隔线。
    pub separator_before: bool,
}

impl<T, S: AsRef<str>> DropItem<T, S> {
    pub fn new(label: S, shortcut: S, command: T) -> Self {
        Self {
            label,
            shortcut,
            command,
            checked: false,
            disabled: false,
            separator_before: false,
        }
    }

    /// 在条目上方加分隔线。
    pub fn separated(mut self) -> Self {
        self.separator_before = true;
        self
    }

    /// 设置勾选标记。
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
}

/// 一个顶层菜单：标题与条目。
#[derive(PartialEq, Eq)]
pub struct DropMenu<T, S = &'static str> {
    pub title: S,
    pub items: Vec<DropItem<T, S>>,
}

fn logical_width(p: &Painter<'_>, text: &str) -> i32 {
    (p.text_width(text, false) + p.scale() - 1) / p.scale()
}

/// 菜单栏各标题的矩形，沿 `bar` 从左到右。
pub fn title_rects<T, S: AsRef<str>>(
    p: &Painter<'_>,
    bar: Rect,
    menus: &[DropMenu<T, S>],
) -> Vec<Rect> {
    let mut x = bar.x + 2;
    menus
        .iter()
        .map(|menu| {
            let width = logical_width(p, menu.title.as_ref()) + 16;
            let rect = Rect::new(x, bar.y, width, bar.height);
            x += width;
            rect
        })
        .collect()
}

/// 一个打开的下拉菜单。
pub struct Popup {
    pub menu: usize,
    pub rect: Rect,
    pub hover: Option<usize>,
}

impl Popup {
    /// 在标题矩形 `title` 的下方打开第 `menu` 个菜单。
    pub fn open<T, S: AsRef<str>>(
        p: &Painter<'_>,
        menus: &[DropMenu<T, S>],
        menu: usize,
        title: Rect,
    ) -> Self {
        let items = &menus[menu].items;
        let width = p.menu_width(
            items
                .iter()
                .map(|item| (item.label.as_ref(), item.shortcut.as_ref())),
        );
        let height = 4 + items
            .iter()
            .map(|item| {
                ITEM_HEIGHT
                    + if item.separator_before {
                        SEPARATOR_HEIGHT
                    } else {
                        0
                    }
            })
            .sum::<i32>();
        Self {
            menu,
            rect: Rect::new(title.x, title.bottom(), width, height),
            hover: None,
        }
    }

    /// 各条目的行矩形（不含分隔线）。
    fn rows<T, S: AsRef<str>>(&self, items: &[DropItem<T, S>]) -> Vec<Rect> {
        let mut y = self.rect.y + 2;
        items
            .iter()
            .map(|item| {
                if item.separator_before {
                    y += SEPARATOR_HEIGHT;
                }
                let row = Rect::new(self.rect.x + 2, y, self.rect.width - 4, ITEM_HEIGHT);
                y += ITEM_HEIGHT;
                row
            })
            .collect()
    }

    /// 点在哪个条目上。
    pub fn item_at<T, S: AsRef<str>>(
        &self,
        items: &[DropItem<T, S>],
        point: (i32, i32),
    ) -> Option<usize> {
        self.rows(items).iter().position(|row| {
            point.0 >= row.x && point.0 < row.right() && point.1 >= row.y && point.1 < row.bottom()
        })
    }

    /// 方向键移动高亮；`step` 为 +1 或 -1，首尾相接。
    pub fn step<T, S: AsRef<str>>(&mut self, items: &[DropItem<T, S>], step: isize) {
        if items.is_empty() {
            self.hover = None;
            return;
        }
        let count = items.len() as isize;
        let mut position = self
            .hover
            .map_or(if step > 0 { -1 } else { count }, |index| index as isize);
        for _ in 0..items.len() {
            position = (position + step).rem_euclid(count);
            if !items[position as usize].disabled {
                self.hover = Some(position as usize);
                return;
            }
        }
        self.hover = None;
    }

    pub fn paint<T, S: AsRef<str>>(&self, p: &mut Painter<'_>, items: &[DropItem<T, S>]) {
        let inner = p.menu_popup(self.rect);
        for (index, (item, row)) in items.iter().zip(self.rows(items)).enumerate() {
            if item.separator_before {
                p.menu_separator(Rect::new(
                    inner.x,
                    row.y - SEPARATOR_HEIGHT,
                    inner.width,
                    SEPARATOR_HEIGHT,
                ));
            }
            p.menu_item(
                row,
                item.label.as_ref(),
                item.shortcut.as_ref(),
                MenuItemState {
                    highlighted: self.hover == Some(index),
                    checked: item.checked,
                    disabled: item.disabled,
                    ..Default::default()
                },
            );
        }
    }
}
