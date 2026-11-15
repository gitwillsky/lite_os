//! ND 接口属性和从 IPv6 地址生成的只读前缀属性。

use super::{
    TITLE,
    catalog::{self, Interface, Prefix},
};
use crate::{
    child,
    configuration::EditTransaction,
    item_properties::{Footer, Request, button, field},
    routes::form,
    workspace::Workspace,
};
use liteos_ui::{
    Dialogs, Event, EventKind, Layout, MessageButtons, MessageIcon, TreeRow, Widget, WidgetKind,
    Window, state,
};
use network_config::configuration::{
    AdvertisedDns, RouterAdvertisements, valid_advertised_prefixes,
};
use std::{io, net::Ipv6Addr};

type Error = Box<dyn std::error::Error>;

/// 新建或修改一条通告配置。移动接口时在同一持久事务撤下旧配置并应用新配置。
/// 错误：对话框创建失败或无法向用户报告提交失败。
pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    choices: Vec<Interface>,
    selected: Option<usize>,
    mut transaction: EditTransaction,
) -> Result<(), Error> {
    let mut current = selected.map(|index| (index, choices[index].policy));
    let title = selected.map_or_else(
        || "新建 ND".to_owned(),
        |index| format!("ND <{}>", choices[index].name),
    );
    let (window, mut events, root) = form(dialogs, parent, &title, 486, 196)?;
    let body = child(
        &root,
        WidgetKind::Row,
        "",
        Layout {
            grow: 1,
            gap: 8,
            ..Layout::default()
        },
    )?;
    let fields = child(
        &body,
        WidgetKind::Column,
        "",
        Layout {
            grow: 1,
            gap: 5,
            ..Layout::default()
        },
    )?;
    let mut decor = Vec::new();
    let interface = field(&fields, &mut decor, "接口", WidgetKind::Combo, "", false)?;
    interface.set_state(state::DROPDOWN_LIST)?;
    let rows: Vec<_> = choices
        .iter()
        .enumerate()
        .filter(|(index, entry)| {
            Some(*index) == selected || entry.policy == RouterAdvertisements::Disabled
        })
        .map(|(index, entry)| TreeRow {
            id: index as u64 + 1,
            parent: 0,
            text: &entry.name,
            expanded: false,
            has_children: false,
        })
        .collect();
    interface.set_tree_rows(&rows)?;
    interface.select(
        selected
            .map(|index| index as u64 + 1)
            .or_else(|| rows.first().map(|row| row.id)),
    )?;
    let (interval, lifetime, addresses) = match current.map(|(_, policy)| policy) {
        Some(RouterAdvertisements::Enabled {
            max_interval_seconds,
            router_lifetime_seconds,
            dns,
        }) => (max_interval_seconds, router_lifetime_seconds, dns),
        _ => (600, 1800, AdvertisedDns::default()),
    };
    let interval = field(
        &fields,
        &mut decor,
        "RA 最大间隔",
        WidgetKind::Edit,
        &interval.to_string(),
        false,
    )?;
    let lifetime = field(
        &fields,
        &mut decor,
        "RA 寿命",
        WidgetKind::Edit,
        &lifetime.to_string(),
        false,
    )?;
    let servers = addresses
        .as_slice()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let dns = field(
        &fields,
        &mut decor,
        "DNS",
        WidgetKind::Edit,
        &servers,
        false,
    )?;
    let buttons = Footer::side(&body)?;
    buttons.set_enabled(true, !rows.is_empty())?;
    interface.focus()?;
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
                    let id = interface
                        .selection()?
                        .and_then(|id| id.checked_sub(1))
                        .and_then(|id| usize::try_from(id).ok())
                        .filter(|index| *index < choices.len())
                        .ok_or_else(|| io::Error::other("请选择一个接口。"))?;
                    let target = &choices[id];
                    let policy = read_policy(&interval, &lifetime, &dns, target)?;
                    if current != Some((id, policy)) {
                        let previous = current
                            .filter(|(index, _)| *index != id)
                            .map(|(index, _)| choices[index].binding.clone());
                        let next = target.binding.clone();
                        transaction
                            .save(move |config, scope| {
                                if let Some(previous) = &previous {
                                    catalog::set(
                                        config,
                                        scope,
                                        previous,
                                        RouterAdvertisements::Disabled,
                                    )?;
                                }
                                catalog::set(config, scope, &next, policy)
                            })
                            .await?;
                        current = Some((id, policy));
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
                                TITLE,
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

fn read_policy(
    interval: &Widget,
    lifetime: &Widget,
    dns: &Widget,
    target: &Interface,
) -> Result<RouterAdvertisements, Error> {
    let (Ok(max_interval_seconds), Ok(router_lifetime_seconds)) = (
        interval.text()?.trim().parse(),
        lifetime.text()?.trim().parse(),
    ) else {
        return Err(io::Error::other("RA 间隔和寿命以整数秒填写。").into());
    };
    let values = dns
        .text()?
        .split(|ch: char| ch == ',' || ch.is_whitespace())
        .filter(|part| !part.is_empty())
        .map(str::parse::<Ipv6Addr>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| io::Error::other("请输入有效的 IPv6 DNS 地址。"))?;
    let dns = AdvertisedDns::new(&values)
        .map_err(|_| io::Error::other("最多填写八个互不重复的 IPv6 单播 DNS 地址。"))?;
    let policy = RouterAdvertisements::Enabled {
        max_interval_seconds,
        router_lifetime_seconds,
        dns,
    };
    if !policy.valid() {
        return Err(io::Error::other(
            "RA 最大间隔为 4–1800 秒。寿命为 0，或介于最大间隔和 9000 秒之间。",
        )
        .into());
    }
    if !valid_advertised_prefixes(target.addresses.iter().copied(), policy.max_prefixes()) {
        return Err(io::Error::other("前缀和 DNS 地址总数超过一份通告的容量。").into());
    }
    Ok(policy)
}

pub(super) struct PrefixProperty {
    pub(super) frame: Widget,
    pub(super) prefix: Prefix,
    ok: Widget,
    _decor: Vec<Widget>,
}

impl PrefixProperty {
    /// 查看从接口地址生成的前缀；其配置留在 IPv6 地址窗口。
    /// 错误：工作区或控件已关闭。
    pub(super) fn open(workspace: &Workspace, prefix: Prefix) -> liteos_ui::Result<Self> {
        let address = format!("{}/64", prefix.network);
        let frame = workspace.item_window(&format!("ND 前缀 <{address}>"), (478, 177))?;
        let root = child(
            &frame,
            WidgetKind::Row,
            "",
            Layout {
                grow: 1,
                gap: 8,
                padding: 8,
                ..Layout::default()
            },
        )?;
        let fields = child(
            &root,
            WidgetKind::Column,
            "",
            Layout {
                grow: 1,
                gap: 5,
                ..Layout::default()
            },
        )?;
        let mut decor = vec![root.clone(), fields.clone()];
        for (label, value) in [
            ("前缀", address.as_str()),
            ("接口", prefix.interface.as_str()),
        ] {
            let entry = field(&fields, &mut decor, label, WidgetKind::Edit, value, true)?;
            decor.push(entry);
        }
        for label in ["链路上", "自治"] {
            let check = child(&fields, WidgetKind::CheckBox, label, Layout::default())?;
            check.set_state(state::CHECKED | state::DISABLED)?;
            decor.push(check);
        }
        let ok = child(&root, WidgetKind::Button, "确定", button())?;
        ok.set_state(state::DEFAULT)?;
        Ok(Self {
            frame,
            prefix,
            ok,
            _decor: decor,
        })
    }

    pub(super) fn closed(&self, event: &Event) -> bool {
        matches!(event.kind, EventKind::Activate { .. })
            && [self.ok.id(), self.frame.id()].contains(&event.node)
    }
}
