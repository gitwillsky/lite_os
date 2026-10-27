//! 任务管理器的菜单内容：命令、更新速度与随标签页变化的菜单栏；绘制与命中由 `classic-ui` 的下拉菜单负责。
use liteos_ui::{Menu, MenuItem, Widget};

/// 更新速度：每次采样的间隔；`Paused` 不再自动采样。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    High,
    Normal,
    Low,
    Paused,
}

impl Speed {
    /// 采样间隔（毫秒）；暂停返回 `None`。
    pub fn interval_ms(self) -> Option<u64> {
        match self {
            Speed::High => Some(500),
            Speed::Normal => Some(2000),
            Speed::Low => Some(4000),
            Speed::Paused => None,
        }
    }
}

/// 当前标签页；决定菜单栏是否出现“窗口”菜单与“CPU 使用记录”子菜单。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Applications,
    Processes,
    Performance,
    Gpu,
}

/// 性能页 CPU 使用记录的排布（Windows 2000 “查看 > CPU 使用记录”）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CpuGraph {
    /// 全部 CPU 合计画在一张图里。
    All,
    /// 每个逻辑 CPU 一张图。
    PerCpu,
}

/// “CPU 使用记录”级联父项的菜单标识；它只展开子菜单，不对应命令。
const CPU_HISTORY: u64 = 14;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Command {
    NewTask,
    Exit,
    Refresh,
    SetSpeed(Speed),
    Minimize,
    Maximize,
    BringToFront,
    About,
    SetCpuGraph(CpuGraph),
}

impl Command {
    pub fn code(self) -> u64 {
        match self {
            Self::NewTask => 1,
            Self::Exit => 2,
            Self::Refresh => 3,
            Self::SetSpeed(Speed::High) => 4,
            Self::SetSpeed(Speed::Normal) => 5,
            Self::SetSpeed(Speed::Low) => 6,
            Self::SetSpeed(Speed::Paused) => 7,
            Self::Minimize => 8,
            Self::Maximize => 9,
            Self::BringToFront => 10,
            Self::About => 11,
            Self::SetCpuGraph(CpuGraph::All) => 12,
            Self::SetCpuGraph(CpuGraph::PerCpu) => 13,
        }
    }
    pub fn decode(code: u64) -> Option<Self> {
        Some(match code {
            1 => Self::NewTask,
            2 => Self::Exit,
            3 => Self::Refresh,
            4 => Self::SetSpeed(Speed::High),
            5 => Self::SetSpeed(Speed::Normal),
            6 => Self::SetSpeed(Speed::Low),
            7 => Self::SetSpeed(Speed::Paused),
            8 => Self::Minimize,
            9 => Self::Maximize,
            10 => Self::BringToFront,
            11 => Self::About,
            12 => Self::SetCpuGraph(CpuGraph::All),
            13 => Self::SetCpuGraph(CpuGraph::PerCpu),
            _ => return None,
        })
    }
}
/// 菜单数据属于应用，绘制、Alt 助记符、键盘遍历与命中属于共享运行库。
pub fn install(
    widget: &Widget,
    page: Page,
    speed: Speed,
    graph: CpuGraph,
) -> liteos_ui::Result<()> {
    let item = |label, shortcut, command: Command, state| MenuItem {
        id: command.code(),
        label,
        shortcut,
        state,
        parent: 0,
    };
    let checked = |on| if on { 2 } else { 0 };
    let file = [
        item("新任务(运行)(N)...", "", Command::NewTask, 0),
        item("退出任务管理器(X)", "", Command::Exit, 4),
    ];
    let mut view = vec![
        item("立即刷新(R)", "F5", Command::Refresh, 0),
        item(
            "更新速度：高(H)",
            "",
            Command::SetSpeed(Speed::High),
            4 | checked(speed == Speed::High),
        ),
        item(
            "更新速度：正常(N)",
            "",
            Command::SetSpeed(Speed::Normal),
            checked(speed == Speed::Normal),
        ),
        item(
            "更新速度：低(L)",
            "",
            Command::SetSpeed(Speed::Low),
            checked(speed == Speed::Low),
        ),
        item(
            "更新速度：已暂停(P)",
            "",
            Command::SetSpeed(Speed::Paused),
            checked(speed == Speed::Paused),
        ),
    ];
    if page == Page::Performance {
        view.extend([
            MenuItem {
                id: CPU_HISTORY,
                label: "CPU 使用记录(C)",
                shortcut: "",
                state: 4,
                parent: 0,
            },
            MenuItem {
                parent: CPU_HISTORY,
                ..item(
                    "所有 CPU 使用一个图表(O)",
                    "",
                    Command::SetCpuGraph(CpuGraph::All),
                    checked(graph == CpuGraph::All),
                )
            },
            MenuItem {
                parent: CPU_HISTORY,
                ..item(
                    "每个 CPU 使用一个图表(P)",
                    "",
                    Command::SetCpuGraph(CpuGraph::PerCpu),
                    checked(graph == CpuGraph::PerCpu),
                )
            },
        ]);
    }
    let windows = [
        item("最小化(M)", "", Command::Minimize, 0),
        item("最大化(X)", "", Command::Maximize, 0),
        item("前移(B)", "", Command::BringToFront, 0),
    ];
    let help = [item("关于任务管理器(A)", "", Command::About, 0)];
    let mut menus = vec![
        Menu {
            title: "文件(F)",
            items: &file,
        },
        Menu {
            title: "查看(V)",
            items: &view,
        },
    ];
    if page == Page::Applications {
        menus.push(Menu {
            title: "窗口(W)",
            items: &windows,
        });
    }
    menus.push(Menu {
        title: "帮助(H)",
        items: &help,
    });
    widget.set_menus(&menus)
}
