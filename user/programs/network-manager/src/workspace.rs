//! WinBox 式多窗口工作区。每项运行视图只有一个子窗口、控件树及业务 owner，关闭只是隐藏。
use crate::{child, navigation};
use liteos_ui::{Event, EventKind, Layout, Widget, WidgetKind, state};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Page {
    Interfaces,
    Bridges,
    Routes,
    Ipv4Routes,
    Ipv6Routes,
    Ipv4Firewall,
    Arp,
    Ipv6Neighbors,
    Ipv6Firewall,
    Configuration,
    Backups,
    Addresses,
    Ping,
    Trace,
    Bandwidth,
}

impl Page {
    /// 子窗口首次打开时的大小（逻辑像素），按各自表格的列宽给出，避免打开即出现横向滚动。
    fn size(self) -> (i32, i32) {
        match self {
            Self::Interfaces => (900, 400),
            Self::Routes | Self::Ipv4Routes | Self::Ipv6Routes => (820, 420),
            Self::Ipv4Firewall | Self::Ipv6Firewall => (940, 430),
            Self::Arp | Self::Ipv6Neighbors => (790, 420),
            Self::Configuration | Self::Backups => (720, 460),
            Self::Bridges => (900, 420),
            Self::Addresses => (720, 360),
            Self::Ping | Self::Trace | Self::Bandwidth => (640, 420),
        }
    }

    const ALL: [Self; 15] = [
        Self::Interfaces,
        Self::Bridges,
        Self::Routes,
        Self::Ipv4Routes,
        Self::Ipv6Routes,
        Self::Ipv4Firewall,
        Self::Arp,
        Self::Ipv6Neighbors,
        Self::Ipv6Firewall,
        Self::Configuration,
        Self::Backups,
        Self::Addresses,
        Self::Ping,
        Self::Trace,
        Self::Bandwidth,
    ];
}

pub(super) struct Workspace {
    area: Widget,
    frames: Vec<Widget>,
}

impl Workspace {
    pub(super) fn new(parent: &Widget, grow: Layout) -> liteos_ui::Result<Self> {
        let area = child(parent, WidgetKind::Workspace, "", grow)?;
        let frames = Page::ALL
            .into_iter()
            .map(|page| {
                let (width, height) = page.size();
                let frame = child(
                    &area,
                    WidgetKind::ChildFrame,
                    navigation::title(page),
                    Layout {
                        width,
                        height,
                        ..Layout::default()
                    },
                )?;
                frame.set_state(state::HIDDEN)?;
                Ok(frame)
            })
            .collect::<liteos_ui::Result<_>>()?;
        Ok(Self { area, frames })
    }

    pub(super) fn content(&self, page: Page) -> liteos_ui::Result<Widget> {
        child(
            &self.frames[page as usize],
            WidgetKind::Column,
            "",
            Layout {
                grow: 1,
                padding: 4,
                gap: 4,
                ..Layout::default()
            },
        )
    }

    pub(super) fn is_open(&self, page: Page) -> liteos_ui::Result<bool> {
        Ok(self.frames[page as usize].state()? & state::HIDDEN == 0)
    }

    /// 显示并置前子窗口；返回此前是否处于关闭状态。
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

    /// 新建一个置前的条目子窗口（属性窗口）。窗口的生命周期由返回的控件持有，释放即从工作区移除；
    /// 关闭按钮和 Ctrl+F4 对该控件发出激活事件，由持有者处理。
    ///
    /// 错误：窗口已关闭。
    pub(super) fn item_window(&self, title: &str, size: (i32, i32)) -> liteos_ui::Result<Widget> {
        let frame = child(
            &self.area,
            WidgetKind::ChildFrame,
            title,
            Layout {
                width: size.0,
                height: size.1,
                ..Layout::default()
            },
        )?;
        self.area.select(Some(frame.id()))?;
        Ok(frame)
    }

    /// 把已打开的子窗口置前。
    ///
    /// 错误：窗口已关闭或子窗口不在此工作区。
    pub(super) fn raise(&self, frame: &Widget) -> liteos_ui::Result<()> {
        self.area.select(Some(frame.id()))
    }

    /// 子窗口关闭按钮或 Ctrl+F4 对应的页面。
    pub(super) fn closed(&self, event: &Event) -> Option<Page> {
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return None;
        }
        Page::ALL
            .into_iter()
            .find(|page| self.frames[*page as usize].id() == event.node)
    }
}
