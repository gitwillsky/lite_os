//! 管理网关当前邻居页的显示和翻页。

use crate::{child, observation::receive};
use liteos_ui::{
    Application, Background, Clock, Column, Event, EventKind, Layout, ListOptions, ListStyle, Task,
    Widget, WidgetKind, Window, state,
};
use network_config::{
    format_mac,
    neighbors::{
        Entry, Key, NeighborObservation, NeighborState, Page as NeighborPage, observation::Update,
    },
};
use std::{cell::RefCell, io, os::fd::AsFd, rc::Rc, sync::Arc};

pub(super) struct Page {
    view: Rc<View>,
    task: Option<Task<()>>,
    visible: bool,
    domain: Option<Arc<str>>,
    // 有序查询以最后一条记录为游标，保存访问过的起点即可向前翻页。
    anchors: Vec<Option<Key>>,
}

struct View {
    list: Widget,
    previous: Widget,
    next: Widget,
    refresh: Widget,
    status: Widget,
    background: Background,
    clock: Clock,
    snapshot: RefCell<Option<NeighborPage>>,
}

impl Page {
    pub(super) fn new(
        app: &Application,
        parent: &Widget,
        background: Background,
        button: Layout,
    ) -> liteos_ui::Result<Self> {
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
        list.set_columns(&[
            Column {
                id: 1,
                title: "接口",
                width: 94,
            },
            Column {
                id: 2,
                title: "IP 地址",
                width: 243,
            },
            Column {
                id: 3,
                title: "MAC 地址",
                width: 137,
            },
            Column {
                id: 4,
                title: "状态",
                width: 100,
            },
            Column {
                id: 5,
                title: "类型",
                width: 58,
            },
        ])?;
        let bar = child(
            parent,
            WidgetKind::Row,
            "",
            Layout {
                gap: 6,
                ..Default::default()
            },
        )?;
        let view = View {
            list,
            previous: child(&bar, WidgetKind::Button, "上一页", button)?,
            next: child(&bar, WidgetKind::Button, "下一页", button)?,
            refresh: child(&bar, WidgetKind::Button, "刷新", button)?,
            status: child(parent, WidgetKind::Status, "", Layout::default())?,
            background,
            clock: app.clock(),
            snapshot: RefCell::default(),
        };
        view.previous.set_state(state::DISABLED)?;
        view.next.set_state(state::DISABLED)?;
        Ok(Self {
            view: Rc::new(view),
            task: None,
            visible: false,
            domain: None,
            anchors: vec![None],
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
        }
        Ok(())
    }

    pub(super) fn handle(
        &mut self,
        app: &mut Application,
        window: &Window,
        event: &Event,
    ) -> liteos_ui::Result<()> {
        if !self.visible || !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(());
        }
        if event.node == self.view.previous.id() && self.anchors.len() > 1 {
            self.anchors.pop();
        } else if event.node == self.view.next.id() {
            let snapshot = self.view.snapshot.borrow();
            let next = snapshot
                .as_ref()
                .filter(|page| page.more)
                .and_then(|page| page.rows.last());
            let Some(row) = next else { return Ok(()) };
            self.anchors.push(Some(row.neighbor.key()));
        } else if event.node != self.view.refresh.id() {
            return Ok(());
        }
        self.subscribe(app, window)
    }

    fn subscribe(&mut self, app: &mut Application, window: &Window) -> liteos_ui::Result<()> {
        self.task = None;
        let view = self.view.clone();
        view.snapshot.borrow_mut().take();
        view.list.set_rows(&[])?;
        for button in [&view.previous, &view.next, &view.refresh] {
            button.set_state(state::DISABLED)?;
        }
        view.status.set_text("正在读取邻居…")?;
        let parent = window.clone();
        let page = self.anchors.len();
        let after = *self.anchors.last().expect("first page anchor");
        let domain = self.domain.clone();
        self.task = Some(app.spawn_local(window, async move {
            if let Err(error) = view.observe(&parent, page, domain, after).await {
                let _ = view
                    .previous
                    .set_state(if page > 1 { 0 } else { state::DISABLED });
                let _ = view.refresh.set_state(0);
                let _ = view
                    .status
                    .set_text(&format!("邻居观察已停止，请刷新。{error}"));
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
        after: Option<Key>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut stream = self
            .background
            .run(move || NeighborObservation::start(domain.as_deref(), after))?
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
                        .apply(previous, after)
                        .map_err(|_| io::ErrorKind::InvalidData)
                        .map_err(io::Error::from)?
                }
            };
            self.present(page, next)?;
        }
    }

    fn present(&self, page: usize, next: NeighborPage) -> liteos_ui::Result<()> {
        let selected = self.list.selection()?.and_then(|id| {
            self.snapshot
                .borrow()
                .as_ref()?
                .rows
                .get((id - 1) as usize)
                .map(|row| row.neighbor.key())
        });
        let selected =
            selected.and_then(|key| next.rows.iter().position(|row| row.neighbor.key() == key));
        let text: Vec<_> = next.rows.iter().map(row_text).collect();
        let rows: Vec<_> = text
            .iter()
            .enumerate()
            .map(|(index, text)| (index as u64 + 1, text.as_str()))
            .collect();
        self.list.set_rows(&rows)?;
        self.list.select(selected.map(|index| index as u64 + 1))?;
        self.previous
            .set_state(if page > 1 { 0 } else { state::DISABLED })?;
        self.next
            .set_state(if next.more { 0 } else { state::DISABLED })?;
        self.refresh.set_state(0)?;
        self.status
            .set_text(&format!("第 {page} 页 · {} 条邻居 · 正在观察", rows.len()))?;
        *self.snapshot.borrow_mut() = Some(next);
        Ok(())
    }
}

fn row_text(row: &Entry) -> String {
    let name = row
        .interface_name
        .split(|&byte| byte == 0)
        .next()
        .unwrap_or_default();
    let name = String::from_utf8_lossy(name);
    let neighbor = row.neighbor;
    let address = neighbor.address;
    let mac = neighbor
        .hardware_address
        .map_or_else(|| "—".into(), format_mac);
    let state = match neighbor.state {
        NeighborState::Incomplete => "解析中",
        NeighborState::Reachable => "可达",
        NeighborState::Stale => "待确认",
        NeighborState::Delay => "等待探测",
        NeighborState::Probe => "探测中",
        NeighborState::Failed => "不可达",
    };
    let kind = if neighbor.is_router { "路由器" } else { "" };
    format!("{name}\t{address}\t{mac}\t{state}\t{kind}")
}
