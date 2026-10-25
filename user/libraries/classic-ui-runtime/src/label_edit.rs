//! 列表原地重命名的草稿和提交阶段；磁盘操作由应用完成后明确接受或恢复编辑。
use super::{
    invalid, invoke, text,
    widgets::{Node, Tree},
};
use canvas::Rect;
use classic_ui::{Edit, EventKind, Painter};
use classic_ui_abi::{self as abi, event, widget};
use std::ffi::c_void;
pub struct LabelEdit {
    pub row: u64,
    pub pending: bool,
}
impl Node {
    pub fn label_rect(&self) -> Option<Rect> {
        let editing = self.label_edit.as_ref()?;
        let index = (0..self.row_count())
            .find(|index| self.row_at(*index).is_some_and(|row| row.0 == editing.row))?;
        let geometry = self.list_geometry();
        let table = self.table.as_ref()?;
        if let Some(grid) = geometry.grid {
            let cell = grid.cell(index, self.position.position);
            if grid.mode == 1 {
                Some(Rect::new(cell.x, cell.y + 34, cell.width, 26))
            } else {
                let left = if self.row_icons.contains_key(&editing.row) {
                    20
                } else {
                    2
                };
                Some(Rect::new(
                    cell.x + left,
                    cell.y,
                    cell.width - left,
                    self.row_height,
                ))
            }
        } else {
            let left = if self.row_icons.contains_key(&editing.row) {
                20
            } else {
                2
            };
            let width = if table.columns.is_empty() {
                geometry.rows.width
            } else {
                table.header.width(0)
            };
            Some(Rect::new(
                geometry.rows.x - table.horizontal.position + left,
                geometry.rows.y + (index as i32 - self.position.position) * self.row_height,
                (width - left).max(0),
                self.row_height,
            ))
        }
    }
    pub fn editing_text(&self) -> bool {
        self.kind == widget::EDIT
            || self.kind == widget::TEXT_EDITOR
            || self
                .label_edit
                .as_ref()
                .is_some_and(|editing| !editing.pending)
    }
}
impl Tree {
    pub fn label_input(
        &mut self,
        window: u64,
        input: &EventKind,
        p: &Painter<'_>,
        shift: bool,
        out: &mut Vec<abi::Event>,
    ) -> bool {
        let Some(id) = self
            .focus
            .filter(|id| self.active(*id) && self.nodes[id].label_edit.is_some())
        else {
            return false;
        };
        let node = self.nodes.get_mut(&id).expect("label editor");
        let area = node.label_rect().unwrap_or(node.rect);
        let editing = node.label_edit.as_mut().expect("editing label");
        if editing.pending {
            return matches!(input, EventKind::Key(_));
        }
        let mut commit = false;
        let mut cancel = false;
        match input {
            EventKind::Key(key) if key.pressed => match key.code {
                1 => cancel = true,
                28 => commit = true,
                _ => {
                    node.edit.as_mut().expect("label draft").key(*key);
                }
            },
            EventKind::Key(_) => return false,
            EventKind::Button {
                code: 0x110,
                pressed: true,
                ..
            } => {
                if self.pointer.0 >= area.x
                    && self.pointer.0 < area.right()
                    && self.pointer.1 >= area.y
                    && self.pointer.1 < area.bottom()
                {
                    let edit = node.edit.as_mut().expect("label draft");
                    let at = p.edit_hit(area, edit, false, self.pointer.0);
                    edit.select_position(at, shift);
                    self.armed = Some(id);
                } else {
                    commit = true;
                }
            }
            EventKind::PointerMove { .. } if self.armed == Some(id) => {
                let edit = node.edit.as_mut().expect("label draft");
                let at = p.edit_hit(area, edit, false, self.pointer.0);
                edit.select_position(at, true);
            }
            EventKind::Button {
                code: 0x110,
                pressed: false,
                ..
            }
            | EventKind::PointerLeave
            | EventKind::Focus(false) => self.armed = None,
            _ => return false,
        }
        if commit {
            editing.pending = true;
            out.push(abi::Event {
                window,
                node: id,
                kind: event::LABEL_EDITED,
                value: editing.row,
                ..Default::default()
            });
        }
        if cancel {
            out.push(abi::Event {
                window,
                node: id,
                kind: event::DISMISS,
                value: editing.row,
                ..Default::default()
            });
            node.label_edit = None;
            node.edit = None;
            self.armed = None;
        }
        true
    }
}
pub(super) unsafe extern "C" fn edit_label(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    row: u64,
    action: u32,
    value: abi::Text,
) -> i32 {
    if action > 2 {
        return abi::INVALID;
    }
    // SAFETY: 所有文本按调用借用，运行库独占行编辑草稿，不保留应用指针或回调。
    unsafe {
        invoke(pointer, |context| {
            context.layout(window)?;
            let value = if action == 0 {
                Some(text(value)?)
            } else {
                None
            };
            let host = context.host_mut(window)?;
            let tree = host.controls_mut()?;
            let node = tree
                .nodes
                .get_mut(&id)
                .filter(|node| node.kind == widget::LIST)
                .ok_or_else(|| invalid("not a list"))?;
            if action == 0 {
                let index = node
                    .rows
                    .iter()
                    .position(|entry| entry.0 == row)
                    .ok_or_else(|| invalid("unknown label row"))?;
                let geometry = node.list_geometry();
                if let Some(grid) = geometry.grid {
                    node.position.position = grid.reveal(index, node.position.position);
                } else {
                    let page = (geometry.rows.height / node.row_height).max(1);
                    node.position.position = node
                        .position
                        .position
                        .min(index as i32)
                        .max(index as i32 + 1 - page)
                        .max(0);
                }
                node.choose_row(index, false, false, false);
                let value = value.expect("label text");
                let mut edit = Edit::new(&value);
                edit.select_position(0, false);
                edit.select_position(value.len(), true);
                node.edit = Some(edit);
                node.label_edit = Some(LabelEdit {
                    row,
                    pending: false,
                });
                tree.focus = Some(id);
            } else if action == 1 {
                node.edit = None;
                node.label_edit = None;
            } else {
                let editing = node
                    .label_edit
                    .as_mut()
                    .ok_or_else(|| invalid("no label draft"))?;
                editing.pending = false;
                tree.focus = Some(id);
            }
            host.frame.invalidate();
            Ok(())
        })
    }
}
