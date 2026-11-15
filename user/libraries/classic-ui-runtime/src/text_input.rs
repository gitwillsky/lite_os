//! 为常规编辑控件投影 Wayland 输入目标，正文仍由原来的编辑模型独占。

use super::{Context, widgets::Tree};
use canvas::Rect;
use classic_theme::SystemColor as C;
use classic_ui::{Ink, Painter, TextInputEdit, TextPurpose};
use classic_ui_abi::{self as abi, event, state, widget};
use std::io;

impl Tree {
    pub fn text_target(&self, painter: &Painter<'_>) -> Option<(u64, Rect, TextPurpose)> {
        if self.popup.is_some()
            || self
                .menubar
                .and_then(|id| self.nodes.get(&id))
                .and_then(|node| node.menu.as_ref())
                .is_some_and(|menu| menu.state.active())
        {
            return None;
        }
        let id = self.focus?;
        if !self.active(id) {
            return None;
        }
        let node = &self.nodes[&id];
        if node.state & state::PASSWORD != 0 {
            return None;
        }
        let (rect, purpose) = if let Some(caret) =
            node.text_caret.filter(|_| node.kind == widget::CANVAS)
        {
            (
                Rect::new(
                    node.rect.x + caret.x,
                    node.rect.y + caret.y,
                    caret.width,
                    caret.height,
                ),
                TextPurpose::Terminal,
            )
        } else if !node.editing_text() || node.label_edit.as_ref().is_some_and(|edit| edit.pending)
        {
            return None;
        } else if let Some(editor) = &node.editor {
            (editor.caret(node.rect, painter), TextPurpose::Normal)
        } else {
            let edit = node.edit.as_ref()?;
            let rect = node.label_rect().unwrap_or(node.rect);
            (
                painter.edit_caret(rect, edit, node.state & state::BORDERLESS == 0),
                TextPurpose::Normal,
            )
        };
        let rect = rect.intersect(node.clip);
        (!rect.is_empty()).then_some((id, rect, purpose))
    }

    pub fn clear_preedit(&mut self) -> bool {
        let Some((id, _)) = self.preedit.take() else {
            return false;
        };
        // 同一控件的旧文字损伤随 revision 集中恢复；不保存第二份正文。
        let _ = self.nodes.get_mut(&id);
        true
    }

    pub fn apply_text_input(
        &mut self,
        window: u64,
        id: u64,
        edit: &TextInputEdit,
        painter: &Painter<'_>,
    ) -> (Option<abi::Event>, bool) {
        if self.text_target(painter).map(|(target, ..)| target) != Some(id) {
            return (None, self.clear_preedit());
        }
        let preedit = edit
            .preedit
            .as_ref()
            .map(|(text, cursor, _)| (id, (text.clone(), *cursor)));
        let preedit_changed = preedit != self.preedit;
        if preedit_changed {
            self.clear_preedit();
            self.preedit = preedit;
        }
        if edit.commit.is_none() && !preedit_changed {
            return (None, false);
        }
        let node = self.nodes.get_mut(&id).expect("active text target");
        let mut action = None;
        if let Some(value) = &edit.commit {
            if node.kind == widget::CANVAS {
                action = Some(event::TEXT_COMMIT);
            } else if let Some(editor) = &mut node.editor {
                if editor.insert(node.rect, painter, value) {
                    action = Some(event::CHANGED);
                }
            } else if let Some(edit) = &mut node.edit
                && edit.insert_text(value)
                && node.label_edit.is_none()
            {
                action = Some(event::CHANGED);
            }
        }
        let event = action.map(|kind| abi::Event {
            window,
            node: id,
            kind,
            value: if kind == event::TEXT_COMMIT {
                edit.commit.as_ref().map_or(0, |text| text.len() as u64)
            } else {
                0
            },
            ..Default::default()
        });
        (event, true)
    }

    pub fn paint_preedit(&self, painter: &mut Painter<'_>) {
        let Some((id, (text, cursor))) = &self.preedit else {
            return;
        };
        let Some((target, caret, _)) = self.text_target(painter) else {
            return;
        };
        if *id != target {
            return;
        }
        let node = &self.nodes[id];
        let scale = painter.scale();
        let layout = painter.text_layout(text, false);
        let width = ((layout.size().0 + scale - 1) / scale).max(1);
        let extent = Rect::new(caret.x, caret.y, width + 1, caret.height);
        painter.clipped(node.clip, |p| {
            p.fill(extent, C::Window);
            p.paint_text_layout(caret.x, caret.y, &layout, Ink::Color(C::WindowText));
            p.fill(
                Rect::new(caret.x, caret.bottom() - 1, width, 1),
                C::WindowText,
            );
            if let Ok(cursor) = usize::try_from(*cursor)
                && text.is_char_boundary(cursor)
            {
                let offset = layout.caret_x(cursor) / scale;
                p.fill(
                    Rect::new(caret.x + offset, caret.y, 1, caret.height),
                    C::WindowText,
                );
            }
        });
    }
}

impl Context {
    pub(super) fn sync_text_target(&mut self) -> io::Result<()> {
        let focus = self.display.keyboard_focus();
        let selected = self
            .windows
            .iter()
            .find(|(id, host)| Some(host.frame.window()) == focus && !self.blocked(**id));
        let field = if let Some((_, host)) = selected
            && let Some(config) = self.display.configuration(host.frame.window())
        {
            let mut scratch = [0];
            let surface = canvas::Surface::new(&mut scratch, 1, 1, 1).expect("caret measurement");
            let font = self.resources.fonts(config.scale)?;
            let painter = Painter::new(surface, self.resources.theme(), &font, config.scale);
            host.controls()?
                .text_target(&painter)
                .map(|(id, caret, purpose)| (host.frame.window(), id, caret, purpose))
        } else {
            None
        };
        self.display.set_text_input(field)
    }
}
