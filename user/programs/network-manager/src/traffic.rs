//! 网络管理器的链路流量页。

use crate::{
    item_list::{Action, Body, Glyph, Item, Summary, Tool, note, set_note, toolbar},
    observation::receive,
    routes::{Dialog, Section},
};
use liteos_ui::{
    Application, Background, Clock, Column, Event, EventKind, ListOptions, ListStyle, Task, Widget,
    Window,
};
use network_config::traffic::{Page as TrafficPage, Row, TrafficObservation, Update};
use std::{
    cell::{Cell, RefCell},
    io,
    os::fd::AsFd,
    rc::Rc,
    sync::Arc,
};

mod analysis;

/// 接口列表交给主窗口执行的操作。
pub(super) enum Request {
    /// 打开配置对话框（「新建」进入网桥与 VLAN 设置）。
    Dialog(Dialog),
    /// 打开所选网卡的属性窗口。
    Properties(Row),
    /// 直接提交所选网卡的管理状态。
    SetEnabled(Row, bool),
}

pub(super) struct Page {
    view: Rc<View>,
    task: Option<Task<()>>,
    visible: bool,
    domain: Option<Arc<str>>,
    cursors: Vec<u32>,
    capture: crate::capture::Controls,
    analysis: Option<Task<()>>,
}

struct View {
    // 列表行与「动作」面板的容器及标题只需存活。
    _body: Body,
    list: Widget,
    details: Widget,
    summary: Summary,
    previous: Tool,
    next: Tool,
    refresh: Tool,
    analyze: Action,
    add: Tool,
    enable: Tool,
    disable: Tool,
    background: Background,
    clock: Clock,
    page: RefCell<Option<TrafficPage>>,
    // 当前页序号，状态栏在选择变化时也要显示。
    number: Cell<usize>,
}

impl Page {
    pub(super) fn new(
        app: &Application,
        parent: &Widget,
        background: Background,
    ) -> liteos_ui::Result<Self> {
        // 「新建」打开网桥与 VLAN 设置：物理网卡由硬件决定，可新建的接口只有网桥、VLAN 网关这类虚拟接口。
        let [add, enable, disable, _, previous, next, refresh] = toolbar(
            parent,
            [
                Item::Command(Glyph::Add, "新建"),
                Item::Command(Glyph::Enable, "启用"),
                Item::Command(Glyph::Disable, "停用"),
                Item::Separator,
                Item::Icon(Glyph::Previous, "上一页"),
                Item::Icon(Glyph::Next, "下一页"),
                Item::Icon(Glyph::Refresh, "刷新"),
            ],
        )?;
        for tool in [&enable, &disable] {
            tool.set_enabled(false)?;
        }
        let body = Body::new(parent)?;
        let analyze = body.action("流量分析…")?;
        analyze.set_enabled(false)?;
        let capture = body.action("抓包…")?;
        let stop = body.action("停止抓包")?;
        let list = body.list.clone();
        list.set_list_style(
            ListStyle::Details,
            ListOptions {
                multiple: false,
                full_row_select: true,
            },
        )?;
        // 首列是 WinBox 的标志列：X 表示管理停用。
        let columns = [
            ("", 26),
            ("接口", 86),
            ("管理状态", 86),
            ("接收速率", 94),
            ("发送速率", 94),
            ("接收总量", 94),
            ("发送总量", 94),
            ("接收丢包", 81),
            ("发送丢包", 81),
        ];
        list.set_columns(&std::array::from_fn::<_, 9, _>(|index| Column {
            id: index as u64 + 1,
            title: columns[index].0,
            width: columns[index].1,
        }))?;
        let details = note(parent)?;
        let summary = Summary::new(parent)?;
        let capture = crate::capture::Controls::new(
            capture,
            stop,
            parent,
            app.dialogs(),
            background.clone(),
        )?;
        let view = View {
            _body: body,
            list,
            details,
            summary,
            previous,
            next,
            refresh,
            analyze,
            add,
            enable,
            disable,
            background,
            clock: app.clock(),
            page: RefCell::default(),
            number: Cell::new(1),
        };
        Ok(Self {
            view: Rc::new(view),
            task: None,
            visible: false,
            domain: None,
            cursors: vec![0],
            capture,
            analysis: None,
        })
    }

    pub(super) fn select_domain(
        &mut self,
        app: &mut Application,
        window: &Window,
        domain: Option<Arc<str>>,
    ) -> liteos_ui::Result<()> {
        self.domain = domain;
        self.cursors.truncate(1);
        self.analysis = None;
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
        if self.visible != visible {
            self.visible = visible;
            if visible {
                self.subscribe(app, window)?;
            } else {
                self.task = None;
                self.analysis = None;
            }
        }
        Ok(())
    }

    /// 列表上需要主窗口执行的请求：新建、属性窗口或启停。
    pub(super) fn request(&self, event: &Event) -> liteos_ui::Result<Option<Request>> {
        if !self.visible || !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(None);
        }
        let view = &self.view;
        if view.add.activated(event) {
            return Ok(Some(Request::Dialog(Dialog::Settings(Some(
                Section::Bridges,
            )))));
        }
        // WinBox 约定：双击或回车打开所选条目的属性窗口，没有单独的属性按钮。
        let enabled = if event.node == view.list.id() {
            None
        } else if view.enable.activated(event) {
            Some(true)
        } else if view.disable.activated(event) {
            Some(false)
        } else {
            return Ok(None);
        };
        let Some(row) = view
            .selected()?
            .filter(|row| row.ethernet_address.is_some())
        else {
            return Ok(None);
        };
        Ok(match enabled {
            None => Some(Request::Properties(row)),
            Some(enabled) => (enabled != row.up).then_some(Request::SetEnabled(row, enabled)),
        })
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
        if event.node == self.view.list.id() && matches!(event.kind, EventKind::Select { .. }) {
            return self.view.show_details();
        }
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(());
        }
        if self.view.analyze.activated(event) {
            let selected = self.view.list.selection()?;
            let page = self.view.page.borrow();
            if let Some(row) = page.as_ref().and_then(|page| {
                page.rows
                    .iter()
                    .find(|row| Some(u64::from(row.index)) == selected)
            }) {
                self.analysis = Some(analysis::open(
                    app,
                    window,
                    self.domain.clone(),
                    row.index,
                    &String::from_utf8_lossy(row.name()),
                    self.view.background.clone(),
                )?);
            }
            return Ok(());
        }
        if self.capture.handle(
            app,
            window,
            event,
            self.domain.clone(),
            self.view.list.selection()?,
        )? {
            return Ok(());
        }
        if self.view.previous.activated(event) && self.cursors.len() > 1 {
            self.cursors.pop();
        } else if self.view.next.activated(event) {
            let next = self.view.page.borrow().as_ref().and_then(TrafficPage::next);
            let Some(next) = next else { return Ok(()) };
            self.cursors.push(next);
        } else if !self.view.refresh.activated(event) {
            return Ok(());
        }
        self.subscribe(app, window)
    }

    pub(super) fn finish_capture_before_close(&self) -> bool {
        self.capture.finish_before_close()
    }

    fn subscribe(&mut self, app: &mut Application, window: &Window) -> liteos_ui::Result<()> {
        self.task = None;
        let after = *self.cursors.last().expect("首页游标");
        let number = self.cursors.len();
        let view = self.view.clone();
        view.page.borrow_mut().take();
        view.list.set_rows(&[])?;
        set_note(&view.details, "")?;
        view.show_details()?;
        for tool in [&view.previous, &view.next, &view.refresh] {
            tool.set_enabled(false)?;
        }
        view.summary.notice("正在读取接口流量…")?;
        let parent = window.clone();
        let domain = self.domain.clone();
        self.task = Some(app.spawn_local(window, async move {
            if let Err(error) = view.observe(&parent, domain, after, number).await {
                let _ = view.previous.set_enabled(number > 1);
                let _ = view.refresh.set_enabled(true);
                let _ = view
                    .summary
                    .notice(&format!("流量观察已停止，请刷新。{error}"));
            }
        })?);
        Ok(())
    }
}

impl View {
    async fn observe(
        &self,
        window: &Window,
        domain: Option<Arc<str>>,
        after: u32,
        number: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut stream = self
            .background
            .run(move || TrafficObservation::start(domain.as_deref(), after))?
            .await?;
        let mut fd = window.watch(stream.as_fd().try_clone_to_owned()?)?;
        loop {
            let page = match receive(&self.clock, &mut fd, stream.remaining(), || {
                stream.try_next()
            })
            .await?
            {
                Update::Heartbeat => continue,
                Update::Snapshot(page) => page,
                Update::Changes(changes) => {
                    let previous = self.page.borrow();
                    let previous = previous
                        .as_ref()
                        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
                    changes
                        .apply(previous, after)
                        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?
                }
            };
            self.present(page, number)?;
        }
    }

    fn present(&self, page: TrafficPage, number: usize) -> liteos_ui::Result<()> {
        let selection = self.selected()?.and_then(|selected| {
            page.rows
                .iter()
                .find(|row| {
                    row.index == selected.index
                        && row.name == selected.name
                        && row.ethernet_address == selected.ethernet_address
                })
                .map(|row| u64::from(row.index))
        });
        let text: Vec<_> = page.rows.iter().map(row_text).collect();
        let rows: Vec<_> = page
            .rows
            .iter()
            .zip(&text)
            .map(|(row, text)| (u64::from(row.index), text.as_str()))
            .collect();
        self.list.set_rows(&rows)?;
        self.list.select(selection)?;
        self.previous.set_enabled(number > 1)?;
        self.next.set_enabled(page.more)?;
        self.refresh.set_enabled(true)?;
        *self.page.borrow_mut() = Some(page);
        self.number.set(number);
        self.show_details()
    }

    fn selected(&self) -> liteos_ui::Result<Option<Row>> {
        let selected = self.list.selection()?;
        Ok(self.page.borrow().as_ref().and_then(|page| {
            page.rows
                .iter()
                .find(|row| Some(u64::from(row.index)) == selected)
                .copied()
        }))
    }

    fn show_details(&self) -> liteos_ui::Result<()> {
        let row = self.selected()?;
        if let Some(page) = self.page.borrow().as_ref() {
            let note = format!("实时（第 {} 页）", self.number.get());
            self.summary
                .show(page.rows.len(), usize::from(row.is_some()), &note)?;
        }
        let editable = row.filter(|row| row.ethernet_address.is_some());
        self.enable
            .set_enabled(editable.is_some_and(|row| !row.up))?;
        self.disable
            .set_enabled(editable.is_some_and(|row| row.up))?;
        self.analyze.set_enabled(row.is_some())?;
        let Some(row) = row else {
            return set_note(&self.details, "");
        };
        let [received, sent, rx_bytes, tx_bytes, rx_errors, tx_errors, ..] = row.counters;
        set_note(
            &self.details,
            &format!(
                "{}　接收 {received} 包 / {rx_bytes} B，错误 {rx_errors}　发送 {sent} 包 / {tx_bytes} B，错误 {tx_errors}",
                String::from_utf8_lossy(row.name()),
            ),
        )
    }
}

fn row_text(row: &Row) -> String {
    let [_, _, received, sent, _, _, rx_drop, tx_drop] = row.counters;
    let [rx, tx] = row.bytes_per_second.map_or_else(
        || ["—".into(), "—".into()],
        |rates| rates.map(|value| volume(value, "/s")),
    );
    let (flag, state) = if row.up {
        ("", "启用")
    } else {
        ("X", "停用")
    };
    format!(
        "{flag}\t{}\t{state}\t{rx}\t{tx}\t{}\t{}\t{rx_drop}\t{tx_drop}",
        String::from_utf8_lossy(row.name()),
        volume(received, ""),
        volume(sent, ""),
    )
}

fn volume(bytes: u64, suffix: &str) -> String {
    if bytes < 1024 {
        return format!("{bytes} B{suffix}");
    }
    let exponent = ((u64::BITS - 1 - bytes.leading_zeros()) / 10) as usize;
    let unit = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"][exponent];
    let value = bytes as f64 / (1u64 << (exponent * 10)) as f64;
    format!("{value:.1} {unit}{suffix}")
}
