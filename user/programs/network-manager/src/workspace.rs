//! 经典 WinBox 的左侧菜单与工作区。每项运行视图只有一个控件树及业务 owner。
use crate::{
    child,
    routes::{Dialog, Section},
};
use liteos_ui::{Event, EventKind, Layout, MenuItem, Widget, WidgetKind, state};

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

const NAVIGATION: &[Menu] = {
    use Action::Open;
    use Dialog::Settings;
    use Page::{
        Backups, Configuration, Connections, Diagnostics, Firewall, Interfaces, Neighbors, Routes,
    };
    &[
        Menu::Item(Entry {
            label: "接口",
            action: Open(Interfaces),
        }),
        Menu::Item(Entry {
            label: "网桥",
            action: Action::Dialog(Settings(Some(Section::Bridges))),
        }),
        Menu::Group(
            "IP",
            &[
                Entry {
                    label: "地址",
                    action: Action::Dialog(Settings(Some(Section::Interfaces))),
                },
                Entry {
                    label: "邻居",
                    action: Open(Neighbors),
                },
                Entry {
                    label: "防火墙",
                    action: Open(Firewall),
                },
                Entry {
                    label: "转发连接",
                    action: Open(Connections),
                },
                Entry {
                    label: "地址分发与 DNS",
                    action: Action::Dialog(Settings(Some(Section::AddressServices))),
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
            &[Entry {
                label: "网络诊断",
                action: Open(Diagnostics),
            }],
        ),
    ]
};

#[derive(Clone, Copy)]
pub(super) enum Action {
    Open(Page),
    Close(Page),
    Dialog(Dialog),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    Interfaces,
    Routes,
    Firewall,
    Neighbors,
    Connections,
    Configuration,
    Backups,
    Diagnostics,
}

impl Page {
    const ALL: [Self; 8] = [
        Self::Interfaces,
        Self::Routes,
        Self::Firewall,
        Self::Neighbors,
        Self::Connections,
        Self::Configuration,
        Self::Backups,
        Self::Diagnostics,
    ];

    fn title(self) -> &'static str {
        NAVIGATION
            .iter()
            .flat_map(|menu| match menu {
                Menu::Item(entry) => std::slice::from_ref(entry),
                Menu::Group(_, entries) => entries,
            })
            .find(|entry| matches!(entry.action, Action::Open(page) if page == self))
            .expect("each page has one menu entry")
            .label
    }
}

pub(super) struct Workspace {
    buttons: Vec<Widget>,
    popup: Widget,
    area: Widget,
    frames: Vec<Widget>,
    // 活动菜单的命令编号只在菜单开闭期间映射到该组条目。
    menu: Option<&'static [Entry]>,
}

impl Workspace {
    pub(super) fn new(parent: &Widget, popup: Widget, grow: Layout) -> liteos_ui::Result<Self> {
        let body = child(parent, WidgetKind::Row, "", grow)?;
        let navigation = child(
            &body,
            WidgetKind::Column,
            "",
            Layout {
                width: 126,
                gap: 1,
                ..Layout::default()
            },
        )?;
        let buttons = NAVIGATION
            .iter()
            .map(|menu| {
                let caption = match menu {
                    Menu::Item(entry) => entry.label.to_owned(),
                    Menu::Group(label, _) => format!("{label}　▸"),
                };
                child(
                    &navigation,
                    WidgetKind::Button,
                    &caption,
                    Layout {
                        height: 23,
                        ..Layout::default()
                    },
                )
            })
            .collect::<liteos_ui::Result<_>>()?;
        let area = child(&body, WidgetKind::Workspace, "", grow)?;
        let frames = Page::ALL
            .into_iter()
            .map(|page| {
                let frame = child(
                    &area,
                    WidgetKind::ChildFrame,
                    page.title(),
                    Layout::default(),
                )?;
                frame.set_state(state::HIDDEN)?;
                Ok(frame)
            })
            .collect::<liteos_ui::Result<_>>()?;
        Ok(Self {
            buttons,
            popup,
            area,
            frames,
            menu: None,
        })
    }

    pub(super) fn content(&self, page: Page) -> liteos_ui::Result<Widget> {
        child(
            &self.frames[page as usize],
            WidgetKind::Column,
            "",
            Layout {
                grow: 1,
                padding: 6,
                gap: 6,
                ..Layout::default()
            },
        )
    }

    pub(super) fn is_open(&self, page: Page) -> liteos_ui::Result<bool> {
        Ok(self.frames[page as usize].state()? & state::HIDDEN == 0)
    }

    pub(super) fn has_scoped_pages(&self) -> liteos_ui::Result<bool> {
        for page in [
            Page::Interfaces,
            Page::Routes,
            Page::Firewall,
            Page::Neighbors,
            Page::Connections,
            Page::Diagnostics,
        ] {
            if self.is_open(page)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn open(&self, page: Page) -> liteos_ui::Result<bool> {
        let opened = !self.is_open(page)?;
        let frame = &self.frames[page as usize];
        frame.set_state(0)?;
        self.area.select(Some(frame.id()))?;
        Ok(opened)
    }

    pub(super) fn close(&self, page: Page) -> liteos_ui::Result<()> {
        self.frames[page as usize].set_state(state::HIDDEN)?;
        if let Some(front) = self.area.selection()? {
            self.area.select(Some(front))?;
        }
        Ok(())
    }

    pub(super) fn action(&mut self, event: &Event) -> liteos_ui::Result<Option<Action>> {
        if event.node == self.popup.id() {
            let action = match event.kind {
                EventKind::Activate { value } => self.menu.take().and_then(|entries| {
                    let index = usize::try_from(value.checked_sub(1)?).ok()?;
                    entries.get(index).map(|entry| entry.action)
                }),
                EventKind::Dismiss => {
                    self.menu = None;
                    None
                }
                _ => None,
            };
            return Ok(action);
        }
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(None);
        }
        if let Some((button, entry)) = self
            .buttons
            .iter()
            .zip(NAVIGATION)
            .find(|(button, _)| button.id() == event.node)
        {
            return match entry {
                Menu::Item(entry) => Ok(Some(entry.action)),
                Menu::Group(_, entries) => {
                    let items = entries
                        .iter()
                        .enumerate()
                        .map(|(index, entry)| MenuItem {
                            id: index as u64 + 1,
                            label: entry.label,
                            shortcut: "",
                            state: 0,
                            parent: 0,
                        })
                        .collect::<Vec<_>>();
                    let bounds = button.geometry()?;
                    self.popup.set_menus(&[liteos_ui::Menu {
                        title: "",
                        items: &items,
                    }])?;
                    self.popup.open_popup(Some((bounds.right(), bounds.y)))?;
                    self.menu = Some(entries);
                    Ok(None)
                }
            };
        }
        Ok(Page::ALL
            .into_iter()
            .find(|page| self.frames[*page as usize].id() == event.node)
            .map(Action::Close))
    }
}
