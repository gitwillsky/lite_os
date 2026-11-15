//! WinBox 属性窗口共享的右对齐字段和底部按钮尺寸。

use crate::child;
use liteos_ui::{Layout, Widget, WidgetKind, WidgetSpec, state};

const LABEL: i32 = 96;
const INPUT: i32 = 250;

/// 属性窗口底部的操作按钮布局。
pub(super) fn button() -> Layout {
    Layout {
        width: 75,
        height: 23,
        ..Layout::default()
    }
}

/// 添加右对齐标签和输入控件。行与标签由属性窗口持有，返回的输入控件交给调用方。
///
/// 错误：工作区窗口已关闭或控件创建失败。
pub(super) fn field(
    parent: &Widget,
    decor: &mut Vec<Widget>,
    label: &str,
    kind: WidgetKind,
    text: &str,
    read_only: bool,
) -> liteos_ui::Result<Widget> {
    let line = child(
        parent,
        WidgetKind::Row,
        "",
        Layout {
            gap: 6,
            ..Layout::default()
        },
    )?;
    let mut caption = WidgetSpec::new(WidgetKind::Label, label);
    caption.state = state::ALIGN_RIGHT;
    caption.layout = Layout {
        width: LABEL,
        ..Layout::default()
    };
    decor.push(line.child(caption)?);
    let mut spec = WidgetSpec::new(kind, text);
    // 单行编辑框没有只读态；只显示的值用禁用态，与 WinBox 灰底的只读字段一致。
    spec.state = if read_only { state::DISABLED } else { 0 };
    spec.layout = Layout {
        width: INPUT,
        ..Layout::default()
    };
    let input = line.child(spec)?;
    decor.push(line);
    Ok(input)
}
