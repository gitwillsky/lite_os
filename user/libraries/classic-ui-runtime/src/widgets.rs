//! 保留式控件树：交互、文本和排版结果在运行库中只有一个 owner。
use canvas::Rect;
use classic_ui::{Edit, Scroll};
use classic_ui_abi::{self as abi, state, widget};
use std::{collections::BTreeMap, io, rc::Rc};

/// 一行的（小图标, 大图标）；列表/树与大图标视图分别取用，均可缺省。
pub type RowIcons = (
    Option<Rc<classic_ui::Bitmap>>,
    Option<Rc<classic_ui::Bitmap>>,
);

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
    pub row_icons: BTreeMap<u64, RowIcons>,
    pub calendar: Option<super::calendar_control::Calendar>,
    pub rows: Vec<(u64, String)>,
    pub selected: Option<u64>,
    pub tabs: super::tab_strip::TabStrip,
    pub row_height: i32,
    pub drawing: Vec<Command>,
    pub range: super::range::RangeState,
    pub position: Scroll,
    pub menu: Option<super::menu::MenuModel>,
    pub table: Option<super::table::Table>,
    pub outline: Option<super::outline::Outline>,
    pub toolbar: Option<super::toolbar::Toolbar>,
    pub rebar: Option<super::rebar::Rebar>,
    pub combo: Option<super::combo::Combo>,
    /// 应用指定的字体：(用途, 磅值, 粗细)，0 表示沿用用途字体；None 用默认消息字体。
    pub font: Option<(classic_ui::FontRole, u16, u16)>,
    /// 状态栏分格宽度（SB_SETPARTS 式，-1 占满剩余）；空表示按文字宽度分格。
    pub status_parts: Vec<i32>,
    /// 容器背景：底色与左上角不平铺的位图（CSS `background: color url() no-repeat`）；None 透明。
    pub background: Option<(Option<classic_theme::Rgb>, Option<Rc<classic_ui::Bitmap>>)>,
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
        if !(widget::COLUMN..=widget::COMBO).contains(&spec.kind) {
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
            edit: matches!(spec.kind, widget::EDIT | widget::COMBO).then(|| Edit::new(&text)),
            editor: (spec.kind == widget::TEXT_EDITOR)
                .then(|| super::text_editor::TextEditor::new(&text)),
            text: if matches!(
                spec.kind,
                widget::EDIT | widget::TEXT_EDITOR | widget::COMBO
            ) {
                String::new()
            } else {
                text
            },
            rows: Vec::new(),
            selected: None,
            tabs: Default::default(),
            row_height: 24,
            drawing: Vec::new(),
            range: super::range::RangeState::default(),
            position: Scroll::default(),
            menu: matches!(spec.kind, widget::MENU_BAR | widget::POPUP_MENU)
                .then(super::menu::MenuModel::new),
            table: matches!(spec.kind, widget::LIST | widget::TREE).then(super::table::Table::new),
            outline: matches!(spec.kind, widget::TREE | widget::COMBO)
                .then(super::outline::Outline::new),
            toolbar: (spec.kind == widget::TOOLBAR).then(Default::default),
            rebar: None,
            combo: (spec.kind == widget::COMBO).then(Default::default),
            font: None,
            status_parts: Vec::new(),
            background: None,
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
                    | widget::COMBO
                    | widget::LINK
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
    /// 应用为标签/链接指定的逻辑字体（由系统用途字体派生）；未指定返回 None。
    pub fn logical_font(
        &self,
        system: &classic_theme::SystemFonts,
    ) -> Option<classic_theme::LogicalFont> {
        let (role, points, weight) = self.font?;
        let mut font = classic_ui::Fonts::logical(system, role).clone();
        if points != 0 {
            font.points = points;
        }
        if weight != 0 {
            font.weight = weight;
        }
        Some(font)
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
            | state::TAB_INPUT
            | state::ALIGN_RIGHT)
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
    /// 窗口当前允许尺寸手柄（可缩放且未最大化/全屏）；由窗口 owner 每次布局时写入，缺少它手柄会在最大化时仍显示。
    pub size_grip: bool,
    /// 上一次推进定时状态的时刻；工具提示据此只在越过显示时刻的那一次请求重绘。
    pub animated: std::time::Instant,
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
            size_grip: false,
            animated: std::time::Instant::now(),
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
                    widget::COLUMN | widget::ROW | widget::TABS | widget::GROUP | widget::REBAR
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

impl Node {
    /// 状态栏右下角尺寸手柄的区域：一个滚动条宽、与状态栏同高。
    pub fn grip_area(&self) -> Rect {
        let width = classic_ui::SCROLLBAR_WIDTH;
        Rect::new(
            self.rect.right() - width,
            self.rect.y,
            width,
            self.rect.height,
        )
    }
}

impl Tree {
    /// 尺寸手柄矩形：窗口允许时，位于客户区右下角的可见状态栏的手柄区域。
    pub fn grip_rect(&self) -> Option<Rect> {
        let (bounds, _) = self.layout_bounds?;
        if !self.size_grip {
            return None;
        }
        self.nodes
            .values()
            .find(|node| {
                node.kind == widget::STATUS
                    && node.visible()
                    && !node.clip.is_empty()
                    && node.rect.right() == bounds.right()
                    && node.rect.bottom() == bounds.bottom()
            })
            .map(Node::grip_area)
    }
}
