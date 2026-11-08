//! 上次成功提交的节点覆盖/版本；只保存几何，不保存像素、字符串或控件内容副本。
use super::{Node, Nodes, Tree};
use canvas::Rect;
use classic_ui::Painter;
use std::time::Instant;

/// 每节点最后一次成功提交的绘制基线；只有 presented 更新，等待/失败不消费变更。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Snapshot {
    pub(super) clip: Rect,
    pub(super) overlay: Rect,
    revision: u64,
    focused: bool,
    pressed: bool,
    pointer: Option<(i32, i32)>,
}
#[derive(Clone, Copy)]
struct Visual {
    focus: Option<u64>,
    armed: Option<u64>,
    pointer: (i32, i32),
    bounds: Rect,
}
impl Snapshot {
    fn of(id: u64, node: &Node, visual: Visual, painter: &Painter<'_>, now: Instant) -> Self {
        let mut overlay = Rect::default();
        if let Some(menu) = &node.menu {
            for rect in menu.state.popup_rects() {
                overlay = cover(overlay, rect);
            }
        }
        if node.combo.as_ref().is_some_and(|combo| combo.open) {
            overlay = cover(overlay, node.combo_list());
        }
        if node.visible()
            && let Some(toolbar) = &node.toolbar
            && let Some(rect) = toolbar.tip_bounds(painter, visual.pointer, visual.bounds, now)
        {
            overlay = cover(overlay, rect);
        }
        Self {
            clip: if node.visible() {
                node.clip
            } else {
                Rect::default()
            },
            overlay: overlay.intersect(visual.bounds),
            revision: node.revision,
            focused: visual.focus == Some(id),
            pressed: visual.armed == Some(id) && node.contains(visual.pointer.0, visual.pointer.1),
            pointer: node.menu.as_ref().map(|_| visual.pointer),
        }
    }
}
fn cover(a: Rect, b: Rect) -> Rect {
    if a.is_empty() {
        return b;
    }
    if b.is_empty() {
        return a;
    }
    let (x, y) = (a.x.min(b.x), a.y.min(b.y));
    Rect::new(
        x,
        y,
        a.right().max(b.right()).saturating_sub(x),
        a.bottom().max(b.bottom()).saturating_sub(y),
    )
}
impl Tree {
    fn visual(&self, keyboard_focus: bool) -> Visual {
        Visual {
            focus: self.focus.filter(|_| keyboard_focus),
            armed: self.armed,
            pointer: self.pointer,
            bounds: self
                .layout_bounds
                .map_or_else(Rect::default, |(bounds, _)| bounds),
        }
    }
    fn cues(&self) -> bool {
        self.menubar
            .and_then(|id| self.nodes.get(&id))
            .and_then(|node| node.menu.as_ref())
            .is_some_and(|menu| menu.state.cues_visible())
    }
    /// 相对上次成功提交的旧/新覆盖。移除项、焦点和覆盖层也参与，不能只看仍存在的脏节点。
    pub(crate) fn damage(
        &self,
        painter: &Painter<'_>,
        keyboard_focus: bool,
        now: Instant,
    ) -> Vec<Rect> {
        let visual = self.visual(keyboard_focus);
        let mut damage = self.retired.clone();
        if self.cues() != self.painted_cues {
            damage.push(visual.bounds);
        }
        for (&id, node) in self.nodes.iter() {
            let current = Snapshot::of(id, node, visual, painter, now);
            if node.painted == Some(current) {
                continue;
            }
            if let Some(previous) = node.painted {
                damage.extend([previous.clip, previous.overlay]);
            }
            damage.extend([current.clip, current.overlay]);
        }
        damage.retain(|rect| !rect.is_empty());
        damage
    }
    /// 仅在 Display 完整提交成功后确认基线；与计算/绘制共用同一个时刻，不能提前确认刚到期的提示。
    pub(crate) fn presented(&mut self, painter: &Painter<'_>, keyboard_focus: bool, now: Instant) {
        let visual = self.visual(keyboard_focus);
        self.painted_cues = self.cues();
        self.retired.clear();
        for (&id, node) in &mut self.nodes.entries {
            node.painted = Some(Snapshot::of(id, node, visual, painter, now));
        }
    }
}
impl Nodes {
    /// 非可视计时推进不改变绘制版本；普通输入借用仍经 get_mut/matching_mut。
    pub(crate) fn animate(&mut self, now: Instant, last: Instant) -> bool {
        let mut repaint = false;
        for node in self.entries.values_mut() {
            let mut changed = false;
            if let Some(rebar) = &mut node.rebar {
                changed |= rebar.tick(now);
            }
            if let Some(menu) = &mut node.menu {
                changed |= menu.state.tick(now);
            }
            if let Some(toolbar) = &mut node.toolbar {
                changed |= toolbar.tick(now, last);
            }
            if changed {
                node.changed();
                repaint = true;
            }
        }
        repaint
    }
}
