//! 防火墙规则和计数的只读观察。选中页的规则身份由收到的快照确定。

use crate::{child, observation::receive};
use liteos_ui::{
    Application, Background, Clock, Column, Event, EventKind, Layout, ListOptions, ListStyle, Task,
    Widget, WidgetKind, Window, state,
};
use network_config::firewall::{
    self, FirewallObservation, RuleConditions,
    counters::{Cursor, LIMIT, Page as CounterPage, observation::Update},
};
use std::{
    cell::{Cell, RefCell},
    io,
    os::fd::AsFd,
    rc::Rc,
    sync::Arc,
};

type Error = Box<dyn std::error::Error>;

pub(super) struct Page {
    view: Rc<View>,
    task: Option<Task<()>>,
    visible: bool,
    domain: Option<Arc<str>>,
}

struct View {
    list: Widget,
    details: Widget,
    previous: Widget,
    next: Widget,
    refresh: Widget,
    status: Widget,
    background: Background,
    clock: Clock,
    // 游标在未收到快照或会话断开时仍保留，使刷新能重新读取当前页。
    cursor: Cell<Cursor>,
    snapshot: RefCell<Option<CounterPage>>,
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
                title: "规则链",
                width: 140,
            },
            Column {
                id: 2,
                title: "序号",
                width: 58,
            },
            Column {
                id: 3,
                title: "动作",
                width: 190,
            },
            Column {
                id: 4,
                title: "报文数",
                width: 107,
            },
            Column {
                id: 5,
                title: "IP 字节数",
                width: 135,
            },
        ])?;
        let details = child(parent, WidgetKind::Label, "", Layout::default())?;
        details.set_state(state::WRAP)?;
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
            details,
            previous: child(&bar, WidgetKind::Button, "上一页", button)?,
            next: child(&bar, WidgetKind::Button, "下一页", button)?,
            refresh: child(&bar, WidgetKind::Button, "刷新", button)?,
            status: child(parent, WidgetKind::Status, "", Layout::default())?,
            background,
            clock: app.clock(),
            cursor: Cell::default(),
            snapshot: RefCell::default(),
        };
        view.buttons()?;
        Ok(Self {
            view: Rc::new(view),
            task: None,
            visible: false,
            domain: None,
        })
    }

    pub(super) fn select_domain(
        &mut self,
        app: &mut Application,
        window: &Window,
        domain: Option<Arc<str>>,
    ) -> liteos_ui::Result<()> {
        self.domain = domain;
        self.view.cursor.set(Cursor::default());
        if self.visible {
            self.subscribe(app, window, Cursor::default())?;
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
            self.subscribe(app, window, self.view.cursor.get())?;
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
        if !self.visible {
            return Ok(());
        }
        let view = &self.view;
        if event.node == view.list.id() && matches!(event.kind, EventKind::Select { .. }) {
            return view.show_details();
        }
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(());
        }
        let cursor = if event.node == view.refresh.id() {
            view.cursor.get()
        } else {
            let snapshot = view.snapshot.borrow();
            let Some(page) = snapshot.as_ref() else {
                return Ok(());
            };
            let offset = if event.node == view.previous.id() && view.cursor.get().offset != 0 {
                view.cursor.get().offset.saturating_sub(LIMIT as u32)
            } else if event.node == view.next.id()
                && let Some(next) = page.next
            {
                next
            } else {
                return Ok(());
            };
            Cursor {
                revision: if offset == 0 { 0 } else { page.revision },
                offset,
            }
        };
        self.subscribe(app, window, cursor)
    }

    fn subscribe(
        &mut self,
        app: &mut Application,
        window: &Window,
        cursor: Cursor,
    ) -> liteos_ui::Result<()> {
        self.task = None;
        let view = self.view.clone();
        view.cursor.set(cursor);
        view.snapshot.borrow_mut().take();
        view.list.set_rows(&[])?;
        view.show_details()?;
        view.buttons()?;
        view.refresh.set_state(state::DISABLED)?;
        view.status.set_text("正在读取防火墙计数…")?;
        let parent = window.clone();
        let domain = self.domain.clone();
        self.task = Some(app.spawn_local(window, async move {
            if let Err(error) = view.observe(&parent, domain, cursor).await {
                let _ = view.refresh.set_state(0);
                let _ = view
                    .status
                    .set_text(&format!("防火墙计数已停止更新，请刷新。{error}"));
            }
        })?);
        Ok(())
    }
}

impl View {
    fn buttons(&self) -> liteos_ui::Result<()> {
        let snapshot = self.snapshot.borrow();
        let enabled = |value| if value { 0 } else { state::DISABLED };
        self.previous
            .set_state(enabled(snapshot.is_some() && self.cursor.get().offset != 0))?;
        self.next.set_state(enabled(
            snapshot.as_ref().is_some_and(|page| page.next.is_some()),
        ))
    }

    async fn observe(
        &self,
        window: &Window,
        domain: Option<Arc<str>>,
        cursor: Cursor,
    ) -> Result<(), Error> {
        let mut stream = self
            .background
            .run(move || FirewallObservation::start(domain.as_deref(), cursor))?
            .await?;
        let mut fd = window.watch(stream.as_fd().try_clone_to_owned()?)?;
        loop {
            match receive(&self.clock, &mut fd, stream.remaining(), || {
                stream.try_next()
            })
            .await?
            {
                Update::Heartbeat => {}
                Update::Snapshot { cursor, page } => {
                    self.cursor.set(cursor);
                    *self.snapshot.borrow_mut() = Some(page);
                    self.list.select(None)?;
                    self.show_details()?;
                    self.present()?;
                    self.buttons()?;
                    self.refresh.set_state(0)?;
                    let page = self.snapshot.borrow();
                    let count = page.as_ref().map_or(0, |page| page.rows.len());
                    if count == 0 {
                        self.status.set_text("没有预路由规则 · 正在观察")?;
                    } else {
                        self.status.set_text(&format!(
                            "第 {} 页 · {count} 条规则 · 正在观察",
                            cursor.offset / LIMIT as u32 + 1
                        ))?;
                    }
                }
                Update::Counts { revision, values } => {
                    {
                        let mut snapshot = self.snapshot.borrow_mut();
                        let page = snapshot.as_mut().ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidData, "未收到防火墙规则快照")
                        })?;
                        if page.revision != revision
                            || values.iter().any(|value| value.index >= page.rows.len())
                        {
                            return Err(io::Error::from(io::ErrorKind::InvalidData).into());
                        }
                        for value in values {
                            let row = &mut page.rows[value.index];
                            row.packets = value.packets;
                            row.bytes = value.bytes;
                        }
                    }
                    self.present()?;
                }
            }
        }
    }

    fn present(&self) -> liteos_ui::Result<()> {
        let snapshot = self.snapshot.borrow();
        let Some(page) = snapshot.as_ref() else {
            return Ok(());
        };
        let rows: Vec<_> = page
            .rows
            .iter()
            .map(|row| {
                let chain = row.chain.as_deref().unwrap_or("入口");
                let action = firewall::action(row);
                format!(
                    "{chain}\t{}\t{action}\t{}\t{}",
                    row.position, row.packets, row.bytes
                )
            })
            .collect();
        let rows: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| (index as u64 + 1, row.as_str()))
            .collect();
        self.list.set_rows(&rows)
    }

    fn show_details(&self) -> liteos_ui::Result<()> {
        let selected = self.list.selection()?;
        let snapshot = self.snapshot.borrow();
        let row = selected.and_then(|id| snapshot.as_ref()?.rows.get((id - 1) as usize));
        let Some(row) = row else {
            return self.details.set_text("");
        };
        let RuleConditions {
            ingress,
            source,
            destination,
            service,
            state,
        } = firewall::conditions(row);
        let action = firewall::action(row);
        self.details.set_text(&format!(
            "入接口 {ingress}　源地址 {source}　目的地址 {destination}　目标服务 {service}　连接状态 {state}\n{action}"
        ))
    }
}
