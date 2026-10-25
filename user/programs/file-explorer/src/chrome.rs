//! 菜单栏、下拉菜单与工具栏的内容定义；绘制见 `ui`。
use crate::{browser::Browser, grid::ViewMode};
use canvas::Rect;

/// 菜单或工具栏按钮触发的操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Open,
    Close,
    Cut,
    Copy,
    Paste,
    Delete,
    Rename,
    NewFolder,
    Restore,
    EmptyTrash,
    Properties,
    Refresh,
    Back,
    Forward,
    Up,
    ToggleToolbar,
    ToggleStatusBar,
    ToggleFolders,
    SetView(ViewMode),
    /// 弹出"查看"方式菜单。
    ViewMenu,
}

pub enum Entry {
    Item {
        label: &'static str,
        shortcut: &'static str,
        /// `None` 表示在当前状态下不可用（以灰色显示）。
        command: Option<Command>,
        checked: bool,
    },
    Separator,
}

pub const MENU_TITLES: [&str; 6] = [
    "文件(F)",
    "编辑(E)",
    "查看(V)",
    "收藏(A)",
    "工具(T)",
    "帮助(H)",
];

fn item(label: &'static str, shortcut: &'static str, command: Option<Command>) -> Entry {
    Entry::Item {
        label,
        shortcut,
        command,
        checked: false,
    }
}

fn checked(label: &'static str, command: Command, on: bool) -> Entry {
    Entry::Item {
        label,
        shortcut: "",
        command: Some(command),
        checked: on,
    }
}

/// 查看方式的四个单选项，工具栏"查看"与"查看"菜单共用。
pub fn view_entries(current: ViewMode) -> Vec<Entry> {
    [
        ("大图标(G)", ViewMode::LargeIcons),
        ("小图标(M)", ViewMode::SmallIcons),
        ("列表(L)", ViewMode::List),
        ("详细资料(D)", ViewMode::Details),
    ]
    .into_iter()
    .map(|(label, mode)| checked(label, Command::SetView(mode), mode == current))
    .collect()
}

/// 第 `index` 个顶层菜单的条目。没有对应功能的项保持灰色，与资源管理器在无选择时的外观一致。
pub fn menu(index: usize, browser: &Browser) -> Vec<Entry> {
    let selected = browser.selected.is_some();
    let editable = browser.can_modify();
    let when = |condition: bool, command| condition.then_some(command);
    match index {
        0 => {
            let mut entries = vec![
                item("打开(O)", "", when(selected, Command::Open)),
                item("新建文件夹(F)", "", when(editable, Command::NewFolder)),
                Entry::Separator,
            ];
            if browser.in_trash() {
                entries.push(item("还原(R)", "", when(selected, Command::Restore)));
                entries.push(item("清空回收站(B)", "", Some(Command::EmptyTrash)));
            }
            entries.push(item(
                "删除(D)",
                "Del",
                when(selected && editable, Command::Delete),
            ));
            entries.push(item(
                "重命名(M)",
                "F2",
                when(selected && editable, Command::Rename),
            ));
            entries.push(item(
                "属性(R)",
                "Alt+Enter",
                when(selected, Command::Properties),
            ));
            entries.push(Entry::Separator);
            entries.push(item("关闭(C)", "", Some(Command::Close)));
            entries
        }
        1 => vec![
            item(
                "剪切(T)",
                "Ctrl+X",
                when(selected && editable, Command::Cut),
            ),
            item("复制(C)", "Ctrl+C", when(selected, Command::Copy)),
            item(
                "粘贴(P)",
                "Ctrl+V",
                when(editable && browser.clipboard.is_some(), Command::Paste),
            ),
        ],
        2 => {
            let mut entries = vec![
                checked("工具栏(T)", Command::ToggleToolbar, browser.show_toolbar),
                checked(
                    "状态栏(B)",
                    Command::ToggleStatusBar,
                    browser.show_status_bar,
                ),
                checked("文件夹(O)", Command::ToggleFolders, browser.show_folders),
                Entry::Separator,
            ];
            entries.extend(view_entries(browser.view));
            entries.push(Entry::Separator);
            entries.push(item("刷新(R)", "F5", Some(Command::Refresh)));
            entries
        }
        3 => vec![item("(空)", "", None)],
        4 => vec![item("文件夹选项(O)...", "", None)],
        _ => vec![item("关于 LiteOS 资源管理器(A)", "", None)],
    }
}

/// 右键菜单：在项目上是打开与编辑类操作，在空白处是粘贴与新建。
pub fn context(browser: &Browser, on_item: bool) -> Vec<Entry> {
    let editable = browser.can_modify();
    let when = |condition: bool, command| condition.then_some(command);
    if on_item {
        let mut entries = vec![item("打开(O)", "", Some(Command::Open))];
        if browser.in_trash() {
            entries.push(item("还原(E)", "", Some(Command::Restore)));
        }
        entries.extend([
            Entry::Separator,
            item("剪切(T)", "", when(editable, Command::Cut)),
            item("复制(C)", "", Some(Command::Copy)),
            Entry::Separator,
            item("删除(D)", "", when(editable, Command::Delete)),
            item("重命名(M)", "", when(editable, Command::Rename)),
            Entry::Separator,
            item("属性(R)", "", Some(Command::Properties)),
        ]);
        entries
    } else {
        vec![
            item(
                "粘贴(P)",
                "",
                when(editable && browser.clipboard.is_some(), Command::Paste),
            ),
            Entry::Separator,
            item("新建文件夹(F)", "", when(editable, Command::NewFolder)),
            Entry::Separator,
            item("刷新(R)", "", Some(Command::Refresh)),
        ]
    }
}

/// 工具栏按钮。
pub struct Tool {
    /// 在 47 帧位图条里的序号。
    pub frame: i32,
    pub label: &'static str,
    /// 悬停时的工具提示。
    pub tip: &'static str,
    pub command: Option<Command>,
    /// 此按钮前面带一道分隔。
    pub gap: bool,
}

const fn tool(
    frame: i32,
    label: &'static str,
    tip: &'static str,
    command: Option<Command>,
    gap: bool,
) -> Tool {
    Tool {
        frame,
        label,
        tip,
        command,
        gap,
    }
}

pub const TOOLS: [Tool; 11] = [
    tool(0, "后退", "后退", Some(Command::Back), false),
    tool(1, "前进", "前进", Some(Command::Forward), false),
    tool(28, "向上", "向上", Some(Command::Up), false),
    tool(17, "搜索", "搜索", None, true),
    tool(3, "文件夹", "文件夹", Some(Command::ToggleFolders), false),
    tool(26, "历史", "历史", None, false),
    tool(43, "", "移至", None, true),
    tool(44, "", "复制到", None, false),
    tool(10, "", "删除", Some(Command::Delete), false),
    tool(8, "", "撤消", None, false),
    tool(20, "", "查看", Some(Command::ViewMenu), true),
];

/// 工具栏按钮宽度与分隔宽度。
const LABELLED_WIDTH: i32 = 54;
const PLAIN_WIDTH: i32 = 26;
const GAP: i32 = 8;

/// 各按钮在工具栏矩形内的位置。
pub fn tool_rects(bar: Rect) -> [Rect; TOOLS.len()] {
    let mut x = bar.x + 6;
    TOOLS.each_ref().map(|tool| {
        if tool.gap {
            x += GAP;
        }
        let width = if tool.label.is_empty() {
            PLAIN_WIDTH
        } else {
            LABELLED_WIDTH
        };
        let rect = Rect::new(x, bar.y + 2, width, bar.height - 4);
        x += width;
        rect
    })
}

/// 按钮是否可用；`Folders` 与 `ViewMenu` 随时可用，导航按钮取决于历史。
pub fn tool_enabled(tool: &Tool, browser: &Browser) -> bool {
    match tool.command {
        Some(Command::Back) => browser.can_back(),
        Some(Command::Forward) => browser.can_forward(),
        Some(Command::Up) => browser.location.parent().is_some(),
        Some(Command::Delete) => browser.selected.is_some() && browser.can_modify(),
        command => command.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Location;

    #[test]
    fn tool_frames_stay_inside_the_47_frame_strip() {
        assert!(TOOLS.iter().all(|t| (0..47).contains(&t.frame)));
    }

    #[test]
    fn toolbar_buttons_do_not_overlap_and_separators_widen_the_gap() {
        let rects = tool_rects(Rect::new(0, 0, 600, 44));
        for pair in rects.windows(2) {
            assert!(pair[0].right() <= pair[1].x);
        }
        assert_eq!(rects[3].x - rects[2].right(), GAP);
        assert_eq!(rects[1].x - rects[0].right(), 0);
    }

    #[test]
    fn navigation_buttons_follow_history_and_open_follows_selection() {
        let browser = Browser::new(Location::MyComputer);
        assert!(!tool_enabled(&TOOLS[0], &browser), "nothing to go back to");
        assert!(
            !tool_enabled(&TOOLS[2], &browser),
            "my computer has no parent"
        );
        assert!(tool_enabled(&TOOLS[4], &browser));
        let Entry::Item { command, .. } = &menu(0, &browser)[0] else {
            unreachable!()
        };
        assert!(command.is_none(), "open is unavailable without a selection");
    }
}
