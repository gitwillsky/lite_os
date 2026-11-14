//! 路由设置中列表的增删改、取消和可选排序交互。

use super::{ROWS, button, disabled, form, form_buttons, grow, row, selection};
use crate::child;
use liteos_ui::{
    Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, WidgetKind, Window, state,
};

pub(super) struct ListLayout<'a> {
    pub title: &'a str,
    pub columns: &'a [Column<'a>],
    pub ordered: bool,
    // 规则按位置执行并分别计数，内容相同的规则仍是不同的行。
    pub allow_duplicates: bool,
}

pub(super) async fn edit<T: PartialEq>(
    dialogs: &Dialogs,
    parent: &Window,
    layout: ListLayout<'_>,
    initial: impl IntoIterator<Item = T>,
    label: impl Fn(&T) -> String,
    edit_item: impl for<'a> AsyncFn(&'a Window, &'a [T], Option<usize>) -> liteos_ui::Result<Option<T>>,
) -> liteos_ui::Result<Option<Vec<T>>> {
    let (window, mut events, root) = form(dialogs, parent, layout.title, 585, 367)?;
    let list = child(&root, WidgetKind::List, "", grow())?;
    list.set_list_style(
        ListStyle::Details,
        ListOptions {
            multiple: false,
            full_row_select: true,
        },
    )?;
    list.set_columns(layout.columns)?;
    let pager = child(&root, WidgetKind::Row, "", row())?;
    let previous = child(&pager, WidgetKind::Button, "上一页", button())?;
    let next = child(&pager, WidgetKind::Button, "下一页", button())?;
    let position = child(&pager, WidgetKind::Label, "", grow())?;
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let add = child(&bar, WidgetKind::Button, "新增…", button())?;
    let edit = child(&bar, WidgetKind::Button, "编辑…", button())?;
    let remove = child(&bar, WidgetKind::Button, "删除", button())?;
    let order = if layout.ordered {
        Some((
            child(&bar, WidgetKind::Button, "上移", button())?,
            child(&bar, WidgetKind::Button, "下移", button())?,
        ))
    } else {
        None
    };
    let status = child(&root, WidgetKind::Label, "", Layout::default())?;
    let (save, cancel) = form_buttons(&root)?;
    let mut values: Vec<_> = initial.into_iter().collect();
    let mut selected = None;
    let mut first = 0;

    'refresh: loop {
        first = first.min(values.len().saturating_sub(1) / ROWS * ROWS);
        let end = (first + ROWS).min(values.len());
        let labels: Vec<_> = values[first..end].iter().map(&label).collect();
        let rows: Vec<_> = labels
            .iter()
            .enumerate()
            .map(|(index, label)| ((first + index) as u64 + 1, label.as_str()))
            .collect();
        list.set_rows(&rows)?;
        list.select(selected.map(|index: usize| index as u64 + 1))?;
        pager.set_state(if values.len() > ROWS {
            0
        } else {
            state::HIDDEN
        })?;
        previous.set_state(disabled(first == 0))?;
        next.set_state(disabled(end == values.len()))?;
        position.set_text(&format!(
            "显示 {}–{} 项，共 {} 项",
            first + 1,
            end,
            values.len()
        ))?;
        loop {
            edit.set_state(disabled(selected.is_none()))?;
            remove.set_state(disabled(selected.is_none()))?;
            if let Some((up, down)) = &order {
                up.set_state(disabled(selected.is_none_or(|index| index == 0)))?;
                down.set_state(disabled(
                    selected.is_none_or(|index| index + 1 == values.len()),
                ))?;
            }
            let Some(event) = events.next().await? else {
                return Ok(None);
            };
            match event.kind {
                EventKind::Close
                | EventKind::Key {
                    code: 1,
                    pressed: true,
                    ..
                } => return Ok(None),
                EventKind::Activate { .. } if event.node == cancel.id() => return Ok(None),
                EventKind::Activate { .. } if event.node == save.id() => return Ok(Some(values)),
                EventKind::Select { .. } if event.node == list.id() => selected = selection(&list)?,
                EventKind::Activate { .. } if event.node == previous.id() && first != 0 => {
                    first -= ROWS;
                    selected = None;
                    continue 'refresh;
                }
                EventKind::Activate { .. } if event.node == next.id() && end < values.len() => {
                    first = end;
                    selected = None;
                    continue 'refresh;
                }
                EventKind::Activate { .. } if event.node == remove.id() => {
                    if let Some(index) = selected.take() {
                        values.remove(index);
                        status.set_text("")?;
                        continue 'refresh;
                    }
                }
                EventKind::Activate { .. }
                    if event.node == add.id()
                        || (event.node == edit.id() && selected.is_some()) =>
                {
                    let old = if event.node == edit.id() {
                        selected
                    } else {
                        None
                    };
                    let Some(value) = edit_item(&window, &values, old).await? else {
                        continue;
                    };
                    if !layout.allow_duplicates
                        && values
                            .iter()
                            .enumerate()
                            .any(|(index, item)| Some(index) != old && *item == value)
                    {
                        status.set_text("已有相同项目。")?;
                        continue;
                    }
                    if let Some(index) = old {
                        values[index] = value;
                    } else {
                        let index = selected
                            .filter(|_| layout.ordered)
                            .map_or(values.len(), |index| index + 1);
                        values.insert(index, value);
                        selected = Some(index);
                        first = index / ROWS * ROWS;
                    }
                    status.set_text("")?;
                    continue 'refresh;
                }
                EventKind::Activate { .. } => {
                    if let (Some(index), Some((up, down))) = (selected, &order) {
                        let next = if event.node == up.id() {
                            index.checked_sub(1)
                        } else if event.node == down.id() && index + 1 < values.len() {
                            Some(index + 1)
                        } else {
                            None
                        };
                        if let Some(next) = next {
                            values.swap(index, next);
                            selected = Some(next);
                            first = next / ROWS * ROWS;
                            continue 'refresh;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}
