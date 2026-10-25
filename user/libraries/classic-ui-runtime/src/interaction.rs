//! 标准控件激活、焦点与选择；领域动作只在输入分发结束后由 ABI 事件交给应用。
use super::widgets::Tree;
use classic_ui::{EventKind, Key};
use classic_ui_abi::{self as abi, event, state, widget};
impl Tree {
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
        if self.calendar_input(window, input, probe, &mut out) {
            return (out, true, true);
        }
        if self.editor_input(window, input, probe, modifiers.shift, &mut out) {
            return (out, true, true);
        }
        if self.outline_input(window, input, &mut out) {
            return (out, true, true);
        }
        if self.table_input(window, input, &mut out) {
            return (out, true, true);
        }
        if self.range_input(window, input, &mut out) {
            return (out, true, true);
        }
        let mut consumed = false;
        match input {
            EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                self.pointer = (x.floor() as i32, y.floor() as i32);
                if let Some(id) = self
                    .armed
                    .filter(|id| self.active(*id) && self.nodes[id].kind == widget::EDIT)
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
                if let Some(id) = self.armed.filter(|id| self.nodes[id].kind == widget::EDIT) {
                    self.edit_pointer(id, probe, modifiers.shift);
                }
            }
            EventKind::Button {
                code: 0x110,
                pressed: false,
                ..
            } => {
                if let Some(id) = self.armed.take().filter(|id| Some(*id) == self.hit()) {
                    self.activate(window, id, true, modifiers, &mut out);
                }
            }
            EventKind::Key(key) if key.pressed => {
                if key.code == 15
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
                    if self.nodes[&id].kind == widget::EDIT {
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
                        consumed = matches!(key.code, 105 | 106 | 103 | 108 | 102 | 107);
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
                        self.activate(window, id, false, modifiers, &mut out);
                    } else {
                        consumed = matches!(self.nodes[&id].kind, widget::LIST | widget::TREE)
                            && (matches!(key.code, 103 | 108 | 102 | 107 | 104 | 109 | 105 | 106)
                                || (key.modifiers.control && key.code == 30));
                        self.list_key(window, id, *key, &mut out);
                    }
                }
            }
            _ => {}
        }
        if out.is_empty()
            && matches!(input,EventKind::Key(key) if key.pressed && key.code==28 && !key.modifiers.control && !key.modifiers.alt)
        {
            if let Some(id) = self.nodes.iter().find_map(|(id, node)| {
                (node.kind == widget::BUTTON
                    && node.state & state::DEFAULT != 0
                    && self.active(*id))
                .then_some(*id)
            }) {
                self.activate(window, id, false, modifiers, &mut out);
                consumed = true;
            }
        }
        self.layout_dirty |= out.iter().any(|event| {
            self.nodes.get(&event.node).is_some_and(|node| {
                (event.kind == event::SELECT && node.kind == widget::TABS)
                    || (event.kind == event::CHANGED && node.kind == widget::EDIT)
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
        let edit = node.edit.as_mut().expect("edit control");
        let position = probe.edit_hit(
            node.rect,
            edit,
            node.state & state::PASSWORD != 0,
            self.pointer.0,
        );
        edit.select_position(position, extend);
    }
    fn activate(
        &mut self,
        window: u64,
        id: u64,
        pointer: bool,
        modifiers: keyboard::Modifiers,
        out: &mut Vec<abi::Event>,
    ) {
        let node = &self.nodes[&id];
        let kind = node.kind;
        if kind == widget::RADIO {
            let (parent, group) = (node.parent, node.group);
            for other in self.nodes.values_mut().filter(|node| {
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
            let index = if let Some(grid) = geometry.grid {
                let Some(index) = grid.at(self.pointer, node.position.position) else {
                    return;
                };
                index
            } else {
                node.position.position as usize
                    + ((self.pointer.1 - geometry.rows.y).max(0) / node.row_height) as usize
            };
            if let Some(selected) = node.choose_row(index, modifiers.shift, modifiers.control, true)
            {
                value = selected;
                let now = std::time::Instant::now();
                let table = node.table.as_mut().expect("selection table");
                let double = table.last_click.is_some_and(|(last, time)| {
                    last == value
                        && now.duration_since(time) < std::time::Duration::from_millis(500)
                });
                table.last_click = if double { None } else { Some((value, now)) };
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
                }
            } else {
                if kind == widget::LIST && !modifiers.control && !modifiers.shift {
                    node.selected = None;
                    if let Some(table) = node.table.as_mut() {
                        table.selection.clear();
                        table.anchor = None;
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
        if kind == widget::TABS {
            if !pointer {
                return;
            }
            if let Some((page, _)) = node.tab_headers.iter().find(|(_, rect)| {
                self.pointer.0 >= rect.x
                    && self.pointer.0 < rect.right()
                    && self.pointer.1 >= rect.y
                    && self.pointer.1 < rect.bottom()
            }) {
                node.selected = Some(*page);
                value = *page;
            } else {
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
                | widget::TABS
        ) {
            out.push(abi::Event {
                window,
                node: id,
                kind: if matches!(kind, widget::LIST | widget::TREE | widget::TABS) {
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
    fn tabs_key(&mut self, window: u64, id: u64, key: Key, out: &mut Vec<abi::Event>) {
        let node = &self.nodes[&id];
        let pages: Vec<_> = node
            .children
            .iter()
            .copied()
            .filter(|id| self.nodes[id].visible() && self.nodes[id].enabled())
            .collect();
        if pages.is_empty() {
            return;
        }
        let current = node
            .selected
            .and_then(|id| pages.iter().position(|page| *page == id))
            .unwrap_or(0);
        let index = match key.code {
            105 | 103 => (current + pages.len() - 1) % pages.len(),
            106 | 108 => (current + 1) % pages.len(),
            102 => 0,
            107 => pages.len() - 1,
            _ => return,
        };
        let page = pages[index];
        self.nodes.get_mut(&id).expect("tab control").selected = Some(page);
        out.push(abi::Event {
            window,
            node: id,
            kind: event::SELECT,
            value: page,
            ..Default::default()
        });
    }
    fn list_key(&mut self, window: u64, id: u64, key: Key, out: &mut Vec<abi::Event>) {
        let node = self.nodes.get_mut(&id).expect("focus node");
        if !matches!(node.kind, widget::LIST | widget::TREE) || node.row_count() == 0 {
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
        let page = geometry.grid.map_or(
            (geometry.rows.height / node.row_height).max(1) as usize,
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
        if let Some(grid) = geometry.grid {
            node.position.position = grid.reveal(index, node.position.position);
        } else {
            let visible = (geometry.rows.height / node.row_height).max(1) as usize;
            if index < node.position.position as usize {
                node.position.position = index as i32;
            } else if index >= node.position.position as usize + visible {
                node.position.position = (index + 1 - visible) as i32;
            }
        }
        out.push(abi::Event {
            window,
            node: id,
            kind: event::SELECT,
            value,
            ..Default::default()
        });
    }
}
