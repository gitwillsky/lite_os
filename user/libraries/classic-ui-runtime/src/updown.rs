//! comctl32 微调钮（msctls_updown32，UDS_WRAP）：上下两个半高按钮，按下步进 1 并回绕，按住后自动重复。
//! 数值由 Node.range 与 Node.position 持有（与滑块共用范围接口）；伙伴编辑框的文字由应用按 CHANGED 更新。
use super::widgets::{Node, Tree};
use canvas::Rect;
use classic_theme::SystemColor as C;
use classic_ui::{ButtonState, EventKind, Painter};
use classic_ui_abi::{self as abi, event, widget};
use std::time::{Duration, Instant};

/// 按住后首次自动重复的延时与之后的间隔（键盘重复的默认节奏）。
const FIRST_REPEAT: Duration = Duration::from_millis(500);
const REPEAT: Duration = Duration::from_millis(100);
const UP: [&str; 3] = ["..#..", ".###.", "#####"];
const DOWN: [&str; 3] = ["#####", ".###.", "..#.."];

/// 按住的方向（+1 上、-1 下）与下一次自动重复的时刻。
#[derive(Default)]
pub struct UpDown {
    held: Option<(i32, Instant)>,
}

impl UpDown {
    pub fn deadline(&self) -> Option<Instant> {
        self.held.map(|(_, due)| due)
    }
}

impl Node {
    fn updown_halves(&self) -> (Rect, Rect) {
        let top = self.rect.height / 2;
        (
            Rect::new(self.rect.x, self.rect.y, self.rect.width, top),
            Rect::new(
                self.rect.x,
                self.rect.y + top,
                self.rect.width,
                self.rect.height - top,
            ),
        )
    }

    /// 步进 `direction` 并回绕；返回新值。
    fn updown_step(&mut self, direction: i32) -> i32 {
        let extent = self.range.maximum - self.range.minimum + 1;
        self.position.position = (self.position.position + direction).rem_euclid(extent.max(1));
        self.range.minimum + self.position.position
    }

    pub fn paint_updown(&self, p: &mut Painter<'_>, disabled: bool) {
        let held = self
            .updown
            .as_ref()
            .and_then(|updown| updown.held)
            .map(|(direction, _)| direction);
        for (half, direction, glyph) in [
            (self.updown_halves().0, 1, &UP),
            (self.updown_halves().1, -1, &DOWN),
        ] {
            let pressed = held == Some(direction);
            p.push_button(
                half,
                "",
                ButtonState {
                    pressed,
                    disabled,
                    ..Default::default()
                },
            );
            let offset = i32::from(pressed);
            let x = half.x + (half.width - 5) / 2 + offset;
            let y = half.y + (half.height - 3) / 2 + offset;
            if disabled {
                p.glyph(x + 1, y + 1, glyph, C::ButtonHilight);
                p.glyph(x, y, glyph, C::ButtonShadow);
            } else {
                p.glyph(x, y, glyph, C::ButtonText);
            }
        }
    }
}

fn changed(window: u64, id: u64, value: i32) -> abi::Event {
    abi::Event {
        window,
        node: id,
        kind: event::CHANGED,
        value: i64::from(value) as u64,
        ..Default::default()
    }
}

impl Tree {
    /// 微调钮的按下、释放；返回是否消费。
    pub fn updown_input(
        &mut self,
        window: u64,
        input: &EventKind,
        out: &mut Vec<abi::Event>,
    ) -> bool {
        match input {
            EventKind::Button {
                code: 0x110,
                pressed: true,
                ..
            } => {
                let Some(id) = self
                    .hit()
                    .filter(|id| self.nodes[id].kind == widget::UPDOWN && self.active(*id))
                else {
                    return false;
                };
                let pointer = self.pointer;
                let node = self.nodes.get_mut(&id).expect("updown node");
                let (top, _) = node.updown_halves();
                let direction = if pointer.1 < top.bottom() { 1 } else { -1 };
                let value = node.updown_step(direction);
                node.updown.get_or_insert_with(UpDown::default).held =
                    Some((direction, Instant::now() + FIRST_REPEAT));
                self.armed = Some(id);
                out.push(changed(window, id, value));
                true
            }
            EventKind::Button {
                code: 0x110,
                pressed: false,
                ..
            }
            | EventKind::PointerLeave
            | EventKind::Focus(false) => {
                let Some(id) = self
                    .armed
                    .filter(|id| self.nodes[id].kind == widget::UPDOWN)
                else {
                    return false;
                };
                if let Some(updown) = self
                    .nodes
                    .get_mut(&id)
                    .and_then(|node| node.updown.as_mut())
                {
                    updown.held = None;
                }
                self.armed = None;
                true
            }
            _ => false,
        }
    }

    /// 到期的自动重复：步进并发出 CHANGED；返回是否需要重绘。
    pub fn repeat_updowns(&mut self, window: u64, now: Instant, out: &mut Vec<abi::Event>) -> bool {
        let mut repaint = false;
        for (id, node) in &mut self.nodes {
            let Some((direction, due)) = node.updown.as_ref().and_then(|updown| updown.held) else {
                continue;
            };
            if due > now {
                continue;
            }
            let value = node.updown_step(direction);
            if let Some(updown) = node.updown.as_mut() {
                updown.held = Some((direction, due + REPEAT));
            }
            out.push(changed(window, *id, value));
            repaint = true;
        }
        repaint
    }
}
