//! 资源管理器的业务命令、菜单与标准按钮数据；无绘制、命中或控件交互状态。
//!
//! 菜单结构取自 Windows 2000 SP4 简中 `browselc.dll` 菜单 266（文件夹框架）与 `shell32.dll` 菜单
//! 210/215/216/221（DefView 合并项、右键菜单、排列图标）；没有后端的命令保留原位并禁用。
use crate::model::Field;
use liteos_ui::{ListStyle, MenuItem, ToolbarButton, ToolbarKind};

/// 无参数命令；数值是菜单与工具栏的命令标识。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum Action {
    Open = 1,
    Close,
    Cut,
    Copy,
    Paste,
    PasteShortcut,
    Undo,
    Delete,
    Rename,
    CreateShortcut,
    NewFolder,
    NewShortcut,
    NewTextDocument,
    SendToDocuments,
    SendToDesktop,
    Restore,
    RestoreAll,
    EmptyTrash,
    Properties,
    SelectAll,
    InvertSelection,
    CopyTo,
    MoveTo,
    ToggleStandardButtons,
    ToggleAddressBar,
    ToggleLinks,
    CustomizeToolbar,
    ToggleStatus,
    BarSearch,
    BarFavorites,
    BarHistory,
    ToggleFolders,
    LargeIcons,
    SmallIcons,
    List,
    Details,
    AutoArrange,
    LineUpIcons,
    ChooseColumns,
    CustomizeFolder,
    Back,
    Forward,
    Up,
    Home,
    Refresh,
    AddFavorite,
    OrganizeFavorites,
    MapDrive,
    DisconnectDrive,
    Synchronize,
    FolderOptions,
    HelpTopics,
    AboutWindows,
    ViewMenu,
    Search,
    History,
    Address,
    /// 网络连接的启用、禁用与状态（netshell 菜单 11010/11012）。
    Enable,
    Disable,
    Status,
}

const ACTIONS: [Action; 60] = [
    Action::Open,
    Action::Close,
    Action::Cut,
    Action::Copy,
    Action::Paste,
    Action::PasteShortcut,
    Action::Undo,
    Action::Delete,
    Action::Rename,
    Action::CreateShortcut,
    Action::NewFolder,
    Action::NewShortcut,
    Action::NewTextDocument,
    Action::SendToDocuments,
    Action::SendToDesktop,
    Action::Restore,
    Action::RestoreAll,
    Action::EmptyTrash,
    Action::Properties,
    Action::SelectAll,
    Action::InvertSelection,
    Action::CopyTo,
    Action::MoveTo,
    Action::ToggleStandardButtons,
    Action::ToggleAddressBar,
    Action::ToggleLinks,
    Action::CustomizeToolbar,
    Action::ToggleStatus,
    Action::BarSearch,
    Action::BarFavorites,
    Action::BarHistory,
    Action::ToggleFolders,
    Action::LargeIcons,
    Action::SmallIcons,
    Action::List,
    Action::Details,
    Action::AutoArrange,
    Action::LineUpIcons,
    Action::ChooseColumns,
    Action::CustomizeFolder,
    Action::Back,
    Action::Forward,
    Action::Up,
    Action::Home,
    Action::Refresh,
    Action::AddFavorite,
    Action::OrganizeFavorites,
    Action::MapDrive,
    Action::DisconnectDrive,
    Action::Synchronize,
    Action::FolderOptions,
    Action::HelpTopics,
    Action::AboutWindows,
    Action::ViewMenu,
    Action::Search,
    Action::History,
    Action::Address,
    Action::Enable,
    Action::Disable,
    Action::Status,
];

/// 菜单与工具栏命令：无参数动作、按某列排列图标、跳到历史记录第 n 项。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    Do(Action),
    Arrange(Field),
    Visit(usize),
}

/// 命令标识的分段：动作 1..，排列 1000 + 列标识，历史 2000 + 下标；子菜单标题 9000..（不产生命令）。
const ARRANGE: u64 = 1000;
const VISIT: u64 = 2000;
const HEADER: u64 = 9000;

impl Command {
    pub fn id(self) -> u64 {
        match self {
            Command::Do(action) => action as u64,
            Command::Arrange(field) => ARRANGE + field as u64,
            Command::Visit(index) => VISIT + index as u64,
        }
    }

    pub fn from_id(id: u64) -> Option<Self> {
        match id {
            ARRANGE..VISIT => Field::from_id(id - ARRANGE).map(Command::Arrange),
            VISIT..HEADER => Some(Command::Visit((id - VISIT) as usize)),
            _ => ACTIONS
                .iter()
                .find(|action| **action as u64 == id)
                .map(|action| Command::Do(*action)),
        }
    }

    pub fn style(self) -> Option<ListStyle> {
        match self {
            Command::Do(Action::LargeIcons) => Some(ListStyle::LargeIcons),
            Command::Do(Action::SmallIcons) => Some(ListStyle::SmallIcons),
            Command::Do(Action::List) => Some(ListStyle::List),
            Command::Do(Action::Details) => Some(ListStyle::Details),
            _ => None,
        }
    }
}

/// 构造菜单时需要的窗口状态。
pub struct Availability {
    pub selected: bool,
    /// 当前位置是可写的真实目录。
    pub editable: bool,
    pub paste: bool,
    /// 当前位置是回收站根目录。
    pub trash: bool,
    /// 回收站中有项目。
    pub trash_items: bool,
    pub standard_buttons: bool,
    pub address_bar: bool,
    pub status: bool,
    pub folders: bool,
    pub style: ListStyle,
    pub busy: bool,
    pub back: bool,
    pub forward: bool,
    pub up: bool,
    /// 排列图标可用的列（当前位置的详细资料列）。
    pub columns: Vec<(Field, &'static str)>,
    /// 当前排序列。
    pub sort: Field,
    /// “转到”菜单末尾的历史记录：（历史下标，标题），当前位置带勾选。
    pub history: Vec<(usize, String, bool)>,
    /// 选中项是网络连接时为它是否启用。
    pub connection: Option<bool>,
}

/// 网络连接的动词（netshell 菜单 11012 启用中 / 11010 已禁用）：启用中的默认动词是“状态”，已禁用的是“启用”。
/// 局域网连接不能删除；系统没有 .lnk 快捷方式与连接名存储，创建快捷方式与重命名保留原位并禁用。
fn connection_verbs(b: &mut Builder, enabled: bool, idle: bool) {
    let state = |default: bool| match (idle, default) {
        (false, _) => DISABLED,
        (true, true) => DEFAULT,
        (true, false) => 0,
    };
    if enabled {
        b.push(Action::Disable as u64, "禁用(B)", "", state(false));
        b.push(Action::Status as u64, "状态(U)", "", state(true));
    } else {
        b.push(Action::Enable as u64, "启用(A)", "", state(true));
        b.action(Action::Status, "状态(U)", "", false);
    }
    b.separator();
    b.action(Action::CreateShortcut, "创建快捷方式(S)", "", false);
    b.action(Action::Delete, "删除(D)", "", false);
    b.action(Action::Rename, "重命名(M)", "", false);
    b.separator();
    b.action(Action::Properties, "属性(R)", "", idle);
}

const DISABLED: u32 = 1;
const CHECKED: u32 = 2;
const SEPARATED: u32 = 4;
const DEFAULT: u32 = 8;

/// 菜单数据的拥有者：标签可能是运行时生成的文字（历史记录、排列方式）。
pub struct Entry {
    pub id: u64,
    pub label: String,
    pub shortcut: &'static str,
    pub state: u32,
    pub parent: u64,
}

impl Entry {
    pub fn item(&self) -> MenuItem<'_> {
        MenuItem {
            id: self.id,
            label: &self.label,
            shortcut: self.shortcut,
            state: self.state,
            parent: self.parent,
        }
    }
}

/// 逐项追加菜单条目；`separated` 标记下一项上方的分隔线。
struct Builder {
    entries: Vec<Entry>,
    parent: u64,
    separated: bool,
    next_header: u64,
}

impl Builder {
    fn new(first_header: u64) -> Self {
        Self {
            entries: Vec::new(),
            parent: 0,
            separated: false,
            next_header: first_header,
        }
    }

    /// 分隔线只出现在同一层的两个条目之间：层首或连续的分隔请求都合并掉。
    fn push(&mut self, id: u64, label: impl Into<String>, shortcut: &'static str, mut state: u32) {
        let sibling = self.entries.iter().any(|entry| entry.parent == self.parent);
        if std::mem::take(&mut self.separated) && sibling {
            state |= SEPARATED;
        }
        self.entries.push(Entry {
            id,
            label: label.into(),
            shortcut,
            state,
            parent: self.parent,
        });
    }

    fn action(&mut self, action: Action, label: &str, shortcut: &'static str, enabled: bool) {
        self.push(
            action as u64,
            label,
            shortcut,
            if enabled { 0 } else { DISABLED },
        );
    }

    fn check(&mut self, action: Action, label: &str, checked: bool, enabled: bool) {
        let mut state = if checked { CHECKED } else { 0 };
        if !enabled {
            state |= DISABLED;
        }
        self.push(action as u64, label, "", state);
    }

    fn separator(&mut self) {
        self.separated = true;
    }

    /// 追加级联子菜单，`build` 在其中追加子条目。
    fn submenu(&mut self, label: &str, enabled: bool, build: impl FnOnce(&mut Self)) {
        let id = self.next_header;
        self.next_header += 1;
        self.push(id, label, "", if enabled { 0 } else { DISABLED });
        let parent = std::mem::replace(&mut self.parent, id);
        self.separated = false;
        build(self);
        self.parent = parent;
        self.separated = false;
    }
}

fn views(b: &mut Builder, style: ListStyle) {
    for (action, label, value) in [
        (Action::LargeIcons, "大图标(G)", ListStyle::LargeIcons),
        (Action::SmallIcons, "小图标(M)", ListStyle::SmallIcons),
        (Action::List, "列表(L)", ListStyle::List),
        (Action::Details, "详细资料(D)", ListStyle::Details),
    ] {
        b.check(action, label, style == value, true);
    }
}

/// shell32 菜单 221 的排列方式；只列当前位置有的列，助记键沿用原版（名称 N、类型 T、大小 Z、日期 D）。
fn arrange(b: &mut Builder, state: &Availability) {
    for (field, title) in &state.columns {
        let mnemonic = match field {
            Field::Name => "(N)",
            Field::Kind => "(T)",
            Field::Size | Field::Capacity => "(Z)",
            Field::Modified | Field::Deleted => "(D)",
            _ => "",
        };
        let id = Command::Arrange(*field).id();
        b.push(
            id,
            format!("按{title}{mnemonic}"),
            "",
            if state.sort == *field { CHECKED } else { 0 },
        );
    }
    b.separator();
    // 列表视图始终按网格自动排列；没有自由摆放模式，因此该项恒为选中且不可切换。
    b.check(Action::AutoArrange, "自动排列(A)", true, false);
}

fn new_items(b: &mut Builder, enabled: bool) {
    b.submenu("新建(W)", enabled, |b| {
        b.action(Action::NewFolder, "文件夹(F)", "", true);
        // LiteOS 没有 .lnk 外壳链接格式。
        b.action(Action::NewShortcut, "快捷方式(S)", "", false);
        b.separator();
        b.action(Action::NewTextDocument, "文本文档", "", true);
    });
}

/// 菜单栏（browselc 266 合并 shell32 216）。
pub fn menus(state: &Availability) -> Vec<(&'static str, Vec<Entry>)> {
    let enabled = !state.busy;
    let selected = state.selected && enabled;
    let modify = state.editable && enabled;
    let mut file = Builder::new(HEADER);
    if let Some(connected) = state.connection {
        connection_verbs(&mut file, connected, enabled);
    } else if state.selected {
        file.push(
            Action::Open as u64,
            "打开(O)",
            "",
            if enabled { DEFAULT } else { DISABLED },
        );
        if state.trash {
            file.action(Action::Restore, "还原(E)", "", selected);
        } else {
            file.separator();
            file.submenu("发送到(N)", selected, |b| {
                b.action(Action::SendToDesktop, "桌面快捷方式", "", false);
                b.action(Action::SendToDocuments, "我的文档", "", true);
            });
        }
        file.separator();
    } else if state.editable && !state.trash {
        new_items(&mut file, modify);
        file.separator();
    }
    if state.trash {
        file.action(
            Action::EmptyTrash,
            "清空回收站(B)",
            "",
            enabled && state.trash_items,
        );
        file.separator();
    }
    if state.connection.is_none() {
        file.action(Action::CreateShortcut, "创建快捷方式(S)", "", false);
        file.action(Action::Delete, "删除(D)", "", selected && modify);
        file.action(
            Action::Rename,
            "重命名(M)",
            "",
            selected && modify && !state.trash,
        );
        file.action(Action::Properties, "属性(R)", "", selected);
    }
    file.separator();
    file.action(Action::Close, "关闭(C)", "", true);

    let mut edit = Builder::new(HEADER + 100);
    edit.action(Action::Undo, "撤消(U)", "Ctrl+Z", false);
    edit.separator();
    edit.action(Action::Cut, "剪切(T)", "Ctrl+X", selected && modify);
    edit.action(Action::Copy, "复制(C)", "Ctrl+C", selected && !state.trash);
    edit.action(
        Action::Paste,
        "粘贴(P)",
        "Ctrl+V",
        modify && state.paste && !state.trash,
    );
    edit.action(Action::PasteShortcut, "粘贴快捷方式(S)", "", false);
    edit.separator();
    edit.action(
        Action::CopyTo,
        "复制到文件夹(F)...",
        "",
        selected && !state.trash,
    );
    edit.action(Action::MoveTo, "移动到文件夹(V)...", "", selected && modify);
    edit.separator();
    edit.action(Action::SelectAll, "全部选定(A)", "Ctrl+A", enabled);
    edit.action(Action::InvertSelection, "反向选择(I)", "", enabled);

    let mut view = Builder::new(HEADER + 200);
    view.submenu("工具栏(T)", true, |b| {
        b.check(
            Action::ToggleStandardButtons,
            "标准按钮(S)",
            state.standard_buttons,
            true,
        );
        b.check(
            Action::ToggleAddressBar,
            "地址栏(A)",
            state.address_bar,
            true,
        );
        // 没有 IE 链接收藏夹。
        b.check(Action::ToggleLinks, "链接(L)", false, false);
        b.separator();
        b.action(Action::CustomizeToolbar, "自定义(C)...", "", false);
    });
    view.check(Action::ToggleStatus, "状态栏(B)", state.status, true);
    view.submenu("浏览栏(E)", true, |b| {
        // 没有搜索助理、收藏夹与历史记录存储。
        b.action(Action::BarSearch, "搜索(S)", "Ctrl+E", false);
        b.action(Action::BarFavorites, "收藏夹(F)", "Ctrl+I", false);
        b.action(Action::BarHistory, "历史记录(H)", "Ctrl+H", false);
        b.check(Action::ToggleFolders, "文件夹(O)", state.folders, true);
    });
    view.separator();
    views(&mut view, state.style);
    view.separator();
    view.submenu("排列图标(I)", enabled, |b| arrange(b, state));
    view.action(Action::LineUpIcons, "对齐图标(U)", "", false);
    view.separator();
    view.action(Action::ChooseColumns, "选择列(N)...", "", false);
    view.action(Action::CustomizeFolder, "自定义文件夹(C)...", "", false);
    view.separator();
    view.submenu("转到(O)", true, |b| {
        b.action(Action::Back, "后退(B)", "Alt+←", enabled && state.back);
        b.action(
            Action::Forward,
            "前进(F)",
            "Alt+→",
            enabled && state.forward,
        );
        b.action(Action::Up, "向上一级(U)", "", enabled && state.up);
        b.separator();
        b.action(Action::Home, "主页(H)", "Alt+Home", false);
        b.separator();
        for (index, title, current) in &state.history {
            let mut state = if *current { CHECKED } else { 0 };
            if !enabled {
                state |= DISABLED;
            }
            b.push(Command::Visit(*index).id(), title.clone(), "", state);
        }
    });
    view.action(Action::Refresh, "刷新(R)", "", enabled);

    let mut favorites = Builder::new(HEADER + 300);
    favorites.action(Action::AddFavorite, "添加到收藏夹(A)...", "", false);
    favorites.action(Action::OrganizeFavorites, "整理收藏夹(O)...", "", false);
    favorites.separator();
    favorites.push(HEADER + 399, "(空)", "", DISABLED);

    let mut tools = Builder::new(HEADER + 400);
    tools.action(Action::MapDrive, "映射网络驱动器(N)...", "", false);
    tools.action(Action::DisconnectDrive, "断开网络驱动器(D)...", "", false);
    tools.action(Action::Synchronize, "同步(S)...", "", false);
    tools.separator();
    tools.action(Action::FolderOptions, "文件夹选项(O)...", "", false);

    let mut help = Builder::new(HEADER + 500);
    help.action(Action::HelpTopics, "帮助主题(H)", "", false);
    help.separator();
    help.action(Action::AboutWindows, "关于 Windows(A)", "", enabled);

    vec![
        ("文件(F)", file.entries),
        ("编辑(E)", edit.entries),
        ("查看(V)", view.entries),
        ("收藏(A)", favorites.entries),
        ("工具(T)", tools.entries),
        ("帮助(H)", help.entries),
    ]
}

/// 右键菜单：项目上按 shell32 210（回收站项为还原/剪切/删除/属性），空白处按 shell32 215 与 220。
pub fn context(state: &Availability, on_item: bool) -> Vec<Entry> {
    let mut b = Builder::new(HEADER + 600);
    if let Some(enabled) = state.connection.filter(|_| on_item) {
        connection_verbs(&mut b, enabled, true);
    } else if on_item && state.trash {
        b.action(Action::Restore, "还原(E)", "", true);
        b.separator();
        b.action(Action::Cut, "剪切(T)", "", state.editable);
        b.separator();
        b.action(Action::Delete, "删除(D)", "", state.editable);
        b.separator();
        b.action(Action::Properties, "属性(R)", "", true);
    } else if on_item {
        b.push(Action::Open as u64, "打开(O)", "", DEFAULT);
        b.separator();
        b.submenu("发送到(N)", true, |b| {
            b.action(Action::SendToDesktop, "桌面快捷方式", "", false);
            b.action(Action::SendToDocuments, "我的文档", "", true);
        });
        b.separator();
        b.action(Action::Cut, "剪切(T)", "", state.editable);
        b.action(Action::Copy, "复制(C)", "", true);
        b.separator();
        b.action(Action::CreateShortcut, "创建快捷方式(S)", "", false);
        b.action(Action::Delete, "删除(D)", "", state.editable);
        b.action(Action::Rename, "重命名(M)", "", state.editable);
        b.separator();
        b.action(Action::Properties, "属性(R)", "", true);
    } else {
        b.submenu("查看(V)", true, |b| views(b, state.style));
        b.separator();
        b.submenu("排列图标(I)", true, |b| arrange(b, state));
        b.action(Action::LineUpIcons, "对齐图标(L)", "", false);
        b.separator();
        b.action(Action::Refresh, "刷新(E)", "", true);
        b.separator();
        b.action(Action::CustomizeFolder, "自定义文件夹(C)...", "", false);
        b.separator();
        b.action(
            Action::Paste,
            "粘贴(P)",
            "",
            state.editable && state.paste && !state.trash,
        );
        b.action(Action::PasteShortcut, "粘贴快捷方式(S)", "", false);
        b.action(Action::Undo, "撤消(U)", "", false);
        if state.editable && !state.trash {
            b.separator();
            new_items(&mut b, true);
        }
        b.separator();
        b.action(Action::Properties, "属性(R)", "", false);
    }
    b.entries
}

/// “查看”按钮的下拉菜单。
pub fn view_menu(style: ListStyle) -> Vec<Entry> {
    let mut b = Builder::new(HEADER + 700);
    views(&mut b, style);
    b.entries
}

/// “后退”/“前进”按钮的下拉菜单：最多 9 个历史位置，离当前最近的在上（browseui 的旅行日志）。
pub fn travel_menu(entries: &[(usize, String)]) -> Vec<Entry> {
    let mut b = Builder::new(HEADER + 800);
    for (index, title) in entries.iter().take(9) {
        b.push(Command::Visit(*index).id(), title.clone(), "", 0);
    }
    b.entries
}

/// 位图条名称与帧号：外壳条 `toolbar`（shell32 272/273/276），IE 条 `ie-toolbar`（browseui 261/262/265）。
const SHELL: &str = "toolbar";
const IE: &str = "ie-toolbar";

/// Windows 2000 资源管理器的标准按钮：后退▾ 前进▾ 向上 ┃ 搜索 文件夹 历史 ┃ 移至 复制到 删除 撤消 ┃ 查看▾，
/// 默认“选择性文字在右侧”（后退、搜索、文件夹、历史带文字）。
pub fn standard_buttons(state: &Availability) -> Vec<ToolbarButton<'static>> {
    let enabled = !state.busy;
    let button = |action: Action, kind, strip, frame, text, show_text, on: bool| ToolbarButton {
        id: action as u64,
        kind,
        strip,
        frame,
        text,
        show_text,
        enabled: on,
        checked: false,
    };
    let separator = |id: u64| ToolbarButton {
        id,
        kind: ToolbarKind::Separator,
        strip: "",
        frame: 0,
        text: "",
        show_text: false,
        enabled: true,
        checked: false,
    };
    let mut folders = button(
        Action::ToggleFolders,
        ToolbarKind::Check,
        SHELL,
        43,
        "文件夹",
        true,
        true,
    );
    folders.checked = state.folders;
    vec![
        button(
            Action::Back,
            ToolbarKind::Dropdown,
            IE,
            0,
            "后退",
            true,
            enabled && state.back,
        ),
        button(
            Action::Forward,
            ToolbarKind::Dropdown,
            IE,
            1,
            "前进",
            false,
            enabled && state.forward,
        ),
        button(
            Action::Up,
            ToolbarKind::Button,
            SHELL,
            28,
            "向上",
            false,
            enabled && state.up,
        ),
        separator(HEADER + 900),
        // 没有搜索助理与历史记录存储。
        button(
            Action::Search,
            ToolbarKind::Button,
            IE,
            5,
            "搜索",
            true,
            false,
        ),
        folders,
        button(
            Action::History,
            ToolbarKind::Check,
            IE,
            12,
            "历史",
            true,
            false,
        ),
        separator(HEADER + 901),
        button(
            Action::MoveTo,
            ToolbarKind::Button,
            SHELL,
            44,
            "移至",
            false,
            enabled && state.selected && state.editable,
        ),
        button(
            Action::CopyTo,
            ToolbarKind::Button,
            SHELL,
            45,
            "复制到",
            false,
            enabled && state.selected && !state.trash,
        ),
        button(
            Action::Delete,
            ToolbarKind::Button,
            SHELL,
            10,
            "删除",
            false,
            enabled && state.selected && state.editable,
        ),
        button(
            Action::Undo,
            ToolbarKind::Button,
            SHELL,
            8,
            "撤消",
            false,
            false,
        ),
        separator(HEADER + 902),
        button(
            Action::ViewMenu,
            ToolbarKind::WholeDropdown,
            SHELL,
            32,
            "查看",
            false,
            enabled,
        ),
    ]
}
