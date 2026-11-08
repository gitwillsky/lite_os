//! 标准控件激活、焦点与选择；领域动作只在输入分发结束后由 ABI 事件交给应用。
use super::widgets::Tree;
use canvas::Rect;
use classic_ui::{EventKind, ITEM_DOUBLE_CLICK, Key};
use classic_ui_abi::{self as abi, event, state, widget};
/// 可发起文件拖放的左键按下。保存行身份，列表在按住期间刷新也不会拖错行。
pub struct FilePress {
    node: u64,
    row: u64,
    point: (i32, i32),
    serial: u32,
}

impl super::widgets::Node {
    fn list_row_at(&self, point: (i32, i32), probe: &classic_ui::Painter<'_>) -> usize {
        let geometry = self.list_geometry();
        if let Some(grid) = geometry.grid {
            // 图标视图只在图标或标签上命中（comctl32 LVHT_ONITEMICON/LABEL）；格子空白与无条目处
            // 按单击背景处理：清除选择。行数作为越界下标交给 choose_row 走同一清除路径。
            let miss = self.row_count();
            grid.at(point, self.position.position)
                .filter(|index| {
                    grid.mode != 1
                        || self.row_at(*index).is_some_and(|(row, text)| {
                            let cell = grid.cell(*index, self.position.position);
                            let size = self
                                .row_icons
                                .get(row)
                                .and_then(|icons| icons.1.as_ref())
                                .map_or(32, |icon| icon.height());
                            let icon =
                                Rect::new(cell.x + (cell.width - size) / 2, cell.y + 4, size, size);
                            let text = text.split('\t').next().unwrap_or("");
                            let full = self.selected == Some(*row);
                            let label = probe.icon_label(cell, size, text, full).rect;
                            [icon, label].iter().any(|rect| {
                                (rect.x..rect.right()).contains(&point.0)
                                    && (rect.y..rect.bottom()).contains(&point.1)
                            })
                        })
                })
                .unwrap_or(miss)
        } else {
            self.position.position as usize
                + ((point.1 - geometry.rows.y).max(0) / self.row_height) as usize
        }
    }
}
impl Tree {
    fn file_drag_input(
        &mut self,
        window: u64,
        input: &EventKind,
        probe: &classic_ui::Painter<'_>,
        out: &mut Vec<abi::Event>,
    ) -> bool {
        match input {
            EventKind::Button {
                code: 0x110,
                pressed: true,
                serial,
            } => {
                self.file_press = self.hit().filter(|id| self.active(*id)).and_then(|id| {
                    let node = self.nodes.get(&id)?;
                    if node.kind != widget::LIST || node.state & state::FILE_DRAG == 0 {
                        return None;
                    }
                    let rows = node.list_geometry().rows;
                    let (x, y) = self.pointer;
                    if !(rows.x..rows.right()).contains(&x) || !(rows.y..rows.bottom()).contains(&y)
                    {
                        return None;
                    }
                    let row = node.row_at(node.list_row_at(self.pointer, probe))?.0;
                    Some(FilePress {
                        node: id,
                        row,
                        point: self.pointer,
                        serial: *serial,
                    })
                });
            }
            EventKind::PointerMove { .. }
                if self.file_press.as_ref().is_some_and(|press| {
                    (self.pointer.0 - press.point.0).abs() > 4
                        || (self.pointer.1 - press.point.1).abs() > 4
                }) =>
            {
                let press = self.file_press.take().expect("checked drag gesture");
                if !self.active(press.node) {
                    return false;
                }
                let Some(node) = self.nodes.get_mut(&press.node) else {
                    return false;
                };
                let Some(index) = (0..node.row_count())
                    .find(|index| node.row_at(*index).is_some_and(|row| row.0 == press.row))
                else {
                    return false;
                };
                if !node.row_selected(press.row) {
                    node.choose_row(index, false, false, false);
                }
                if let Some(table) = &mut node.table {
                    table.last_click = None;
                }
                self.armed = None;
                out.push(abi::Event {
                    window,
                    node: press.node,
                    kind: event::FILE_DRAG,
                    value: press.row,
                    code: press.serial,
                    ..Default::default()
                });
                return true;
            }
            EventKind::Button {
                code: 0x110,
                pressed: false,
                ..
            }
            | EventKind::PointerLeave
            | EventKind::Focus(false) => self.file_press = None,
            _ => {}
        }
        false
    }

    pub fn input(
        &mut self,
        window: u64,
        input: &EventKind,
        probe: &classic_ui::Painter<'_>,
        modifiers: keyboard::Modifiers,
    ) -> (Vec<abi::Event>, bool, bool) {
        let before = (
            self.focus,
            self.armed,
            self.armed.filter(|id| self.hit() == Some(*id)),
        );
        let mut out = Vec::new();
        if matches!(
            input,
            EventKind::Button { pressed: true, .. }
                | EventKind::Focus(false)
                | EventKind::Wheel { .. }
        ) || matches!(input, EventKind::Key(key) if key.pressed)
        {
            self.rename_click = None;
        }
        if let EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } = input {
            self.pointer = (x.floor() as i32, y.floor() as i32);
        }
        if self.label_input(window, input, probe, modifiers.shift, &mut out) {
            return (out, true, true);
        }
        let (menu_handled, menu_repaint) = self.menu_input(window, input, probe, &mut out);
        if menu_handled {
            return (out, true, menu_repaint);
        }
        if let EventKind::Key(key) = input
            && let Some(request) = self.list_context_menu(window, *key)
        {
            out.push(request);
            return (out, true, true);
        }
        let (combo_handled, combo_repaint) = self.combo_input(window, input, &mut out);
        if combo_handled {
            return (out, true, combo_repaint);
        }
        let (toolbar_handled, toolbar_repaint) = self.toolbar_input(window, input, &mut out);
        if toolbar_handled {
            return (out, true, true);
        }
        let menu_repaint = menu_repaint || toolbar_repaint || combo_repaint;
        if self.calendar_input(window, input, &mut out) {
            return (out, true, true);
        }
        if self.editor_input(window, input, probe, modifiers.shift, &mut out) {
            return (out, true, true);
        }
        if self.outline_input(window, input, &mut out) {
            return (out, true, true);
        }
        if self.table_input(window, input, probe, &mut out) {
            return (out, true, true);
        }
        if self.updown_input(window, input, &mut out) {
            return (out, true, true);
        }
        if self.range_input(window, input, &mut out) {
            return (out, true, true);
        }
        if self.file_drag_input(window, input, probe, &mut out) {
            return (out, true, true);
        }
        let mut consumed = false;
        match input {
            EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                self.pointer = (x.floor() as i32, y.floor() as i32);
                if let Some(id) = self
                    .armed
                    .filter(|id| self.active(*id) && self.nodes[id].text_editable())
                {
                    self.edit_pointer(id, probe, true);
                    consumed = true;
                }
            }
            EventKind::PointerLeave => {
                self.pointer = (-1, -1);
                self.armed = None;
            }
            EventKind::Focus(false) => self.armed = None,
            EventKind::Button {
                code: 0x110,
                pressed: true,
                ..
            } => {
                self.armed = self.hit();
                if self.armed.is_some() {
                    self.focus = self.armed;
                }
                if let Some(id) = self.armed.filter(|id| self.nodes[id].text_editable()) {
                    self.edit_pointer(id, probe, modifiers.shift);
                }
                // comctl32 在按下时切换标签页，释放时不再重复激活。
                if let Some(id) = self.armed.filter(|id| self.nodes[id].kind == widget::TABS) {
                    self.activate(window, id, true, modifiers, probe, &mut out);
                }
            }
            EventKind::Button {
                code: 0x110,
                pressed: false,
                ..
            } => {
                if let Some(id) = self
                    .armed
                    .take()
                    .filter(|id| Some(*id) == self.hit() && self.nodes[id].kind != widget::TABS)
                {
                    self.activate(window, id, true, modifiers, probe, &mut out);
                }
            }
            EventKind::Key(key) if key.pressed => {
                // 属性表约定：Ctrl+Tab / Ctrl+PgDn 下一页，加 Shift 或 Ctrl+PgUp 上一页，焦点在页内控件时同样有效。
                if key.modifiers.control
                    && matches!(key.code, 15 | 104 | 109)
                    && self.cycle_tabs(
                        window,
                        key.code == 104 || (key.code == 15 && key.modifiers.shift),
                        &mut out,
                    )
                {
                    consumed = true;
                } else if key.code == 15
                    && !self.focus.is_some_and(|id| {
                        self.nodes[&id].kind == widget::CANVAS
                            && self.nodes[&id].state & state::TAB_INPUT != 0
                    })
                {
                    self.focus_next(key.modifiers.shift);
                    consumed = true;
                } else if let Some(id) = self.focus.filter(|id| {
                    self.nodes
                        .get(id)
                        .is_some_and(|node| node.focusable() && self.active(*id))
                }) {
                    if self.nodes[&id].text_editable() {
                        let edit = self
                            .nodes
                            .get_mut(&id)
                            .expect("focus node")
                            .edit
                            .as_mut()
                            .expect("edit node");
                        consumed = (!key.modifiers.control
                            && !key.modifiers.alt
                            && !key.modifiers.super_key
                            && (matches!(key.code, 105 | 106 | 102 | 107 | 14 | 111)
                                || key.text.is_some_and(|text| !text.is_control())))
                            || (key.modifiers.control && key.code == 30);
                        let before = edit.text().to_owned();
                        edit.key(*key);
                        if edit.text() != before {
                            out.push(abi::Event {
                                window,
                                node: id,
                                kind: event::CHANGED,
                                ..Default::default()
                            });
                        }
                    } else if self.nodes[&id].kind == widget::TABS {
                        consumed = matches!(key.code, 105 | 106 | 102 | 107);
                        self.tabs_key(window, id, *key, &mut out);
                    } else if matches!(self.nodes[&id].kind, widget::LIST | widget::TREE)
                        && key.code == 57
                    {
                        consumed = true;
                        self.list_key(window, id, *key, &mut out);
                    } else if key.code == 28 && key.modifiers.alt {
                        consumed = false;
                    } else if matches!(key.code, 28 | 57) {
                        consumed = self.nodes[&id].kind != widget::CANVAS;
                        self.activate(window, id, false, modifiers, probe, &mut out);
                    } else {
                        consumed = matches!(self.nodes[&id].kind, widget::LIST | widget::TREE)
                            && (matches!(key.code, 102..=109)
                                || (key.modifiers.control && key.code == 30));
                        self.list_key(window, id, *key, &mut out);
                    }
                }
            }
            _ => {}
        }
        if out.is_empty()
            && matches!(input,EventKind::Key(key) if key.pressed && key.code==28 && !key.modifiers.control && !key.modifiers.alt)
            && let Some(id) = self.nodes.iter().find_map(|(id, node)| {
                (node.kind == widget::BUTTON
                    && node.state & state::DEFAULT != 0
                    && self.active(*id))
                .then_some(*id)
            })
        {
            self.activate(window, id, false, modifiers, probe, &mut out);
            consumed = true;
        }
        self.layout_dirty |= out.iter().any(|event| {
            self.nodes.get(&event.node).is_some_and(|node| {
                (event.kind == event::SELECT && node.kind == widget::TABS)
                    || (event.kind == event::CHANGED
                        && matches!(node.kind, widget::EDIT | widget::COMBO))
            })
        });
        let after = (
            self.focus,
            self.armed,
            self.armed.filter(|id| self.hit() == Some(*id)),
        );
        let repaint = menu_repaint
            || consumed
            || before != after
            || !out.is_empty()
            || matches!(input, EventKind::Wheel { .. });
        (out, consumed, repaint)
    }
    fn edit_pointer(&mut self, id: u64, probe: &classic_ui::Painter<'_>, extend: bool) {
        let node = self.nodes.get_mut(&id).expect("edit target");
        let margins = if node.kind == widget::COMBO {
            node.combo_margins()
        } else {
            (0, 0)
        };
        let edit = node.edit.as_mut().expect("edit control");
        let position = if node.kind == widget::EDIT && node.state & state::BORDERLESS != 0 {
            probe.edit_hit_borderless(node.rect, edit, self.pointer.0)
        } else {
            probe.edit_hit_with_margins(
                node.rect,
                edit,
                node.state & state::PASSWORD != 0,
                self.pointer.0,
                margins,
            )
        };
        edit.select_position(position, extend);
    }
    fn activate(
        &mut self,
        window: u64,
        id: u64,
        pointer: bool,
        modifiers: keyboard::Modifiers,
        probe: &classic_ui::Painter<'_>,
        out: &mut Vec<abi::Event>,
    ) {
        let node = &self.nodes[&id];
        let kind = node.kind;
        if kind == widget::TABS {
            // 命中与绘制共用 TabStrip；禁用页的表头可见但不能选中。
            let page = node
                .tabs
                .hit(self.pointer, node.selected)
                .filter(|page| pointer && self.nodes[page].enabled());
            if let Some(page) = page {
                self.select_tab(window, id, page, out);
            }
            return;
        }
        if kind == widget::RADIO {
            let (parent, group) = (node.parent, node.group);
            for (_, other) in self.nodes.matching_mut(|node| {
                node.kind == widget::RADIO && node.parent == parent && node.group == group
            }) {
                other.state &= !state::CHECKED;
            }
            self.nodes.get_mut(&id).expect("active radio").state |= state::CHECKED;
        }
        let node = self.nodes.get_mut(&id).expect("active node");
        let mut value = 0;
        if kind == widget::CHECKBOX {
            node.state ^= state::CHECKED;
        }
        if matches!(kind, widget::LIST | widget::TREE) {
            if !pointer {
                if let Some(value) = node.selected {
                    out.push(abi::Event {
                        window,
                        node: id,
                        kind: event::ACTIVATE,
                        value,
                        ..Default::default()
                    });
                }
                return;
            }
            let geometry = node.list_geometry();
            if self.pointer.0 < geometry.rows.x
                || self.pointer.0 >= geometry.rows.right()
                || self.pointer.1 < geometry.rows.y
                || self.pointer.1 >= geometry.rows.bottom()
            {
                return;
            }
            let index = node.list_row_at(self.pointer, probe);
            if let Some((row, _)) = node.row_at(index)
                && let Some(table) = &node.table
                && let Some(check) = table.check(*row)
            {
                let rect = table.check_rect(Rect::new(
                    geometry.rows.x,
                    geometry.rows.y + (index as i32 - node.position.position) * node.row_height,
                    geometry.rows.width,
                    node.row_height,
                ));
                if (rect.x..rect.right()).contains(&self.pointer.0)
                    && (rect.y..rect.bottom()).contains(&self.pointer.1)
                {
                    request_check(window, id, *row, check, out);
                    return;
                }
            }
            // 树项只在图标或文字标签上命中（comctl32 TVHT_ONITEM），与绘制共用 RowGeometry；
            // 缩进、连线和标签右侧空白不改变选择。
            if let (Some(outline), Some((row_id, text)), true) = (
                node.outline.as_ref(),
                node.row_at(index),
                node.table
                    .as_ref()
                    .is_some_and(|table| table.columns.is_empty()),
            ) {
                let icon = node
                    .row_icons
                    .get(row_id)
                    .is_some_and(|icons| icons.0.is_some());
                let row = Rect::new(
                    geometry.rows.x - node.table.as_ref().expect("tree model").horizontal.position,
                    geometry.rows.y
                        + (index - node.position.position as usize) as i32 * node.row_height,
                    geometry.rows.width,
                    node.row_height,
                );
                let row_geometry = outline.row(row, index, icon);
                let width = (probe.text_width(text, false) + probe.scale() - 1) / probe.scale();
                let left = if icon {
                    row_geometry.icon
                } else {
                    row_geometry.label
                };
                let right = row_geometry.label + super::outline::Outline::label_width(width);
                if self.pointer.0 < left || self.pointer.0 >= right {
                    return;
                }
            }
            let rename = kind == widget::LIST
                && node.state & state::EDIT_LABELS != 0
                && !modifiers.shift
                && !modifiers.control
                && !modifiers.alt
                && !modifiers.super_key
                && node
                    .row_at(index)
                    .is_some_and(|(row, _)| node.selected == Some(*row) && node.row_selected(*row))
                && node.label_contains(index, self.pointer, probe);
            if let Some(selected) = node.choose_row(index, modifiers.shift, modifiers.control, true)
            {
                value = selected;
                let now = std::time::Instant::now();
                let table = node.table.as_mut().expect("selection table");
                let double = table
                    .last_click
                    .as_ref()
                    .is_some_and(|last| last.follows(value, self.pointer, now));
                table.last_click =
                    (!double).then(|| classic_ui::ItemClick::new(value, self.pointer, now));
                if double {
                    if let Some(outline) = node.outline.as_mut() {
                        outline.toggle_event(window, id, value, out);
                        node.clamp_list();
                    }
                    out.push(abi::Event {
                        window,
                        node: id,
                        kind: event::ACTIVATE,
                        value,
                        ..Default::default()
                    });
                } else if rename {
                    self.rename_click = Some((id, value, now + ITEM_DOUBLE_CLICK));
                }
            } else {
                if kind == widget::LIST && !modifiers.control && !modifiers.shift {
                    node.selected = None;
                    if let Some(table) = node.table.as_mut() {
                        table.selection.clear();
                        table.anchor = None;
                        table.last_click = None;
                    }
                    out.push(abi::Event {
                        window,
                        node: id,
                        kind: event::SELECT,
                        ..Default::default()
                    });
                }
                return;
            }
        }
        if matches!(
            kind,
            widget::BUTTON
                | widget::CHECKBOX
                | widget::RADIO
                | widget::LIST
                | widget::TREE
                | widget::LINK
        ) {
            out.push(abi::Event {
                window,
                node: id,
                kind: if matches!(kind, widget::LIST | widget::TREE) {
                    event::SELECT
                } else {
                    event::ACTIVATE
                },
                value: if matches!(kind, widget::RADIO | widget::CHECKBOX) {
                    u64::from(node.state & state::CHECKED != 0)
                } else {
                    value
                },
                ..Default::default()
            });
        }
    }
    /// 可见且可用的页面，按表头顺序；隐藏与禁用页不参与键盘切换。
    fn selectable_pages(&self, id: u64) -> Vec<u64> {
        self.nodes[&id]
            .tabs
            .headers
            .iter()
            .map(|(page, _)| *page)
            .filter(|page| self.nodes[page].enabled())
            .collect()
    }
    /// 切换到指定页；与当前页相同时不发事件（comctl32 只在选择改变时通知）。
    fn select_tab(&mut self, window: u64, id: u64, page: u64, out: &mut Vec<abi::Event>) {
        let node = self.nodes.get_mut(&id).expect("tab control");
        if node.selected == Some(page) {
            return;
        }
        node.selected = Some(page);
        out.push(abi::Event {
            window,
            node: id,
            kind: event::SELECT,
            value: page,
            ..Default::default()
        });
    }
    /// 标签控件自身的方向键：Left/Right 逐页且到端点停止，Home/End 跳到首尾。
    fn tabs_key(&mut self, window: u64, id: u64, key: Key, out: &mut Vec<abi::Event>) {
        let pages = self.selectable_pages(id);
        let Some(last) = pages.len().checked_sub(1) else {
            return;
        };
        let current = self.nodes[&id]
            .selected
            .and_then(|selected| pages.iter().position(|page| *page == selected));
        let index = match (key.code, current) {
            (105, Some(current)) => current.saturating_sub(1),
            (106, Some(current)) => (current + 1).min(last),
            (105 | 106, None) | (102, _) => 0,
            (107, _) => last,
            _ => return,
        };
        self.select_tab(window, id, pages[index], out);
    }
    /// 属性表的 Ctrl+Tab 循环：优先焦点所在的标签控件，否则窗口内第一个可用标签控件。
    /// 没有可切换的标签控件时返回 false，按键继续按普通焦点遍历处理。
    fn cycle_tabs(&mut self, window: u64, reverse: bool, out: &mut Vec<abi::Event>) -> bool {
        let mut ancestor = self.focus.unwrap_or(0);
        while ancestor != 0 && self.nodes[&ancestor].kind != widget::TABS {
            ancestor = self.nodes[&ancestor].parent;
        }
        let id = Some(ancestor).filter(|id| *id != 0).or_else(|| {
            self.nodes
                .iter()
                .find(|(id, node)| node.kind == widget::TABS && self.active(**id))
                .map(|(id, _)| *id)
        });
        let Some(id) = id.filter(|id| self.active(*id)) else {
            return false;
        };
        let pages = self.selectable_pages(id);
        if pages.len() < 2 {
            return false;
        }
        let current = self.nodes[&id]
            .selected
            .and_then(|selected| pages.iter().position(|page| *page == selected))
            .unwrap_or(0);
        let index = if reverse {
            (current + pages.len() - 1) % pages.len()
        } else {
            (current + 1) % pages.len()
        };
        self.select_tab(window, id, pages[index], out);
        true
    }

    fn list_context_menu(&mut self, window: u64, key: Key) -> Option<abi::Event> {
        let modifiers = key.modifiers;
        // KEY_COMPOSE 是桌面键盘的 Applications 键，KEY_MENU 也用于实体菜单键。
        if !key.pressed
            || key.repeat
            || modifiers.control
            || modifiers.alt
            || modifiers.super_key
            || !matches!((key.code, modifiers.shift), (68, true) | (127 | 139, false))
        {
            return None;
        }
        let id = self.focus.filter(|id| self.active(*id))?;
        let node = self.nodes.get_mut(&id)?;
        if node.kind != widget::LIST || node.state & state::KEYBOARD_CONTEXT_MENU == 0 {
            return None;
        }
        let table = node.table.as_ref().expect("list model");
        let row = node
            .selected
            .filter(|row| node.row_selected(*row))
            .or_else(|| table.selection.first().copied());
        let index = row.and_then(|row| node.rows.iter().position(|entry| entry.0 == row));
        let geometry = node.list_geometry();
        let viewport = geometry.rows.intersect(node.clip);
        let (x, y) = if let Some(index) = index {
            // Ctrl+方向键可把焦点行移出所选集合。让单项命令与菜单锚点指向同一选中项。
            node.selected = row;
            node.reveal_list_row(index, geometry);
            let scroll = node.position.position;
            let cell = geometry.grid.map_or_else(
                || {
                    Rect::new(
                        geometry.rows.x,
                        geometry.rows.y + (index as i32 - scroll) * node.row_height,
                        geometry.rows.width,
                        node.row_height,
                    )
                },
                |grid| grid.cell(index, scroll),
            );
            let visible = cell.intersect(viewport);
            (visible.x, visible.bottom())
        } else {
            (viewport.x, viewport.y)
        };
        Some(abi::Event {
            window,
            node: id,
            kind: event::CONTEXT_MENU,
            value: row.unwrap_or(0),
            x: f64::from(x),
            y: f64::from(y),
            ..Default::default()
        })
    }

    fn list_key(&mut self, window: u64, id: u64, key: Key, out: &mut Vec<abi::Event>) {
        let node = self.nodes.get_mut(&id).expect("focus node");
        if !matches!(node.kind, widget::LIST | widget::TREE) || node.row_count() == 0 {
            return;
        }
        if key.code == 57
            && let Some(row) = node.selected
            && let Some(check) = node.table.as_ref().and_then(|table| table.check(row))
        {
            if !key.repeat {
                request_check(window, id, row, check, out);
            }
            return;
        }
        if key.modifiers.control && key.code == 30 {
            if let Some(table) = node.table.as_mut().filter(|table| table.multiple) {
                table.selection = node.rows.iter().map(|row| row.0).collect();
                if node.selected.is_none() {
                    node.selected = node.rows.first().map(|row| row.0);
                }
                table.anchor = node.selected;
            }
            out.push(abi::Event {
                window,
                node: id,
                kind: event::SELECT,
                value: node.selected.unwrap_or(0),
                ..Default::default()
            });
            return;
        }
        let current = node
            .selected
            .and_then(|id| {
                (0..node.row_count())
                    .position(|index| node.row_at(index).is_some_and(|row| row.0 == id))
            })
            .unwrap_or(0);
        let geometry = node.list_geometry();
        let page = geometry.grid.map_or_else(
            || (geometry.rows.height / node.row_height).max(1) as usize,
            |grid| {
                let rows = (grid.viewport.height / grid.height).max(1);
                let columns = (grid.viewport.width / grid.width).max(1);
                (rows * columns) as usize
            },
        );
        let index = match key.code {
            105 | 106 | 103 | 108 if node.selected.is_none() => 0,
            105 | 106 | 103 | 108 if geometry.grid.is_some() => {
                geometry.grid.expect("item grid").step(
                    current,
                    if key.code == 105 {
                        -1
                    } else if key.code == 106 {
                        1
                    } else {
                        0
                    },
                    if key.code == 103 {
                        -1
                    } else if key.code == 108 {
                        1
                    } else {
                        0
                    },
                )
            }
            57 => current,
            104 => current.saturating_sub(page),
            109 => current.saturating_add(page).min(node.row_count() - 1),
            103 => current.saturating_sub(1),
            108 => (current + 1).min(node.row_count() - 1),
            102 => 0,
            107 => node.row_count() - 1,
            _ => return,
        };
        let value = node
            .choose_row(
                index,
                key.modifiers.shift,
                key.modifiers.control,
                key.code == 57,
            )
            .expect("visible row");
        node.reveal_list_row(index, geometry);
        out.push(abi::Event {
            window,
            node: id,
            kind: event::SELECT,
            value,
            ..Default::default()
        });
    }
}

fn request_check(window: u64, node: u64, row: u64, check: u32, out: &mut Vec<abi::Event>) {
    if check & state::DISABLED == 0 {
        out.push(abi::Event {
            window,
            node,
            kind: event::TOGGLE_ROW_CHECK,
            value: row,
            ..Default::default()
        });
    }
}
