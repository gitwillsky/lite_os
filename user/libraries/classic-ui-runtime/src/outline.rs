//! 树视图的结构、展开和可见索引；文本仍由 Node.rows 唯一持有，滚动复用列表实现。
use super::{
    invalid, invoke, text,
    widgets::{Node, Tree},
};
use canvas::Rect;
use classic_theme::SystemColor as C;
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
    /// 同一父节点下是否还有后续兄弟；分支竖线据此决定是否延伸到行底。
    /// 缺少它只能逐行扫描兄弟表，或把最后一个孩子的竖线画到空处。
    last: bool,
    /// 是否为同一父节点下的第一个孩子；第一个根节点没有向上的分支线。
    first: bool,
}

/// comctl32 树视图默认缩进（`uIndent`），每层一列。
const INDENT: i32 = 19;
/// 展开框边长；中心与本层分支线对齐。
const BOX: i32 = 9;
/// 图标宽度与图标后到文字标签的间距。
const ICON: i32 = 16;
const ICON_GAP: i32 = 3;
/// 文字标签左右留白；高亮与焦点框覆盖整个标签。
const LABEL_PADDING: i32 = 2;

/// 一行树项的逻辑几何；绘制、命中和内容宽度共用，坐标已包含水平滚动。
pub struct RowGeometry {
    /// 本层分支线所在列的中心 x 与行中心 y。
    pub center: (i32, i32),
    /// 展开框；没有孩子时为 None。
    pub expander: Option<Rect>,
    /// 图标左上 x；没有图标时等于标签起点。
    pub icon: i32,
    /// 文字标签起点；标签宽度为文字宽度加两侧留白。
    pub label: i32,
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
    /// 可见行的几何（TVS_HASLINES | TVS_LINESATROOT | TVS_HASBUTTONS）。
    ///
    /// `row` 是该行矩形（已减去水平滚动），`icon` 表示该行是否有图标。
    pub fn row(&self, row: Rect, index: usize, icon: bool) -> RowGeometry {
        let branch = &self.branches[self.visible[index]];
        let column = row.x + branch.depth * INDENT;
        let center = (column + INDENT / 2, row.y + row.height / 2);
        let icon_x = column + INDENT;
        RowGeometry {
            center,
            expander: branch
                .children
                .then(|| Rect::new(center.0 - BOX / 2, center.1 - BOX / 2, BOX, BOX)),
            icon: icon_x,
            label: icon_x + if icon { ICON + ICON_GAP } else { 0 },
        }
    }
    /// 第 `index` 个可见行的层级（根为 0）。
    pub fn depth(&self, index: usize) -> i32 {
        self.visible
            .get(index)
            .map_or(0, |branch| self.branches[*branch].depth)
    }

    /// 标签宽度：文字逻辑宽度加两侧留白。
    pub fn label_width(text_width: i32) -> i32 {
        text_width + 2 * LABEL_PADDING
    }
    /// 文字相对标签起点的偏移。
    pub fn text_offset() -> i32 {
        LABEL_PADDING
    }
    /// 画本行的分支虚线和展开框；虚线按绝对逻辑坐标奇偶取点，跨行与滚动后保持连续。
    pub fn paint_lines(&self, p: &mut Painter<'_>, row: Rect, index: usize, icon: bool) {
        let geometry = self.row(row, index, icon);
        let (cx, cy) = geometry.center;
        let mut dots = |x0: i32, y0: i32, x1: i32, y1: i32| {
            for y in y0..y1 {
                for x in x0..x1 {
                    if (x + y) % 2 == 0 {
                        p.fill(Rect::new(x, y, 1, 1), C::GrayText);
                    }
                }
            }
        };
        let branch = &self.branches[self.visible[index]];
        // 本层：上半段连到父节点或前一个兄弟，有后续兄弟才延伸到行底；横线连到图标。
        if !(branch.parent == 0 && branch.first) {
            dots(cx, row.y, cx + 1, cy);
        }
        if !branch.last {
            dots(cx, cy, cx + 1, row.bottom());
        }
        dots(cx, cy, geometry.icon - 1, cy + 1);
        // 祖先层：祖先还有后续兄弟时，竖线贯穿整行。
        let mut parent = branch.parent;
        while let Some(&ancestor) = self.indices.get(&parent) {
            let ancestor = &self.branches[ancestor];
            if !ancestor.last {
                let x = row.x + ancestor.depth * INDENT + INDENT / 2;
                dots(x, row.y, x + 1, row.bottom());
            }
            parent = ancestor.parent;
        }
        if let Some(rect) = geometry.expander {
            p.fill(rect, C::Window);
            p.outline(rect, C::GrayText);
            p.fill(Rect::new(cx - 2, cy, 5, 1), C::WindowText);
            if !branch.expanded {
                p.fill(Rect::new(cx, cy - 2, 1, 5), C::WindowText);
            }
        }
    }
    /// 指针下的展开框所属节点；没有孩子或不在框内返回 None。
    fn expander_hit(&self, row: Rect, index: usize, point: (i32, i32)) -> Option<u64> {
        let rect = self.row(row, index, false).expander?;
        (point.0 >= rect.x
            && point.0 < rect.right()
            && point.1 >= rect.y
            && point.1 < rect.bottom())
        .then(|| self.branches[self.visible[index]].id)
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
                let row_rect = Rect::new(
                    geometry.rows.x - node.table.as_ref().expect("list model").horizontal.position,
                    geometry.rows.y
                        + (row - node.position.position as usize) as i32 * node.row_height,
                    geometry.rows.width,
                    node.row_height,
                );
                let Some(value) = outline.expander_hit(row_rect, row, self.pointer) else {
                    return false;
                };
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
            // comctl32：Left 折叠或回到父项，Right 展开或进入首个孩子；
            // 小键盘 +/- 只展开/折叠，Backspace 回到父项。
            EventKind::Key(key) if key.pressed && matches!(key.code, 105 | 106 | 78 | 74 | 14) => {
                let Some(index) = node
                    .selected
                    .and_then(|value| outline.indices.get(&value))
                    .copied()
                else {
                    return true;
                };
                let branch = &outline.branches[index];
                match (key.code, branch.children, branch.expanded) {
                    (105 | 74, true, true) | (106 | 78, true, false) => {
                        let value = branch.id;
                        outline.toggle_event(window, id, value, out);
                    }
                    (105 | 14, _, _) if branch.parent != 0 => select = Some(branch.parent),
                    (106, true, true) => {
                        select = branch
                            .descendants
                            .first()
                            .map(|child| outline.branches[*child].id)
                    }
                    _ => {}
                }
            }
            _ => return false,
        }
        if let Some(value) = select {
            node.selected = Some(value);
            // 键盘移动到父项/子项后保持可见，与列表方向键一致。
            if let Some(index) = outline
                .visible
                .iter()
                .position(|branch| outline.branches[*branch].id == value)
            {
                let page = (geometry.rows.height / node.row_height).max(1) as usize;
                let top = node.position.position as usize;
                if index < top {
                    node.position.position = index as i32;
                } else if index >= top + page {
                    node.position.position = (index + 1 - page) as i32;
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
                    last: false,
                    first: false,
                });
                rows.push((value.id, text(value.text)?));
            }
            // 每个父节点（含根层）的首尾孩子一次标记，绘制时不再扫描兄弟表。
            let mut ends: BTreeMap<u64, (usize, usize)> = BTreeMap::new();
            for (index, branch) in outline.branches.iter().enumerate() {
                ends.entry(branch.parent)
                    .and_modify(|end| end.1 = index)
                    .or_insert((index, index));
            }
            for (first, last) in ends.into_values() {
                outline.branches[first].first = true;
                outline.branches[last].last = true;
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
            if node.table.is_some() {
                node.clamp_list();
            }
            host.frame.invalidate();
            Ok(())
        })
    }
}
