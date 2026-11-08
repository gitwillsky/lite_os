//! 通用消息对话框的布局、图标、默认按钮与系统声音由共享运行库拥有；SDK 只适配异步结果。
use super::{context::Context, property_sheet::add, widgets::Tree};
use classic_ui_abi::{self as abi, message_style, state, widget};
use std::io;

/// MB_ICON* 对应的 user32 图标与默认声音方案事件；无图标时是“默认声音”（MessageBeep(MB_OK)）。
fn icon(style: u32) -> Option<(Option<&'static str>, sound::Event)> {
    Some(match style & message_style::ICON {
        0 => (None, sound::Event::Default),
        message_style::ICON_HAND => (Some("message-error"), sound::Event::CriticalStop),
        message_style::ICON_QUESTION => (Some("message-question"), sound::Event::Question),
        message_style::ICON_EXCLAMATION => (Some("message-warning"), sound::Event::Exclamation),
        message_style::ICON_ASTERISK => (Some("message-information"), sound::Event::Asterisk),
        _ => return None,
    })
}
impl Context {
    pub(super) fn message_box(
        &mut self,
        parent: u64,
        title: &str,
        message: &str,
        style: u32,
    ) -> io::Result<abi::MessageBox> {
        let (icon, event) = icon(style).ok_or_else(|| super::invalid("unknown message icon"))?;
        if style & !(message_style::BUTTONS | message_style::ICON) != 0 {
            return Err(super::invalid("unknown message style"));
        }
        let labels: &[&str] = match style & message_style::BUTTONS {
            0 => &["确定"],
            1 => &["确定", "取消"],
            2 => &["是(Y)", "否(N)", "取消"],
            3 => &["是(Y)", "否(N)"],
            _ => return Err(super::invalid("unknown message buttons")),
        };
        let mut tree = Tree::new();
        let root = add(
            &mut tree,
            0,
            widget::COLUMN,
            "",
            abi::Layout {
                grow: 1,
                padding: 12,
                gap: 12,
                ..Default::default()
            },
        )?;
        // USER32 的版式：图标在左上，正文在其右侧换行。
        let body = add(
            &mut tree,
            root,
            widget::ROW,
            "",
            abi::Layout {
                grow: 1,
                gap: 12,
                ..Default::default()
            },
        )?;
        if let Some(name) = icon {
            let image = add(
                &mut tree,
                body,
                widget::IMAGE,
                "",
                abi::Layout {
                    width: 32,
                    height: 32,
                    ..Default::default()
                },
            )?;
            tree.nodes.get_mut(&image).expect("message icon").image = self.icon_asset(name, 0, 32);
        }
        let message = add(
            &mut tree,
            body,
            widget::LABEL,
            message,
            abi::Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        tree.nodes.get_mut(&message).expect("message label").state = state::WRAP;
        let row = add(
            &mut tree,
            root,
            widget::ROW,
            "",
            abi::Layout {
                height: 26,
                gap: 6,
                ..Default::default()
            },
        )?;
        add(
            &mut tree,
            row,
            widget::LABEL,
            "",
            abi::Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        let mut handles = abi::MessageBox::default();
        for (index, label) in labels.iter().enumerate() {
            handles.buttons[index] = add(
                &mut tree,
                row,
                widget::BUTTON,
                label,
                abi::Layout {
                    width: 80,
                    height: 26,
                    ..Default::default()
                },
            )?;
        }
        tree.nodes
            .get_mut(&handles.buttons[0])
            .expect("default button")
            .state |= state::DEFAULT;
        let app_id = if parent == 0 {
            String::new()
        } else {
            self.host(parent)?.app_id.clone()
        };
        let window = self.window(title, &app_id, (400, 200), true, parent, parent != 0)?;
        self.host_mut(window)?.body = super::context::WindowBody::Widgets(tree);
        handles.window = window;
        // MessageBox 出现时按图标播放 MessageBeep 对应的系统声音；播放在独立进程，不阻塞界面。
        event.play();
        Ok(handles)
    }
}
