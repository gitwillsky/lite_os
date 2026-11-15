//! WinBox 式项目列表窗口：上方工具栏（带文字的图标命令、纯图标命令和分隔），中间列表与右侧「动作」面板，
//! 底部状态栏显示条目数、选中数与观察状态。
//!
//! 工具栏按钮与动作链接的启用状态各由本模块唯一持有；[`Tool`]、[`Action`] 只是指向其中一项的句柄，
//! 异步观察任务也经它们更新状态。控件句柄释放即删除节点，所以静态标签由返回的结构体持有。
use liteos_ui::{
    Event, EventKind, FontRole, Layout, ToolbarButton, ToolbarKind, Widget, WidgetKind, state,
};
use std::{cell::RefCell, rc::Rc};

pub(super) mod filter;
mod find;
mod table;
pub(super) use find::{Find, FindAction, Scan};
pub(super) use table::Table;

/// 原创位图条 `list-actions` 的帧序号；与 `scripts/list_action_icons.py` 的 `FRAMES` 顺序一致。
#[derive(Clone, Copy)]
pub(super) enum Glyph {
    Add,
    Remove,
    Enable,
    Disable,
    Filter,
    Previous,
    Next,
    Refresh,
}

/// 工具栏的一项。
pub(super) enum Item {
    /// 图标右侧显示文字的命令（WinBox 的 新建/启用/停用/删除）。
    Command(Glyph, &'static str),
    /// 只有图标的命令，文字作为悬停提示（翻页、刷新、筛选）。
    Icon(Glyph, &'static str),
    /// 没有专用图标的文字命令。
    Text(&'static str),
    Separator,
}

struct Bar {
    widget: Widget,
    items: Vec<(Item, bool)>,
}

impl Bar {
    /// 整组提交按钮：运行库按相同按钮标识保留悬停与按下状态，只有启用状态变化时重画。
    fn publish(&self) -> liteos_ui::Result<()> {
        let buttons: Vec<_> = self
            .items
            .iter()
            .enumerate()
            .map(|(index, (item, enabled))| {
                let (kind, strip, frame, text, show_text) = match item {
                    Item::Command(glyph, text) => (
                        ToolbarKind::Button,
                        "list-actions",
                        *glyph as u32,
                        *text,
                        true,
                    ),
                    Item::Icon(glyph, text) => (
                        ToolbarKind::Button,
                        "list-actions",
                        *glyph as u32,
                        *text,
                        false,
                    ),
                    Item::Text(text) => (ToolbarKind::Button, "", 0, *text, true),
                    Item::Separator => (ToolbarKind::Separator, "", 0, "", false),
                };
                ToolbarButton {
                    id: index as u64 + 1,
                    kind,
                    strip,
                    frame,
                    text,
                    show_text,
                    enabled: *enabled,
                    checked: false,
                }
            })
            .collect();
        self.widget.set_toolbar(&buttons)
    }
}

/// 工具栏中一项的句柄；克隆后仍指向同一按钮。
#[derive(Clone)]
pub(super) struct Tool {
    bar: Rc<RefCell<Bar>>,
    index: usize,
}

impl Tool {
    /// 设置按钮是否可用；状态不变时不重新提交工具栏。
    ///
    /// 错误：窗口已关闭。
    pub(super) fn set_enabled(&self, enabled: bool) -> liteos_ui::Result<()> {
        let mut bar = self.bar.borrow_mut();
        if bar.items[self.index].1 == enabled {
            return Ok(());
        }
        bar.items[self.index].1 = enabled;
        bar.publish()
    }

    /// 事件是否为点击本按钮；禁用按钮不会产生激活事件。
    pub(super) fn activated(&self, event: &Event) -> bool {
        matches!(event.kind, EventKind::Activate { value } if value == self.index as u64 + 1)
            && event.node == self.bar.borrow().widget.id()
    }
}

/// 在 `parent` 中建立按按钮自然宽度排列的工具栏，按 `items` 顺序返回各项句柄（分隔也有句柄，便于解构）。
/// 所有按钮初始可用。工具栏不伸缩：放在纵向容器里时伸缩会拉高整行，需要右对齐的后续控件由调用者加弹性空白。
///
/// 错误：窗口已关闭或运行库拒绝按钮数据。
pub(super) fn toolbar<const N: usize>(
    parent: &Widget,
    items: [Item; N],
) -> liteos_ui::Result<[Tool; N]> {
    let widget = crate::child(parent, WidgetKind::Toolbar, "", Layout::default())?;
    let bar = Bar {
        widget,
        items: items.into_iter().map(|item| (item, true)).collect(),
    };
    bar.publish()?;
    let bar = Rc::new(RefCell::new(bar));
    Ok(std::array::from_fn(|index| Tool {
        bar: bar.clone(),
        index,
    }))
}

/// 行内弹性空白：把其后的控件推到行尾。返回的控件必须由调用者持有，释放即删除节点。
///
/// 错误：窗口已关闭。
pub(super) fn spacer(row: &Widget) -> liteos_ui::Result<Widget> {
    crate::child(
        row,
        WidgetKind::Label,
        "",
        Layout {
            grow: 1,
            ..Layout::default()
        },
    )
}

/// 列表与其右侧「动作」面板。列表占满剩余宽度，面板只列出针对选中条目的文字命令。
pub(super) struct Body {
    pub(super) list: Widget,
    // 面板标题只需存活。
    _title: Widget,
    panel: Widget,
}

/// 「动作」面板里的一条文字命令；禁用时显示为灰色且不响应点击。
#[derive(Clone)]
pub(super) struct Action {
    link: Widget,
}

impl Action {
    /// 设置命令是否可用。
    ///
    /// 错误：窗口已关闭。
    pub(super) fn set_enabled(&self, enabled: bool) -> liteos_ui::Result<()> {
        self.link
            .set_state(if enabled { 0 } else { state::DISABLED })
    }

    /// 事件是否为点击本命令。
    pub(super) fn activated(&self, event: &Event) -> bool {
        event.node == self.link.id() && matches!(event.kind, EventKind::Activate { .. })
    }
}

impl Body {
    /// 在 `parent` 中建立占满剩余高度的列表行，右侧附「动作」面板。
    ///
    /// 错误：窗口已关闭。
    pub(super) fn new(parent: &Widget) -> liteos_ui::Result<Self> {
        let row = crate::child(
            parent,
            WidgetKind::Row,
            "",
            Layout {
                grow: 1,
                gap: 0,
                ..Layout::default()
            },
        )?;
        let list = crate::child(
            &row,
            WidgetKind::List,
            "",
            Layout {
                grow: 1,
                ..Layout::default()
            },
        )?;
        let panel = crate::child(
            &row,
            WidgetKind::Column,
            "",
            Layout {
                width: 128,
                padding: 6,
                gap: 6,
                ..Layout::default()
            },
        )?;
        // 面板在添加第一条命令时才显示；没有动作的列表（如地址）不留空白面板。
        panel.set_state(state::HIDDEN)?;
        let title = crate::child(&panel, WidgetKind::Label, "动作", Layout::default())?;
        title.set_font(FontRole::Message, None, Some(700))?;
        Ok(Self {
            list,
            _title: title,
            panel,
        })
    }

    /// 在面板末尾追加一条命令。
    ///
    /// 错误：窗口已关闭。
    pub(super) fn action(&self, text: &str) -> liteos_ui::Result<Action> {
        self.panel.set_state(0)?;
        Ok(Action {
            link: crate::child(&self.panel, WidgetKind::Link, text, Layout::default())?,
        })
    }
}

/// 列表窗口底部的状态栏：条目数、选中数、观察状态三格。
#[derive(Clone)]
pub(super) struct Summary {
    status: Widget,
}

impl Summary {
    /// 在 `parent` 末尾建立状态栏。
    ///
    /// 错误：窗口已关闭。
    pub(super) fn new(parent: &Widget) -> liteos_ui::Result<Self> {
        let status = crate::child(parent, WidgetKind::Status, "", Layout::default())?;
        status.set_status_parts(&[64, 72, -1])?;
        Ok(Self { status })
    }

    /// 显示条目数、选中数（0 时留空）与观察状态或提示。
    ///
    /// 错误：窗口已关闭。
    pub(super) fn show(&self, count: usize, selected: usize, note: &str) -> liteos_ui::Result<()> {
        let selected = if selected == 0 {
            String::new()
        } else {
            format!("已选 {selected}")
        };
        self.status
            .set_text(&format!("{count}\t{selected}\t{note}"))
    }

    /// 只更新第三格的状态或错误提示，前两格清空（读取中或观察中断时没有可靠的条目数）。
    ///
    /// 错误：窗口已关闭。
    pub(super) fn notice(&self, note: &str) -> liteos_ui::Result<()> {
        self.status.set_text(&format!("\t\t{note}"))
    }
}

/// 建立列表下方的说明行（选中项详情、操作进度）。初始隐藏：WinBox 列表窗口没有常驻空白行，
/// 列表应直接贴住状态栏。
///
/// 错误：窗口已关闭。
pub(super) fn note(parent: &Widget) -> liteos_ui::Result<Widget> {
    let mut spec = liteos_ui::WidgetSpec::new(WidgetKind::Label, "");
    spec.state = state::WRAP | state::HIDDEN;
    parent.child(spec)
}

/// 更新说明行；空文本时隐藏该行，释放其占用的高度。
///
/// 错误：窗口已关闭。
pub(super) fn set_note(label: &Widget, text: &str) -> liteos_ui::Result<()> {
    label.set_text(text)?;
    label.set_state(if text.is_empty() {
        state::WRAP | state::HIDDEN
    } else {
        state::WRAP
    })
}
