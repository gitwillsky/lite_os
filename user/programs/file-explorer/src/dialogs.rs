//! 窗口内的模态对话框：删除确认与错误提示；画在列表之上，不是独立窗口。
use crate::{
    chrome::Command,
    ui::{Scene, draw},
};
use canvas::Rect;
use classic_theme::SystemColor as C;
use classic_ui::{Bitmap, ButtonState, CaptionButton, FrameKind, Ink, Painter};

/// 删除等操作的确认框；窗口内的模态覆盖层，确认前不执行 `command`。
pub struct Confirm {
    pub text: String,
    pub command: Command,
}

/// 确认框与它的"是""否"按钮，居中于客户区。
pub fn confirm_rects(client: Rect) -> (Rect, Rect, Rect) {
    let dialog = Rect::new(
        client.x + (client.width - 330) / 2,
        client.y + (client.height - 120) / 2,
        330,
        120,
    );
    let yes = Rect::new(dialog.x + 85, dialog.bottom() - 34, 75, 23);
    let no = Rect::new(dialog.x + 170, dialog.bottom() - 34, 75, 23);
    (dialog, yes, no)
}

/// 只有"确定"的错误提示框；显示 `browser.message` 里的失败原因，关闭前阻塞窗口内其他输入。
pub struct Notice {
    pub text: String,
}

/// 提示框与它的"确定"按钮，居中于客户区。
pub fn notice_rects(client: Rect) -> (Rect, Rect) {
    let dialog = Rect::new(
        client.x + (client.width - 330) / 2,
        client.y + (client.height - 120) / 2,
        330,
        120,
    );
    (
        dialog,
        Rect::new(dialog.x + 127, dialog.bottom() - 34, 75, 23),
    )
}

/// 画消息框的框架、标题、左侧 32×32 图标与至多两行文字；按钮由调用者绘制。
fn paint_message(
    p: &mut Painter<'_>,
    dialog: Rect,
    title: &str,
    icon: Option<&Bitmap>,
    text: &str,
) {
    let inner = p.window_frame(dialog, FrameKind::Fixed);
    p.fill(inner, C::ButtonFace);
    p.caption_bar(
        Rect::new(inner.x, inner.y, inner.width, 19),
        title,
        None,
        true,
        &[(CaptionButton::Close, false)],
        None,
    );
    draw(icon, p, dialog.x + 18, dialog.y + 36);
    for (row, line) in text.lines().take(2).enumerate() {
        p.text(
            dialog.x + 62,
            dialog.y + 38 + row as i32 * 16,
            &p.fit(line, dialog.width - 76),
            Ink::Color(C::ButtonText),
            false,
        );
    }
}

pub fn paint_confirm(p: &mut Painter<'_>, scene: &Scene<'_>, confirm: &Confirm, client: Rect) {
    let (dialog, yes, no) = confirm_rects(client);
    paint_message(
        p,
        dialog,
        "确认",
        scene.assets.question.as_ref(),
        &confirm.text,
    );
    p.push_button(
        yes,
        "是(Y)",
        ButtonState {
            default: true,
            ..Default::default()
        },
    );
    p.push_button(no, "否(N)", ButtonState::default());
}

pub fn paint_notice(p: &mut Painter<'_>, scene: &Scene<'_>, notice: &Notice, client: Rect) {
    let (dialog, ok) = notice_rects(client);
    paint_message(
        p,
        dialog,
        "资源管理器",
        scene.assets.error.as_ref(),
        &notice.text,
    );
    p.push_button(
        ok,
        "确定",
        ButtonState {
            default: true,
            ..Default::default()
        },
    );
}
