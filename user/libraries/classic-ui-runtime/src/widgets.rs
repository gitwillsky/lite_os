//! 保留式控件树：交互、文本和排版结果在运行库中只有一个 owner。
use canvas::Rect;
use classic_ui::{Edit, Scroll};
use classic_ui_abi::{self as abi, state, widget};
use std::{collections::BTreeMap, io, ops::Deref, rc::Rc};
mod damage;
use damage::Snapshot;

/// 一行的（小图标, 大图标）；列表/树与大图标视图分别取用，均可缺省。
pub type RowIcons = (
    Option<Rc<classic_ui::Bitmap>>,
    Option<Rc<classic_ui::Bitmap>>,
);

pub struct Node {
    // OWNER: 只经 Nodes 的可变入口推进；与已成功提交的 Snapshot 比较，不代表布局失效。
    revision: u64,
    painted: Option<Snapshot>,
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
    /// Canvas 的媒体帧（设备像素）；缺失时只绘显示列表。
    pub frame: Option<Box<classic_ui::Bitmap>>,
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
    /// 微调钮的按住状态。
    pub updown: Option<super::updown::UpDown>,
    /// 横向滑块的凹槽自绘；None 画标准凹陷凹槽。
    pub channel: Option<SliderChannel>,
}

/// 滑块凹槽自绘的位图区域：x 从 `source_x` 起、宽 `width`，行程等于该宽度。
pub struct SliderChannel {
    pub bitmap: Rc<classic_ui::Bitmap>,
    pub source_x: i32,
    pub width: i32,
}

pub struct Command {
    pub kind: u32,
    pub rect: Rect,
    pub color: u32,
    pub bold: bool,
    pub text: String,
    /// kind 8 在设置时按 `text` 解析出的系统位图；缺图为 None。
    pub bitmap: Option<Rc<classic_ui::Bitmap>>,
}

/// 显示列表比较只看应用提交的命令字段：`bitmap` 由 `text` 中的位图名唯一决定，
/// 不能按指针比较，否则资源缓存逐出后同一命令会被误判为变化并整块重绘。
impl PartialEq for Command {
    fn eq(&self, other: &Self) -> bool {
        (self.kind, self.rect, self.color, self.bold, &self.text)
            == (other.kind, other.rect, other.color, other.bold, &other.text)
    }
}

impl Node {
    pub fn new(parent: u64, mut spec: abi::Widget, text: String) -> io::Result<Self> {
        if !(widget::COLUMN..=widget::UPDOWN).contains(&spec.kind) {
            return Err(invalid("unknown widget kind"));
        }
        if spec.kind == widget::POPUP_MENU {
            spec.layout.width = 0;
            spec.layout.height = 0;
            spec.layout.grow = 0;
        }
        validate_layout(spec.layout)?;
        validate_state(spec.kind, spec.state)?;
        Ok(Self {
            revision: 1,
            painted: None,
            parent,
            label_edit: None,
            image: None,
            row_icons: BTreeMap::new(),
            frame: None,
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
            updown: None,
            channel: None,
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

    /// 滑块绘制与命中共用的几何：无刻度（TBS_NOTICKS）时滑块高度固定，在被布局拉伸的矩形里沿垂直轨道方向居中，
    /// 与同一行的按钮对齐；有刻度时占满整个矩形。绘制和输入必须用同一个矩形，否则点中的不是看到的滑块。
    pub fn slider_rect(&self) -> Rect {
        let rect = self.rect;
        if self.state & state::TRACK_NO_TICKS == 0 {
            return rect;
        }
        let across = classic_ui::Trackbar::THUMB_ACROSS;
        if self.range.vertical {
            Rect::new(
                rect.x + (rect.width - across).max(0) / 2,
                rect.y,
                across.min(rect.width),
                rect.height,
            )
        } else {
            Rect::new(
                rect.x,
                rect.y + (rect.height - across).max(0) / 2,
                rect.width,
                across.min(rect.height),
            )
        }
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
pub fn validate_state(kind: u32, value: u32) -> io::Result<()> {
    if value
        & !(state::DISABLED
            | state::CHECKED
            | state::HIDDEN
            | state::WRAP
            | state::PASSWORD
            | state::DEFAULT
            | state::TAB_INPUT
            | state::ALIGN_RIGHT
            | state::CLIENT_EDGE
            | state::BORDERLESS
            | state::DROPDOWN_LIST
            | state::FILE_DRAG
            | state::EDIT_LABELS
            | state::KEYBOARD_CONTEXT_MENU
            | state::TRACK_NO_TICKS
            | state::TRACK_BOTH
            | state::VSCROLL
            | state::READ_ONLY)
        != 0
    {
        return Err(invalid("unknown widget state"));
    }
    if value & state::VSCROLL != 0 && kind != widget::COLUMN {
        return Err(invalid("vertical viewport requires a column"));
    }
    if value & state::READ_ONLY != 0 && kind != widget::TEXT_EDITOR {
        return Err(invalid("read-only state requires a multiline editor"));
    }
    Ok(())
}

/// 控件集合只有只读 Deref；所有可变借用都经过版本入口，不能从公开 map 绕过绘制失效。
#[derive(Default)]
pub struct Nodes {
    entries: BTreeMap<u64, Node>,
}
impl Deref for Nodes {
    type Target = BTreeMap<u64, Node>;
    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}
impl Nodes {
    pub fn get_mut(&mut self, id: &u64) -> Option<&mut Node> {
        let node = self.entries.get_mut(id)?;
        node.changed();
        Some(node)
    }
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut Node> {
        for node in self.entries.values_mut() {
            node.changed();
        }
        self.entries.values_mut()
    }
    /// 外部图像资源到达时内容字段不变，但依赖该资源的控件像素已经失效。
    pub fn invalidate_where(&mut self, mut selected: impl FnMut(&Node) -> bool) {
        for node in self.entries.values_mut().filter(|node| selected(node)) {
            node.changed();
        }
    }
    /// 先只读筛选再借出可变节点，不能因定时扫描或遍历无关控件而把全树标成变化。
    pub fn matching_mut(
        &mut self,
        mut selected: impl FnMut(&Node) -> bool,
    ) -> impl Iterator<Item = (&u64, &mut Node)> {
        self.entries
            .iter_mut()
            .filter(move |(_, node)| selected(node))
            .map(|(id, node)| {
                node.changed();
                (id, node)
            })
    }
}
impl Node {
    fn changed(&mut self) {
        self.revision = self
            .revision
            .checked_add(1)
            .expect("node paint revision exhausted");
    }
}

pub struct Tree {
    pub nodes: Nodes,
    /// 已提交节点被删除时仍须擦除它的旧覆盖；只在窗口成功提交后清空。
    retired: Vec<Rect>,
    painted_cues: bool,
    next: u64,
    root_order: Vec<u64>,
    pub menubar: Option<u64>,
    pub popup: Option<u64>,
    pub focus: Option<u64>,
    /// 显式焦点请求在下一次排版后滚入视口；消费后清空，普通滚轮不把旧焦点拉回。
    pub reveal: Option<u64>,
    pub armed: Option<u64>,
    pub file_press: Option<super::interaction::FilePress>,
    /// 一次选中标签的单击（控件、行与等待双击的截止时刻）。
    pub rename_click: Option<(u64, u64, std::time::Instant)>,
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
            nodes: Nodes::default(),
            retired: Vec::new(),
            painted_cues: false,
            next: 1,
            root_order: Vec::new(),
            menubar: None,
            popup: None,
            focus: None,
            reveal: None,
            armed: None,
            file_press: None,
            rename_click: None,
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
        self.nodes.entries.insert(id, node);
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
            .entries
            .remove(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown widget"))?;
        if let Some(painted) = node.painted {
            for rect in [painted.clip, painted.overlay] {
                if !rect.is_empty() {
                    self.retired.push(rect);
                }
            }
        }
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
        if !self.focus_available(id) {
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
        self.request_reveal(order[next]);
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
            .or_else(|| {
                // 微调钮可点击但不是 Tab 停靠点（msctls_updown32 没有 WS_TABSTOP）。
                let clickable = node.kind == widget::UPDOWN && node.enabled() && node.visible();
                (node.focusable() || clickable || node.scrolling()).then_some(id)
            })
    }
}

impl Node {
    /// 是否接受文字输入与光标：编辑框，以及非下拉列表式的组合框。
    pub fn text_editable(&self) -> bool {
        self.kind == widget::EDIT
            || (self.kind == widget::COMBO && self.state & state::DROPDOWN_LIST == 0)
    }

    /// 布局内边距：layout.padding 加上 CLIENT_EDGE 的 2 像素边框。
    pub fn inset(&self) -> i32 {
        let edge = if matches!(self.kind, widget::COLUMN | widget::ROW)
            && self.state & state::CLIENT_EDGE != 0
        {
            classic_theme::EDGE_WIDTH
        } else {
            0
        };
        self.layout.padding + edge
    }

    /// 列表的边框宽度：默认凹陷 2 像素，BORDERLESS 为 0；绘制与几何共用。
    pub fn list_edge(&self) -> i32 {
        if self.state & state::BORDERLESS != 0 {
            0
        } else {
            classic_theme::EDGE_WIDTH
        }
    }

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
