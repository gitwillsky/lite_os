//! 当前生效路由页。成员跨页继续显示，选择详情可读完整双栈地址。

use crate::{
    APP_ID, child,
    item_list::{Glyph, Item, Summary, Tool, note, set_note, spacer, toolbar},
    observation::receive,
};
use liteos_ui::{
    Application, Background, Clock, Column, Dialogs, Event, EventKind, EventStream, Layout,
    ListOptions, ListStyle, MessageButtons, MessageIcon, Task, TreeRow, Widget, WidgetKind,
    WidgetSpec, Window, WindowOptions, state,
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
mod static_routes;
pub(crate) use editor::{RuleKind, edit_rule, interface_names, rule_address};

pub(super) use static_routes::Page as StaticRoutes;

pub(super) use editor::{
    ADMIN_STATES, BridgePage, Field, Fields, LiveInterface, Section, admin_index, apply_fields,
    edit_ipv4_of, edit_ipv6_of,
};

#[derive(Clone, Copy)]
pub(super) enum Dialog {
    Settings(Option<Section>),
    Ospf,
    Bgp,
}

type Error = Box<dyn std::error::Error>;

pub(super) struct Page {
    view: Rc<View>,
    task: Option<Task<()>>,
    dialog: Option<Task<()>>,
    visible: bool,
    domain: Option<Arc<str>>,
    family: Family,
    filter: Option<IpPrefix>,
    // 生效路径按当前位置读取，返回前一页只需保留访问过的页首位置。
    anchors: Vec<Position>,
}

struct View {
    // 弹性空白与「查找」标签只需存活；控件句柄释放即删除节点。
    _decor: [Widget; 2],
    list: Widget,
    details: Widget,
    previous: Tool,
    next: Tool,
    refresh: Tool,
    filter: Tool,
    family: Widget,
    destination: Widget,
    summary: Summary,
    background: Background,
    clock: Clock,
    snapshot: RefCell<Option<RoutePage>>,
}

/// 地址族下拉框的行标识。
const IPV4_ROW: u64 = 1;
const IPV6_ROW: u64 = 2;

impl Page {
    pub(super) fn new(
        app: &Application,
        parent: &Widget,
        background: Background,
    ) -> liteos_ui::Result<Self> {
        let bar = child(
            parent,
            WidgetKind::Row,
            "",
            Layout {
                gap: 4,
                ..Default::default()
            },
        )?;
        let [filter, _, previous, next, refresh] = toolbar(
            &bar,
            [
                Item::Icon(Glyph::Filter, "按目的网段筛选"),
                Item::Separator,
                Item::Icon(Glyph::Previous, "上一页"),
                Item::Icon(Glyph::Next, "下一页"),
                Item::Icon(Glyph::Refresh, "刷新"),
            ],
        )?;
        // WinBox 把查找框和视图下拉框放在工具栏右端：在查找框回车或点漏斗按目的网段筛选，清空后恢复。
        let spacer = spacer(&bar)?;
        let find = child(&bar, WidgetKind::Label, "查找", Layout::default())?;
        let destination = child(
            &bar,
            WidgetKind::Edit,
            "",
            Layout {
                width: 190,
                min_width: 52,
                ..Default::default()
            },
        )?;
        let family = child(
            &bar,
            WidgetKind::Combo,
            "",
            Layout {
                width: 70,
                ..Default::default()
            },
        )?;
        family.set_state(state::DROPDOWN_LIST)?;
        family.set_tree_rows(&[
            TreeRow {
                id: IPV4_ROW,
                parent: 0,
                text: "IPv4",
                expanded: false,
                has_children: false,
            },
            TreeRow {
                id: IPV6_ROW,
                parent: 0,
                text: "IPv6",
                expanded: false,
                has_children: false,
            },
        ])?;
        family.select(Some(IPV4_ROW))?;
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
        // 首列是 RouterOS 的路由标志，见 [`flags`]。
        list.set_columns(&[
            Column {
                id: 1,
                title: "",
                width: 40,
            },
            Column {
                id: 2,
                title: "目的网段",
                width: 220,
            },
            Column {
                id: 3,
                title: "路由表",
                width: 74,
            },
            Column {
                id: 4,
                title: "出口",
                width: 80,
            },
            Column {
                id: 5,
                title: "下一跳",
                width: 195,
            },
            Column {
                id: 6,
                title: "权重",
                width: 65,
            },
        ])?;
        let details = note(parent)?;
        let view = View {
            _decor: [spacer, find],
            family,
            destination,
            filter,
            list,
            details,
            background,
            clock: app.clock(),
            previous,
            next,
            refresh,
            summary: Summary::new(parent)?,
            snapshot: RefCell::default(),
        };
        view.previous.set_enabled(false)?;
        view.next.set_enabled(false)?;
        Ok(Self {
            view: Rc::new(view),
            task: None,
            dialog: None,
            visible: false,
            domain: None,
            family: Family::Ipv4,
            filter: None,
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
        let family = if event.node == self.view.family.id()
            && matches!(event.kind, EventKind::Select { .. })
        {
            match self.view.family.selection()? {
                Some(IPV6_ROW) => Some(Family::Ipv6),
                Some(_) => Some(Family::Ipv4),
                None => None,
            }
        } else {
            None
        };
        if family.is_none() && !matches!(event.kind, EventKind::Activate { .. }) {
            return Ok(());
        }
        if self.view.filter.activated(event) || event.node == self.view.destination.id() {
            let text = self.view.destination.text()?;
            let filter = if text.trim().is_empty() {
                None
            } else if let Some(prefix) = prefix(&text) {
                Some(prefix)
            } else {
                let message = app.dialogs().message(
                    window,
                    "筛选路由",
                    "请输入规范的目的网段，例如 192.0.2.0/24 或 2001:db8::/32。",
                    MessageButtons::Ok,
                    MessageIcon::Warning,
                )?;
                self.dialog = Some(app.spawn_local(window, async move {
                    let _ = message.await;
                })?);
                return Ok(());
            };
            if let Some(prefix) = filter {
                self.family = if prefix.network.is_ipv4() {
                    Family::Ipv4
                } else {
                    Family::Ipv6
                };
                self.view.family.select(Some(match self.family {
                    Family::Ipv4 => IPV4_ROW,
                    Family::Ipv6 => IPV6_ROW,
                }))?;
            }
            self.filter = filter;
            self.anchors.truncate(1);
        } else if let Some(family) = family {
            if self.family == family {
                return Ok(());
            }
            self.family = family;
            self.filter = None;
            self.view.destination.set_text("")?;
            self.anchors.truncate(1);
        } else if self.view.previous.activated(event) && self.anchors.len() > 1 {
            self.anchors.pop();
        } else if self.view.next.activated(event) {
            let next = self
                .view
                .snapshot
                .borrow()
                .as_ref()
                .and_then(|page| page.next);
            let Some(next) = next else { return Ok(()) };
            self.anchors.push(next);
        } else if !self.view.refresh.activated(event) {
            return Ok(());
        }
        self.subscribe(app, window)
    }

    pub(super) fn open(
        &mut self,
        app: &mut Application,
        window: &Window,
        dialog: Dialog,
    ) -> liteos_ui::Result<()> {
        if self.dialog.as_ref().is_some_and(|task| !task.is_finished()) {
            return Ok(());
        }
        let dialogs = app.dialogs();
        let parent = window.clone();
        let background = self.view.background.clone();
        let domain = self.domain.clone();
        self.dialog = Some(app.spawn_local(window, async move {
            let (title, result) = match dialog {
                Dialog::Settings(section) => (
                    "路由设置",
                    editor::run(&dialogs, &parent, &background, domain.as_deref(), section).await,
                ),
                Dialog::Ospf => (
                    "OSPF 邻居",
                    ospf::show(&dialogs, &parent, &background, domain)
                        .await
                        .map(|()| None),
                ),
                Dialog::Bgp => (
                    "BGP",
                    bgp::show(&dialogs, &parent, &background, domain)
                        .await
                        .map(|()| None),
                ),
            };
            let (message, icon) = match result {
                Ok(Some(message)) => (message, MessageIcon::Information),
                Ok(None) => return,
                Err(error) => (error.to_string(), MessageIcon::Error),
            };
            if let Ok(message) = dialogs.message(&parent, title, &message, MessageButtons::Ok, icon)
            {
                let _ = message.await;
            }
        })?);
        Ok(())
    }

    fn subscribe(&mut self, app: &mut Application, window: &Window) -> liteos_ui::Result<()> {
        self.task = None;
        let view = self.view.clone();
        view.snapshot.borrow_mut().take();
        view.list.set_rows(&[])?;
        view.show_details()?;
        for tool in [&view.previous, &view.next, &view.refresh] {
            tool.set_enabled(false)?;
        }
        view.summary.notice("正在读取生效路由…")?;
        let parent = window.clone();
        let page = self.anchors.len();
        let cursor = Cursor {
            family: self.family,
            start: *self.anchors.last().expect("first page anchor"),
            prefix: self.filter,
        };
        let domain = self.domain.clone();
        self.task = Some(app.spawn_local(window, async move {
            if let Err(error) = view.observe(&parent, page, domain, cursor).await {
                let _ = view.previous.set_enabled(page > 1);
                let _ = view.refresh.set_enabled(true);
                let _ = view
                    .summary
                    .notice(&format!("路由观察已停止，请刷新。{error}"));
            }
        })?);
        Ok(())
    }
}

fn prefix(text: &str) -> Option<IpPrefix> {
    text.parse().ok()
}

fn protocol_label(protocol: u8) -> String {
    match protocol {
        2 => "系统".into(),
        3 => "启动配置".into(),
        4 => "静态".into(),
        9 => "路由器通告".into(),
        16 => "DHCP".into(),
        network_config::configuration::bgp::ROUTE_PROTOCOL => "BGP".into(),
        network_config::configuration::ospf::ROUTE_PROTOCOL => "OSPF".into(),
        number => number.to_string(),
    }
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
        self.previous.set_enabled(page > 1)?;
        self.next.set_enabled(next.next.is_some())?;
        self.refresh.set_enabled(true)?;
        let selected = usize::from(self.list.selection()?.is_some());
        self.summary
            .show(rows.len(), selected, &format!("实时（第 {page} 页）"))?;
        *self.snapshot.borrow_mut() = Some(next);
        self.show_details()
    }

    fn show_details(&self) -> liteos_ui::Result<()> {
        let Some(row) = self.selected()? else {
            return set_note(&self.details, "");
        };
        let kind = kind_label(row.kind);
        let preferred = row
            .preferred_source
            .map_or_else(|| "自动".into(), |ip| ip.to_string());
        let gateway = next_hop(row);
        let protocol = protocol_label(row.protocol);
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
        set_note(
            &self.details,
            &format!(
                "{}/{}　{kind}　下一跳 {gateway}{on_link}\n来源 {protocol}　度量值 {}　范围 {scope}　首选源 {preferred}",
                row.prefix.network, row.prefix.length, row.metric
            ),
        )
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
        "{}\t{}/{}\t{table}\t{name}\t{gateway}\t{weight}",
        flags(row),
        row.prefix.network,
        row.prefix.length
    )
}

/// RouterOS 7 的路由标志：D 动态、A 生效，后缀为来源（c 直连、s 静态、d DHCP、b BGP、o OSPF）。
/// 观察到的都是已安装的路由，因此都带 A；静态与启动配置是管理员配置，不带 D。
fn flags(row: &Row) -> String {
    let source = match row.protocol {
        2 => "c",
        3 | 4 => "s",
        16 => "d",
        network_config::configuration::bgp::ROUTE_PROTOCOL => "b",
        network_config::configuration::ospf::ROUTE_PROTOCOL => "o",
        _ => "",
    };
    let dynamic = if matches!(row.protocol, 3 | 4) {
        ""
    } else {
        "D"
    };
    format!("{dynamic}A{source}")
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
