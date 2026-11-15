//! IP → ARP 与 IPv6 → 邻居使用同一套单族观察窗口。

use crate::{
    child,
    item_list::{Glyph, Item, Summary, Tool, spacer, toolbar},
    item_properties::{button, field},
    observation::receive,
    workspace::Workspace,
};
use liteos_ui::{
    Application, Background, Clock, Column, Event, EventKind, Layout, ListOptions, ListStyle, Task,
    Widget, WidgetKind, Window,
};
use network_config::{
    format_mac,
    neighbors::{
        Cursor, Entry, Family, Key, NeighborObservation, NeighborState, Page as NeighborPage,
        observation::Update,
    },
};
use std::{cell::RefCell, io, os::fd::AsFd, rc::Rc, sync::Arc};

pub(super) struct Page {
    view: Rc<View>,
    task: Option<Task<()>>,
    visible: bool,
    domain: Option<Arc<str>>,
    // 有序查询以最后一条记录为游标，保存访问过的起点即可向前翻页。
    anchors: Vec<Cursor>,
}

struct View {
    list: Widget,
    previous: Tool,
    next: Tool,
    refresh: Tool,
    summary: Summary,
    background: Background,
    clock: Clock,
    family: Family,
    snapshot: RefCell<Option<NeighborPage>>,
    // 只跟踪当前页的一条属性窗口。翻页、切域或记录消失时释放窗口。
    property: RefCell<Option<Property>>,
}

impl Page {
    pub(super) fn new(
        app: &Application,
        parent: &Widget,
        background: Background,
        family: Family,
    ) -> liteos_ui::Result<Self> {
        let [previous, next, refresh] = toolbar(
            parent,
            [
                Item::Icon(Glyph::Previous, "上一页"),
                Item::Icon(Glyph::Next, "下一页"),
                Item::Icon(Glyph::Refresh, "刷新"),
            ],
        )?;
        let list = child(
            parent,
            WidgetKind::List,
            "",
            Layout {
                grow: 1,
                ..Default::default()
            },
        )?;
        list.set_list_style(
            ListStyle::Details,
            ListOptions {
                multiple: false,
                full_row_select: true,
            },
        )?;
        let [address, second, third, fourth, status] = fields(family).map(|field| Column {
            id: field as u64 + 2,
            title: field.label(family),
            width: field.width(),
        });
        let flags = Column {
            id: 1,
            title: "",
            width: 58,
        };
        list.set_columns(&[flags, address, second, third, fourth, status])?;
        let view = View {
            list,
            previous,
            next,
            refresh,
            summary: Summary::new(parent)?,
            background,
            clock: app.clock(),
            family,
            snapshot: RefCell::default(),
            property: RefCell::default(),
        };
        view.previous.set_enabled(false)?;
        view.next.set_enabled(false)?;
        Ok(Self {
            view: Rc::new(view),
            task: None,
            visible: false,
            domain: None,
            anchors: vec![Cursor::first(family)],
        })
    }

    pub(super) fn select_domain(
        &mut self,
        app: &mut Application,
        window: &Window,
        domain: Option<Arc<str>>,
    ) -> liteos_ui::Result<()> {
        self.domain = domain;
        self.anchors.truncate(1);
        if self.visible {
            self.subscribe(app, window)?;
        }
        Ok(())
    }

    pub(super) fn selected_tab(
        &mut self,
        app: &mut Application,
        window: &Window,
        visible: bool,
    ) -> liteos_ui::Result<()> {
        if self.visible == visible {
            return Ok(());
        }
        self.visible = visible;
        if visible {
            self.subscribe(app, window)?;
        } else {
            self.task = None;
            self.view.property.borrow_mut().take();
        }
        Ok(())
    }

    pub(super) fn handle(
        &mut self,
        app: &mut Application,
        window: &Window,
        workspace: &Workspace,
        event: &Event,
    ) -> liteos_ui::Result<()> {
        if !self.visible {
            return Ok(());
        }
        if event.node == self.view.list.id() {
            match event.kind {
                EventKind::Select { .. } => return self.view.show_summary(self.anchors.len()),
                EventKind::Activate { .. } => {
                    if let Some(row) = self.view.selected()? {
                        self.view
                            .open_property(workspace, row, self.domain.as_deref())?;
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
        if matches!(event.kind, EventKind::Activate { .. }) {
            let mut property = self.view.property.borrow_mut();
            if property
                .as_ref()
                .is_some_and(|view| view.closes(event.node))
            {
                property.take();
                return Ok(());
            }
        }
        if self.view.previous.activated(event) && self.anchors.len() > 1 {
            self.anchors.pop();
        } else if self.view.next.activated(event) {
            let snapshot = self.view.snapshot.borrow();
            let next = snapshot
                .as_ref()
                .filter(|page| page.more)
                .and_then(|page| page.rows.last());
            let Some(row) = next else { return Ok(()) };
            let cursor = Cursor::after(self.view.family, row.neighbor.key())
                .expect("observed page already validated against its address family");
            self.anchors.push(cursor);
        } else if !self.view.refresh.activated(event) {
            return Ok(());
        }
        self.subscribe(app, window)
    }

    fn subscribe(&mut self, app: &mut Application, window: &Window) -> liteos_ui::Result<()> {
        self.task = None;
        let view = self.view.clone();
        view.snapshot.borrow_mut().take();
        view.property.borrow_mut().take();
        view.list.set_rows(&[])?;
        for tool in [&view.previous, &view.next, &view.refresh] {
            tool.set_enabled(false)?;
        }
        view.summary.notice("正在读取邻居…")?;
        let parent = window.clone();
        let page = self.anchors.len();
        let cursor = *self.anchors.last().expect("first page anchor");
        let domain = self.domain.clone();
        self.task = Some(app.spawn_local(window, async move {
            if let Err(error) = view.observe(&parent, page, domain, cursor).await {
                view.property.borrow_mut().take();
                view.snapshot.borrow_mut().take();
                let _ = view.list.set_rows(&[]);
                let _ = view.previous.set_enabled(page > 1);
                let _ = view.refresh.set_enabled(true);
                let _ = view
                    .summary
                    .notice(&format!("邻居观察已停止，请刷新。{error}"));
            }
        })?);
        Ok(())
    }
}

impl View {
    async fn observe(
        &self,
        window: &Window,
        page: usize,
        domain: Option<Arc<str>>,
        cursor: Cursor,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let scope = domain.clone();
        let mut stream = self
            .background
            .run(move || NeighborObservation::start(scope.as_deref(), cursor))?
            .await?;
        let mut fd = window.watch(stream.as_fd().try_clone_to_owned()?)?;
        loop {
            let next = match receive(&self.clock, &mut fd, stream.remaining(), || {
                stream.try_next()
            })
            .await?
            {
                Update::Heartbeat => continue,
                Update::Snapshot(snapshot) => snapshot,
                Update::Changes(changes) => {
                    let previous = self.snapshot.borrow();
                    let previous = previous.as_ref().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "未收到邻居快照")
                    })?;
                    changes
                        .apply(previous, cursor)
                        .map_err(|_| io::ErrorKind::InvalidData)
                        .map_err(io::Error::from)?
                }
            };
            self.present(page, domain.as_deref(), next)?;
        }
    }

    fn selected(&self) -> liteos_ui::Result<Option<Entry>> {
        Ok(self.list.selection()?.and_then(|id| {
            self.snapshot
                .borrow()
                .as_ref()?
                .rows
                .get(id.checked_sub(1)? as usize)
                .copied()
        }))
    }

    fn show_summary(&self, page: usize) -> liteos_ui::Result<()> {
        let snapshot = self.snapshot.borrow();
        let Some(snapshot) = snapshot.as_ref() else {
            return Ok(());
        };
        self.summary.show(
            snapshot.rows.len(),
            usize::from(self.list.selection()?.is_some()),
            &format!("实时（第 {page} 页）"),
        )
    }

    fn open_property(
        &self,
        workspace: &Workspace,
        row: Entry,
        domain: Option<&str>,
    ) -> liteos_ui::Result<()> {
        let mut current = self.property.borrow_mut();
        if let Some(open) = current
            .as_ref()
            .filter(|open| open.key == row.neighbor.key())
        {
            return workspace.raise(&open.frame);
        }
        *current = Some(Property::new(workspace, row, self.family, domain)?);
        Ok(())
    }

    fn present(
        &self,
        page: usize,
        domain: Option<&str>,
        next: NeighborPage,
    ) -> liteos_ui::Result<()> {
        let selected = self.selected()?.and_then(|entry| {
            next.rows
                .iter()
                .position(|row| row.neighbor.key() == entry.neighbor.key())
        });
        let text: Vec<_> = next
            .rows
            .iter()
            .map(|row| row_text(row, self.family, domain))
            .collect();
        let rows: Vec<_> = text
            .iter()
            .enumerate()
            .map(|(index, text)| (index as u64 + 1, text.as_str()))
            .collect();
        self.list.set_rows(&rows)?;
        self.list.select(selected.map(|index| index as u64 + 1))?;
        self.previous.set_enabled(page > 1)?;
        self.next.set_enabled(next.more)?;
        self.refresh.set_enabled(true)?;
        let mut property = self.property.borrow_mut();
        if let Some(view) = property.as_ref() {
            if let Some(row) = next.rows.iter().find(|row| row.neighbor.key() == view.key) {
                view.present(row, domain)?;
            } else {
                property.take();
            }
        }
        *self.snapshot.borrow_mut() = Some(next);
        self.show_summary(page)
    }
}

#[derive(Clone, Copy)]
enum Field {
    Address,
    Mac,
    Interface,
    Vrf,
    Status,
}

fn fields(family: Family) -> [Field; 5] {
    use Field::*;
    match family {
        Family::Ipv4 => [Address, Mac, Interface, Vrf, Status],
        Family::Ipv6 => [Address, Interface, Vrf, Mac, Status],
    }
}

impl Field {
    fn label(self, family: Family) -> &'static str {
        match self {
            Self::Address if family == Family::Ipv4 => "IP 地址",
            Self::Address => "地址",
            Self::Mac => "MAC 地址",
            Self::Interface => "接口",
            Self::Vrf => "VRF",
            Self::Status => "状态",
        }
    }

    fn width(self) -> i32 {
        match self {
            Self::Address => 230,
            Self::Mac => 144,
            Self::Interface | Self::Vrf | Self::Status => 104,
        }
    }

    fn text<'a>(self, row: &'a Entry, domain: Option<&'a str>) -> std::borrow::Cow<'a, str> {
        match self {
            Self::Address => row.neighbor.address.to_string().into(),
            Self::Mac => row
                .neighbor
                .hardware_address
                .map_or_else(|| "".into(), |mac| format_mac(mac).into()),
            Self::Interface => {
                let name = row
                    .interface_name
                    .split(|&byte| byte == 0)
                    .next()
                    .unwrap_or_default();
                String::from_utf8_lossy(name)
            }
            Self::Vrf => domain.unwrap_or("main").into(),
            Self::Status => match row.neighbor.state {
                NeighborState::Incomplete => "解析中",
                NeighborState::Reachable => "可达",
                NeighborState::Stale => "陈旧",
                NeighborState::Delay => "等待探测",
                NeighborState::Probe => "探测中",
                NeighborState::Failed => "不可达",
            }
            .into(),
        }
    }
}

fn row_text(row: &Entry, family: Family, domain: Option<&str>) -> String {
    let mut text = String::from(if row.neighbor.hardware_address.is_some() {
        "DC"
    } else {
        "D"
    });
    for field in fields(family) {
        text.push('\t');
        text.push_str(&field.text(row, domain));
    }
    text
}

struct Property {
    frame: Widget,
    key: Key,
    values: Vec<(Field, Widget)>,
    complete: Widget,
    cancel: Widget,
    // 容器和标签由窗口持有，释放时一起移除。
    _layout: Vec<Widget>,
}

impl Property {
    fn new(
        workspace: &Workspace,
        row: Entry,
        family: Family,
        domain: Option<&str>,
    ) -> liteos_ui::Result<Self> {
        let name = if family == Family::Ipv4 {
            "ARP"
        } else {
            "邻居"
        };
        let frame =
            workspace.item_window(&format!("{name} > {}", row.neighbor.address), (397, 245))?;
        let root = child(
            &frame,
            WidgetKind::Column,
            "",
            Layout {
                grow: 1,
                padding: 8,
                gap: 5,
                ..Default::default()
            },
        )?;
        let body = child(
            &root,
            WidgetKind::Column,
            "",
            Layout {
                grow: 1,
                gap: 5,
                ..Default::default()
            },
        )?;
        let mut layout = Vec::new();
        let values = fields(family)
            .into_iter()
            .map(|key| {
                let input = field(
                    &body,
                    &mut layout,
                    key.label(family),
                    WidgetKind::Edit,
                    "",
                    true,
                )?;
                Ok((key, input))
            })
            .collect::<liteos_ui::Result<Vec<_>>>()?;
        let footer = child(
            &root,
            WidgetKind::Row,
            "",
            Layout {
                gap: 5,
                ..Default::default()
            },
        )?;
        let badge = Layout {
            padding: 2,
            ..Default::default()
        };
        let dynamic = child(&footer, WidgetKind::Label, "动态", badge)?;
        let complete = child(&footer, WidgetKind::Label, "完整", badge)?;
        let space = spacer(&footer)?;
        let cancel = child(&footer, WidgetKind::Button, "取消", button())?;
        layout.extend([root, body, footer, dynamic, space]);
        let view = Self {
            frame,
            key: row.neighbor.key(),
            values,
            complete,
            cancel,
            _layout: layout,
        };
        view.present(&row, domain)?;
        Ok(view)
    }

    fn present(&self, entry: &Entry, domain: Option<&str>) -> liteos_ui::Result<()> {
        for (field, control) in &self.values {
            control.set_text(&field.text(entry, domain))?;
        }
        self.complete
            .set_state(if entry.neighbor.hardware_address.is_some() {
                0
            } else {
                liteos_ui::state::HIDDEN
            })
    }

    fn closes(&self, node: u64) -> bool {
        node == self.frame.id() || node == self.cancel.id()
    }
}
