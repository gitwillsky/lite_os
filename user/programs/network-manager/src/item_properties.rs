//! WinBox 属性窗口共享的右对齐字段和底部按钮尺寸。

use crate::child;
use liteos_ui::{Event, EventKind, Layout, Widget, WidgetKind, WidgetSpec, state};

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

/// 工作区属性窗口的关闭、应用和删除操作。
pub(super) enum Request {
    Close,
    Save { close: bool },
    Remove,
}

/// 在编辑正文下方放置右对齐的取消、应用和确定按钮。
pub(super) struct Footer {
    cancel: Widget,
    apply: Widget,
    ok: Widget,
    _layout: (Widget, Option<Widget>),
}

impl Footer {
    /// 建立属性窗口的底部按钮。控件的创建错误由调用者处理。
    pub(super) fn new(parent: &Widget) -> liteos_ui::Result<Self> {
        let row = child(
            parent,
            WidgetKind::Row,
            "",
            Layout {
                gap: 5,
                ..Layout::default()
            },
        )?;
        let space = crate::item_list::spacer(&row)?;
        Ok(Self {
            cancel: child(&row, WidgetKind::Button, "取消", button())?,
            apply: child(&row, WidgetKind::Button, "应用", button())?,
            ok: child(&row, WidgetKind::Button, "确定", button())?,
            _layout: (row, Some(space)),
        })
    }

    /// 在设置正文右侧放置确定、取消和应用，沿用相同的事件与可用状态。
    /// 错误：控件无法创建。
    pub(super) fn side(parent: &Widget) -> liteos_ui::Result<Self> {
        let column = child(
            parent,
            WidgetKind::Column,
            "",
            Layout {
                gap: 5,
                ..Layout::default()
            },
        )?;
        Ok(Self {
            ok: child(&column, WidgetKind::Button, "确定", button())?,
            cancel: child(&column, WidgetKind::Button, "取消", button())?,
            apply: child(&column, WidgetKind::Button, "应用", button())?,
            _layout: (column, None),
        })
    }

    /// 将底部按钮和子窗口自身的关闭事件转换为编辑请求。
    pub(super) fn request(&self, frame: &Widget, event: &Event) -> Option<Request> {
        self.command(event).or_else(|| {
            (event.node == frame.id() && matches!(event.kind, EventKind::Activate { .. }))
                .then_some(Request::Close)
        })
    }

    /// 将按钮激活转换为请求，供独立对话框处理其原生关闭事件。
    pub(super) fn command(&self, event: &Event) -> Option<Request> {
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return None;
        }
        if event.node == self.cancel.id() {
            Some(Request::Close)
        } else if event.node == self.apply.id() || event.node == self.ok.id() {
            Some(Request::Save {
                close: event.node == self.ok.id(),
            })
        } else {
            None
        }
    }

    /// 提交时禁止重复应用或关闭。已保存但无法确认代际时只允许关闭。
    ///
    /// 错误：工作区窗口已关闭。
    pub(super) fn set_enabled(&self, close: bool, save: bool) -> liteos_ui::Result<()> {
        self.cancel
            .set_state(if close { 0 } else { state::DISABLED })?;
        self.apply
            .set_state(if save { 0 } else { state::DISABLED })?;
        self.ok.set_state(if save {
            state::DEFAULT
        } else {
            state::DISABLED
        })
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
    let line = field_row(parent, decor, label)?;
    let input = line.child(input(kind, text, read_only, INPUT))?;
    Ok(input)
}

/// 创建带尾部省略号按钮的只读引用字段，返回文本和菜单按钮。
/// 当引用没有目标时，隐藏按钮即可让文本恢复到字段全宽。
///
/// 错误：工作区窗口已关闭或控件无法创建。
pub(super) fn reference_field(
    parent: &Widget,
    decor: &mut Vec<Widget>,
    label: &str,
) -> liteos_ui::Result<(Widget, Widget)> {
    let line = field_row(parent, decor, label)?;
    let reference = child(
        &line,
        WidgetKind::Row,
        "",
        Layout {
            width: INPUT,
            ..Layout::default()
        },
    )?;
    let mut spec = input(WidgetKind::Edit, "", true, 0);
    spec.layout.grow = 1;
    let value = reference.child(spec)?;
    let command = child(
        &reference,
        WidgetKind::Button,
        "…",
        Layout {
            width: 25,
            height: 23,
            ..Layout::default()
        },
    )?;
    decor.push(reference);
    Ok((value, command))
}

fn field_row(parent: &Widget, decor: &mut Vec<Widget>, label: &str) -> liteos_ui::Result<Widget> {
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
    decor.push(line.clone());
    Ok(line)
}

fn input(kind: WidgetKind, text: &str, read_only: bool, width: i32) -> WidgetSpec<'_> {
    let mut spec = WidgetSpec::new(kind, text);
    spec.state = match (read_only, kind) {
        (true, WidgetKind::Edit) => state::READ_ONLY,
        (true, _) => state::DISABLED,
        (false, _) => 0,
    };
    spec.layout = Layout {
        width,
        ..Layout::default()
    };
    spec
}
