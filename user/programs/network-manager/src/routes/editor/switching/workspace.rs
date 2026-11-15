//! 工作区中的网桥、端口和 VLAN 列表。三个页签从同一份已保存配置投影，修改沿路由配置事务提交。

use super::{clear_port_addresses, edit_bridge, gateway, port};
use crate::{
    child,
    configuration::review_and_save,
    item_list::{Body, Glyph, Item, Summary, Tool, toolbar},
};
use liteos_ui::{
    Application, Background, Column, Dialogs, Event, EventKind, Layout, ListOptions, ListStyle,
    MessageButtons, MessageIcon, MessageResult, Task, Widget, WidgetKind, Window,
};
use network_config::{
    ManagementClient, Revision,
    configuration::{
        Bridge, ConfigurationName as Name, RouterConfiguration, RouterInterface, VlanEgress,
    },
};
use std::{
    borrow::Cow,
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    io,
    rc::Rc,
    sync::Arc,
};

mod vlan;
type Error = Box<dyn std::error::Error>;

#[derive(Clone, Copy)]
enum Tab {
    Bridges,
    Ports,
    Vlans,
}

impl Tab {
    const ALL: [Self; 3] = [Self::Bridges, Self::Ports, Self::Vlans];

    fn layout(self) -> (&'static str, &'static [(&'static str, i32)]) {
        match self {
            Self::Bridges => (
                "网桥",
                &[
                    ("名称", 172),
                    ("协议模式", 105),
                    ("优先级", 94),
                    ("端口数", 82),
                    ("三层网关", 100),
                ],
            ),
            Self::Ports => (
                "端口",
                &[
                    ("接口", 138),
                    ("网桥", 114),
                    ("PVID", 68),
                    ("隔离", 66),
                    ("边缘端口", 92),
                    ("点到点", 82),
                    ("路径代价", 96),
                ],
            ),
            Self::Vlans => (
                "VLAN",
                &[
                    ("网桥", 133),
                    ("VLAN ID", 78),
                    ("带标签成员", 191),
                    ("去标签成员", 191),
                    ("三层网关", 136),
                ],
            ),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum Key {
    Bridge(Name),
    Port(Name, Name),
    Vlan(Name, u16),
}

enum Command {
    Add(Tab),
    Edit(Key),
    Remove(Key),
    Gateway(Name, u16),
}

struct Row {
    key: Key,
    text: String,
}

struct Snapshot {
    revision: Revision,
    rows: [Vec<Row>; 3],
}

fn yes(value: bool) -> &'static str {
    if value { "是" } else { "否" }
}

fn snapshot(client: &mut ManagementClient, domain: Option<&str>) -> io::Result<Snapshot> {
    let (revision, bytes) = client.export()?;
    let config = RouterConfiguration::decode(&bytes).map_err(io::Error::other)?;
    let bridges = if let Some(domain) = domain {
        &config
            .routing_domains
            .get(domain)
            .ok_or_else(missing_domain)?
            .bridges
    } else {
        &config.bridges
    };
    let [mut bridge_rows, mut port_rows, mut vlan_rows] = std::array::from_fn(|_| Vec::new());
    for (name, bridge) in bridges {
        bridge_rows.push(Row {
            key: Key::Bridge(name.clone()),
            text: format!(
                "{}\t{}\t{}\t{}\t{}",
                name.as_str(),
                if bridge.rstp { "RSTP" } else { "无" },
                bridge.priority,
                bridge.ports.len(),
                bridge.gateways.len()
            ),
        });
        let mut members = BTreeMap::<u16, (Vec<&str>, Vec<&str>)>::new();
        for (interface, port) in &bridge.ports {
            port_rows.push(Row {
                key: Key::Port(name.clone(), interface.clone()),
                text: format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    interface.as_str(),
                    name.as_str(),
                    port.pvid.map_or_else(String::new, |vid| vid.to_string()),
                    yes(port.isolated),
                    yes(port.edge),
                    yes(port.point_to_point),
                    port.path_cost
                ),
            });
            for (&vid, mode) in &port.vlans {
                let (tagged, untagged) = members.entry(vid).or_default();
                match mode {
                    VlanEgress::Tagged => tagged.push(interface.as_str()),
                    VlanEgress::Untagged => untagged.push(interface.as_str()),
                }
            }
        }
        for (vid, (tagged, untagged)) in members {
            let gateway = bridge
                .gateways
                .get(&vid)
                .map_or("", |gateway| gateway.name.as_str());
            vlan_rows.push(Row {
                key: Key::Vlan(name.clone(), vid),
                text: format!(
                    "{}\t{vid}\t{}\t{}\t{gateway}",
                    name.as_str(),
                    tagged.join(", "),
                    untagged.join(", ")
                ),
            });
        }
    }
    Ok(Snapshot {
        revision,
        rows: [bridge_rows, port_rows, vlan_rows],
    })
}

struct Table {
    _body: Body,
    list: Widget,
    add: Tool,
    remove: Tool,
    refresh: Tool,
    gateway: Option<crate::item_list::Action>,
    summary: Summary,
}

impl Table {
    fn new(tabs: &Widget, tab: Tab) -> liteos_ui::Result<Self> {
        let (title, columns) = tab.layout();
        let parent = child(
            tabs,
            WidgetKind::Column,
            title,
            Layout {
                grow: 1,
                padding: 4,
                gap: 3,
                ..Layout::default()
            },
        )?;
        let [add, remove, _, refresh] = toolbar(
            &parent,
            [
                Item::Command(Glyph::Add, "新建"),
                Item::Command(Glyph::Remove, "删除"),
                Item::Separator,
                Item::Icon(Glyph::Refresh, "刷新"),
            ],
        )?;
        let body = Body::new(&parent)?;
        let list = body.list.clone();
        list.set_list_style(
            ListStyle::Details,
            ListOptions {
                full_row_select: true,
                ..Default::default()
            },
        )?;
        let columns = columns
            .iter()
            .enumerate()
            .map(|(index, &(title, width))| Column {
                id: index as u64 + 1,
                title,
                width,
            })
            .collect::<Vec<_>>();
        list.set_columns(&columns)?;
        let gateway = matches!(tab, Tab::Vlans)
            .then(|| body.action("三层网关…"))
            .transpose()?;
        let summary = Summary::new(&parent)?;
        let table = Self {
            _body: body,
            list,
            add,
            remove,
            refresh,
            gateway,
            summary,
        };
        table.selection_changed(0, false)?;
        Ok(table)
    }

    fn selection_changed(&self, count: usize, available: bool) -> liteos_ui::Result<()> {
        let selected = available && self.list.selection()?.is_some();
        self.add.set_enabled(available)?;
        self.remove.set_enabled(selected)?;
        if let Some(gateway) = &self.gateway {
            gateway.set_enabled(selected)?;
        }
        self.summary
            .show(count, usize::from(selected), "已保存配置")
    }
}

struct View {
    _tabs: Widget,
    tables: [Table; 3],
    snapshot: RefCell<Option<Snapshot>>,
    background: Background,
    dialogs: Dialogs,
}

impl View {
    fn clear(&self, note: &str) -> liteos_ui::Result<()> {
        self.snapshot.take();
        for table in &self.tables {
            table.list.set_rows(&[])?;
            table.selection_changed(0, false)?;
            table.summary.notice(note)?;
        }
        Ok(())
    }

    fn present(&self, next: Snapshot) -> liteos_ui::Result<()> {
        let previous = self.snapshot.replace(Some(next));
        let current = self.snapshot.borrow();
        let current = current.as_ref().expect("installed snapshot");
        for (i, table) in self.tables.iter().enumerate() {
            let selected = table.list.selection()?;
            let key = previous
                .as_ref()
                .and_then(|prev| prev.rows[i].get(selected? as usize - 1).map(|row| &row.key));
            let rows = &current.rows[i];
            let list = rows
                .iter()
                .enumerate()
                .map(|(i, row)| (i as u64 + 1, row.text.as_str()))
                .collect::<Vec<_>>();
            let selection = key.and_then(|key| rows.iter().position(|row| &row.key == key));
            table.list.set_rows(&list)?;
            table.list.select(selection.map(|i| i as u64 + 1))?;
            table.selection_changed(rows.len(), true)?;
        }
        Ok(())
    }

    async fn load(&self, domain: Option<Arc<str>>) -> Result<(), Error> {
        let current = self
            .background
            .run(move || snapshot(&mut ManagementClient::local(), domain.as_deref()))?
            .await?;
        Ok(self.present(current)?)
    }

    fn request(&self, event: &Event) -> liteos_ui::Result<Option<(usize, Command, Revision)>> {
        let current = self.snapshot.borrow();
        let Some(current) = current.as_ref() else {
            return Ok(None);
        };
        for (i, table) in self.tables.iter().enumerate() {
            if table.add.activated(event) {
                return Ok(Some((i, Command::Add(Tab::ALL[i]), current.revision)));
            }
            let Some(row) = table
                .list
                .selection()?
                .and_then(|id| current.rows[i].get(id as usize - 1))
            else {
                continue;
            };
            let command = if table.remove.activated(event) {
                Command::Remove(row.key.clone())
            } else if event.node == table.list.id()
                && matches!(event.kind, EventKind::Activate { .. })
            {
                Command::Edit(row.key.clone())
            } else if table
                .gateway
                .as_ref()
                .is_some_and(|action| action.activated(event))
            {
                let Key::Vlan(bridge, vid) = &row.key else {
                    continue;
                };
                Command::Gateway(bridge.clone(), *vid)
            } else {
                continue;
            };
            return Ok(Some((i, command, current.revision)));
        }
        Ok(None)
    }
}

pub(crate) struct Page {
    view: Rc<View>,
    task: Option<Task<()>>,
    domain: Option<Arc<str>>,
    visible: bool,
}

impl Page {
    pub(crate) fn new(
        app: &Application,
        parent: &Widget,
        background: Background,
    ) -> liteos_ui::Result<Self> {
        let tabs = child(
            parent,
            WidgetKind::Tabs,
            "",
            Layout {
                grow: 1,
                ..Layout::default()
            },
        )?;
        let tables = [
            Table::new(&tabs, Tab::Bridges)?,
            Table::new(&tabs, Tab::Ports)?,
            Table::new(&tabs, Tab::Vlans)?,
        ];
        Ok(Self {
            view: Rc::new(View {
                _tabs: tabs,
                tables,
                snapshot: RefCell::default(),
                background,
                dialogs: app.dialogs(),
            }),
            task: None,
            domain: None,
            visible: false,
        })
    }

    pub(crate) fn selected_tab(
        &mut self,
        app: &mut Application,
        window: &Window,
        visible: bool,
    ) -> liteos_ui::Result<()> {
        if self.visible != visible {
            self.visible = visible;
            if visible {
                self.reload(app, window)?;
            } else {
                self.task = None;
            }
        }
        Ok(())
    }

    pub(crate) fn select_domain(
        &mut self,
        app: &mut Application,
        window: &Window,
        domain: Option<Arc<str>>,
    ) -> liteos_ui::Result<()> {
        self.domain = domain;
        if self.visible {
            self.reload(app, window)?;
        }
        Ok(())
    }

    fn reload(&mut self, app: &mut Application, window: &Window) -> liteos_ui::Result<()> {
        self.view.clear("正在读取配置…")?;
        let view = self.view.clone();
        let domain = self.domain.clone();
        self.task = Some(app.spawn_local(window, async move {
            if let Err(error) = view.load(domain).await {
                let _ = view.clear(&format!("无法读取网桥配置。{error}"));
            }
        })?);
        Ok(())
    }

    pub(crate) fn handle(
        &mut self,
        app: &mut Application,
        window: &Window,
        event: &Event,
    ) -> liteos_ui::Result<()> {
        if !self.visible || self.task.as_ref().is_some_and(|task| !task.is_finished()) {
            return Ok(());
        }
        for (i, table) in self.view.tables.iter().enumerate() {
            if event.node == table.list.id()
                && matches!(event.kind, EventKind::Select { .. })
                && let Some(current) = self.view.snapshot.borrow().as_ref()
            {
                table.selection_changed(current.rows[i].len(), true)?;
            }
            if table.refresh.activated(event) {
                return self.reload(app, window);
            }
        }
        let Some((tab, command, revision)) = self.view.request(event)? else {
            return Ok(());
        };
        let parent = window.clone();
        let view = self.view.clone();
        let domain = self.domain.clone();
        self.task = Some(app.spawn_local(window, async move {
            let result = mutate(&view, &parent, domain.clone(), command, revision).await;
            // 即使打开属性后发现配置已过期，也刷新表格，让用户可重新选择。
            if let Err(error) = view.load(domain).await {
                let _ = view.clear(&format!("无法刷新网桥配置。{error}"));
            }
            match result {
                Ok(Some(message)) => {
                    let _ = view.tables[tab].summary.notice(&message);
                }
                Ok(None) => {}
                Err(error) => {
                    if let Ok(message) = view.dialogs.message(
                        &parent,
                        "网桥",
                        &error.to_string(),
                        MessageButtons::Ok,
                        MessageIcon::Error,
                    ) {
                        let _ = message.await;
                    }
                }
            }
        })?);
        Ok(())
    }
}

fn missing_domain() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "所选路由域已删除。")
}

/// 冻结发起操作时的代际与域；同一链路地址和拓扑只在已确认的草稿中更新。
async fn mutate(
    view: &View,
    parent: &Window,
    domain: Option<Arc<str>>,
    command: Command,
    expected: Revision,
) -> Result<Option<String>, Error> {
    let dialogs = &view.dialogs;
    let discover_ports = domain.is_none()
        && matches!(
            command,
            Command::Add(Tab::Bridges | Tab::Ports)
                | Command::Edit(Key::Bridge(_) | Key::Port(_, _))
        );
    let (mut config, devices) = view
        .background
        .run(move || {
            let mut client = ManagementClient::local();
            let (revision, bytes) = client.export()?;
            if revision != expected {
                return Err(io::Error::other("配置已变化，请在刷新后重新选择。"));
            }
            let config = RouterConfiguration::decode(&bytes).map_err(io::Error::other)?;
            let devices = if discover_ports {
                let snapshot = client.snapshot()?;
                if snapshot.generation != revision {
                    return Err(io::Error::other("接口已变化，请在刷新后重新选择。"));
                }
                snapshot.interfaces
            } else {
                Vec::new()
            };
            Ok((config, devices))
        })?
        .await?;
    // 默认域可直接选当前设备。临时展现的接口配置只有在成为成员后才写入草稿。
    let original = config.interfaces.keys().cloned().collect::<BTreeSet<_>>();
    for device in devices {
        if config.routing_domain_for_mac(device.mac).is_none()
            && config
                .matching(&device.name, device.mac)
                .is_ok_and(|profile| profile.is_none())
        {
            let _ = config.ensure_interface(&device.name, Some(device.mac));
        }
    }
    let (interfaces, bridges) = if let Some(scope) = domain.as_deref() {
        let scope = config
            .routing_domains
            .get_mut(scope)
            .ok_or_else(missing_domain)?;
        (&mut scope.interfaces, &mut scope.bridges)
    } else {
        (&mut config.interfaces, &mut config.bridges)
    };
    let changed = match command {
        Command::Add(Tab::Bridges) => {
            bridge_property(dialogs, parent, interfaces, bridges, None).await?
        }
        Command::Edit(Key::Bridge(key)) => {
            bridge_property(dialogs, parent, interfaces, bridges, Some(key)).await?
        }
        Command::Add(Tab::Ports) => {
            let Some(bridge) = vlan::choose_bridge(dialogs, parent, bridges).await? else {
                return Ok(None);
            };
            port_property(dialogs, parent, interfaces, bridges, &bridge, None).await?
        }
        Command::Edit(Key::Port(bridge, port)) => {
            port_property(dialogs, parent, interfaces, bridges, &bridge, Some(port)).await?
        }
        Command::Add(Tab::Vlans) => {
            let Some(bridge) = vlan::choose_bridge(dialogs, parent, bridges).await? else {
                return Ok(None);
            };
            vlan::edit(
                dialogs,
                parent,
                bridges.get_mut(&bridge).expect("chosen bridge"),
                None,
            )
            .await?
        }
        Command::Edit(Key::Vlan(bridge, vid)) => {
            vlan::edit(
                dialogs,
                parent,
                bridges.get_mut(&bridge).expect("visible bridge"),
                Some(vid),
            )
            .await?
        }
        Command::Remove(key) => remove(dialogs, parent, bridges, key).await?,
        Command::Gateway(bridge, vid) => {
            vlan::gateway(dialogs, parent, interfaces, bridges, &bridge, vid).await?
        }
    };
    if !changed {
        return Ok(None);
    }
    clear_port_addresses(interfaces, bridges);
    if discover_ports {
        let members = bridges
            .values()
            .flat_map(|bridge| bridge.ports.keys())
            .collect::<BTreeSet<_>>();
        interfaces.retain(|name, _| original.contains(name) || members.contains(name));
    }
    let candidate = view
        .background
        .run(move || config.encode().map_err(io::Error::other))?
        .await?;
    let result = review_and_save(
        dialogs,
        parent,
        &view.background,
        None,
        candidate,
        Some(expected),
    )
    .await?;
    Ok(Some(result.message))
}

async fn bridge_property(
    dialogs: &Dialogs,
    parent: &Window,
    interfaces: &BTreeMap<Name, RouterInterface>,
    bridges: &mut BTreeMap<Name, Bridge>,
    current: Option<Name>,
) -> Result<bool, Error> {
    let list = bridges
        .iter()
        .map(|(name, bridge)| (name.clone(), Cow::Borrowed(bridge)))
        .collect::<Vec<_>>();
    let index = current
        .as_ref()
        .and_then(|current| list.iter().position(|(name, _)| name == current));
    let Some((name, bridge)) = edit_bridge(dialogs, parent, interfaces, &list, index).await? else {
        return Ok(false);
    };
    if let Some(old) = current {
        bridges.remove(&old);
    }
    bridges.insert(name, bridge);
    Ok(true)
}

async fn port_property(
    dialogs: &Dialogs,
    parent: &Window,
    interfaces: &BTreeMap<Name, RouterInterface>,
    bridges: &mut BTreeMap<Name, Bridge>,
    bridge: &Name,
    selected: Option<Name>,
) -> Result<bool, Error> {
    let occupied = bridges
        .values()
        .flat_map(|bridge| bridge.ports.keys())
        .collect::<BTreeSet<_>>();
    let choices = interfaces
        .iter()
        .filter(|(name, _)| !occupied.contains(name) || selected.as_ref() == Some(*name))
        .collect::<Vec<_>>();
    let current = selected.map(|name| {
        let port = bridges[bridge].ports[&name].clone();
        (name, port)
    });
    let Some((name, port)) = port::edit_port(dialogs, parent, &choices, current.as_ref()).await?
    else {
        return Ok(false);
    };
    let ports = &mut bridges.get_mut(bridge).expect("visible bridge").ports;
    if let Some((old, _)) = current {
        ports.remove(&old);
    }
    ports.insert(name, port);
    Ok(true)
}

async fn remove(
    dialogs: &Dialogs,
    parent: &Window,
    bridges: &mut BTreeMap<Name, Bridge>,
    key: Key,
) -> Result<bool, Error> {
    let name = match &key {
        Key::Bridge(bridge) => format!("网桥 {} 及其成员和网关", bridge.as_str()),
        Key::Port(bridge, port) => format!("网桥 {} 的端口 {}", bridge.as_str(), port.as_str()),
        Key::Vlan(bridge, vid) => format!("网桥 {} 的 VLAN {vid}", bridge.as_str()),
    };
    if dialogs
        .message(
            parent,
            "删除",
            &format!("删除{name}？"),
            MessageButtons::YesNo,
            MessageIcon::Warning,
        )?
        .await?
        != MessageResult::Yes
    {
        return Ok(false);
    }
    match key {
        Key::Bridge(bridge) => {
            bridges.remove(&bridge);
        }
        Key::Port(bridge, port) => {
            bridges
                .get_mut(&bridge)
                .expect("visible bridge")
                .ports
                .remove(&port);
        }
        Key::Vlan(bridge, vid) => {
            vlan::remove(bridges.get_mut(&bridge).expect("visible bridge"), vid)?;
        }
    }
    Ok(true)
}
