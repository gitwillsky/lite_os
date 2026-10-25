//! 树视图的结构、展开和可见索引；文本仍由 Node.rows 唯一持有，滚动复用列表实现。
use super::{
    invalid, invoke, text,
    widgets::{Node, Tree},
};
use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{EventKind, Painter};
use classic_ui_abi::{self as abi, event, widget};
use std::{collections::BTreeMap, ffi::c_void, io, slice};

pub struct Branch {
    id: u64,
    parent: u64,
    depth: i32,
    expanded: bool,
    children: bool,
    descendants: Vec<usize>,
}
pub struct Outline {
    branches: Vec<Branch>,
    indices: BTreeMap<u64, usize>,
    visible: Vec<usize>,
}
impl Outline {
    pub fn new() -> Self {
        Self {
            branches: Vec::new(),
            indices: BTreeMap::new(),
            visible: Vec::new(),
        }
    }
    fn rebuild(&mut self) {
        self.visible.clear();
        let mut pending: Vec<_> = self
            .branches
            .iter()
            .enumerate()
            .filter(|(_, branch)| branch.parent == 0)
            .map(|(index, _)| index)
            .rev()
            .collect();
        while let Some(index) = pending.pop() {
            self.visible.push(index);
            if self.branches[index].expanded {
                pending.extend(self.branches[index].descendants.iter().rev().copied());
            }
        }
    }
    pub fn toggle_event(&mut self, window: u64, node: u64, id: u64, out: &mut Vec<abi::Event>) {
        let Some(index) = self.indices.get(&id).copied() else {
            return;
        };
        if !self.branches[index].children {
            return;
        }
        self.toggle(id);
        out.push(abi::Event {
            window,
            node,
            kind: event::EXPAND,
            value: id,
            flags: u32::from(self.branches[index].expanded),
            ..Default::default()
        });
    }
    pub fn toggle(&mut self, id: u64) {
        let Some(index) = self.indices.get(&id).copied() else {
            return;
        };
        if self.branches[index].children {
            self.branches[index].expanded ^= true;
            self.rebuild();
        }
    }
    pub fn reveal(&mut self, mut id: u64) {
        while let Some(index) = self.indices.get(&id).copied() {
            id = self.branches[index].parent;
            if let Some(parent) = self.indices.get(&id).copied() {
                self.branches[parent].expanded = true;
            }
        }
        self.rebuild();
    }
    pub fn indent(&self, row: usize) -> i32 {
        (self.branches[self.visible[row]].depth + 1) * 18
    }
    fn expander(&self, rows: Rect, height: i32, position: usize, row: usize) -> Rect {
        Rect::new(
            rows.x + self.indent(row) - 13,
            rows.y + (row - position) as i32 * height + (height - 9) / 2,
            9,
            9,
        )
    }
    pub fn paint_expander(&self, p: &mut Painter<'_>, row: Rect, index: usize) {
        let branch = &self.branches[self.visible[index]];
        if !branch.children {
            return;
        }
        let rect = Rect::new(
            row.x + self.indent(index) - 13,
            row.y + (row.height - 9) / 2,
            9,
            9,
        );
        p.edge(rect, Edge::Sunken);
        p.fill(Rect::new(rect.x + 2, rect.y + 4, 5, 1), C::WindowText);
        if !branch.expanded {
            p.fill(Rect::new(rect.x + 4, rect.y + 2, 1, 5), C::WindowText);
        }
    }
}
impl Node {
    pub fn row_count(&self) -> usize {
        self.outline
            .as_ref()
            .map_or(self.rows.len(), |outline| outline.visible.len())
    }
    pub fn row_at(&self, index: usize) -> Option<&(u64, String)> {
        self.rows.get(match &self.outline {
            Some(outline) => *outline.visible.get(index)?,
            None => index,
        })
    }
}
impl Tree {
    pub fn outline_input(
        &mut self,
        window: u64,
        input: &EventKind,
        out: &mut Vec<abi::Event>,
    ) -> bool {
        let id = match input {
            EventKind::Key(_) => self.focus,
            _ => self.hit(),
        };
        let Some(id) = id.filter(|id| self.active(*id) && self.nodes[id].kind == widget::TREE)
        else {
            return false;
        };
        let node = self.nodes.get_mut(&id).expect("tree target");
        let geometry = node.list_geometry();
        let outline = node.outline.as_mut().expect("outline model");
        let mut select = None;
        match input {
            EventKind::Button {
                code: 0x110,
                pressed: true,
                ..
            } => {
                if self.pointer.1 < geometry.rows.y || self.pointer.1 >= geometry.rows.bottom() {
                    return false;
                }
                let row = node.position.position as usize
                    + ((self.pointer.1 - geometry.rows.y) / node.row_height) as usize;
                if row >= outline.visible.len() {
                    return false;
                }
                let rect = outline.expander(
                    Rect::new(
                        geometry.rows.x
                            - node.table.as_ref().expect("list model").horizontal.position,
                        geometry.rows.y,
                        geometry.rows.width,
                        geometry.rows.height,
                    ),
                    node.row_height,
                    node.position.position as usize,
                    row,
                );
                if self.pointer.0 < rect.x
                    || self.pointer.0 >= rect.right()
                    || self.pointer.1 < rect.y
                    || self.pointer.1 >= rect.bottom()
                {
                    return false;
                }
                let value = outline.branches[outline.visible[row]].id;
                let branch = &outline.branches[outline.visible[row]];
                if !branch.children {
                    return false;
                }
                self.focus = Some(id);
                self.armed = None;
                outline.toggle_event(window, id, value, out);
                // 只有折叠隐藏当前焦点时才移到分支；普通展开不改变目录选择。
                if node.selected.is_some_and(|selected| {
                    !outline
                        .visible
                        .iter()
                        .any(|index| outline.branches[*index].id == selected)
                }) {
                    select = Some(value);
                }
            }
            EventKind::Key(key) if key.pressed && matches!(key.code, 105 | 106) => {
                let Some(index) = node
                    .selected
                    .and_then(|value| outline.indices.get(&value))
                    .copied()
                else {
                    return true;
                };
                let branch = &outline.branches[index];
                match (key.code, branch.children, branch.expanded) {
                    (105, true, true) | (106, true, false) => {
                        let value = branch.id;
                        outline.toggle_event(window, id, value, out);
                    }
                    (105, _, _) if branch.parent != 0 => select = Some(branch.parent),
                    (106, true, true) => {
                        select = outline
                            .branches
                            .iter()
                            .find(|child| child.parent == branch.id)
                            .map(|child| child.id)
                    }
                    _ => {}
                }
            }
            _ => return false,
        }
        if let Some(value) = select {
            node.selected = Some(value);
            out.push(abi::Event {
                window,
                node: id,
                kind: event::SELECT,
                value,
                ..Default::default()
            });
        }
        node.clamp_list();
        self.layout_dirty = true;
        true
    }
}
pub(super) unsafe extern "C" fn tree_rows(
    pointer: *mut c_void,
    window: u64,
    id: u64,
    values: *const abi::TreeRow,
    count: usize,
) -> i32 {
    if count > 1_000_000 || (count != 0 && values.is_null()) {
        return abi::INVALID;
    }
    // SAFETY: ABI 输入按调用借用，先验证并复制所有字符串；原子发布，不保留调用者指针。
    unsafe {
        invoke(pointer, |context| {
            let values = if count == 0 {
                &[]
            } else {
                slice::from_raw_parts(values, count)
            };
            let mut rows = Vec::with_capacity(count);
            let mut outline = Outline::new();
            for value in values {
                if value.id == 0 || outline.indices.contains_key(&value.id) || value.state & !3 != 0
                {
                    return Err(invalid("invalid tree identity/state"));
                }
                let depth = if value.parent == 0 {
                    0
                } else {
                    let index = *outline
                        .indices
                        .get(&value.parent)
                        .ok_or_else(|| invalid("tree parent must precede child"))?;
                    outline.branches[index].children = true;
                    outline.branches[index].descendants.push(rows.len());
                    outline.branches[index].depth + 1
                };
                if depth > 64 {
                    return Err(invalid("tree nesting limit reached"));
                }
                outline.indices.insert(value.id, rows.len());
                outline.branches.push(Branch {
                    id: value.id,
                    parent: value.parent,
                    depth,
                    expanded: value.state & 1 != 0,
                    children: value.state & 2 != 0,
                    descendants: Vec::new(),
                });
                rows.push((value.id, text(value.text)?));
            }
            let host = context.host_mut(window)?;
            let node = host
                .controls_mut()?
                .nodes
                .get_mut(&id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown tree"))?;
            let previous = node.outline.as_ref().ok_or_else(|| invalid("not a tree"))?;
            for branch in &mut outline.branches {
                if let Some(index) = previous.indices.get(&branch.id) {
                    branch.expanded = previous.branches[*index].expanded;
                }
            }
            node.selected = node.selected.filter(|id| outline.indices.contains_key(id));
            if let Some(selected) = node.selected {
                outline.reveal(selected);
            } else {
                outline.rebuild();
            }
            node.row_icons
                .retain(|id, _| outline.indices.contains_key(id));
            node.rows = rows;
            node.outline = Some(outline);
            node.clamp_list();
            host.frame.invalidate();
            Ok(())
        })
    }
}
