//! 应用侧文本输入由每个 seat 的协议对象和当前编辑目标共同限定。

use super::{Display, EventKind, WindowId};
use canvas::Rect;
use std::io;
use wayland_wire::{protocol::zwp_text_input_v3 as text, wire::ObjectId};

/// 文本编辑目标的用途。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextPurpose {
    /// 普通可输入文本的控件。
    Normal,
    /// 接收 UTF-8 上屏文本的自绘终端。
    Terminal,
}

/// 在同一个 Wayland done 边界提交的 UTF-8 编辑事务。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextInputEdit {
    /// 光标前、后需删除的 UTF-8 字节数。
    pub delete: (u32, u32),
    /// 已确认的文本，插入或替换当前选区。
    pub commit: Option<String>,
    /// 预编辑文本和 UTF-8 光标范围。空值清除预编辑。
    pub preedit: Option<(String, i32, i32)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Field {
    window: WindowId,
    target: u64,
    caret: Rect,
    purpose: TextPurpose,
}

#[derive(Default)]
pub(super) struct TextInput {
    pub object: Option<ObjectId>,
    focused: Option<WindowId>,
    requested: Option<Field>,
    sent: Option<Field>,
    serial: u32,
    enabled_at: u32,
    pending: TextInputEdit,
}

impl Display {
    /// 选择当前窗口内接受输入法的目标；None 停止文本输入。
    ///
    /// `target` 是本窗口内持久的焦点身份；`caret` 是含装饰的 surface 局部矩形。
    /// 合成器未提供 text-input 时此调用无效果。协议或 socket 错误原样返回。
    pub fn set_text_input(
        &mut self,
        field: Option<(WindowId, u64, Rect, TextPurpose)>,
    ) -> io::Result<()> {
        self.text_input.requested = field.map(|(window, target, caret, purpose)| Field {
            window,
            target,
            caret,
            purpose,
        });
        self.sync_text_input()
    }

    fn sync_text_input(&mut self) -> io::Result<()> {
        let state = &mut self.text_input;
        let Some(object) = state.object else {
            return Ok(());
        };
        let next = state.requested.filter(|field| {
            Some(field.window) == state.focused && self.windows.contains_key(&field.window)
        });
        if next == state.sent {
            return Ok(());
        }
        let identity = |field: Option<Field>| field.map(|f| (f.window, f.target, f.purpose));
        let new_field = identity(next) != identity(state.sent);
        let writer = &mut self.transport.writer;
        if new_field && state.sent.is_some() {
            text::Request::Disable
                .encode(object, writer)
                .map_err(io::Error::other)?;
            text::Request::Commit
                .encode(object, writer)
                .map_err(io::Error::other)?;
            state.serial = state.serial.wrapping_add(1);
            state.pending = TextInputEdit::default();
            // 协议队列里已读取但尚未应用的旧字段消息不能进入下一个字段。
            self.events
                .retain(|event| !matches!(event.kind, EventKind::TextInput { .. }));
        }
        if let Some(field) = next {
            if new_field {
                text::Request::Enable
                    .encode(object, writer)
                    .map_err(io::Error::other)?;
                let purpose = match field.purpose {
                    TextPurpose::Normal => text::ContentPurpose::Normal,
                    TextPurpose::Terminal => text::ContentPurpose::Terminal,
                };
                text::Request::SetContentType { hint: 0, purpose }
                    .encode(object, writer)
                    .map_err(io::Error::other)?;
            }
            let Rect {
                x,
                y,
                width,
                height,
            } = field.caret;
            text::Request::SetCursorRectangle {
                x,
                y,
                width,
                height,
            }
            .encode(object, writer)
            .map_err(io::Error::other)?;
            text::Request::Commit
                .encode(object, writer)
                .map_err(io::Error::other)?;
            state.serial = state.serial.wrapping_add(1);
            if new_field {
                state.enabled_at = state.serial;
            }
        }
        state.sent = next;
        self.transport.flush()
    }

    pub(super) fn handle_text_input(&mut self, event: text::Event) -> io::Result<()> {
        let state = &mut self.text_input;
        match event {
            text::Event::Enter { surface } => {
                state.focused = Some(WindowId(surface));
                state.sent = None;
                state.pending = TextInputEdit::default();
                self.sync_text_input()?;
            }
            text::Event::Leave { surface } => {
                if state.focused == Some(WindowId(surface)) {
                    state.focused = None;
                    state.sent = None;
                    state.pending = TextInputEdit::default();
                    self.events.retain(|event| {
                        event.window != WindowId(surface)
                            || !matches!(event.kind, EventKind::TextInput { .. })
                    });
                }
            }
            text::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                state.pending.preedit = text
                    .filter(|text| !text.is_empty())
                    .map(|text| (text, cursor_begin, cursor_end));
            }
            text::Event::CommitString { text } => state.pending.commit = text,
            text::Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                state.pending.delete = (before_length, after_length);
            }
            text::Event::Done { serial } => {
                let edit = std::mem::take(&mut state.pending);
                if let Some(field) = state.sent
                    && state.serial.wrapping_sub(serial)
                        <= state.serial.wrapping_sub(state.enabled_at)
                {
                    self.repeat = None;
                    self.emit(
                        Some(field.window),
                        EventKind::TextInput {
                            target: field.target,
                            edit,
                        },
                    );
                }
            }
        }
        Ok(())
    }
}
