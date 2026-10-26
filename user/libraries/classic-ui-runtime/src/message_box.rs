//! 通用消息对话框的布局与默认按钮由共享运行库拥有；SDK 只适配异步结果。
use super::{context::Context, property_sheet::add, widgets::Tree};
use classic_ui_abi::{self as abi, state, widget};
use std::io;
impl Context {
    pub(super) fn message_box(
        &mut self,
        parent: u64,
        title: &str,
        message: &str,
        buttons: u32,
    ) -> io::Result<abi::MessageBox> {
        let labels: &[&str] = match buttons {
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
        let message = add(
            &mut tree,
            root,
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
        let app_id = self.host(parent)?.app_id.clone();
        let window = self.window(title, &app_id, (400, 200), true, parent, true)?;
        self.host_mut(window)?.body = super::context::WindowBody::Widgets(tree);
        handles.window = window;
        Ok(handles)
    }
}
