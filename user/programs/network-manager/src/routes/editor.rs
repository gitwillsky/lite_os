//! 编辑各路由域的设置。所有域的修改共用一份可审阅的草稿。

use super::{Error, TABLE_MAIN, form, prefix};
use crate::{child, configuration::review_and_save};
use liteos_ui::{
    Background, Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, MessageButtons,
    MessageIcon, MessageResult, TreeRow, Widget, WidgetKind, Window, state,
};
use network_config::{
    ManagementClient,
    configuration::{
        ConfigurationName, Firewall, PolicyRouting, RouterConfiguration, StaticAction, StaticRoute,
        StaticTable, bgp as bgp_config, forwarding_cpu_mask,
    },
};
use std::{io, mem};

mod bgp;
mod entries;
mod firewall;
mod interfaces;
mod ipv6;
mod ospf;
mod policy;
mod route;
mod routing_domains;
mod services;
mod switching;

const ROWS: usize = 64;

pub(super) async fn run(
    dialogs: &Dialogs,
    parent: &Window,
    background: &Background,
    domain: Option<&str>,
) -> Result<Option<String>, Error> {
    let (revision, mut config) = background
        .run(|| {
            let (revision, document) = ManagementClient::local().export()?;
            let config = RouterConfiguration::decode(&document).map_err(io::Error::other)?;
            Ok::<_, io::Error>((revision, config))
        })?
        .await?;
    let mut selected_domain = domain
        .map(|name| {
            config
                .routing_domains
                .get_key_value(name)
                .map(|(name, _)| name.clone())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "所选路由域已删除，请选择其他域。")
                })
        })
        .transpose()?;
    let mut interfaces = interface_names(&config, selected_domain.as_ref());
    let (window, mut events, root) = form(dialogs, parent, "路由设置", 735, 496)?;
    let scopes = child(&root, WidgetKind::Row, "", row())?;
    let _scope_label = child(&scopes, WidgetKind::Label, "路由域", Layout::default())?;
    let domain = choice(&scopes, std::iter::empty(), 220)?;
    routing_domains::set_choices(&domain, &config, selected_domain.as_ref())?;
    let create_domain = child(&scopes, WidgetKind::Button, "新建域…", button())?;
    let delete_domain = child(&scopes, WidgetKind::Button, "删除域…", button())?;
    let assign = child(
        &scopes,
        WidgetKind::Button,
        "分配网卡…",
        Layout {
            width: 104,
            ..button()
        },
    )?;
    let forwarding_cpu = child(
        &scopes,
        WidgetKind::Button,
        "转发专用 CPU…",
        Layout {
            width: 122,
            ..button()
        },
    )?;
    let network = child(&root, WidgetKind::Row, "", row())?;
    let edit_interfaces = child(&network, WidgetKind::Button, "路由接口…", button())?;
    let address_services = child(
        &network,
        WidgetKind::Button,
        "地址分发与 DNS…",
        Layout {
            width: 130,
            ..button()
        },
    )?;
    let bridges = child(
        &network,
        WidgetKind::Button,
        "网桥与 VLAN…",
        Layout {
            width: 117,
            ..button()
        },
    )?;
    let filtering = child(
        &network,
        WidgetKind::Button,
        "防火墙与 NAT…",
        Layout {
            width: 124,
            ..button()
        },
    )?;
    let ospf = child(&network, WidgetKind::Button, "OSPF…", button())?;
    let bgp = child(&network, WidgetKind::Button, "BGP…", button())?;
    let selectors = child(&root, WidgetKind::Row, "", row())?;
    let _table_label = child(&selectors, WidgetKind::Label, "路由表", Layout::default())?;
    let table = choice(&selectors, std::iter::empty(), 148)?;
    let draft = Draft::new(&mut config, selected_domain.as_ref());
    set_tables(&table, &draft.policy.tables, 0)?;
    let create_table = child(&selectors, WidgetKind::Button, "新建表…", button())?;
    let delete_table = child(&selectors, WidgetKind::Button, "删除表", button())?;
    let rules = child(&selectors, WidgetKind::Button, "选路规则…", button())?;
    let query = child(&selectors, WidgetKind::Edit, "", grow())?;
    let find = child(&selectors, WidgetKind::Button, "查找网段", button())?;
    let list = child(&root, WidgetKind::List, "", grow())?;
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
            width: 224,
        },
        Column {
            id: 2,
            title: "优先级",
            width: 80,
        },
        Column {
            id: 3,
            title: "度量",
            width: 80,
        },
        Column {
            id: 4,
            title: "动作",
            width: 115,
        },
        Column {
            id: 5,
            title: "下一跳数",
            width: 100,
        },
    ])?;
    let bar = child(&root, WidgetKind::Row, "", row())?;
    let previous = child(&bar, WidgetKind::Button, "上一页", button())?;
    let next = child(&bar, WidgetKind::Button, "下一页", button())?;
    let add = child(&bar, WidgetKind::Button, "新增…", button())?;
    let edit = child(&bar, WidgetKind::Button, "编辑…", button())?;
    let remove = child(&bar, WidgetKind::Button, "删除", button())?;
    let _spacer = child(&bar, WidgetKind::Column, "", grow())?;
    let review = child(&bar, WidgetKind::Button, "审阅…", button())?;
    let cancel = child(&bar, WidgetKind::Button, "取消", button())?;
    let summary = child(&root, WidgetKind::Label, "", Layout::default())?;
    let status = child(&root, WidgetKind::Status, "", Layout::default())?;
    let mut dirty = false;
    let mut selected_table = 0;
    let mut first = 0;
    let mut selected = None;

    loop {
        let current = Draft::new(&mut config, selected_domain.as_ref()).routes(selected_table);
        let count = current.len();
        if first >= count {
            first = count.saturating_sub(1) / ROWS * ROWS;
        }
        let end = (first + ROWS).min(count);
        selected = selected.filter(|index| (first..end).contains(index));
        let text: Vec<_> = current[first..end].iter().map(route_text).collect();
        let rows: Vec<_> = (first..end)
            .zip(&text)
            .map(|(index, text)| (index as u64 + 1, text.as_str()))
            .collect();
        list.set_rows(&rows)?;
        list.select(selected.map(|index| index as u64 + 1))?;
        previous.set_state(disabled(first == 0))?;
        next.set_state(disabled(end == count))?;
        edit.set_state(disabled(selected.is_none()))?;
        remove.set_state(disabled(selected.is_none()))?;
        delete_table.set_state(disabled(selected_table == 0))?;
        delete_domain.set_state(disabled(selected_domain.is_none()))?;
        address_services.set_state(disabled(selected_domain.is_some()))?;
        bridges.set_state(disabled(selected_domain.is_some()))?;
        review.set_state(disabled(!dirty))?;
        summary.set_text(&format!(
            "显示 {}–{} 条，共 {} 条{}",
            if count == 0 { 0 } else { first + 1 },
            end,
            count,
            if dirty { " · 有未提交改动" } else { "" }
        ))?;

        loop {
            let Some(event) = events.next().await? else {
                return Ok(None);
            };
            let closing = matches!(
                event.kind,
                EventKind::Close
                    | EventKind::Key {
                        code: 1,
                        pressed: true,
                        ..
                    }
            ) || (event.node == cancel.id()
                && matches!(event.kind, EventKind::Activate { .. }));
            if closing {
                if !dirty
                    || dialogs
                        .message(
                            &window,
                            "路由设置",
                            "是否放弃尚未提交的路由修改？",
                            MessageButtons::YesNo,
                            MessageIcon::Warning,
                        )?
                        .await?
                        == MessageResult::Yes
                {
                    return Ok(None);
                }
                continue;
            }
            if matches!(event.kind, EventKind::Select { .. }) {
                if event.node == list.id() {
                    selected = selection(&list)?;
                    edit.set_state(disabled(selected.is_none()))?;
                    remove.set_state(disabled(selected.is_none()))?;
                } else if event.node == domain.id() {
                    let chosen = selection(&domain)?
                        .and_then(|index| index.checked_sub(1))
                        .and_then(|index| config.routing_domains.keys().nth(index))
                        .cloned();
                    if chosen != selected_domain {
                        selected_domain = chosen;
                        interfaces = interface_names(&config, selected_domain.as_ref());
                        selected_table = 0;
                        let draft = Draft::new(&mut config, selected_domain.as_ref());
                        set_tables(&table, &draft.policy.tables, 0)?;
                        first = 0;
                        selected = None;
                        status.set_text("")?;
                        break;
                    }
                } else if event.node == table.id() {
                    let chosen = selection(&table)?.unwrap_or(0);
                    if chosen != selected_table {
                        selected_table = chosen;
                        first = 0;
                        selected = None;
                        break;
                    }
                }
                continue;
            }
            if !matches!(event.kind, EventKind::Activate { .. }) {
                continue;
            }
            if event.node == create_domain.id() {
                let Some(name) = routing_domains::create(dialogs, &window, &config).await? else {
                    continue;
                };
                config
                    .routing_domains
                    .insert(name.clone(), Default::default());
                selected_domain = Some(name);
            } else if event.node == delete_domain.id()
                && let Some(name) = &selected_domain
            {
                if !routing_domains::confirm_delete(
                    dialogs,
                    &window,
                    name,
                    &config.routing_domains[name],
                )
                .await?
                {
                    continue;
                }
                let removed = config.routing_domains.remove(name);
                // 整域删除可能释放大量路由和规则，沿用路由表删除的后台释放方式。
                background.run(move || drop(removed))?.await;
                selected_domain = None;
            } else if event.node == assign.id() {
                match background
                    .run(|| ManagementClient::local().snapshot())?
                    .await
                {
                    Ok(snapshot) => {
                        if routing_domains::assign(
                            dialogs,
                            &window,
                            &snapshot.interfaces,
                            &mut config,
                            selected_domain.as_ref(),
                        )
                        .await?
                        {
                            interfaces = interface_names(&config, selected_domain.as_ref());
                            dirty = true;
                            status.set_text("请检查原路由域中引用此接口的设置，再审阅提交。")?;
                        }
                    }
                    Err(error) => status.set_text(&format!("无法读取当前接口。{error}"))?,
                }
                break;
            } else if event.node == forwarding_cpu.id() {
                if let Some(cpus) =
                    edit_forwarding_cpus(dialogs, &window, &config.forwarding_cpus).await?
                    && config.forwarding_cpus != cpus
                {
                    config.forwarding_cpus = cpus;
                    dirty = true;
                    status.set_text("")?;
                }
            } else if event.node == create_table.id() {
                let policy = Draft::new(&mut config, selected_domain.as_ref()).policy;
                if let Some(id) = policy::table_id(dialogs, &window, &policy.tables).await? {
                    policy.tables.push(StaticTable {
                        id,
                        routes: Vec::new(),
                    });
                    selected_table = policy.tables.len();
                    set_tables(&table, &policy.tables, selected_table)?;
                    first = 0;
                    selected = None;
                    dirty = true;
                    status.set_text("")?;
                }
            } else if event.node == delete_table.id() && selected_table != 0 {
                let policy = Draft::new(&mut config, selected_domain.as_ref()).policy;
                let id = policy.tables[selected_table - 1].id;
                if policy.rules.iter().any(|rule| rule.table == id) {
                    status.set_text("仍有选路规则使用这张表，请先修改或删除相关规则。")?;
                    continue;
                }
                let count = policy.tables[selected_table - 1].routes.len();
                let message = format!("删除路由表 {id} 及其中的 {count} 条静态路由？");
                if dialogs
                    .message(
                        &window,
                        "删除路由表",
                        &message,
                        MessageButtons::YesNo,
                        MessageIcon::Warning,
                    )?
                    .await?
                    == MessageResult::Yes
                {
                    let removed = policy.tables.remove(selected_table - 1);
                    // 大表的逐条释放留在后台，避免阻塞同一窗口的事件循环。
                    background.run(move || drop(removed))?.await;
                    selected_table = 0;
                    set_tables(&table, &policy.tables, selected_table)?;
                    first = 0;
                    selected = None;
                    dirty = true;
                    status.set_text("")?;
                }
            } else if event.node == edit_interfaces.id() {
                let changed = if let Some(name) = &selected_domain {
                    let domain = config
                        .routing_domains
                        .get_mut(name)
                        .expect("selected domain");
                    interfaces::edit(dialogs, &window, &[], interfaces::Draft::Named(domain))
                        .await?
                } else {
                    let snapshot = match background
                        .run(|| ManagementClient::local().snapshot())?
                        .await
                    {
                        Ok(snapshot) => snapshot,
                        Err(error) => {
                            status.set_text(&format!("无法读取当前接口。{error}"))?;
                            continue;
                        }
                    };
                    let mut devices = snapshot.interfaces;
                    // 草稿中的归属可能已变更，而运行状态尚未随提交更新。
                    devices.retain(|device| config.routing_domain_for_mac(device.mac).is_none());
                    interfaces::edit(
                        dialogs,
                        &window,
                        &devices,
                        interfaces::Draft::Default(&mut config),
                    )
                    .await?
                };
                if changed {
                    interfaces = interface_names(&config, selected_domain.as_ref());
                    dirty = true;
                    status.set_text("")?;
                }
            } else if event.node == address_services.id() && selected_domain.is_none() {
                if services::edit(dialogs, &window, &mut config).await? {
                    dirty = true;
                    status.set_text("")?;
                }
            } else if event.node == bridges.id() && selected_domain.is_none() {
                if switching::edit(dialogs, &window, &mut config).await? {
                    interfaces = interface_names(&config, None);
                    dirty = true;
                    status.set_text("")?;
                }
            } else if event.node == filtering.id() {
                let draft = Draft::new(&mut config, selected_domain.as_ref());
                if firewall::edit(dialogs, &window, &interfaces, draft.firewall).await? {
                    dirty = true;
                    status.set_text("")?;
                }
            } else if event.node == ospf.id() {
                let (configuration, links) = if let Some(name) = &selected_domain {
                    let domain = config
                        .routing_domains
                        .get_mut(name)
                        .expect("selected domain");
                    (&mut domain.ospf, interfaces.iter().collect::<Vec<_>>())
                } else {
                    let links = interfaces
                        .iter()
                        .filter(|name| {
                            !config
                                .bridges
                                .values()
                                .any(|bridge| bridge.ports.contains_key(*name))
                        })
                        .collect();
                    (&mut config.ospf, links)
                };
                if ospf::edit(dialogs, &window, configuration, &links).await? {
                    dirty = true;
                    status.set_text("")?;
                }
            } else if event.node == bgp.id() {
                let draft = Draft::new(&mut config, selected_domain.as_ref());
                if bgp::edit(dialogs, &window, draft.bgp).await? {
                    dirty = true;
                    status.set_text("")?;
                }
            } else if event.node == rules.id() {
                let draft = Draft::new(&mut config, selected_domain.as_ref());
                if policy::edit_rules(dialogs, &window, &interfaces, draft.policy).await? {
                    dirty = true;
                    status.set_text("")?;
                }
            } else if event.node == previous.id() {
                first = first.saturating_sub(ROWS);
            } else if event.node == next.id() && end < count {
                first = end;
            } else if event.node == find.id() {
                let Some(prefix) = prefix(&query.text()?) else {
                    status.set_text("请输入规范网段，例如 192.0.2.0/24 或 2001:db8::/32。")?;
                    query.focus()?;
                    continue;
                };
                let after = selected.map_or(0, |index| index + 1);
                let routes =
                    Draft::new(&mut config, selected_domain.as_ref()).routes(selected_table);
                let list = mem::take(routes);
                let (list, found) = background
                    .run(move || {
                        let found = (after..list.len()).chain(0..after).find(|&index| {
                            let route = &list[index];
                            route.destination == prefix.network
                                && route.prefix_length == prefix.length
                        });
                        (list, found)
                    })?
                    .await;
                *routes = list;
                let Some(index) = found else {
                    status.set_text("未找到该网段。")?;
                    continue;
                };
                selected = Some(index);
                first = index;
                status.set_text("")?;
            } else if event.node == add.id() {
                if let Some(value) = route::edit(dialogs, &window, &interfaces, None).await? {
                    let routes =
                        Draft::new(&mut config, selected_domain.as_ref()).routes(selected_table);
                    selected = Some(routes.len());
                    first = routes.len();
                    routes.push(value);
                    dirty = true;
                }
            } else if event.node == edit.id() {
                let routes =
                    Draft::new(&mut config, selected_domain.as_ref()).routes(selected_table);
                if let Some(index) = selected
                    && let Some(value) =
                        route::edit(dialogs, &window, &interfaces, routes.get(index)).await?
                    && routes[index] != value
                {
                    routes[index] = value;
                    dirty = true;
                }
            } else if event.node == remove.id() {
                if let Some(index) = selected {
                    Draft::new(&mut config, selected_domain.as_ref())
                        .routes(selected_table)
                        .remove(index);
                    selected = None;
                    dirty = true;
                }
            } else if event.node == review.id() && dirty {
                // 移交模型给后台编码，避免为百万条路由保留一份克隆。取消审阅时继续使用原草稿。
                let model = mem::take(&mut config);
                let (model, candidate) = background
                    .run(move || {
                        let candidate = model.encode();
                        (model, candidate)
                    })?
                    .await;
                config = model;
                let result = match candidate {
                    Ok(candidate) => review_and_save(
                        dialogs,
                        &window,
                        background,
                        &status,
                        candidate,
                        Some(revision),
                    )
                    .await
                    .map(|result| (result.saved, result.message)),
                    Err(error) => Err(io::Error::other(error).into()),
                };
                match result {
                    Ok((true, message)) => return Ok(Some(message)),
                    Ok((false, message)) => {
                        status.set_text(&message)?;
                    }
                    Err(error) => {
                        status.set_text(&error.to_string())?;
                    }
                }
                // 审阅后重取同一表的借用，保留结果直到下一次操作。
                break;
            } else {
                continue;
            }
            if event.node == create_domain.id() || event.node == delete_domain.id() {
                routing_domains::set_choices(&domain, &config, selected_domain.as_ref())?;
                interfaces = interface_names(&config, selected_domain.as_ref());
                selected_table = 0;
                let draft = Draft::new(&mut config, selected_domain.as_ref());
                set_tables(&table, &draft.policy.tables, 0)?;
                first = 0;
                selected = None;
                dirty = true;
                status.set_text("")?;
            }
            break;
        }
    }
}

async fn edit_forwarding_cpus(
    dialogs: &Dialogs,
    parent: &Window,
    cpus: &[u32],
) -> liteos_ui::Result<Option<Vec<u32>>> {
    let (_window, mut events, root) = form(dialogs, parent, "转发专用 CPU", 478, 180)?;
    let current = cpus
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let (_, cpu) = field(&root, "逻辑 CPU", &current)?;
    let status = child(
        &root,
        WidgetKind::Label,
        "用逗号分隔 CPU 编号。留空使用共享 CPU。",
        Layout::default(),
    )?;
    let (save, cancel) = form_buttons(&root)?;
    cpu.focus()?;
    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => break,
            EventKind::Activate { .. } if event.node == cancel.id() => break,
            EventKind::Activate { .. } if event.node == save.id() => {
                let text = cpu.text()?;
                if text.trim().is_empty() {
                    return Ok(Some(Vec::new()));
                }
                let cpus = text
                    .split([',', '，'])
                    .map(|id| id.trim().parse::<u32>())
                    .collect::<Result<Vec<_>, _>>();
                if let Ok(mut cpus) = cpus {
                    cpus.sort_unstable();
                    if forwarding_cpu_mask(&cpus).is_some() {
                        return Ok(Some(cpus));
                    }
                }
                status.set_text("请输入互不重复的 0–63 编号，用逗号分隔。")?;
                cpu.focus()?;
            }
            _ => {}
        }
    }
    Ok(None)
}

fn interface_names(
    config: &RouterConfiguration,
    domain: Option<&ConfigurationName>,
) -> Vec<ConfigurationName> {
    if let Some(name) = domain {
        return config.routing_domains[name]
            .interfaces
            .keys()
            .cloned()
            .collect();
    }
    config
        .interfaces
        .keys()
        .chain(
            config
                .bridges
                .values()
                .flat_map(|bridge| bridge.gateways.values().map(|gateway| &gateway.name)),
        )
        .cloned()
        .collect()
}

struct Draft<'a> {
    main_routes: &'a mut Vec<StaticRoute>,
    policy: &'a mut PolicyRouting,
    firewall: &'a mut Firewall,
    bgp: &'a mut Option<bgp_config::Configuration>,
}

impl<'a> Draft<'a> {
    fn new(config: &'a mut RouterConfiguration, selected: Option<&ConfigurationName>) -> Self {
        if let Some(name) = selected {
            let domain = config
                .routing_domains
                .get_mut(name)
                .expect("selected domain");
            Self {
                main_routes: &mut domain.routes,
                policy: &mut domain.policy_routing,
                firewall: &mut domain.firewall,
                bgp: &mut domain.bgp,
            }
        } else {
            Self {
                main_routes: &mut config.routes,
                policy: &mut config.policy_routing,
                firewall: &mut config.firewall,
                bgp: &mut config.bgp,
            }
        }
    }

    fn routes(self, table: usize) -> &'a mut Vec<StaticRoute> {
        if table == 0 {
            self.main_routes
        } else {
            &mut self.policy.tables[table - 1].routes
        }
    }
}

fn set_tables(widget: &Widget, tables: &[StaticTable], active: usize) -> liteos_ui::Result<()> {
    let main = format!("主路由表 ({TABLE_MAIN})");
    let labels: Vec<_> = tables
        .iter()
        .map(|table| format!("路由表 {}", table.id))
        .collect();
    set_choices(
        widget,
        std::iter::once(main.as_str()).chain(labels.iter().map(String::as_str)),
    )?;
    widget.select(Some(active as u64 + 1))
}

fn route_text(route: &StaticRoute) -> String {
    let (action, count) = match &route.action {
        StaticAction::Forward { members } => ("转发", members.len()),
        StaticAction::Discard => ("丢弃", 0),
        StaticAction::Unreachable => ("不可达", 0),
        StaticAction::Prohibit => ("禁止", 0),
    };
    format!(
        "{}/{}\t{}\t{}\t{action}\t{count}",
        route.destination, route.prefix_length, route.preference, route.metric
    )
}

fn choice<'a>(
    parent: &Widget,
    labels: impl IntoIterator<Item = &'a str>,
    width: i32,
) -> liteos_ui::Result<Widget> {
    let widget = child(
        parent,
        WidgetKind::Combo,
        "",
        Layout {
            width,
            height: 23,
            ..Layout::default()
        },
    )?;
    widget.set_state(state::DROPDOWN_LIST)?;
    set_choices(&widget, labels)?;
    Ok(widget)
}

fn set_choices<'a>(
    widget: &Widget,
    labels: impl IntoIterator<Item = &'a str>,
) -> liteos_ui::Result<()> {
    let rows: Vec<_> = labels
        .into_iter()
        .enumerate()
        .map(|(index, text)| TreeRow {
            id: index as u64 + 1,
            parent: 0,
            text,
            expanded: false,
            has_children: false,
        })
        .collect();
    widget.set_tree_rows(&rows)
}

fn selection(widget: &Widget) -> liteos_ui::Result<Option<usize>> {
    Ok(widget.selection()?.map(|id| (id - 1) as usize))
}

fn field(parent: &Widget, title: &str, text: &str) -> liteos_ui::Result<(Widget, Widget)> {
    let line = child(parent, WidgetKind::Row, "", row())?;
    let label = child(
        &line,
        WidgetKind::Label,
        title,
        Layout {
            width: 102,
            ..Layout::default()
        },
    )?;
    let input = child(&line, WidgetKind::Edit, text, grow())?;
    Ok((label, input))
}

fn form_buttons(parent: &Widget) -> liteos_ui::Result<(Widget, Widget)> {
    let bar = child(parent, WidgetKind::Row, "", row())?;
    let _spacer = child(&bar, WidgetKind::Column, "", grow())?;
    let save = child(&bar, WidgetKind::Button, "确定", button())?;
    let cancel = child(&bar, WidgetKind::Button, "取消", button())?;
    save.set_state(state::DEFAULT)?;
    Ok((save, cancel))
}

fn checkbox(parent: &Widget, label: &str, on: bool) -> liteos_ui::Result<Widget> {
    let widget = child(parent, WidgetKind::CheckBox, label, Layout::default())?;
    widget.set_state(if on { state::CHECKED } else { 0 })?;
    Ok(widget)
}

fn checked(widget: &Widget) -> liteos_ui::Result<bool> {
    Ok(widget.state()? & state::CHECKED != 0)
}

fn optional_word(text: &str) -> Result<Option<u32>, std::num::ParseIntError> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let value = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => text.parse(),
    }?;
    Ok(Some(value))
}

fn row() -> Layout {
    Layout {
        gap: 6,
        ..Layout::default()
    }
}
fn grow() -> Layout {
    Layout {
        grow: 1,
        ..Layout::default()
    }
}
fn button() -> Layout {
    Layout {
        width: 79,
        height: 23,
        ..Layout::default()
    }
}
fn disabled(value: bool) -> u32 {
    if value { state::DISABLED } else { 0 }
}
