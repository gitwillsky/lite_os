//! 窗口内的模态对话框：删除确认与文件属性；画在列表之上，不是独立窗口。
use crate::{
    browser::Properties,
    chrome::Command,
    ui::{Assets, Scene, draw, fit},
};
use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
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
            &fit(p, line, dialog.width - 76),
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

/// "属性"对话框的窗口矩形与它的"确定"按钮。
pub fn properties_rects(client: Rect) -> (Rect, Rect) {
    let dialog = Rect::new(
        client.x + (client.width - 300) / 2,
        client.y + (client.height - 300).max(0) / 2,
        300,
        300,
    );
    (
        dialog,
        Rect::new(dialog.right() - 85, dialog.bottom() - 34, 75, 23),
    )
}

pub fn paint_properties(p: &mut Painter<'_>, scene: &Scene<'_>, info: &Properties, client: Rect) {
    let (dialog, ok) = properties_rects(client);
    let inner = p.window_frame(dialog, FrameKind::Fixed);
    p.fill(inner, C::ButtonFace);
    p.caption_bar(
        Rect::new(inner.x, inner.y, inner.width, 19),
        &format!("{} 属性", info.name),
        None,
        true,
        &[(CaptionButton::Close, false)],
        None,
    );
    let page = Rect::new(
        dialog.x + 8,
        dialog.y + 47,
        dialog.width - 16,
        dialog.height - 47 - 42,
    );
    let face = p.edge(page, Edge::Raised);
    p.fill(face, C::ButtonFace);
    p.tab(
        Rect::new(
            dialog.x + 8,
            dialog.y + 26,
            50,
            page.y + 2 - (dialog.y + 26),
        ),
        "常规",
        true,
    );
    draw(
        Assets::icon(&scene.assets.large, info.icon),
        p,
        page.x + 14,
        page.y + 14,
    );
    let name = fit(p, &info.name, page.width - 80);
    p.text(
        page.x + 60,
        page.y + 22,
        &name,
        Ink::Color(C::ButtonText),
        false,
    );
    p.fill(
        Rect::new(page.x + 12, page.y + 56, page.width - 24, 2),
        C::ButtonShadow,
    );
    p.fill(
        Rect::new(page.x + 12, page.y + 58, page.width - 24, 1),
        C::ButtonHilight,
    );
    let step = p.line_height() + 8;
    for (row, (label, value)) in info.rows.iter().enumerate() {
        let y = page.y + 70 + row as i32 * step;
        p.text(page.x + 14, y, label, Ink::Color(C::ButtonText), false);
        let shown = fit(p, value, page.width - 100);
        p.text(page.x + 86, y, &shown, Ink::Color(C::ButtonText), false);
    }
    p.push_button(
        ok,
        "确定",
        ButtonState {
            default: true,
            ..Default::default()
        },
    );
}
