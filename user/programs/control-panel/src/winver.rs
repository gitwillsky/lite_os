//! 产品信息由共享标签和图标控件排版。
use crate::{Page, image, label};
use liteos_ui::{PropertySheet, Widget};
pub struct Winver {
    _controls: Vec<Widget>,
}
impl Winver {
    pub fn build(sheet: &PropertySheet) -> liteos_ui::Result<Self> {
        let page = sheet.page("版本")?;
        let mut controls = vec![image(&page, "windows-logo")?];
        for text in [
            "LiteOS 2000 Professional",
            "基于 LiteOS 技术",
            "本机账户认证、独立用户会话与 Wayland 窗口。",
            "按 Ctrl+Alt+Delete 进入安全界面。",
            "版权所有 © LiteOS 贡献者",
        ] {
            controls.push(label(&page, text)?);
        }
        controls.push(page);
        Ok(Self {
            _controls: controls,
        })
    }
}
impl Page for Winver {}
