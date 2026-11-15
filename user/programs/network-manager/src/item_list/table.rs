//! 带工具栏、查找与状态栏的详情列表。行序由业务视图提供，选区由控件持有。

use super::{Find, FindAction, Item, Summary, Tool, spacer, toolbar};
use crate::child;
use liteos_ui::{Column, Event, Layout, ListOptions, ListStyle, Widget, WidgetKind};
use std::cell::RefCell;

pub(crate) struct Table {
    pub(crate) pane: Widget,
    _bar: Widget,
    pub(crate) list: Widget,
    pub(crate) summary: Summary,
    find: Find,
    rows: RefCell<Vec<String>>,
    _spacer: Widget,
}

impl Table {
    pub(crate) fn new<const N: usize>(
        tabs: &Widget,
        title: &str,
        columns: &[(&str, i32)],
        commands: [Item; N],
    ) -> liteos_ui::Result<(Self, [Tool; N])> {
        let pane = child(
            tabs,
            WidgetKind::Column,
            title,
            Layout {
                grow: 1,
                gap: 3,
                padding: 3,
                ..Layout::default()
            },
        )?;
        let bar = child(&pane, WidgetKind::Row, "", Layout::default())?;
        let tools = toolbar(&bar, commands)?;
        let space = spacer(&bar)?;
        let find = Find::new(&bar)?;
        let list = child(
            &pane,
            WidgetKind::List,
            "",
            Layout {
                grow: 1,
                ..Layout::default()
            },
        )?;
        list.set_list_style(
            ListStyle::Details,
            ListOptions {
                multiple: false,
                full_row_select: true,
            },
        )?;
        let columns: Vec<_> = columns
            .iter()
            .enumerate()
            .map(|(id, &(title, width))| Column {
                id: id as u64 + 1,
                title,
                width,
            })
            .collect();
        list.set_columns(&columns)?;
        let summary = Summary::new(&pane)?;
        Ok((
            Self {
                pane,
                _bar: bar,
                list,
                find,
                summary,
                rows: RefCell::default(),
                _spacer: space,
            },
            tools,
        ))
    }

    pub(crate) fn selection(&self) -> liteos_ui::Result<Option<usize>> {
        Ok(self
            .list
            .selection()?
            .and_then(|id| id.checked_sub(1))
            .map(|id| id as usize))
    }

    pub(crate) fn selection_changed(&self) -> liteos_ui::Result<()> {
        self.summary.show(
            self.rows.borrow().len(),
            usize::from(self.selection()?.is_some()),
            "",
        )
    }

    pub(crate) fn replace(
        &self,
        rows: Vec<String>,
        selected: Option<usize>,
    ) -> liteos_ui::Result<()> {
        let mut current = self.rows.borrow_mut();
        if rows != *current {
            let visible: Vec<_> = rows
                .iter()
                .enumerate()
                .map(|(i, row)| (i as u64 + 1, row.as_str()))
                .collect();
            self.list.set_rows(&visible)?;
            self.find.present(&self.list)?;
            *current = rows;
        }
        drop(current);
        if self.selection()? != selected {
            self.list.select(selected.map(|index| index as u64 + 1))?;
        }
        self.selection_changed()
    }

    /// 更新或关闭共用查找。命中独立于条目选择。
    pub(crate) fn handle_find(&self, event: &Event) -> liteos_ui::Result<()> {
        match self.find.handle(event)? {
            Some(FindAction::Close) => self.clear_find(),
            Some(FindAction::Find(scan)) => self.find.scan_rows(
                &self.list,
                self.rows
                    .borrow()
                    .iter()
                    .enumerate()
                    .map(|(index, row)| (index as u64 + 1, row.as_str())),
                scan,
            ),
            None => Ok(()),
        }
    }

    /// 清除上一数据范围的查找位置。
    pub(crate) fn clear_find(&self) -> liteos_ui::Result<()> {
        self.find.locate(&self.list, None)
    }
}
