//! WinBox 式左侧导航：深蓝竖排品牌条与平面菜单。菜单项悬停高亮，带 ▸ 的组在右侧弹出子菜单。
//! 菜单几何、悬停与键盘位置只由本模块持有；绘制经两个画布的显示列表提交。
use crate::{
    child,
    routes::{Dialog, Section},
    workspace::Page,
};
use liteos_ui::{
    ColorRole, Draw, Event, EventKind, FontRole, Layout, MenuItem, Rect, Widget, WidgetKind,
};

#[derive(Clone, Copy)]
struct Entry {
    label: &'static str,
    action: Action,
}

#[derive(Clone, Copy)]
enum Menu {
    Item(Entry),
    Group(&'static str, &'static [Entry]),
}

/// 菜单命令：打开工作区窗口、打开配置对话框、启动系统终端或退出。
#[derive(Clone, Copy)]
pub(super) enum Action {
    Open(Page),
    Dialog(Dialog),
    Terminal,
    Exit,
}

const NAVIGATION: &[Menu] = {
    use Action::Open;
    use Dialog::Settings;
    use Page::{
        Addresses, Arp, Backups, Bandwidth, Bridges, Configuration, Interfaces, Ipv4Firewall,
        Ipv4Routes, Ipv6Firewall, Ipv6Neighbors, Ipv6Routes, Ping, Routes, Trace,
    };
    &[
        Menu::Item(Entry {
            label: "接口",
            action: Open(Interfaces),
        }),
        Menu::Item(Entry {
            label: "网桥",
            action: Open(Bridges),
        }),
        Menu::Group(
            "IP",
            &[
                Entry {
                    label: "ARP",
                    action: Open(Arp),
                },
                Entry {
                    label: "地址",
                    action: Open(Addresses),
                },
                Entry {
                    label: "防火墙",
                    action: Open(Ipv4Firewall),
                },
                Entry {
                    label: "地址分发与 DNS",
                    action: Action::Dialog(Settings(Some(Section::AddressServices))),
                },
                Entry {
                    label: "路由",
                    action: Open(Ipv4Routes),
                },
            ],
        ),
        Menu::Group(
            "IPv6",
            &[
                Entry {
                    label: "防火墙",
                    action: Open(Ipv6Firewall),
                },
                Entry {
                    label: "邻居",
                    action: Open(Ipv6Neighbors),
                },
                Entry {
                    label: "路由",
                    action: Open(Ipv6Routes),
                },
            ],
        ),
        Menu::Group(
            "路由",
            &[
                Entry {
                    label: "路由表",
                    action: Open(Routes),
                },
                Entry {
                    label: "路由域与选路",
                    action: Action::Dialog(Settings(None)),
                },
                Entry {
                    label: "路由准入",
                    action: Action::Dialog(Settings(Some(Section::Admission))),
                },
                Entry {
                    label: "OSPF 邻居",
                    action: Action::Dialog(Dialog::Ospf),
                },
                Entry {
                    label: "OSPF 设置",
                    action: Action::Dialog(Settings(Some(Section::Ospf))),
                },
                Entry {
                    label: "BGP 会话",
                    action: Action::Dialog(Dialog::Bgp),
                },
                Entry {
                    label: "BGP 设置",
                    action: Action::Dialog(Settings(Some(Section::Bgp))),
                },
            ],
        ),
        Menu::Group(
            "系统",
            &[
                Entry {
                    label: "路由配置",
                    action: Open(Configuration),
                },
                Entry {
                    label: "配置备份",
                    action: Open(Backups),
                },
            ],
        ),
        Menu::Group(
            "工具",
            &[
                Entry {
                    label: "Ping",
                    action: Open(Ping),
                },
                Entry {
                    label: "路由跟踪",
                    action: Open(Trace),
                },
                Entry {
                    label: "带宽测试",
                    action: Open(Bandwidth),
                },
            ],
        ),
        Menu::Item(Entry {
            label: "新终端",
            action: Action::Terminal,
        }),
        Menu::Item(Entry {
            label: "退出",
            action: Action::Exit,
        }),
    ]
};

/// 工作区窗口默认使用菜单项名称；IPv6 防火墙在标题上注明地址族。
pub(super) fn title(page: Page) -> &'static str {
    if page == Page::Ipv6Firewall {
        return "IPv6 防火墙";
    }
    NAVIGATION
        .iter()
        .flat_map(|menu| match menu {
            Menu::Item(entry) => std::slice::from_ref(entry),
            Menu::Group(_, entries) => entries,
        })
        .find(|entry| matches!(entry.action, Action::Open(open) if open == page))
        .expect("each page has one menu entry")
        .label
}

/// 菜单行高、品牌条宽与菜单列宽（逻辑像素）。
const ROW: i32 = 20;
const BRAND: i32 = 26;
const WIDTH: i32 = 136;
const BRAND_TEXT: &str = "LiteOS 网络管理器";
const BRAND_FILL: u32 = 0x2A3C8C;
const BRAND_INK: u32 = 0xBCC8EE;

/// evdev 键码：方向键与回车。
const KEY_UP: u32 = 103;
const KEY_DOWN: u32 = 108;
const KEY_RIGHT: u32 = 106;
const KEY_ENTER: u32 = 28;

pub(super) struct Navigation {
    brand: Widget,
    menu: Widget,
    popup: Widget,
    pointer: (f64, f64),
    // 指针所在或键盘选中的行；指针离开画布时清除，避免残留高亮。
    hot: Option<usize>,
    // 子菜单弹出期间，该组保持高亮，弹出菜单的命令编号只映射到这一组条目。
    open: Option<usize>,
}

impl Navigation {
    /// 在 `parent` 横向容器的左端建立品牌条与菜单列。
    ///
    /// 错误：窗口已关闭或运行库拒绝字体设置。
    pub(super) fn new(parent: &Widget, popup: Widget) -> liteos_ui::Result<Self> {
        let brand = child(
            parent,
            WidgetKind::Canvas,
            "",
            Layout {
                width: BRAND,
                min_width: BRAND,
                ..Layout::default()
            },
        )?;
        brand.set_font(FontRole::Caption, Some(14), Some(700))?;
        let menu = child(
            parent,
            WidgetKind::Canvas,
            "",
            Layout {
                width: WIDTH,
                min_width: WIDTH,
                ..Layout::default()
            },
        )?;
        Ok(Self {
            brand,
            menu,
            popup,
            pointer: (0.0, 0.0),
            hot: None,
            open: None,
        })
    }

    /// 处理菜单输入；选中的命令作为返回值交给主循环执行。窗口尺寸变化时重画。
    ///
    /// 错误：窗口已关闭。
    pub(super) fn handle(&mut self, event: &Event) -> liteos_ui::Result<Option<Action>> {
        if event.node == self.popup.id() {
            let action = match event.kind {
                EventKind::Activate { value } => self.open.take().and_then(|group| {
                    let Menu::Group(_, entries) = NAVIGATION[group] else {
                        return None;
                    };
                    let index = usize::try_from(value.checked_sub(1)?).ok()?;
                    entries.get(index).map(|entry| entry.action)
                }),
                EventKind::Dismiss => {
                    self.open = None;
                    None
                }
                _ => return Ok(None),
            };
            // 菜单关闭时指针多半已在子菜单或工作区上，组行不应残留悬停高亮。
            self.hot = None;
            self.paint()?;
            return Ok(action);
        }
        match event.kind {
            EventKind::Configure { .. } => self.paint().map(|()| None),
            EventKind::Pointer { x, y } => {
                self.pointer = (x, y);
                let hot = if event.node == self.menu.id() {
                    self.row_at()?
                } else {
                    None
                };
                if hot != self.hot {
                    self.hot = hot;
                    self.paint()?;
                }
                Ok(None)
            }
            EventKind::Button {
                code: 0x110,
                pressed: true,
                ..
            } if event.node == self.menu.id() => match self.row_at()? {
                Some(row) => self.choose(row),
                None => Ok(None),
            },
            EventKind::Key {
                code,
                pressed: true,
                ..
            } if event.node == self.menu.id() => self.key(code),
            _ => Ok(None),
        }
    }

    fn key(&mut self, code: u32) -> liteos_ui::Result<Option<Action>> {
        let last = NAVIGATION.len() - 1;
        let hot = match (code, self.hot) {
            (KEY_UP, Some(row)) => row.saturating_sub(1),
            (KEY_UP, None) => last,
            (KEY_DOWN, Some(row)) => (row + 1).min(last),
            (KEY_DOWN, None) => 0,
            (KEY_ENTER | KEY_RIGHT, Some(row)) => return self.choose(row),
            _ => return Ok(None),
        };
        self.hot = Some(hot);
        self.paint()?;
        Ok(None)
    }

    /// 单项直接返回命令；组在该行右侧弹出子菜单，命令随后经弹出菜单事件返回。
    fn choose(&mut self, row: usize) -> liteos_ui::Result<Option<Action>> {
        let entries = match NAVIGATION[row] {
            Menu::Item(entry) => return Ok(Some(entry.action)),
            Menu::Group(_, entries) => entries,
        };
        let items: Vec<_> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| MenuItem {
                id: index as u64 + 1,
                label: entry.label,
                shortcut: "",
                state: 0,
                parent: 0,
            })
            .collect();
        self.popup.set_menus(&[liteos_ui::Menu {
            title: "",
            items: &items,
        }])?;
        let bounds = self.menu.geometry()?;
        self.popup
            .open_popup(Some((bounds.right(), bounds.y + row as i32 * ROW)))?;
        self.open = Some(row);
        self.paint()?;
        Ok(None)
    }

    fn row_at(&self) -> liteos_ui::Result<Option<usize>> {
        let bounds = self.menu.geometry()?;
        let (x, y) = (
            self.pointer.0 as i32 - bounds.x,
            self.pointer.1 as i32 - bounds.y,
        );
        if !(0..bounds.width).contains(&x) || y < 0 {
            return Ok(None);
        }
        let row = (y / ROW) as usize;
        Ok((row < NAVIGATION.len()).then_some(row))
    }

    fn paint(&self) -> liteos_ui::Result<()> {
        let height = self.brand.geometry()?.height;
        // 品牌字自底边向上书写，底部留 10 像素；布局前高度可能不足，矩形高度不能为负。
        self.brand.set_drawing(&[
            fill(Rect::new(0, 0, BRAND, height), BRAND_FILL),
            Draw {
                kind: 9,
                rect: Rect::new(0, 0, BRAND, (height - 10).max(0)),
                color: BRAND_INK,
                bold: true,
                text: BRAND_TEXT,
            },
        ])?;
        let face = self.menu.color(ColorRole::Control)?;
        let text = self.menu.color(ColorRole::WindowText)?;
        let highlight = self.menu.color(ColorRole::Highlight)?;
        let highlight_text = self.menu.color(ColorRole::HighlightText)?;
        // 经典蚀刻分隔线：按钮面的阴影色（约六成亮度）在上，白色高光在下。
        let shadow = [16, 8, 0]
            .iter()
            .map(|shift| (((face >> shift) & 0xFF) * 3 / 5) << shift)
            .sum::<u32>();
        let mut commands = vec![fill(
            Rect::new(0, 0, WIDTH, self.menu.geometry()?.height),
            face,
        )];
        for (index, menu) in NAVIGATION.iter().enumerate() {
            let y = index as i32 * ROW;
            let active =
                self.open == Some(index) || (self.open.is_none() && self.hot == Some(index));
            let ink = if active {
                commands.push(fill(Rect::new(1, y + 1, WIDTH - 2, ROW - 3), highlight));
                highlight_text
            } else {
                text
            };
            let label = match menu {
                Menu::Item(entry) => entry.label,
                Menu::Group(label, _) => {
                    // 右指三角：自左向右四列，高度 7、5、3、1。
                    for column in 0..4 {
                        commands.push(fill(
                            Rect::new(WIDTH - 14 + column, y + 6 + column, 1, 7 - 2 * column),
                            ink,
                        ));
                    }
                    label
                }
            };
            commands.push(Draw {
                kind: 3,
                rect: Rect::new(8, y + 3, WIDTH - 26, ROW - 4),
                color: ink,
                bold: false,
                text: label,
            });
            commands.push(fill(Rect::new(2, y + ROW - 2, WIDTH - 4, 1), shadow));
            commands.push(fill(Rect::new(2, y + ROW - 1, WIDTH - 4, 1), 0xFFFFFF));
        }
        self.menu.set_drawing(&commands)
    }
}

fn fill(rect: Rect, color: u32) -> Draw<'static> {
    Draw {
        kind: 1,
        rect,
        color,
        bold: false,
        text: "",
    }
}
