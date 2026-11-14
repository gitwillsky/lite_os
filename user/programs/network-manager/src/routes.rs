//! 当前生效路由页。成员跨页继续显示，选择详情可读完整双栈地址。

use crate::{APP_ID, child, observation::receive};
use liteos_ui::{
    Application, Background, Clock, Column, Dialogs, Event, EventKind, EventStream, Layout,
    ListOptions, ListStyle, MessageButtons, MessageIcon, Task, Widget, WidgetKind, WidgetSpec,
    Window, WindowOptions, state,
};
use network_config::configuration::IpPrefix;
use network_config::routes::{
    Cursor, Family, Page as RoutePage, Position, RouteKind, RouteObservation, Row, TABLE_LOCAL,
    TABLE_MAIN, observation::Update,
};
use std::{cell::RefCell, io, os::fd::AsFd, rc::Rc, sync::Arc};

mod bgp;
mod editor;
mod ospf;

type Error = Box<dyn std::error::Error>;

pub(super) struct Page {
    view: Rc<View>,
    task: Option<Task<()>>,
    dialog: Option<Task<()>>,
    visible: bool,
    domain: Option<Arc<str>>,
    family: Family,
    // 生效路径按当前位置读取，返回前一页只需保留访问过的页首位置。
    anchors: Vec<Position>,
}

struct View {
    list: Widget,
    details: Widget,
    previous: Widget,
    next: Widget,
    refresh: Widget,
    configure: Widget,
    ospf: Widget,
    bgp: Widget,
    ipv4: Widget,
    ipv6: Widget,
    status: Widget,
    background: Background,
    clock: Clock,
    snapshot: RefCell<Option<RoutePage>>,
}

impl Page {
    pub(super) fn new(
        app: &Application,
        parent: &Widget,
        background: Background,
        button: Layout,
    ) -> liteos_ui::Result<Self> {
        let bar = child(
            parent,
            WidgetKind::Row,
            "",
            Layout {
                gap: 6,
                ..Default::default()
            },
        )?;
        let mut ipv4 = WidgetSpec::new(WidgetKind::Radio, "IPv4");
        ipv4.state = state::CHECKED;
        let ipv4 = bar.child(ipv4)?;
        let ipv6 = bar.child(WidgetSpec::new(WidgetKind::Radio, "IPv6"))?;
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
                title: "目的网段",
                width: 220,
            },
            Column {
                id: 2,
                title: "路由表",
                width: 74,
            },
            Column {
                id: 3,
                title: "出口",
                width: 80,
            },
            Column {
                id: 4,
                title: "下一跳",
                width: 195,
            },
            Column {
                id: 5,
                title: "权重",
                width: 65,
            },
        ])?;
        let details = child(parent, WidgetKind::Label, "", Layout::default())?;
        details.set_state(state::WRAP)?;
        let controls = child(
            parent,
            WidgetKind::Row,
            "",
            Layout {
                gap: 6,
                ..Default::default()
            },
        )?;
        let view = View {
            ipv4,
            ipv6,
            list,
            details,
            background,
            clock: app.clock(),
            previous: child(&controls, WidgetKind::Button, "上一页", button)?,
            next: child(&controls, WidgetKind::Button, "下一页", button)?,
            refresh: child(&controls, WidgetKind::Button, "刷新", button)?,
            configure: child(&controls, WidgetKind::Button, "路由设置…", button)?,
            ospf: child(
                &controls,
                WidgetKind::Button,
                "OSPF 邻居…",
                Layout {
                    width: 108,
                    ..button
                },
            )?,
            bgp: child(&controls, WidgetKind::Button, "BGP…", button)?,
            status: child(parent, WidgetKind::Status, "", Layout::default())?,
            snapshot: RefCell::default(),
        };
        view.previous.set_state(state::DISABLED)?;
        view.next.set_state(state::DISABLED)?;
        Ok(Self {
            view: Rc::new(view),
            task: None,
            dialog: None,
            visible: false,
            domain: None,
            family: Family::Ipv4,
            anchors: vec![Position::default()],
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
        if !self.visible {
            return Ok(());
        }
        if event.node == self.view.list.id() && matches!(event.kind, EventKind::Select { .. }) {
            return self.view.show_details();
        }
        if !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(());
        }
        let configure = event.node == self.view.configure.id();
        let ospf = event.node == self.view.ospf.id();
        if configure || ospf || event.node == self.view.bgp.id() {
            if self.dialog.as_ref().is_some_and(|task| !task.is_finished()) {
                return Ok(());
            }
            let dialogs = app.dialogs();
            let parent = window.clone();
            let background = self.view.background.clone();
            let domain = self.domain.clone();
            self.dialog = Some(app.spawn_local(window, async move {
                let (title, result) = if configure {
                    (
                        "路由设置",
                        editor::run(&dialogs, &parent, &background, domain.as_deref()).await,
                    )
                } else if ospf {
                    (
                        "OSPF 邻居",
                        ospf::show(&dialogs, &parent, &background, domain)
                            .await
                            .map(|()| None),
                    )
                } else {
                    (
                        "BGP",
                        bgp::show(&dialogs, &parent, &background, domain)
                            .await
                            .map(|()| None),
                    )
                };
                let (message, icon) = match result {
                    Ok(Some(message)) => (message, MessageIcon::Information),
                    Ok(None) => return,
                    Err(error) => (error.to_string(), MessageIcon::Error),
                };
                if let Ok(message) =
                    dialogs.message(&parent, title, &message, MessageButtons::Ok, icon)
                {
                    let _ = message.await;
                }
            })?);
            return Ok(());
        }
        let family = if event.node == self.view.ipv4.id() {
            Some(Family::Ipv4)
        } else if event.node == self.view.ipv6.id() {
            Some(Family::Ipv6)
        } else {
            None
        };
        if let Some(family) = family {
            if self.family == family {
                return Ok(());
            }
            self.family = family;
            self.anchors.truncate(1);
        } else if event.node == self.view.previous.id() && self.anchors.len() > 1 {
            self.anchors.pop();
        } else if event.node == self.view.next.id() {
            let next = self
                .view
                .snapshot
                .borrow()
                .as_ref()
                .and_then(|page| page.next);
            let Some(next) = next else { return Ok(()) };
            self.anchors.push(next);
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
        view.show_details()?;
        for button in [&view.previous, &view.next, &view.refresh] {
            button.set_state(state::DISABLED)?;
        }
        view.status.set_text("正在读取生效路由…")?;
        let parent = window.clone();
        let page = self.anchors.len();
        let cursor = Cursor {
            family: self.family,
            start: *self.anchors.last().expect("first page anchor"),
        };
        let domain = self.domain.clone();
        self.task = Some(app.spawn_local(window, async move {
            if let Err(error) = view.observe(&parent, page, domain, cursor).await {
                let _ = view
                    .previous
                    .set_state(if page > 1 { 0 } else { state::DISABLED });
                let _ = view.refresh.set_state(0);
                let _ = view
                    .status
                    .set_text(&format!("路由观察已停止，请刷新。{error}"));
            }
        })?);
        Ok(())
    }
}

fn prefix(text: &str) -> Option<IpPrefix> {
    let (address, length) = text.trim().split_once('/')?;
    let value = IpPrefix {
        network: address.parse().ok()?,
        length: length.parse().ok()?,
    };
    value.valid().then_some(value)
}

fn form(
    dialogs: &Dialogs,
    parent: &Window,
    title: &str,
    width: i32,
    height: i32,
) -> liteos_ui::Result<(Window, EventStream, Widget)> {
    let mut options = WindowOptions::new(title, APP_ID, width, height);
    options.parent = Some(parent);
    options.modal = true;
    let (window, events) = dialogs.window(options)?;
    let root = window.widget(WidgetSpec {
        layout: Layout {
            grow: 1,
            padding: 10,
            gap: 6,
            ..Layout::default()
        },
        ..WidgetSpec::new(WidgetKind::Column, "")
    })?;
    Ok((window, events, root))
}

impl View {
    async fn observe(
        &self,
        window: &Window,
        page: usize,
        domain: Option<Arc<str>>,
        cursor: Cursor,
    ) -> Result<(), Error> {
        let mut stream = self
            .background
            .run(move || RouteObservation::start(domain.as_deref(), cursor))?
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
                        io::Error::new(io::ErrorKind::InvalidData, "未收到路由快照")
                    })?;
                    changes
                        .apply(previous, cursor)
                        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?
                }
            };
            self.present(page, next)?;
        }
    }

    fn selected(&self) -> liteos_ui::Result<Option<Row>> {
        let index = self.list.selection()?.map(|id| (id - 1) as usize);
        let snapshot = self.snapshot.borrow();
        Ok(index.and_then(|index| snapshot.as_ref()?.rows.get(index).copied()))
    }

    fn present(&self, page: usize, next: RoutePage) -> liteos_ui::Result<()> {
        let selected = self.selected()?.and_then(|old| {
            next.rows.iter().position(|row| {
                *row == Row {
                    position: row.position,
                    ..old
                }
            })
        });
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
        self.next.set_state(if next.next.is_some() {
            0
        } else {
            state::DISABLED
        })?;
        self.refresh.set_state(0)?;
        self.status
            .set_text(&format!("第 {page} 页 · {} 条路径 · 正在观察", rows.len()))?;
        *self.snapshot.borrow_mut() = Some(next);
        self.show_details()
    }

    fn show_details(&self) -> liteos_ui::Result<()> {
        let Some(row) = self.selected()? else {
            return self.details.set_text("");
        };
        let kind = kind_label(row.kind);
        let preferred = row
            .preferred_source
            .map_or_else(|| "自动".into(), |ip| ip.to_string());
        let gateway = next_hop(row);
        let protocol = match row.protocol {
            2 => "系统".into(),
            3 => "启动配置".into(),
            4 => "静态".into(),
            9 => "路由器通告".into(),
            16 => "DHCP".into(),
            186 => "BGP".into(),
            188 => "OSPF".into(),
            number => number.to_string(),
        };
        let scope = match row.scope {
            0 => "全局".into(),
            253 => "链路".into(),
            254 => "本机".into(),
            number => number.to_string(),
        };
        let on_link = if row.hop.is_some_and(|hop| hop.on_link) {
            "（按直连网关处理）"
        } else {
            ""
        };
        self.details.set_text(&format!(
            "{}/{}　{kind}　下一跳 {gateway}{on_link}\n来源 {protocol}　度量值 {}　范围 {scope}　首选源 {preferred}",
            row.prefix.network, row.prefix.length, row.metric
        ))
    }
}

fn row_text(row: &Row) -> String {
    let table = match row.table {
        TABLE_MAIN => "主表".into(),
        TABLE_LOCAL => "本地表".into(),
        table => table.to_string(),
    };
    let (name, weight) = row.hop.map_or_else(
        || ("—".into(), "—".into()),
        |hop| {
            let end = hop
                .name
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(hop.name.len());
            (
                String::from_utf8_lossy(&hop.name[..end]).into_owned(),
                hop.weight.to_string(),
            )
        },
    );
    let gateway = next_hop(*row);
    format!(
        "{}/{}\t{table}\t{name}\t{gateway}\t{weight}",
        row.prefix.network, row.prefix.length
    )
}

fn next_hop(row: Row) -> String {
    if let Some(gateway) = row.hop.and_then(|hop| hop.gateway) {
        gateway.to_string()
    } else if row.kind == RouteKind::Unicast && row.hop.is_some() {
        "直连".into()
    } else {
        kind_label(row.kind).into()
    }
}

fn kind_label(kind: RouteKind) -> &'static str {
    match kind {
        RouteKind::Unicast => "单播",
        RouteKind::Local => "本机",
        RouteKind::Broadcast => "广播",
        RouteKind::Unreachable => "不可达",
        RouteKind::Throw => "继续查表",
        RouteKind::Blackhole => "丢弃",
        RouteKind::Prohibit => "禁止",
    }
}
