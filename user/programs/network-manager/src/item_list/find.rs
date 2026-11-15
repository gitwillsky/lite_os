//! WinBox 工具栏的查找交互。匹配的位置独立于用于编辑和删除的列表选择。

use super::{Item, Tool, toolbar};
use crate::child;
use liteos_ui::{EditCommand, Event, EventKind, Layout, ListMatch, Widget, WidgetKind, state};
use std::cell::Cell;

#[derive(Clone, Copy)]
pub(in crate::item_list) enum Direction {
    First,
    Next,
    Previous,
}

pub(crate) enum FindAction {
    Find(Scan<()>),
    Close,
}

pub(crate) struct Find {
    entry: Tool,
    panel: Widget,
    input: Widget,
    close: Widget,
    next: Widget,
    previous: Widget,
    current: Cell<Option<ListMatch>>,
}

impl Find {
    /// 在 `bar` 右侧建立可切换的查找入口和输入行。
    ///
    /// 错误：窗口已关闭或控件拒绝初始化。
    pub(crate) fn new(bar: &Widget) -> liteos_ui::Result<Self> {
        let [entry] = toolbar(bar, [Item::Text("查找")])?;
        let panel = child(
            bar,
            WidgetKind::Row,
            "",
            Layout {
                gap: 0,
                ..Layout::default()
            },
        )?;
        let layout = |width| Layout {
            width,
            height: 23,
            ..Layout::default()
        };
        let close = child(&panel, WidgetKind::Button, "×", layout(24))?;
        let input = child(&panel, WidgetKind::Edit, "", layout(124))?;
        let next = child(&panel, WidgetKind::Button, "▼", layout(24))?;
        let previous = child(&panel, WidgetKind::Button, "▲", layout(24))?;
        panel.set_state(state::HIDDEN)?;
        Ok(Self {
            entry,
            panel,
            input,
            close,
            next,
            previous,
            current: Cell::new(None),
        })
    }

    /// 数据暂不可用时禁用查找入口和输入行；保留当前可见性。
    ///
    /// 错误：查找控件已关闭。
    pub(crate) fn set_enabled(&self, enabled: bool) -> liteos_ui::Result<()> {
        self.entry.set_enabled(enabled)?;
        let state = self.panel.state()? & !state::DISABLED;
        self.panel
            .set_state(state | if enabled { 0 } else { state::DISABLED })
    }

    /// 将属于查找控件的交互转换为新的扫描或关闭动作。
    ///
    /// 错误：窗口已关闭或查询输入无法读取。
    pub(crate) fn handle(&self, event: &Event) -> liteos_ui::Result<Option<FindAction>> {
        if self.entry.activated(event) {
            self.panel.set_state(0)?;
            self.entry.bar.borrow().widget.set_state(state::HIDDEN)?;
            self.input.focus()?;
            self.input.edit(EditCommand::SelectAll)?;
            return self.request(Direction::First).map(Some);
        }
        let direction = match event.kind {
            EventKind::Activate { .. } if event.node == self.close.id() => {
                return self.close().map(Some);
            }
            EventKind::Activate { .. } if event.node == self.next.id() => Direction::Next,
            EventKind::Activate { .. } if event.node == self.previous.id() => Direction::Previous,
            EventKind::Changed { .. } if event.node == self.input.id() => {
                return self.request(Direction::First).map(Some);
            }
            EventKind::Key {
                code,
                pressed: true,
                ..
            } if event.node == self.input.id() => match code {
                1 => return self.close().map(Some), // Escape
                28 | 108 => Direction::Next,        // Enter, Down
                103 => Direction::Previous,         // Up
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        self.input.focus()?;
        self.request(direction).map(Some)
    }

    fn close(&self) -> liteos_ui::Result<FindAction> {
        self.panel.set_state(state::HIDDEN)?;
        self.entry.bar.borrow().widget.set_state(0)?;
        self.current.set(None);
        Ok(FindAction::Close)
    }

    fn request(&self, direction: Direction) -> liteos_ui::Result<FindAction> {
        if matches!(direction, Direction::First) {
            self.current.set(None);
        }
        Ok(FindAction::Find(Scan::new(
            self.input.text()?,
            direction,
            self.current.get(),
        )))
    }

    /// 新的数据范围从其首条匹配开始，收起时返回空查询。
    ///
    /// 错误：查找控件已关闭。
    pub(crate) fn restart(&self) -> liteos_ui::Result<Scan<()>> {
        self.current.set(None);
        Ok(Scan::new(self.query()?, Direction::First, None))
    }

    /// 列表行刷新后复用当前查找投影。收起时只隐藏匹配，输入仍可在重新打开后继续使用。
    ///
    /// 错误：窗口关闭或列表未设置为整行选择的详细列表。
    pub(crate) fn present(&self, list: &Widget) -> liteos_ui::Result<()> {
        list.set_list_find(&self.query()?, self.current.get())
    }

    /// 更新当前匹配并定位。None 只保留其他浅色命中。
    ///
    /// 错误：同 `present`。
    pub(crate) fn locate(
        &self,
        list: &Widget,
        position: Option<ListMatch>,
    ) -> liteos_ui::Result<()> {
        self.current.set(position);
        self.present(list)
    }

    /// 顺序扫描一页可见行并定位命中；操作选区保持独立。
    ///
    /// 错误：同 `present`。
    pub(crate) fn scan_rows<'a>(
        &self,
        list: &Widget,
        rows: impl Iterator<Item = (u64, &'a str)>,
        mut scan: Scan<()>,
    ) -> liteos_ui::Result<()> {
        for (row, text) in rows {
            if scan.visit(row, text, ()) {
                break;
            }
        }
        self.locate(list, scan.found().map(|(position, ())| position))
    }

    fn query(&self) -> liteos_ui::Result<String> {
        if self.panel.state()? & state::HIDDEN == 0 {
            self.input.text()
        } else {
            Ok(String::new())
        }
    }
}

/// 按列表行标识递增、单元格顺序和字节位置扫描。调用者提供行的显示文本与来源，
/// 如当前路由表或连接页序号。只保留当前方向的候选及回环候选。
pub(crate) struct Scan<T> {
    needle: String,
    direction: Direction,
    origin: Option<ListMatch>,
    hit: Option<(ListMatch, T)>,
    wrap: Option<(ListMatch, T)>,
}

impl<T: Copy> Scan<T> {
    fn new(needle: String, direction: Direction, origin: Option<ListMatch>) -> Self {
        Self {
            needle,
            direction,
            origin,
            hit: None,
            wrap: None,
        }
    }

    /// 切换来源类型，供分页调用者记录命中所在页。
    pub(crate) fn sources<U: Copy>(self) -> Scan<U> {
        Scan::new(self.needle, self.direction, self.origin)
    }

    /// 页内命中若在扫描方向上更靠近当前位置，可直接定位，无需读其它页。
    pub(crate) fn on_page(
        &self,
        after: u64,
        rows: impl Iterator<Item = (u64, impl AsRef<str>)>,
    ) -> Option<ListMatch> {
        if after != 0 && self.origin.is_none_or(|origin| origin.row <= after) {
            return None;
        }
        let mut page = Scan::new(self.needle.clone(), self.direction, self.origin);
        for (row, text) in rows {
            if page.visit(row, text.as_ref(), ()) {
                break;
            }
        }
        page.hit.map(|(position, ())| position)
    }

    /// 空查询不用扫描数据。
    pub(crate) fn is_empty(&self) -> bool {
        self.needle.is_empty()
    }

    /// 读取一行的各个匹配。true 表示已经找到确定命中，无需继续读取后面的行。
    pub(crate) fn visit(&mut self, row: u64, text: &str, source: T) -> bool {
        if self.is_empty() {
            return true;
        }
        for (column, cell) in text.split('\t').enumerate() {
            for (offset, _) in cell.match_indices(&self.needle) {
                let position = ListMatch {
                    row,
                    column: column as u32,
                    offset: offset as u32,
                };
                let occurrence = (position, source);
                match self.direction {
                    Direction::First | Direction::Next => {
                        if self.origin.is_none_or(|origin| position > origin) {
                            self.hit = Some(occurrence);
                            return true;
                        }
                        self.wrap.get_or_insert(occurrence);
                    }
                    Direction::Previous => {
                        if self.origin.is_none_or(|origin| position < origin) {
                            self.hit = Some(occurrence);
                        } else if self.hit.is_some() {
                            return true;
                        }
                        self.wrap = Some(occurrence);
                    }
                }
            }
        }
        false
    }

    /// 扫描结束后取当前方向的首个合适匹配，无后续时回环。
    pub(crate) fn found(self) -> Option<(ListMatch, T)> {
        self.hit.or(self.wrap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_visits_each_occurrence_in_display_order_and_wraps_between_pages() {
        let rows = [(4, "前缀10\t10/10"), (71, "目的\t10")];
        let lookup = |direction, origin| {
            let mut scan = Scan::new("10".into(), direction, origin);
            for (page, (row, text)) in rows.into_iter().enumerate() {
                if scan.visit(row, text, page) {
                    break;
                }
            }
            scan.found().unwrap()
        };
        let mark = |row, column, offset| ListMatch {
            row,
            column,
            offset,
        };
        let first = mark(4, 0, 6);
        let second = mark(4, 1, 0);
        let third = mark(4, 1, 3);
        let last = mark(71, 1, 0);
        assert_eq!(lookup(Direction::First, None), (first, 0));
        assert_eq!(lookup(Direction::Next, Some(first)), (second, 0));
        assert_eq!(lookup(Direction::Next, Some(second)), (third, 0));
        assert_eq!(lookup(Direction::Previous, Some(last)), (third, 0));
        assert_eq!(lookup(Direction::Previous, Some(first)), (last, 1));
        assert_eq!(lookup(Direction::Next, Some(last)), (first, 0));
    }
}
