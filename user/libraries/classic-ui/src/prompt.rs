//! 窗口内的单行输入对话框：标题、说明、一个带标签的输入框和"确定""取消"，用于"运行""打开""另存为"这类只要一个字符串的场合。

use canvas::Rect;
use classic_theme::SystemColor as C;

use crate::{ButtonState, Edit, FrameKind, Ink, Key, Painter, window::CaptionButton};

const WIDTH: i32 = 330;
const HEIGHT: i32 = 130;
const ENTER: u32 = 28;
const ESCAPE: u32 = 1;

pub struct Prompt {
    title: &'static str,
    message: &'static str,
    label: &'static str,
    edit: Edit,
}

impl Prompt {
    /// `message` 可含换行（至多两行显示）；`label` 是输入框左侧的标签，如 `打开(O):`。
    pub fn new(
        title: &'static str,
        message: &'static str,
        label: &'static str,
        initial: &str,
    ) -> Self {
        Self {
            title,
            message,
            label,
            edit: Edit::new(initial),
        }
    }

    /// 输入框当前的文本。
    pub fn text(&self) -> &str {
        self.edit.text()
    }

    /// 对话框、输入框、确定、取消；居中偏上于 `client`。
    fn rects(client: Rect) -> (Rect, Rect, Rect, Rect) {
        let dialog = Rect::new(
            client.x + (client.width - WIDTH) / 2,
            client.y + (client.height - HEIGHT) / 3,
            WIDTH,
            HEIGHT,
        );
        let y = dialog.bottom() - 34;
        (
            dialog,
            Rect::new(dialog.x + 70, dialog.y + 66, dialog.width - 86, 21),
            Rect::new(dialog.right() - 169, y, 75, 23),
            Rect::new(dialog.right() - 88, y, 75, 23),
        )
    }

    /// 在 `client` 上画出对话框（盖在其他内容之上）。
    pub fn paint(&self, p: &mut Painter<'_>, client: Rect) {
        let (dialog, edit, ok, cancel) = Self::rects(client);
        let inner = p.window_frame(dialog, FrameKind::Fixed);
        p.fill(inner, C::ButtonFace);
        p.caption_bar(
            Rect::new(inner.x, inner.y, inner.width, 19),
            self.title,
            None,
            true,
            &[(CaptionButton::Close, false)],
            None,
        );
        let ink = Ink::Color(C::ButtonText);
        for (row, line) in self.message.lines().take(2).enumerate() {
            p.text(
                dialog.x + 14,
                dialog.y + 30 + row as i32 * 16,
                line,
                ink,
                false,
            );
        }
        p.label(dialog.x + 14, dialog.y + 70, self.label, ink, false);
        p.edit(edit, &self.edit, true, false);
        p.push_button(
            ok,
            "确定",
            ButtonState {
                default: true,
                ..Default::default()
            },
        );
        p.push_button(cancel, "取消", ButtonState::default());
    }

    /// 左键按下：点中确定返回 `Some(true)`，取消返回 `Some(false)`，其他位置返回 `None`。
    pub fn press(&self, client: Rect, point: (i32, i32)) -> Option<bool> {
        let (_, _, ok, cancel) = Self::rects(client);
        let inside = |rect: Rect| {
            point.0 >= rect.x
                && point.0 < rect.right()
                && point.1 >= rect.y
                && point.1 < rect.bottom()
        };
        if inside(ok) {
            Some(true)
        } else if inside(cancel) {
            Some(false)
        } else {
            None
        }
    }

    /// 按键：Enter 确定、Esc 取消，其余交给输入框。
    pub fn key(&mut self, key: Key) -> Option<bool> {
        match key.code {
            ENTER => Some(true),
            ESCAPE => Some(false),
            _ => {
                self.edit.key(key);
                None
            }
        }
    }
}
