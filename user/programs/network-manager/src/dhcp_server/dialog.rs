//! DHCP Setup、服务器属性、网络属性共享池值的输入语义和原生校验。

use super::{catalog, catalog::Server, duration};
use crate::{
    child,
    configuration::EditTransaction,
    item_list::spacer,
    item_properties::{Footer, Request, button, field},
    routes::form,
};
use liteos_ui::{
    Dialogs, EventKind, Layout, MessageButtons, MessageIcon, TreeRow, Widget, WidgetKind, Window,
    state,
};
use network_config::configuration::{ConfigurationName as Name, DhcpPool, DhcpV4Pool};
use std::{io, net::Ipv4Addr, time::Duration};

pub(super) type Error = Box<dyn std::error::Error>;

pub(super) async fn save_pool(
    transaction: &mut EditTransaction,
    interface: Name,
    pool: Option<DhcpV4Pool>,
) -> Result<(), Error> {
    transaction
        .save(move |config, domain| catalog::set_pool(config, domain, &interface, pool))
        .await
}

const WIZARD_FIELDS: [&str; 5] = [
    "DHCP 地址空间",
    "网关",
    "分发地址范围",
    "DNS 服务器",
    "租期",
];

pub(super) async fn setup(
    dialogs: &Dialogs,
    parent: &Window,
    targets: &[Server],
    mut transaction: EditTransaction,
) -> Result<(), Error> {
    let choices: Vec<_> = targets
        .iter()
        .filter(|server| server.pool.is_none())
        .collect();
    if choices.is_empty() {
        dialogs
            .message(
                parent,
                "DHCP 设置向导",
                "没有尚未配置 DHCP 服务器的静态 IPv4 接口。",
                MessageButtons::Ok,
                MessageIcon::Information,
            )?
            .await?;
        return Ok(());
    }
    let (window, mut events, root) = form(dialogs, parent, "DHCP 设置向导", 405, 156)?;
    let mut decor = Vec::new();
    let select_interface = child(&root, WidgetKind::Column, "", Layout::default())?;
    let interface = field(
        &select_interface,
        &mut decor,
        "DHCP 服务器接口",
        WidgetKind::Combo,
        "",
        false,
    )?;
    interface.set_state(state::DROPDOWN_LIST)?;
    let rows: Vec<_> = choices
        .iter()
        .enumerate()
        .map(|(index, server)| TreeRow {
            id: index as u64 + 1,
            parent: 0,
            text: server.interface.as_str(),
            expanded: false,
            has_children: false,
        })
        .collect();
    interface.set_tree_rows(&rows)?;
    interface.select(Some(1))?;
    // item_properties 持有每个字段的行。通过独立容器显示当前步骤，保留返回时的输入。
    let mut steps = vec![select_interface];
    let mut fields = Vec::new();
    for (index, title) in WIZARD_FIELDS.iter().enumerate() {
        let pane = child(&root, WidgetKind::Column, "", Layout::default())?;
        let entry = field(&pane, &mut decor, title, WidgetKind::Edit, "", index == 0)?;
        steps.push(pane);
        fields.push(entry);
    }
    let status = child(&root, WidgetKind::Label, "", Layout::default())?;
    status.set_state(state::WRAP | state::HIDDEN)?;
    let bar = child(
        &root,
        WidgetKind::Row,
        "",
        Layout {
            gap: 5,
            ..Layout::default()
        },
    )?;
    let _space = spacer(&bar)?;
    let back = child(&bar, WidgetKind::Button, "上一步", button())?;
    let next = child(&bar, WidgetKind::Button, "下一步", button())?;
    let cancel = child(&bar, WidgetKind::Button, "取消", button())?;
    next.set_state(state::DEFAULT)?;
    let mut current = 0;
    let mut target = None;
    // 只在步骤切换时更新焦点。文本输入和下拉框事件继续由本步骤的控件处理。
    let mut shown = None;
    loop {
        if shown != Some(current) {
            for (index, step) in steps.iter().enumerate() {
                step.set_state(if index == current { 0 } else { state::HIDDEN })?;
            }
            back.set_state(if current == 0 { state::DISABLED } else { 0 })?;
            if current >= 2 {
                fields[current - 1].focus()?;
            } else {
                next.focus()?;
            }
            shown = Some(current);
        }
        let Some(event) = events.next().await? else {
            return Ok(());
        };
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => return Ok(()),
            EventKind::Activate { .. } if event.node == cancel.id() => return Ok(()),
            EventKind::Activate { .. } if event.node == back.id() && current > 0 => {
                current -= 1;
                status.set_state(state::WRAP | state::HIDDEN)?;
            }
            EventKind::Activate { .. } if event.node == next.id() => {
                if current == 0 {
                    let Some((index, server)) = interface
                        .selection()?
                        .and_then(|id| id.checked_sub(1))
                        .map(|id| id as usize)
                        .and_then(|index| choices.get(index).map(|server| (index, *server)))
                    else {
                        continue;
                    };
                    if Some(index) != target {
                        fields[0].set_text(&server.network())?;
                        fields[1].set_text(&server.address.to_string())?;
                        fields[2].set_text(&default_range(server))?;
                        fields[3].set_text("")?;
                        fields[4].set_text("00:10:00")?;
                        target = Some(index);
                    }
                }
                let validation = match current {
                    2 => gateway(&fields[1]).map(drop),
                    3 => range(&fields[2]).map(drop),
                    4 => dns(&fields[3]).map(drop),
                    _ => Ok(()),
                };
                if let Err(error) = validation {
                    status.set_text(&error.to_string())?;
                    status.set_state(state::WRAP)?;
                    continue;
                }
                status.set_state(state::WRAP | state::HIDDEN)?;
                if current < steps.len() - 1 {
                    current += 1;
                    continue;
                }
                let server = choices[target.expect("selected before leaving interface")];
                let result = async {
                    let (first, last) = range(&fields[2])?;
                    let pool = DhcpV4Pool {
                        first,
                        last,
                        router: gateway(&fields[1])?,
                        dns: dns(&fields[3])?,
                        lease_seconds: lease_time(&fields[4])?,
                        probe_addresses: true,
                    };
                    validate(server, &pool)?;
                    save_pool(&mut transaction, server.interface.clone(), Some(pool)).await
                }
                .await;
                match result {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        dialogs
                            .message(
                                &window,
                                "DHCP 设置向导",
                                &error.to_string(),
                                MessageButtons::Ok,
                                MessageIcon::Error,
                            )?
                            .await?;
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Pane {
    Dhcp,
    Networks,
    Leases,
}

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    server: &Server,
    pane: Pane,
    mut transaction: EditTransaction,
) -> Result<(), Error> {
    let mut saved = server.pool.clone().expect("selected server has a pool");
    let title = if pane == Pane::Dhcp {
        format!("DHCP 服务器 <{}>", server.interface.as_str())
    } else {
        format!("DHCP 网络 <{}>", server.network())
    };
    let (window, mut events, root) = form(dialogs, parent, &title, 490, 184)?;
    let body = child(
        &root,
        WidgetKind::Row,
        "",
        Layout {
            gap: 8,
            grow: 1,
            ..Layout::default()
        },
    )?;
    let left = child(
        &body,
        WidgetKind::Column,
        "",
        Layout {
            gap: 5,
            grow: 1,
            ..Layout::default()
        },
    )?;
    let mut decor = Vec::new();
    let (caption, identity) = if pane == Pane::Dhcp {
        ("接口", server.interface.as_str().to_owned())
    } else {
        ("地址", server.network())
    };
    let _identity = field(
        &left,
        &mut decor,
        caption,
        WidgetKind::Edit,
        &identity,
        true,
    )?;
    let (first, second, probe) = if pane == Pane::Dhcp {
        let time = duration(Duration::from_secs(u64::from(saved.lease_seconds)));
        let range = format!("{}-{}", saved.first, saved.last);
        let probe = child(&left, WidgetKind::CheckBox, "冲突检测", Layout::default())?;
        probe.set_state(if saved.probe_addresses {
            state::CHECKED
        } else {
            0
        })?;
        (
            field(&left, &mut decor, "租期", WidgetKind::Edit, &time, false)?,
            field(&left, &mut decor, "地址池", WidgetKind::Edit, &range, false)?,
            Some(probe),
        )
    } else {
        let gateway = saved
            .router
            .map_or_else(String::new, |address| address.to_string());
        let dns = saved
            .dns
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        (
            field(&left, &mut decor, "网关", WidgetKind::Edit, &gateway, false)?,
            field(
                &left,
                &mut decor,
                "DNS 服务器",
                WidgetKind::Edit,
                &dns,
                false,
            )?,
            None,
        )
    };
    let buttons = Footer::side(&body)?;
    buttons.set_enabled(true, true)?;
    while let Some(event) = events.next().await? {
        if matches!(
            event.kind,
            EventKind::Close
                | EventKind::Key {
                    code: 1,
                    pressed: true,
                    ..
                }
        ) {
            return Ok(());
        }
        match buttons.command(&event) {
            Some(Request::Close) => return Ok(()),
            Some(Request::Save { close }) => {
                let result = async {
                    let mut candidate = saved.clone();
                    if let Some(probe) = &probe {
                        candidate.lease_seconds = lease_time(&first)?;
                        (candidate.first, candidate.last) = range(&second)?;
                        candidate.probe_addresses = probe.state()? & state::CHECKED != 0;
                    } else {
                        candidate.router = gateway(&first)?;
                        candidate.dns = dns(&second)?;
                    }
                    if candidate != saved {
                        validate(server, &candidate)?;
                        save_pool(
                            &mut transaction,
                            server.interface.clone(),
                            Some(candidate.clone()),
                        )
                        .await?;
                        saved = candidate;
                    }
                    Ok::<(), Error>(())
                }
                .await;
                match result {
                    Ok(()) if close => return Ok(()),
                    Ok(()) => {}
                    Err(error) => {
                        dialogs
                            .message(
                                &window,
                                &title,
                                &error.to_string(),
                                MessageButtons::Ok,
                                MessageIcon::Error,
                            )?
                            .await?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate(server: &Server, options: &DhcpV4Pool) -> Result<(), Error> {
    DhcpPool {
        server: server.address,
        prefix_length: server.prefix,
        options: options.clone(),
    }
    .mask()
    .map(|_| ())
    .ok_or_else(|| {
        io::Error::other("请检查下游网络、地址池、网关和 DNS。地址池应避开本机和网关。").into()
    })
}

fn default_range(server: &Server) -> String {
    let mask = u32::MAX
        .checked_shl(32 - u32::from(server.prefix))
        .unwrap_or(0);
    let address = u32::from(server.address);
    let first = (address & mask) + 1;
    let last = (address | !mask) - 1;
    let (low, high) = if address.saturating_sub(first) >= last.saturating_sub(address) {
        (first, address.saturating_sub(1))
    } else {
        (address.saturating_add(1), last)
    };
    format!("{}-{}", Ipv4Addr::from(low), Ipv4Addr::from(high))
}

fn range(field: &Widget) -> Result<(Ipv4Addr, Ipv4Addr), Error> {
    let input = field.text()?;
    let (first, last) = input.trim().split_once('-').unwrap_or((&input, &input));
    let error = |_| io::Error::other("请输入 IPv4 地址范围，例如 192.0.2.2-192.0.2.20。");
    Ok((
        first.trim().parse().map_err(error)?,
        last.trim().parse().map_err(error)?,
    ))
}

fn gateway(field: &Widget) -> Result<Option<Ipv4Addr>, Error> {
    let input = field.text()?;
    if input.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(input.trim().parse().map_err(|_| {
        io::Error::other("请输入有效的 IPv4 网关。")
    })?))
}

fn dns(field: &Widget) -> Result<Vec<Ipv4Addr>, Error> {
    field
        .text()?
        .split(|character: char| character == ',' || character.is_whitespace())
        .filter(|value| !value.is_empty())
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(|_| io::Error::other("请输入以逗号分隔的 IPv4 DNS 地址。").into())
}

fn lease_time(field: &Widget) -> Result<u32, Error> {
    let input = field.text()?;
    let parts = input
        .trim()
        .split(':')
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>();
    let total = match parts.as_deref() {
        Ok([seconds]) => Some(*seconds),
        Ok([hours, minutes, seconds]) if *minutes < 60 && *seconds < 60 => hours
            .checked_mul(3600)
            .and_then(|base| base.checked_add(minutes * 60 + seconds)),
        _ => None,
    };
    total
        .filter(|value| *value != 0)
        .ok_or_else(|| io::Error::other("请输入正整数秒或 HH:MM:SS 租期。").into())
}
