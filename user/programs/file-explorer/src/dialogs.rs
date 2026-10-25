//! 窗口内的模态对话框：删除确认与文件属性；画在列表之上，不是独立窗口。
use crate::{
    browser::Properties,
    chrome::Command,
    ui::{Assets, Scene, draw, fit},
};
use canvas::Rect;
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{ButtonState, CaptionButton, FrameKind, Ink, Painter};

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

pub fn paint_confirm(p: &mut Painter<'_>, scene: &Scene<'_>, confirm: &Confirm, client: Rect) {
    let (dialog, yes, no) = confirm_rects(client);
    let inner = p.window_frame(dialog, FrameKind::Fixed);
    p.fill(inner, C::ButtonFace);
    p.caption_bar(
        Rect::new(inner.x, inner.y, inner.width, 19),
        "确认",
        true,
        &[(CaptionButton::Close, false)],
        None,
    );
    draw(
        scene.assets.question.as_ref(),
        p,
        dialog.x + 18,
        dialog.y + 36,
    );
    let mut lines = confirm.text.lines();
    for row in 0..2 {
        if let Some(line) = lines.next() {
            let y = dialog.y + 38 + row * 16;
            p.text(
                dialog.x + 62,
                y,
                &fit(p, line, dialog.width - 76),
                Ink::Color(C::ButtonText),
                false,
            );
        }
    }
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
        true,
        &[(CaptionButton::Close, false)],
        None,
    );
    let tab = Rect::new(dialog.x + 8, dialog.y + 28, 50, 20);
    let page = Rect::new(
        dialog.x + 8,
        dialog.y + 47,
        dialog.width - 16,
        dialog.height - 47 - 42,
    );
    let face = p.edge(page, Edge::Raised);
    p.fill(face, C::ButtonFace);
    let tab_face = p.edge(tab, Edge::Raised);
    p.fill(tab_face, C::ButtonFace);
    p.text_centered(tab, "常规", Ink::Color(C::ButtonText), false);
    p.fill(
        Rect::new(tab.x + 2, page.y, tab.width - 4, 2),
        C::ButtonFace,
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
