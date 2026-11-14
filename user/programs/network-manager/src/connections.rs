//! 转发连接观察。服务按身份分页，窗口只持有当前页和一条前瞻记录。

use crate::child;
use liteos_ui::{
    Application, AsyncFd, Background, Clock, Column, Dialogs, Event, EventKind, Layout,
    ListOptions, ListStyle, MessageButtons, MessageIcon, MessageResult, Sleep, Task, Widget,
    WidgetKind, Window, state,
};
use network_config::{
    ManagementClient,
    configuration::{
        PendingConnectionWatch,
        connections::{Connection, Cursor, Identity, Phase, Protocol, changes},
    },
};
use std::{
    cell::{Cell, RefCell},
    future::{Future, poll_fn},
    io,
    os::fd::AsFd,
    pin::Pin,
    rc::Rc,
    sync::Arc,
    task::{Poll, Waker},
    time::Duration,
};

type Error = Box<dyn std::error::Error>;
const DISPLAY_ROWS: usize = 64;
const WINDOW_ROWS: usize = DISPLAY_ROWS + 1;

pub(super) struct Page {
    view: Rc<View>,
    observer: Option<Task<()>>,
    clear: Option<Task<()>>,
    domain: Option<Arc<str>>,
}

#[derive(Default)]
struct Snapshot {
    cursor: changes::Cursor,
    rows: Vec<Connection>,
}

struct View {
    list: Widget,
    previous: Widget,
    next: Widget,
    refresh: Widget,
    clear: Widget,
    status: Widget,
    count: Widget,
    background: Background,
    dialogs: Dialogs,
    clock: Clock,
    visible: Cell<bool>,
    // 请求序号区分重复刷新与翻页；后台结果不能覆盖后来选择的页。
    request: Cell<u64>,
    // 单一观察任务等待就绪时，用同一个 waker 响应翻页，保留在途服务端等待。
    changed: Cell<Option<Waker>>,
    // 只记录走过的页边界，回退无需从第一条扫描。
    anchors: RefCell<Vec<u64>>,
    snapshot: RefCell<Snapshot>,
    // 读取和补页未完成时，屏幕上仍可能有旧行，暂不允许使用这些行执行操作。
    available: Cell<bool>,
    // 清除确认和请求共用一个操作期，观察更新不能提前启用清除按钮。
    clearing: Cell<bool>,
}

impl Page {
    pub(super) fn new(
        app: &mut Application,
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
                title: "协议",
                width: 90,
            },
            Column {
                id: 2,
                title: "源地址",
                width: 210,
            },
            Column {
                id: 3,
                title: "目标地址",
                width: 210,
            },
            Column {
                id: 4,
                title: "状态",
                width: 105,
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
            clear: child(&bar, WidgetKind::Button, "清除连接…", button)?,
            count: child(
                &bar,
                WidgetKind::Label,
                "",
                Layout {
                    grow: 1,
                    ..Default::default()
                },
            )?,
            status: child(parent, WidgetKind::Status, "", Layout::default())?,
            background,
            dialogs: app.dialogs(),
            clock: app.clock(),
            visible: Cell::new(false),
            request: Cell::new(0),
            changed: Cell::default(),
            anchors: RefCell::new(vec![0]),
            snapshot: RefCell::default(),
            available: Cell::new(false),
            clearing: Cell::new(false),
        };
        view.controls()?;
        Ok(Self {
            view: Rc::new(view),
            observer: None,
            clear: None,
            domain: None,
        })
    }

    pub(super) fn select_domain(
        &mut self,
        app: &mut Application,
        window: &Window,
        domain: Option<Arc<str>>,
    ) -> liteos_ui::Result<()> {
        self.observer = None;
        self.view.changed.take();
        self.domain = domain;
        self.view.available.set(false);
        self.view.anchors.borrow_mut().truncate(1);
        *self.view.snapshot.borrow_mut() = Snapshot::default();
        self.view.present(true)?;
        if self.view.visible.get() {
            self.reload(app, window)?;
        }
        Ok(())
    }

    pub(super) fn selected_tab(
        &mut self,
        app: &mut Application,
        window: &Window,
        visible: bool,
    ) -> liteos_ui::Result<()> {
        if self.view.visible.replace(visible) == visible || !visible {
            return Ok(());
        }
        self.reload(app, window)
    }

    fn reload(&mut self, app: &mut Application, window: &Window) -> liteos_ui::Result<()> {
        let view = &self.view;
        view.request.set(view.request.get().wrapping_add(1));
        view.available.set(false);
        view.controls()?;
        view.status.set_text("正在读取转发连接…")?;
        if let Some(waker) = view.changed.take() {
            waker.wake();
        }
        if self.observer.as_ref().is_none_or(Task::is_finished) {
            let view = view.clone();
            let parent = window.clone();
            let domain = self.domain.clone();
            self.observer = Some(app.spawn_local(window, async move {
                let result = view.observe(&parent, domain.as_ref()).await;
                view.changed.take();
                if let Err(error) = result
                    && view.visible.get()
                {
                    view.available.set(false);
                    let _ = view.controls();
                    let _ = view
                        .status
                        .set_text(&format!("连接观察已停止，请刷新。{error}"));
                }
            })?);
        }
        Ok(())
    }

    pub(super) fn handle(
        &mut self,
        app: &mut Application,
        window: &Window,
        event: &Event,
    ) -> liteos_ui::Result<()> {
        let view = &self.view;
        if !view.visible.get() {
            return Ok(());
        }
        if event.node == view.list.id() && matches!(event.kind, EventKind::Select { .. }) {
            return view.controls();
        }
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(());
        }
        if event.node == view.refresh.id() {
            self.reload(app, window)?;
        } else if view.available.get() && event.node == view.previous.id() {
            if view.anchors.borrow().len() > 1 {
                view.anchors.borrow_mut().pop();
                self.reload(app, window)?;
            }
        } else if view.available.get() && event.node == view.next.id() {
            let next = {
                let snapshot = view.snapshot.borrow();
                (snapshot.rows.len() > DISPLAY_ROWS).then(|| snapshot.rows[DISPLAY_ROWS - 1].id)
            };
            if let Some(after) = next {
                view.anchors.borrow_mut().push(after);
                self.reload(app, window)?;
            }
        } else if event.node == view.clear.id()
            && !view.clearing.get()
            && let Some((identity, row)) = view.selected()?
        {
            view.clearing.set(true);
            view.controls()?;
            let view = view.clone();
            let parent = window.clone();
            let domain = self.domain.clone();
            let request = view.request.get();
            self.clear = Some(app.spawn_local(window, async move {
                let result = view.clear_connection(&parent, domain, identity, row).await;
                let text = match result {
                    Ok(true) => "已清除所选转发连接".into(),
                    Ok(false) => "已取消清除".into(),
                    Err(error) => format!("未确认清除结果，请核对列表。{error}"),
                };
                view.clearing.set(false);
                let _ = view.controls();
                if request == view.request.get() {
                    let _ = view.status.set_text(&text);
                }
            })?);
        }
        Ok(())
    }
}

impl View {
    fn selected(&self) -> liteos_ui::Result<Option<(Identity, Connection)>> {
        if !self.available.get() {
            return Ok(None);
        }
        let Some(id) = self.list.selection()? else {
            return Ok(None);
        };
        let snapshot = self.snapshot.borrow();
        let row = snapshot
            .rows
            .iter()
            .take(DISPLAY_ROWS)
            .find(|row| row.id == id);
        Ok(row.map(|row| {
            (
                Identity {
                    epoch: snapshot.cursor.epoch,
                    id,
                },
                *row,
            )
        }))
    }

    fn controls(&self) -> liteos_ui::Result<()> {
        let available = self.available.get();
        let button_state = |enabled| if enabled { 0 } else { state::DISABLED };
        self.previous
            .set_state(button_state(available && self.anchors.borrow().len() > 1))?;
        self.next.set_state(button_state(
            available && self.snapshot.borrow().rows.len() > DISPLAY_ROWS,
        ))?;
        self.clear.set_state(button_state(
            !self.clearing.get() && self.selected()?.is_some(),
        ))
    }

    fn present(&self, new_page: bool) -> liteos_ui::Result<()> {
        if new_page {
            self.list.select(None)?;
        }
        let snapshot = self.snapshot.borrow();
        let text: Vec<_> = snapshot
            .rows
            .iter()
            .take(DISPLAY_ROWS)
            .map(row_text)
            .collect();
        let rows: Vec<_> = snapshot
            .rows
            .iter()
            .zip(&text)
            .map(|(row, text)| (row.id, text.as_str()))
            .collect();
        self.list.set_rows(&rows)?;
        self.count.set_text(&format!(
            "第 {} 页 · {} 条",
            self.anchors.borrow().len(),
            rows.len()
        ))?;
        drop(snapshot);
        self.controls()
    }

    async fn observe(&self, window: &Window, domain: Option<&Arc<str>>) -> Result<(), Error> {
        let mut waiting = None::<Watch>;
        let mut displayed = None;
        let mut restart = None;
        loop {
            let request = self.request.get();
            if !self.visible.get() {
                let Some(watch) = waiting.as_mut() else {
                    return Ok(());
                };
                if watch.wait(self, request).await? {
                    waiting = None;
                }
                continue;
            }
            if displayed != Some(request) || restart.is_some() {
                let after = *self.anchors.borrow().last().expect("first page anchor");
                let epoch = self.snapshot.borrow().cursor.epoch;
                let baseline = restart.take();
                let scope = domain.cloned();
                let result = self
                    .background
                    .run(move || snapshot(scope.as_deref(), after, epoch, baseline))?
                    .await;
                if !self.visible.get() || self.request.get() != request {
                    continue;
                }
                let (page, first_page) = result?;
                if first_page {
                    self.anchors.borrow_mut().truncate(1);
                    waiting = None;
                }
                *self.snapshot.borrow_mut() = page;
                self.available.set(true);
                self.present(true)?;
                self.status.set_text("正在实时观察转发连接")?;
                displayed = Some(request);
            }

            let cursor = self.snapshot.borrow().cursor;
            let scope = domain.cloned();
            let result = self
                .background
                .run(move || {
                    ManagementClient::local().connection_changes(scope.as_deref(), cursor)
                })?
                .await;
            if !self.visible.get() || self.request.get() != request {
                continue;
            }
            let batch = result?;
            if batch.reset {
                restart = Some(batch.cursor);
                self.available.set(false);
                self.controls()?;
                self.status.set_text("正在重新同步转发连接…")?;
            } else if !batch.changes.is_empty() {
                self.apply(domain, batch, request).await?;
            } else {
                if waiting.is_none() {
                    let scope = domain.cloned();
                    let result = self
                        .background
                        .run(move || PendingConnectionWatch::start(scope.as_deref(), cursor))?
                        .await;
                    let reply = match result {
                        Ok(reply) => reply,
                        Err(_) if !self.visible.get() || self.request.get() != request => continue,
                        Err(error) => return Err(error.into()),
                    };
                    waiting = Some(Watch {
                        fd: window.watch(reply.as_fd().try_clone_to_owned()?)?,
                        deadline: self.clock.sleep(reply.remaining())?,
                        reply,
                    });
                }
                match waiting
                    .as_mut()
                    .expect("armed watch")
                    .wait(self, request)
                    .await
                {
                    Ok(true) => waiting = None,
                    Ok(false) => {}
                    Err(error)
                        if error
                            .downcast_ref::<io::Error>()
                            .is_some_and(|error| error.kind() == io::ErrorKind::ResourceBusy) =>
                    {
                        // 离开的域可能还有一个有界等待。保留当前游标，退避后从同一增量入口继续。
                        waiting = None;
                        self.clock.sleep(Duration::from_secs(1))?.await;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }

    async fn apply(
        &self,
        domain: Option<&Arc<str>>,
        batch: changes::Batch,
        request: u64,
    ) -> Result<(), Error> {
        let after = *self.anchors.borrow().last().expect("first page anchor");
        let epoch = batch.cursor.epoch;
        let (changed, refill) = {
            let mut page = self.snapshot.borrow_mut();
            let rows = &mut page.rows;
            // 满页后的尾部尚未读取。先只修改已覆盖区间，再从尾部补页，避免跳过未显示的连接。
            let last_known = (rows.len() == WINDOW_ROWS).then(|| rows[WINDOW_ROWS - 1].id);
            let mut changed = false;
            for change in batch.changes {
                match change {
                    changes::Change::Removed(id) => {
                        if let Ok(index) = rows.binary_search_by_key(&id, |row| row.id) {
                            rows.remove(index);
                            changed = true;
                        }
                    }
                    changes::Change::Added(row) | changes::Change::Updated(row)
                        if row.id > after && last_known.is_none_or(|last| row.id <= last) =>
                    {
                        match rows.binary_search_by_key(&row.id, |item| item.id) {
                            Ok(index) => {
                                changed |= rows[index].phase != row.phase;
                                rows[index] = row;
                            }
                            Err(index) if index < WINDOW_ROWS => {
                                rows.insert(index, row);
                                rows.truncate(WINDOW_ROWS);
                                changed = true;
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            let missing = WINDOW_ROWS - rows.len();
            let refill = (last_known.is_some() && missing != 0)
                .then(|| (rows.last().map_or(after, |row| row.id), missing));
            page.cursor = batch.cursor;
            (changed, refill)
        };
        if let Some((tail, missing)) = refill {
            self.available.set(false);
            self.controls()?;
            let scope = domain.cloned();
            let result = self
                .background
                .run(move || {
                    read_rows(
                        &mut ManagementClient::local(),
                        scope.as_deref(),
                        epoch,
                        tail,
                        missing,
                    )
                })?
                .await;
            if self.request.get() != request || !self.visible.get() {
                return Ok(());
            }
            self.snapshot.borrow_mut().rows.extend(result?);
            self.available.set(true);
        }
        if changed {
            self.present(false)?;
        }
        Ok(())
    }

    async fn clear_connection(
        &self,
        parent: &Window,
        domain: Option<Arc<str>>,
        identity: Identity,
        row: Connection,
    ) -> Result<bool, Error> {
        let message = format!(
            "确定清除所选域中的这条转发连接？\n路由域：{}\n{} → {}",
            domain.as_deref().unwrap_or("默认路由域"),
            row.source,
            row.destination
        );
        if self
            .dialogs
            .message(
                parent,
                "清除连接",
                &message,
                MessageButtons::YesNo,
                MessageIcon::Warning,
            )?
            .await?
            != MessageResult::Yes
        {
            return Ok(false);
        }
        self.background
            .run(move || ManagementClient::local().clear_connection(domain.as_deref(), identity))?
            .await?;
        Ok(true)
    }
}

fn snapshot(
    domain: Option<&str>,
    after: u64,
    epoch: u64,
    baseline: Option<changes::Cursor>,
) -> io::Result<(Snapshot, bool)> {
    let mut client = ManagementClient::local();
    let cursor = match baseline {
        Some(cursor) => cursor,
        None => {
            client
                .connection_changes(domain, changes::Cursor::default())?
                .cursor
        }
    };
    let first_page = cursor.epoch != epoch;
    let rows = read_rows(
        &mut client,
        domain,
        cursor.epoch,
        if first_page { 0 } else { after },
        WINDOW_ROWS,
    )?;
    Ok((Snapshot { cursor, rows }, first_page))
}

fn read_rows(
    client: &mut ManagementClient,
    domain: Option<&str>,
    epoch: u64,
    after: u64,
    limit: usize,
) -> io::Result<Vec<Connection>> {
    let mut cursor = if after == 0 {
        Cursor::default()
    } else {
        Cursor { epoch, after }
    };
    let mut rows = Vec::with_capacity(limit);
    loop {
        let page = client.connections(domain, cursor)?;
        if page.epoch != epoch {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "转发服务实例已更换",
            ));
        }
        rows.extend(page.rows.into_iter().take(limit - rows.len()));
        match page.next {
            Some(after) if rows.len() < limit => cursor = Cursor { epoch, after },
            _ => return Ok(rows),
        }
    }
}

fn row_text(row: &Connection) -> String {
    let protocol = match row.protocol {
        Protocol::Tcp => "TCP",
        Protocol::Udp => "UDP",
        Protocol::Echo => "ICMP Echo",
    };
    let phase = match row.phase {
        Phase::Opening if row.protocol == Protocol::Tcp => "握手中",
        Phase::Opening => "等待回包",
        Phase::Established => "已建立",
        Phase::Closing => "关闭中",
        Phase::Reset => "已重置",
    };
    if row.protocol == Protocol::Echo {
        format!(
            "{protocol}\t{}（ID {}）\t{}\t{phase}",
            row.source.ip(),
            row.source.port(),
            row.destination.ip()
        )
    } else {
        format!("{protocol}\t{}\t{}\t{phase}", row.source, row.destination)
    }
}

struct Watch {
    reply: PendingConnectionWatch,
    fd: AsyncFd,
    deadline: Sleep,
}

impl Watch {
    // 翻页时唤醒观察任务，留住原等待。它仍能通知新游标之后的变化，且不会重复占用服务名额。
    async fn wait(&mut self, view: &View, request: u64) -> Result<bool, Error> {
        loop {
            if view.visible.get() && view.request.get() != request {
                return Ok(false);
            }
            if self.reply.try_complete()? {
                return Ok(true);
            }
            let mut readiness = self.fd.readable();
            let ready = poll_fn(|cx| {
                if view.visible.get() && view.request.get() != request {
                    return Poll::Ready(Ok(false));
                }
                if Pin::new(&mut self.deadline).poll(cx).is_ready() {
                    return Poll::Ready(Err(io::Error::from(io::ErrorKind::TimedOut).into()));
                }
                if let Poll::Ready(result) = Pin::new(&mut readiness).poll(cx) {
                    return Poll::Ready(result.map(|_| true).map_err(Error::from));
                }
                view.changed.set(Some(cx.waker().clone()));
                Poll::Pending
            })
            .await;
            view.changed.take();
            if !ready? {
                return Ok(false);
            }
        }
    }
}
