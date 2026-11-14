//! 网络管理器的链路流量页。

use crate::{child, observation::receive};
use liteos_ui::{
    Application, Background, Clock, Column, Event, EventKind, Layout, ListOptions, ListStyle, Task,
    Widget, WidgetKind, Window, state,
};
use network_config::traffic::{Page as TrafficPage, Row, TrafficObservation, Update};
use std::{cell::RefCell, io, os::fd::AsFd, rc::Rc};

mod analysis;

pub(super) struct Page {
    view: Rc<View>,
    task: Option<Task<()>>,
    visible: bool,
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
            ("接口", 98),
            ("接收速率", 94),
            ("发送速率", 94),
            ("接收总量", 94),
            ("发送总量", 94),
            ("接收丢包", 81),
            ("发送丢包", 81),
        ];
        list.set_columns(&std::array::from_fn::<_, 7, _>(|index| Column {
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
            background,
            clock: app.clock(),
            page: RefCell::default(),
        };
        Ok(Self {
            view: Rc::new(view),
            task: None,
            visible: false,
            cursors: vec![0],
            capture,
            analysis: None,
        })
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
                    row.index,
                    &String::from_utf8_lossy(row.name()),
                    self.view.background.clone(),
                )?);
            }
            return Ok(());
        }
        if self
            .capture
            .handle(app, window, event.node, self.view.list.selection()?)?
        {
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
        view.analyze.set_state(state::DISABLED)?;
        for button in [&view.previous, &view.next, &view.refresh] {
            button.set_state(state::DISABLED)?;
        }
        view.status.set_text("正在读取接口流量…")?;
        let parent = window.clone();
        self.task = Some(app.spawn_local(window, async move {
            if let Err(error) = view.observe(&parent, after, number).await {
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
        after: u32,
        number: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut stream = self
            .background
            .run(move || TrafficObservation::start(after))?
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
        let selection = self
            .list
            .selection()?
            .filter(|&id| page.rows.iter().any(|row| u64::from(row.index) == id));
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

    fn show_details(&self) -> liteos_ui::Result<()> {
        let selected = self.list.selection()?;
        let page = self.page.borrow();
        let Some(row) = page.as_ref().and_then(|page| {
            page.rows
                .iter()
                .find(|row| Some(u64::from(row.index)) == selected)
        }) else {
            self.analyze.set_state(state::DISABLED)?;
            return self.details.set_text("");
        };
        self.analyze.set_state(0)?;
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
