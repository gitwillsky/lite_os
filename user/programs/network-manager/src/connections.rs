//! 转发连接观察。服务按身份分页，窗口只持有当前页和一条前瞻记录。

mod property;

use crate::{
    child,
    item_list::{Glyph, Item, Summary, Tool, toolbar},
    workspace::Workspace,
};
use liteos_ui::{
    Application, AsyncFd, Background, Clock, Column, Dialogs, Event, EventKind, Layout,
    ListOptions, ListStyle, MessageButtons, MessageIcon, MessageResult, Sleep, Task, Widget,
    WidgetKind, Window, state,
};
use network_config::{
    ManagementClient,
    configuration::{
        PendingConnectionWatch,
        connections::{Connection, Cursor, Family, Identity, changes},
    },
};
use property::{Field, Property};
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
    family: Family,
    domain: Option<Arc<str>>,
}

#[derive(Default)]
struct Snapshot {
    cursor: changes::Cursor,
    rows: Vec<Connection>,
}

struct View {
    list: Widget,
    previous: Tool,
    next: Tool,
    refresh: Tool,
    paging: Widget,
    clear: Tool,
    summary: Summary,
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
    property: RefCell<Option<Property>>,
    // 读取和补页未完成时，屏幕上仍可能有旧行，暂不允许使用这些行执行操作。
    available: Cell<bool>,
    // 清除确认和请求共用一个操作期，观察更新不能提前启用清除按钮。
    clearing: Cell<bool>,
}

impl Page {
    pub(super) fn new(
        app: &Application,
        parent: &Widget,
        background: Background,
        family: Family,
    ) -> liteos_ui::Result<Self> {
        let [clear] = toolbar(parent, [Item::Command(Glyph::Remove, "删除")])?;
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
        let columns: Vec<_> = Field::COLUMNS
            .into_iter()
            .enumerate()
            .map(|(index, (field, width))| Column {
                id: index as u64 + 1,
                title: field.label(),
                width,
            })
            .collect();
        list.set_columns(&columns)?;
        let paging = child(parent, WidgetKind::Column, "", Layout::default())?;
        paging.set_state(state::HIDDEN)?;
        let [previous, next, refresh] = toolbar(
            &paging,
            [
                Item::Icon(Glyph::Previous, "上一页"),
                Item::Icon(Glyph::Next, "下一页"),
                Item::Icon(Glyph::Refresh, "刷新"),
            ],
        )?;
        let view = View {
            list,
            previous,
            next,
            refresh,
            paging,
            clear,
            summary: Summary::new(parent)?,
            background,
            dialogs: app.dialogs(),
            clock: app.clock(),
            visible: Cell::new(false),
            request: Cell::new(0),
            changed: Cell::default(),
            anchors: RefCell::new(vec![0]),
            snapshot: RefCell::default(),
            property: RefCell::default(),
            available: Cell::new(false),
            clearing: Cell::new(false),
        };
        view.controls()?;
        Ok(Self {
            view: Rc::new(view),
            observer: None,
            clear: None,
            family,
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
        self.view.property.borrow_mut().take();
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
        if self.view.visible.replace(visible) == visible {
            return Ok(());
        }
        if visible {
            self.reload(app, window)
        } else {
            self.view.property.borrow_mut().take();
            Ok(())
        }
    }

    fn reload(&mut self, app: &mut Application, window: &Window) -> liteos_ui::Result<()> {
        let view = &self.view;
        view.property.borrow_mut().take();
        view.refresh.set_enabled(false)?;
        view.request.set(view.request.get().wrapping_add(1));
        view.available.set(false);
        view.controls()?;
        view.summary.notice("正在读取转发连接…")?;
        if let Some(waker) = view.changed.take() {
            waker.wake();
        }
        if self.observer.as_ref().is_none_or(Task::is_finished) {
            let view = view.clone();
            let parent = window.clone();
            let domain = self.domain.clone();
            let family = self.family;
            self.observer = Some(app.spawn_local(window, async move {
                let result = view.observe(&parent, family, domain.as_ref()).await;
                view.changed.take();
                if let Err(error) = result
                    && view.visible.get()
                {
                    view.available.set(false);
                    view.property.borrow_mut().take();
                    let _ = view.refresh.set_enabled(true);
                    let _ = view.paging.set_state(0);
                    let _ = view.controls();
                    let _ = view
                        .summary
                        .notice(&format!("连接观察已停止，请刷新。{error}"));
                }
            })?);
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
        let view = &self.view;
        if !view.visible.get() {
            return Ok(());
        }
        if event.node == view.list.id() && matches!(event.kind, EventKind::Select { .. }) {
            if view.available.get() {
                view.show_summary()?;
            }
            return view.controls();
        }
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(());
        }
        if event.node == view.list.id() {
            if let Some((identity, row)) = view.selected()? {
                let mut property = view.property.borrow_mut();
                if let Some(current) = property
                    .as_ref()
                    .filter(|current| current.identity == identity)
                {
                    workspace.raise(&current.frame)?;
                } else {
                    let new = Property::new(workspace, identity, &row)?;
                    new.remove.set_enabled(!view.clearing.get())?;
                    *property = Some(new);
                }
            }
            return Ok(());
        }
        let remove = {
            let mut property = view.property.borrow_mut();
            if property
                .as_ref()
                .is_some_and(|property| property.closes(event))
            {
                property.take();
                return Ok(());
            }
            property
                .as_ref()
                .filter(|property| property.remove.activated(event))
                .and_then(|property| view.lookup(property.identity))
        };
        if view.refresh.activated(event) {
            self.reload(app, window)?;
        } else if view.available.get() && view.previous.activated(event) {
            if view.anchors.borrow().len() > 1 {
                view.anchors.borrow_mut().pop();
                self.reload(app, window)?;
            }
        } else if view.available.get() && view.next.activated(event) {
            let next = {
                let snapshot = view.snapshot.borrow();
                (snapshot.rows.len() > DISPLAY_ROWS).then(|| snapshot.rows[DISPLAY_ROWS - 1].id)
            };
            if let Some(after) = next {
                view.anchors.borrow_mut().push(after);
                self.reload(app, window)?;
            }
        } else if view.clearing.get() {
            return Ok(());
        } else if let Some((identity, row)) = if view.clear.activated(event) {
            view.selected()?
        } else {
            remove
        } {
            view.clearing.set(true);
            view.controls()?;
            let view = view.clone();
            let parent = window.clone();
            let domain = self.domain.clone();
            let request = view.request.get();
            self.clear = Some(app.spawn_local(window, async move {
                let result = view.clear_connection(&parent, domain, identity, row).await;
                view.clearing.set(false);
                let _ = view.controls();
                if let Err(error) = result
                    && request == view.request.get()
                    && view.visible.get()
                {
                    let _ = view.summary.notice(&format!("无法确认连接已删除。{error}"));
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
        let identity = Identity {
            epoch: self.snapshot.borrow().cursor.epoch,
            id,
        };
        Ok(self.lookup(identity))
    }

    fn lookup(&self, identity: Identity) -> Option<(Identity, Connection)> {
        let snapshot = self.snapshot.borrow();
        if !self.available.get() || snapshot.cursor.epoch != identity.epoch {
            return None;
        }
        snapshot
            .rows
            .iter()
            .take(DISPLAY_ROWS)
            .find(|row| row.id == identity.id)
            .map(|row| (identity, *row))
    }

    fn controls(&self) -> liteos_ui::Result<()> {
        let available = self.available.get();
        let previous = self.anchors.borrow().len() > 1;
        let next = self.snapshot.borrow().rows.len() > DISPLAY_ROWS;
        self.previous.set_enabled(available && previous)?;
        self.next.set_enabled(available && next)?;
        if available {
            self.paging
                .set_state(if previous || next { 0 } else { state::HIDDEN })?;
        }
        if let Some(property) = self.property.borrow().as_ref() {
            property
                .remove
                .set_enabled(available && !self.clearing.get())?;
        }
        self.clear
            .set_enabled(!self.clearing.get() && self.selected()?.is_some())
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
        drop(snapshot);
        let mut property = self.property.borrow_mut();
        if let Some(current) = property.as_ref() {
            if let Some((_, row)) = self.lookup(current.identity) {
                current.present(&row)?;
            } else {
                property.take();
            }
        }
        drop(property);
        self.show_summary()?;
        self.controls()
    }

    fn show_summary(&self) -> liteos_ui::Result<()> {
        let count = self.snapshot.borrow().rows.len().min(DISPLAY_ROWS);
        let selected = usize::from(self.list.selection()?.is_some());
        self.summary.show(count, selected, "实时")
    }

    async fn observe(
        &self,
        window: &Window,
        family: Family,
        domain: Option<&Arc<str>>,
    ) -> Result<(), Error> {
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
                    .run(move || snapshot(scope.as_deref(), family, after, epoch, baseline))?
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
                self.summary.notice("正在重新同步转发连接…")?;
            } else if !batch.changes.is_empty() {
                self.apply(domain, family, batch, request).await?;
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
        family: Family,
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
                        if family.includes(row.source.ip())
                            && row.id > after
                            && last_known.is_none_or(|last| row.id <= last) =>
                    {
                        match rows.binary_search_by_key(&row.id, |item| item.id) {
                            Ok(index) => {
                                changed |= (rows[index].phase, rows[index].seen_reply)
                                    != (row.phase, row.seen_reply);
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
                        family,
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
    ) -> Result<(), Error> {
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
            return Ok(());
        }
        self.background
            .run(move || ManagementClient::local().clear_connection(domain.as_deref(), identity))?
            .await?;
        Ok(())
    }
}

fn snapshot(
    domain: Option<&str>,
    family: Family,
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
        family,
        cursor.epoch,
        if first_page { 0 } else { after },
        WINDOW_ROWS,
    )?;
    Ok((Snapshot { cursor, rows }, first_page))
}

fn read_rows(
    client: &mut ManagementClient,
    domain: Option<&str>,
    family: Family,
    epoch: u64,
    after: u64,
    limit: usize,
) -> io::Result<Vec<Connection>> {
    let mut cursor = if after == 0 {
        Cursor {
            family,
            ..Cursor::default()
        }
    } else {
        Cursor {
            family,
            epoch,
            after,
        }
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
            Some(after) if rows.len() < limit => {
                cursor = Cursor {
                    after,
                    epoch,
                    ..cursor
                }
            }
            _ => return Ok(rows),
        }
    }
}

fn row_text(row: &Connection) -> String {
    Field::COLUMNS.map(|(field, _)| field.text(row)).join("\t")
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
