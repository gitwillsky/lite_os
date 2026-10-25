//! 资源管理器的业务命令与菜单数据；无绘制、命中或控件交互状态。
use liteos_ui::{ListStyle, MenuItem};
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum Command {
    Open = 1,
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
    SelectAll,
    ToggleToolbar,
    ToggleStatus,
    ToggleFolders,
    Refresh,
    Back,
    Forward,
    Up,
    LargeIcons,
    SmallIcons,
    List,
    Details,
    ViewMenu,
    Address,
    About,
}
impl Command {
    pub fn from_id(id: u64) -> Option<Self> {
        Some(match id {
            1 => Self::Open,
            2 => Self::Close,
            3 => Self::Cut,
            4 => Self::Copy,
            5 => Self::Paste,
            6 => Self::Delete,
            7 => Self::Rename,
            8 => Self::NewFolder,
            9 => Self::Restore,
            10 => Self::EmptyTrash,
            11 => Self::Properties,
            12 => Self::SelectAll,
            13 => Self::ToggleToolbar,
            14 => Self::ToggleStatus,
            15 => Self::ToggleFolders,
            16 => Self::Refresh,
            17 => Self::Back,
            18 => Self::Forward,
            19 => Self::Up,
            20 => Self::LargeIcons,
            21 => Self::SmallIcons,
            22 => Self::List,
            23 => Self::Details,
            24 => Self::ViewMenu,
            25 => Self::Address,
            26 => Self::About,
            _ => return None,
        })
    }
    pub fn style(self) -> Option<ListStyle> {
        match self {
            Self::LargeIcons => Some(ListStyle::LargeIcons),
            Self::SmallIcons => Some(ListStyle::SmallIcons),
            Self::List => Some(ListStyle::List),
            Self::Details => Some(ListStyle::Details),
            _ => None,
        }
    }
}
pub struct Availability {
    pub selected: bool,
    pub editable: bool,
    pub paste: bool,
    pub trash: bool,
    pub toolbar: bool,
    pub status: bool,
    pub folders: bool,
    pub style: ListStyle,
    pub busy: bool,
}
fn item(
    command: Command,
    label: &'static str,
    shortcut: &'static str,
    enabled: bool,
) -> MenuItem<'static> {
    MenuItem {
        id: command as u64,
        label,
        shortcut,
        state: u32::from(!enabled),
    }
}
fn separated(mut item: MenuItem<'static>) -> MenuItem<'static> {
    item.state |= 4;
    item
}
pub fn views(style: ListStyle) -> Vec<MenuItem<'static>> {
    [
        (Command::LargeIcons, "大图标(G)", ListStyle::LargeIcons),
        (Command::SmallIcons, "小图标(M)", ListStyle::SmallIcons),
        (Command::List, "列表(L)", ListStyle::List),
        (Command::Details, "详细资料(D)", ListStyle::Details),
    ]
    .into_iter()
    .map(|(command, title, value)| MenuItem {
        id: command as u64,
        label: title,
        shortcut: "",
        state: if style == value { 2 } else { 0 },
    })
    .collect()
}
pub fn menus(state: &Availability) -> Vec<(&'static str, Vec<MenuItem<'static>>)> {
    let enabled = !state.busy;
    let selected = state.selected && enabled;
    let modify = state.editable && enabled;
    let mut file = vec![
        item(Command::Open, "打开(O)", "Enter", selected),
        item(Command::NewFolder, "新建文件夹(F)", "", modify),
    ];
    if state.trash {
        file.push(separated(item(Command::Restore, "还原(R)", "", selected)));
        file.push(item(Command::EmptyTrash, "清空回收站(B)", "", enabled));
    }
    file.extend([
        separated(item(Command::Delete, "删除(D)", "Del", selected && modify)),
        item(Command::Rename, "重命名(M)", "F2", selected && modify),
        item(Command::Properties, "属性(R)", "Alt+Enter", selected),
        separated(item(Command::Close, "关闭(C)", "", true)),
    ]);
    let edit = vec![
        item(Command::Cut, "剪切(T)", "Ctrl+X", selected && modify),
        item(Command::Copy, "复制(C)", "Ctrl+C", selected),
        item(Command::Paste, "粘贴(P)", "Ctrl+V", modify && state.paste),
        separated(item(Command::SelectAll, "全部选中(A)", "Ctrl+A", enabled)),
    ];
    let mut view = vec![
        MenuItem {
            id: Command::ToggleToolbar as u64,
            label: "工具栏(T)",
            shortcut: "",
            state: if state.toolbar { 2 } else { 0 },
        },
        MenuItem {
            id: Command::ToggleStatus as u64,
            label: "状态栏(B)",
            shortcut: "",
            state: if state.status { 2 } else { 0 },
        },
        MenuItem {
            id: Command::ToggleFolders as u64,
            label: "文件夹(O)",
            shortcut: "",
            state: if state.folders { 2 } else { 0 },
        },
    ];
    let mut styles = views(state.style);
    if let Some(first) = styles.first_mut() {
        first.state |= 4;
    }
    view.extend(styles);
    view.push(separated(item(Command::Refresh, "刷新(R)", "F5", enabled)));
    vec![
        ("文件(F)", file),
        ("编辑(E)", edit),
        ("查看(V)", view),
        (
            "收藏(A)",
            vec![MenuItem {
                id: 1000,
                label: "(空)",
                shortcut: "",
                state: 1,
            }],
        ),
        (
            "工具(T)",
            vec![MenuItem {
                id: 1001,
                label: "文件夹选项(O)...",
                shortcut: "",
                state: 1,
            }],
        ),
        (
            "帮助(H)",
            vec![item(
                Command::About,
                "关于 LiteOS 资源管理器(A)",
                "",
                enabled,
            )],
        ),
    ]
}
pub fn context(state: &Availability, on_item: bool) -> Vec<MenuItem<'static>> {
    let mut items = Vec::new();
    if on_item {
        items.extend([
            item(Command::Open, "打开(O)", "", true),
            separated(item(Command::Cut, "剪切(T)", "", state.editable)),
            item(Command::Copy, "复制(C)", "", true),
            item(Command::Delete, "删除(D)", "", state.editable),
            item(Command::Rename, "重命名(M)", "", state.editable),
            separated(item(Command::Properties, "属性(R)", "", true)),
        ]);
        if state.trash {
            items.push(item(Command::Restore, "还原(R)", "", true));
        }
    } else {
        items.extend([
            item(Command::Paste, "粘贴(P)", "", state.editable && state.paste),
            item(Command::NewFolder, "新建文件夹(F)", "", state.editable),
            separated(item(Command::Refresh, "刷新(R)", "", true)),
            item(Command::ViewMenu, "查看(V)", "", true),
        ]);
    }
    items
}
