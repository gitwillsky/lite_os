//! 窗口内的菜单栏与下拉菜单：标题排布、下拉框的几何、命中与绘制；命令类型由调用者决定。

use canvas::Rect;

use crate::{MenuItemState, Painter};

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
    /// 默认项（双击等价的命令），标签粗体显示。
    pub default: bool,
    /// 级联子菜单的条目；非空时本条目只打开子菜单，不产生命令。
    pub children: Vec<DropItem<T, S>>,
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
            default: false,
            children: Vec::new(),
        }
    }

    /// 是否为级联子菜单的父条目。
    pub fn has_submenu(&self) -> bool {
        !self.children.is_empty()
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
    p.measure_role(crate::FontRole::Menu, text, false)
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

/// 一个打开的下拉菜单或级联子菜单。
pub struct Popup {
    pub menu: usize,
    /// 从顶层菜单条目到本子菜单父条目的索引路径；顶层下拉菜单为空。
    pub trail: Vec<usize>,
    pub rect: Rect,
    pub hover: Option<usize>,
    /// 打开时由菜单字体与主题度量算出的项高与分隔条高；行几何、命中与绘制共用。
    item_height: i32,
    separator_height: i32,
}

/// 项高与分隔条高，规则见 [`crate::MenuMetrics`]。
fn item_metrics(p: &Painter<'_>) -> (i32, i32) {
    let metrics = crate::MenuMetrics::new(p.theme(), p.fonts(), p.scale());
    (metrics.item_height(), metrics.separator_height())
}

/// 由索引路径取子菜单条目；路径失效（菜单已被替换）时为空。
pub fn submenu_items<'a, T, S>(
    items: &'a [DropItem<T, S>],
    trail: &[usize],
) -> &'a [DropItem<T, S>] {
    trail
        .iter()
        .try_fold(items, |items, index| {
            Some(items.get(*index)?.children.as_slice())
        })
        .unwrap_or(&[])
}

impl Popup {
    /// 本弹出层的条目。
    pub fn items<'a, T, S>(&self, menus: &'a [DropMenu<T, S>]) -> &'a [DropItem<T, S>] {
        menus
            .get(self.menu)
            .map_or(&[], |menu| submenu_items(&menu.items, &self.trail))
    }

    /// 在标题矩形 `title` 的下方打开第 `menu` 个菜单。
    pub fn open<T, S: AsRef<str>>(
        p: &Painter<'_>,
        menus: &[DropMenu<T, S>],
        menu: usize,
        title: Rect,
    ) -> Self {
        let mut popup = Self::sized(p, &menus[menu].items, menu, Vec::new());
        popup.rect.x = title.x;
        popup.rect.y = title.bottom();
        popup
    }

    /// 打开 `parent` 第 `index` 项的级联子菜单：与父条目顶端对齐、压住父菜单右边框 3 像素；
    /// 右侧超出 `bounds` 时翻到父菜单左侧，下方超出时上移（USER32 的子菜单定位）。
    pub fn submenu<T, S: AsRef<str>>(
        p: &Painter<'_>,
        menus: &[DropMenu<T, S>],
        parent: &Popup,
        index: usize,
        bounds: Option<Rect>,
    ) -> Self {
        let mut trail = parent.trail.clone();
        trail.push(index);
        let items = submenu_items(&menus[parent.menu].items, &trail);
        let mut popup = Self::sized(p, items, parent.menu, trail);
        let row = parent.rows(parent.items(menus))[index];
        popup.rect.x = parent.rect.right() - 3;
        popup.rect.y = row.y - 2;
        if let Some(bounds) = bounds {
            if popup.rect.right() > bounds.right() {
                popup.rect.x = parent.rect.x - popup.rect.width + 3;
            }
            popup.rect.x = popup.rect.x.max(bounds.x);
            popup.rect.y = popup
                .rect
                .y
                .min(bounds.bottom() - popup.rect.height)
                .max(bounds.y);
        }
        popup
    }

    fn sized<T, S: AsRef<str>>(
        p: &Painter<'_>,
        items: &[DropItem<T, S>],
        menu: usize,
        trail: Vec<usize>,
    ) -> Self {
        let width = p.menu_width(
            items
                .iter()
                .map(|item| (item.label.as_ref(), item.shortcut.as_ref(), item.default)),
        );
        let (item_height, separator_height) = item_metrics(p);
        let height = 2 * crate::MenuMetrics::MARGIN
            + items
                .iter()
                .map(|item| {
                    item_height
                        + if item.separator_before {
                            separator_height
                        } else {
                            0
                        }
                })
                .sum::<i32>();
        Self {
            menu,
            trail,
            rect: Rect::new(0, 0, width, height),
            hover: None,
            item_height,
            separator_height,
        }
    }

    /// 各条目的行矩形（不含分隔线）。
    pub fn rows<T, S: AsRef<str>>(&self, items: &[DropItem<T, S>]) -> Vec<Rect> {
        let mut y = self.rect.y + crate::MenuMetrics::MARGIN;
        items
            .iter()
            .map(|item| {
                if item.separator_before {
                    y += self.separator_height;
                }
                let row = Rect::new(
                    self.rect.x + crate::MenuMetrics::MARGIN,
                    y,
                    self.rect.width - 2 * crate::MenuMetrics::MARGIN,
                    self.item_height,
                );
                y += self.item_height;
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
                    row.y - self.separator_height,
                    inner.width,
                    self.separator_height,
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
                    has_submenu: item.has_submenu(),
                    default: item.default,
                },
            );
        }
    }
}
