//! 保留式控件树：交互、文本和排版结果在运行库中只有一个 owner。
use canvas::Rect;
use classic_ui::{Edit, Scroll};
use classic_ui_abi::{self as abi, state, widget};
use std::{collections::BTreeMap, io, rc::Rc};

pub struct Node {
    pub parent: u64,
    pub children: Vec<u64>,
    pub kind: u32,
    pub state: u32,
    pub group: u64,
    pub layout: abi::Layout,
    pub rect: Rect,
    pub clip: Rect,
    pub text: String,
    pub edit: Option<Edit>,
    pub label_edit: Option<super::label_edit::LabelEdit>,
    pub editor: Option<super::text_editor::TextEditor>,
    pub image: Option<Rc<classic_ui::Bitmap>>,
    pub row_icons: BTreeMap<
        u64,
        (
            Option<Rc<classic_ui::Bitmap>>,
            Option<Rc<classic_ui::Bitmap>>,
        ),
    >,
    pub calendar: Option<super::calendar_control::Calendar>,
    pub rows: Vec<(u64, String)>,
    pub selected: Option<u64>,
    pub tab_headers: Vec<(u64, Rect)>,
    pub row_height: i32,
    pub drawing: Vec<Command>,
    pub range: super::range::RangeState,
    pub position: Scroll,
    pub menu: Option<super::menu::MenuModel>,
    pub table: Option<super::table::Table>,
    pub outline: Option<super::outline::Outline>,
}

#[derive(PartialEq, Eq)]
pub struct Command {
    pub kind: u32,
    pub rect: Rect,
    pub color: u32,
    pub bold: bool,
    pub text: String,
}

impl Node {
    pub fn new(parent: u64, mut spec: abi::Widget, text: String) -> io::Result<Self> {
        if !(widget::COLUMN..=widget::POPUP_MENU).contains(&spec.kind) {
            return Err(invalid("unknown widget kind"));
        }
        if spec.kind == widget::POPUP_MENU {
            spec.layout.width = 0;
            spec.layout.height = 0;
            spec.layout.grow = 0;
        }
        validate_layout(spec.layout)?;
        validate_state(spec.state)?;
        Ok(Self {
            parent,
            label_edit: None,
            image: None,
            row_icons: BTreeMap::new(),
            calendar: (spec.kind == widget::CALENDAR).then(super::calendar_control::Calendar::new),
            children: Vec::new(),
            kind: spec.kind,
            state: spec.state,
            group: spec.group,
            layout: spec.layout,
            rect: Rect::new(0, 0, 0, 0),
            clip: Rect::new(0, 0, 0, 0),
            edit: (spec.kind == widget::EDIT).then(|| Edit::new(&text)),
            editor: (spec.kind == widget::TEXT_EDITOR)
                .then(|| super::text_editor::TextEditor::new(&text)),
            text: if matches!(spec.kind, widget::EDIT | widget::TEXT_EDITOR) {
                String::new()
            } else {
                text
            },
            rows: Vec::new(),
            selected: None,
            tab_headers: Vec::new(),
            row_height: 24,
            drawing: Vec::new(),
            range: super::range::RangeState::default(),
            position: Scroll::default(),
            menu: matches!(spec.kind, widget::MENU_BAR | widget::POPUP_MENU)
                .then(super::menu::MenuModel::new),
            table: matches!(spec.kind, widget::LIST | widget::TREE).then(super::table::Table::new),
            outline: (spec.kind == widget::TREE).then(super::outline::Outline::new),
        })
    }
    pub fn text(&self) -> &str {
        self.edit.as_ref().map_or(&self.text, Edit::text)
    }
    pub fn visible(&self) -> bool {
        self.state & state::HIDDEN == 0
    }
    pub fn enabled(&self) -> bool {
        self.state & state::DISABLED == 0
    }
    pub fn focusable(&self) -> bool {
        self.enabled()
            && self.visible()
            && matches!(
                self.kind,
                widget::BUTTON
                    | widget::EDIT
                    | widget::TEXT_EDITOR
                    | widget::CALENDAR
                    | widget::CHECKBOX
                    | widget::RADIO
                    | widget::LIST
                    | widget::TREE
                    | widget::TABS
                    | widget::SCROLLBAR
                    | widget::SLIDER
                    | widget::CANVAS
            )
    }
    pub fn contains(&self, x: i32, y: i32) -> bool {
        self.visible()
            && self.enabled()
            && x >= self.clip.x
            && y >= self.clip.y
            && x < self.clip.right()
            && y < self.clip.bottom()
    }
}

pub fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
pub fn validate_layout(l: abi::Layout) -> io::Result<()> {
    if l.width < -1
        || l.height < -1
        || [l.min_width, l.min_height, l.padding, l.gap]
            .iter()
            .any(|v| *v < 0 || *v > 32768)
        || l.width > 32768
        || l.height > 32768
        || l.grow > 65535
    {
        return Err(invalid("invalid layout extent"));
    }
    Ok(())
}
pub fn validate_state(value: u32) -> io::Result<()> {
    if value
        & !(state::DISABLED
            | state::CHECKED
            | state::HIDDEN
            | state::WRAP
            | state::PASSWORD
            | state::DEFAULT
            | state::TAB_INPUT)
        != 0
    {
        return Err(invalid("unknown widget state"));
    }
    Ok(())
}

pub struct Tree {
    pub nodes: BTreeMap<u64, Node>,
    next: u64,
    root_order: Vec<u64>,
    pub menubar: Option<u64>,
    pub popup: Option<u64>,
    pub focus: Option<u64>,
    pub armed: Option<u64>,
    pub pointer: (i32, i32),
    /// Tree 独占布局失效；内容/规则或 configure 改变才重新排版，普通指针移动复用现有几何。
    pub layout_dirty: bool,
    pub layout_bounds: Option<(Rect, i32)>,
}
impl Tree {
    pub fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            next: 1,
            root_order: Vec::new(),
            menubar: None,
            popup: None,
            focus: None,
            armed: None,
            pointer: (-1, -1),
            layout_dirty: true,
            layout_bounds: None,
        }
    }
    pub fn insert(&mut self, parent: u64, spec: abi::Widget, text: String) -> io::Result<u64> {
        if self.nodes.len() >= 16384 {
            return Err(invalid("widget limit reached"));
        }
        let mut ancestor = parent;
        for depth in 0..=64 {
            if ancestor == 0 {
                break;
            }
            if depth == 64 {
                return Err(invalid("layout nesting limit reached"));
            }
            ancestor = self
                .nodes
                .get(&ancestor)
                .ok_or_else(|| invalid("unknown parent"))?
                .parent;
        }
        if parent != 0
            && !self.nodes.get(&parent).is_some_and(|p| {
                matches!(
                    p.kind,
                    widget::COLUMN | widget::ROW | widget::TABS | widget::GROUP
                )
            })
        {
            return Err(invalid("parent is not a layout container"));
        }
        if spec.kind == widget::MENU_BAR && self.menubar.is_some() {
            return Err(invalid("window already has a menu bar"));
        }
        if spec.kind == widget::POPUP_MENU && self.popup.is_some() {
            return Err(invalid("window already has a popup menu"));
        }
        let node = Node::new(parent, spec, text)?;
        let id = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| invalid("widget identities exhausted"))?;
        self.nodes.insert(id, node);
        self.layout_dirty = true;
        if spec.kind == widget::MENU_BAR {
            self.menubar = Some(id);
        }
        if spec.kind == widget::POPUP_MENU {
            self.popup = Some(id);
        }
        if parent == 0 {
            self.root_order.push(id);
        }
        if parent != 0 {
            self.nodes
                .get_mut(&parent)
                .expect("validated parent")
                .children
                .push(id);
        }
        Ok(id)
    }
    pub fn remove(&mut self, id: u64) -> io::Result<()> {
        self.layout_dirty = true;
        let node = self
            .nodes
            .remove(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown widget"))?;
        if node.parent == 0 {
            self.root_order.retain(|root| *root != id);
        }
        if let Some(parent) = self.nodes.get_mut(&node.parent) {
            parent.children.retain(|child| *child != id);
            if parent.selected == Some(id) {
                parent.selected = None;
            }
        }
        for child in node.children {
            self.remove(child)?;
        }
        if self.menubar == Some(id) {
            self.menubar = None;
        }
        if self.popup == Some(id) {
            self.popup = None;
        }
        if self.focus == Some(id) {
            self.focus = None;
        }
        if self.armed == Some(id) {
            self.armed = None;
        }
        Ok(())
    }
    pub fn roots(&self) -> Vec<u64> {
        self.root_order.clone()
    }
    pub fn active(&self, mut id: u64) -> bool {
        while id != 0 {
            let Some(node) = self.nodes.get(&id) else {
                return false;
            };
            if !node.visible() || !node.enabled() || node.clip.is_empty() {
                return false;
            }
            id = node.parent;
        }
        true
    }
    fn focus_order(&self, id: u64, order: &mut Vec<u64>) {
        let node = &self.nodes[&id];
        if !self.active(id) {
            return;
        }
        if node.focusable() {
            order.push(id);
        }
        for child in &node.children {
            self.focus_order(*child, order);
        }
    }
    pub fn focus_next(&mut self, reverse: bool) {
        let mut order = Vec::new();
        for root in &self.root_order {
            self.focus_order(*root, &mut order);
        }
        if order.is_empty() {
            self.focus = None;
            return;
        }
        let index = self
            .focus
            .and_then(|id| order.iter().position(|candidate| *candidate == id));
        let next = match (index, reverse) {
            (Some(i), true) => (i + order.len() - 1) % order.len(),
            (Some(i), false) => (i + 1) % order.len(),
            (None, true) => order.len() - 1,
            (None, false) => 0,
        };
        self.focus = Some(order[next]);
    }
    pub fn hit(&self) -> Option<u64> {
        self.root_order
            .iter()
            .rev()
            .find_map(|id| self.hit_node(*id))
    }
    fn hit_node(&self, id: u64) -> Option<u64> {
        let node = &self.nodes[&id];
        if !node.contains(self.pointer.0, self.pointer.1) {
            return None;
        }
        node.children
            .iter()
            .rev()
            .find_map(|id| self.hit_node(*id))
            .or_else(|| node.focusable().then_some(id))
    }
}
