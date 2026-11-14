//! 网络管理器的链路流量页。

use crate::{
    child,
    observation::receive,
    routes::{Dialog, InterfaceAction},
};
use liteos_ui::{
    Application, Background, Clock, Column, Event, EventKind, Layout, ListOptions, ListStyle, Task,
    Widget, WidgetKind, Window, state,
};
use network_config::traffic::{Page as TrafficPage, Row, TrafficObservation, Update};
use std::{cell::RefCell, io, os::fd::AsFd, rc::Rc, sync::Arc};

mod analysis;

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
    list: Widget,
    details: Widget,
    status: Widget,
    previous: Widget,
    next: Widget,
    refresh: Widget,
    analyze: Widget,
    properties: Widget,
    enable: Widget,
    disable: Widget,
    background: Background,
    clock: Clock,
    page: RefCell<Option<TrafficPage>>,
}

impl Page {
    pub(super) fn new(
        app: &Application,
        parent: &Widget,
        background: Background,
        button: Layout,
    ) -> liteos_ui::Result<Self> {
        let actions = child(
            parent,
            WidgetKind::Row,
            "",
            Layout {
                gap: 6,
                ..Layout::default()
            },
        )?;
        let properties = child(&actions, WidgetKind::Button, "属性…", button)?;
        let enable = child(&actions, WidgetKind::Button, "启用", button)?;
        let disable = child(&actions, WidgetKind::Button, "停用", button)?;
        for widget in [&properties, &enable, &disable] {
            widget.set_state(state::DISABLED)?;
        }
        let list = child(
            parent,
            WidgetKind::List,
            "",
            Layout {
                grow: 1,
                ..Layout::default()
            },
        )?;
        list.set_list_style(
            ListStyle::Details,
            ListOptions {
                multiple: false,
                full_row_select: true,
            },
        )?;
        let columns = [
            ("接口", 86),
            ("管理状态", 86),
            ("接收速率", 94),
            ("发送速率", 94),
            ("接收总量", 94),
            ("发送总量", 94),
            ("接收丢包", 81),
            ("发送丢包", 81),
        ];
        list.set_columns(&std::array::from_fn::<_, 8, _>(|index| Column {
            id: index as u64 + 1,
            title: columns[index].0,
            width: columns[index].1,
        }))?;
        let details = child(parent, WidgetKind::Label, "", Layout::default())?;
        details.set_state(state::WRAP)?;
        let controls = child(
            parent,
            WidgetKind::Row,
            "",
            Layout {
                gap: 6,
                ..Layout::default()
            },
        )?;
        let previous = child(&controls, WidgetKind::Button, "上一页", button)?;
        let next = child(&controls, WidgetKind::Button, "下一页", button)?;
        let refresh = child(&controls, WidgetKind::Button, "刷新", button)?;
        let analyze = child(&controls, WidgetKind::Button, "流量分析…", button)?;
        analyze.set_state(state::DISABLED)?;
        let capture = crate::capture::Controls::new(
            &controls,
            parent,
            app.dialogs(),
            background.clone(),
            button,
        )?;
        let view = View {
            list,
            details,
            status: child(parent, WidgetKind::Status, "", Layout::default())?,
            previous,
            next,
            refresh,
            analyze,
            properties,
            enable,
            disable,
            background,
            clock: app.clock(),
            page: RefCell::default(),
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

    pub(super) fn configuration_requested(
        &self,
        event: &Event,
    ) -> liteos_ui::Result<Option<Dialog>> {
        if !self.visible || !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(None);
        }
        let view = &self.view;
        let action = if event.node == view.properties.id() || event.node == view.list.id() {
            InterfaceAction::Properties
        } else if event.node == view.enable.id() {
            InterfaceAction::SetEnabled(true)
        } else if event.node == view.disable.id() {
            InterfaceAction::SetEnabled(false)
        } else {
            return Ok(None);
        };
        let selected = view.selected()?;
        Ok(selected
            .filter(|row| {
                row.ethernet_address.is_some()
                    && match action {
                        InterfaceAction::Properties => true,
                        InterfaceAction::SetEnabled(enabled) => enabled != row.up,
                    }
            })
            .map(|row| Dialog::Interface { row, action }))
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
        if event.node == self.view.analyze.id() {
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
            event.node,
            self.domain.clone(),
            self.view.list.selection()?,
        )? {
            return Ok(());
        }
        if event.node == self.view.previous.id() && self.cursors.len() > 1 {
            self.cursors.pop();
        } else if event.node == self.view.next.id() {
            let next = self.view.page.borrow().as_ref().and_then(TrafficPage::next);
            let Some(next) = next else { return Ok(()) };
            self.cursors.push(next);
        } else if event.node != self.view.refresh.id() {
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
        view.details.set_text("")?;
        view.show_details()?;
        for button in [&view.previous, &view.next, &view.refresh] {
            button.set_state(state::DISABLED)?;
        }
        view.status.set_text("正在读取接口流量…")?;
        let parent = window.clone();
        let domain = self.domain.clone();
        self.task = Some(app.spawn_local(window, async move {
            if let Err(error) = view.observe(&parent, domain, after, number).await {
                if number > 1 {
                    let _ = view.previous.set_state(0);
                }
                let _ = view.refresh.set_state(0);
                let _ = view
                    .status
                    .set_text(&format!("流量观察已停止，请刷新。{error}"));
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
        self.previous
            .set_state(if number > 1 { 0 } else { state::DISABLED })?;
        self.next
            .set_state(if page.more { 0 } else { state::DISABLED })?;
        self.refresh.set_state(0)?;
        self.status
            .set_text(&format!("第 {number} 页 · {} 个接口", rows.len()))?;
        *self.page.borrow_mut() = Some(page);
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
        let editable = row.filter(|row| row.ethernet_address.is_some());
        for (control, available) in [
            (&self.properties, editable.is_some()),
            (&self.enable, editable.is_some_and(|row| !row.up)),
            (&self.disable, editable.is_some_and(|row| row.up)),
            (&self.analyze, row.is_some()),
        ] {
            control.set_state(if available { 0 } else { state::DISABLED })?;
        }
        let Some(row) = row else {
            return self.details.set_text("");
        };
        let [received, sent, rx_bytes, tx_bytes, rx_errors, tx_errors, ..] = row.counters;
        self.details.set_text(&format!(
            "{}　接收 {received} 包 / {rx_bytes} B，错误 {rx_errors}　发送 {sent} 包 / {tx_bytes} B，错误 {tx_errors}",
            String::from_utf8_lossy(row.name()),
        ))
    }
}

fn row_text(row: &Row) -> String {
    let [_, _, received, sent, _, _, rx_drop, tx_drop] = row.counters;
    let [rx, tx] = row.bytes_per_second.map_or_else(
        || ["—".into(), "—".into()],
        |rates| rates.map(|value| volume(value, "/s")),
    );
    format!(
        "{}\t{rx}\t{tx}\t{}\t{}\t{rx_drop}\t{tx_drop}",
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
