//! 普通容器的纵向视口：Node.position 唯一持有像素偏移，range 只投影排版得到的内容和页高。
use super::widgets::{Node, Tree};
use canvas::Rect;
use classic_ui::{EventKind, SCROLLBAR_WIDTH};
use classic_ui_abi::{state, widget};

impl Node {
    pub(super) fn scrolling(&self) -> bool {
        self.kind == widget::COLUMN && self.state & state::VSCROLL != 0
    }
    pub(super) fn viewport(&self) -> Rect {
        let mut rect = self.rect.inset(self.inset());
        if self.scrolling() && self.range.maximum >= self.range.page {
            rect.width = (rect.width - SCROLLBAR_WIDTH).max(0);
        }
        rect
    }
    pub(super) fn scroll_bar(&self) -> Option<Rect> {
        if !self.scrolling() || self.range.maximum < self.range.page {
            return None;
        }
        let rect = self.rect.inset(self.inset());
        let width = rect.width.clamp(0, SCROLLBAR_WIDTH);
        Some(Rect::new(rect.right() - width, rect.y, width, rect.height))
    }
}
impl Tree {
    /// 只有滚动祖先需要排版后露出焦点；普通窗口切换焦点仍只失效焦点外观。
    pub(super) fn request_reveal(&mut self, mut id: u64) {
        let target = id;
        while let Some(node) = self.nodes.get(&id) {
            if node.scrolling() {
                self.reveal = Some(target);
                self.layout_dirty = true;
                break;
            }
            id = node.parent;
        }
    }
    /// 可滚动区域外的控件仍参与 Tab 顺序；隐藏、禁用和非当前页始终不参与。
    pub(super) fn focus_available(&self, mut id: u64) -> bool {
        let target = id;
        let mut scrolling = false;
        while id != 0 {
            let Some(node) = self.nodes.get(&id) else {
                return false;
            };
            if !node.visible() || !node.enabled() {
                return false;
            }
            scrolling |= node.scrolling();
            if let Some(parent) = self.nodes.get(&node.parent)
                && parent.kind == widget::TABS
                && parent.selected != Some(id)
            {
                return false;
            }
            id = node.parent;
        }
        scrolling || self.active(target)
    }
    /// 排版后消费一次显式焦点请求，逐层滚入；滚轮只改变视口，不反向改变键盘焦点。
    pub(super) fn reveal_focus(&mut self) -> bool {
        let Some(id) = self.reveal.take().filter(|id| self.focus_available(*id)) else {
            return false;
        };
        let mut target = self.nodes[&id].rect;
        let mut parent = self.nodes[&id].parent;
        let mut changed = false;
        while parent != 0 {
            let node = self.nodes.get_mut(&parent).expect("focus ancestor");
            parent = node.parent;
            if !node.scrolling() {
                continue;
            }
            let page = node.viewport();
            let delta = if target.y < page.y || target.height > page.height {
                target.y - page.y
            } else {
                (target.bottom() - page.bottom()).max(0)
            };
            let before = node.position.position;
            node.position.position = before.saturating_add(delta);
            node.position
                .clamp(node.range.maximum + 1 - node.range.page);
            let movement = node.position.position - before;
            target.y = target.y.saturating_sub(movement);
            changed |= movement != 0;
        }
        changed
    }
    pub(super) fn scroll_input(&mut self, input: &EventKind, line: i32) -> bool {
        if let Some(id) = self
            .armed
            .filter(|id| self.nodes[id].scrolling() && !self.active(*id))
        {
            self.nodes
                .get_mut(&id)
                .expect("unavailable viewport")
                .position
                .release();
            self.armed = None;
        }
        let mut id = match input {
            EventKind::PointerMove { .. }
            | EventKind::PointerEnter { .. }
            | EventKind::Button { pressed: false, .. }
            | EventKind::PointerLeave
            | EventKind::Focus(false) => self.armed,
            EventKind::Button {
                code: 0x110,
                pressed: true,
                ..
            }
            | EventKind::Wheel { .. } => self.hit(),
            _ => None,
        };
        while let Some(candidate) = id {
            let node = &self.nodes[&candidate];
            if node.scrolling() {
                let captured = self.armed == Some(candidate);
                let bar = node.scroll_bar();
                if captured
                    || matches!(input, EventKind::Wheel { .. }) && bar.is_some()
                    || bar.is_some_and(|rect| {
                        rect.x <= self.pointer.0
                            && self.pointer.0 < rect.right()
                            && rect.y <= self.pointer.1
                            && self.pointer.1 < rect.bottom()
                    })
                {
                    let node = self.nodes.get_mut(&candidate).expect("scroll target");
                    let before = node.position.position;
                    let content = node.range.maximum + 1;
                    let page = node.range.page;
                    match input {
                        EventKind::Wheel { vertical, .. } => {
                            node.position.wheel(*vertical, line, content - page)
                        }
                        EventKind::Button { pressed: true, .. } => {
                            self.armed = Some(candidate);
                            if let Some(bar) = bar {
                                node.position
                                    .press(bar, true, (content, page, line), self.pointer);
                            }
                        }
                        EventKind::PointerMove { .. } | EventKind::PointerEnter { .. } => {
                            if let Some(bar) = bar {
                                node.position.drag(bar, true, (content, page), self.pointer);
                            }
                        }
                        _ => {
                            node.position.release();
                            self.armed = None;
                        }
                    }
                    self.layout_dirty |= before != node.position.position;
                    return true;
                }
            }
            id = (node.parent != 0).then_some(node.parent);
        }
        false
    }
}
