//! 任务管理器的菜单内容：命令、更新速度与随标签页变化的菜单栏；绘制与命中由 `classic-ui` 的下拉菜单负责。
use classic_ui::{DropItem, DropMenu};

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
}

/// 按当前标签页和更新速度构造整条菜单栏。"窗口"只在应用程序页出现，与 Windows 2000 一致。
pub fn menu_bar(applications_tab: bool, speed: Speed) -> Vec<DropMenu<Command>> {
    let speed_item = |label, value| {
        let item = DropItem::new(label, "", Command::SetSpeed(value)).checked(speed == value);
        if value == Speed::High {
            item.separated()
        } else {
            item
        }
    };
    let mut bar = vec![
        DropMenu {
            title: "文件(F)",
            items: vec![
                DropItem::new("新任务(运行)(N)...", "", Command::NewTask),
                DropItem::new("退出任务管理器(X)", "", Command::Exit).separated(),
            ],
        },
        DropMenu {
            title: "查看(V)",
            items: vec![
                DropItem::new("立即刷新(R)", "F5", Command::Refresh),
                speed_item("更新速度：高(H)", Speed::High),
                speed_item("更新速度：正常(N)", Speed::Normal),
                speed_item("更新速度：低(L)", Speed::Low),
                speed_item("更新速度：已暂停(P)", Speed::Paused),
            ],
        },
    ];
    if applications_tab {
        bar.push(DropMenu {
            title: "窗口(W)",
            items: vec![
                DropItem::new("最小化(M)", "", Command::Minimize),
                DropItem::new("最大化(X)", "", Command::Maximize),
                DropItem::new("前移(B)", "", Command::BringToFront),
            ],
        });
    }
    bar.push(DropMenu {
        title: "帮助(H)",
        items: vec![DropItem::new("关于任务管理器(A)", "", Command::About)],
    });
    bar
}
