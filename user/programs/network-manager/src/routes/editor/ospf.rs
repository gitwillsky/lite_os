//! 管理 OSPF 路由器与接口设置，确认后写回当前路由草稿。

use super::{
    button, checkbox, checked, choice, disabled, field, form, form_buttons, grow, row, selection,
};
use crate::child;
use liteos_ui::{
    Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, MessageButtons, MessageIcon,
    MessageResult, Widget, WidgetKind, Window,
};
use network_config::configuration::{
    ConfigurationName,
    ospf::{Configuration, Link, Timers, V3},
};
use std::{net::Ipv4Addr, num::NonZeroU16};

mod authentication;

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    config: &mut Option<Configuration>,
    interfaces: &[&ConfigurationName],
) -> liteos_ui::Result<bool> {
    let (window, mut events, root) = form(dialogs, parent, "OSPF 设置", 626, 386)?;
    let mut draft = config.clone().unwrap_or(Configuration {
        router_id: Ipv4Addr::UNSPECIFIED,
        links: Default::default(),
    });
    let initial_id = config.as_ref().map(|ospf| ospf.router_id.to_string());
    let (_, router_id) = field(&root, "路由器 ID", initial_id.as_deref().unwrap_or(""))?;
    let list = child(&root, WidgetKind::List, "", grow())?;
    list.set_list_style(
        ListStyle::Details,
        ListOptions {
            multiple: false,
            full_row_select: true,
        },
    )?;
    list.set_columns(
        &[
            (1, "接口", 155),
            (2, "区域", 105),
            (3, "代价", 58),
            (4, "版本", 125),
            (5, "邻接", 135),
        ]
        .map(|(id, title, width)| Column { id, title, width }),
    )?;
    let actions = child(&root, WidgetKind::Row, "", row())?;
    let add = child(&actions, WidgetKind::Button, "新增…", button())?;
    let properties = child(&actions, WidgetKind::Button, "属性…", button())?;
    let remove = child(&actions, WidgetKind::Button, "删除", button())?;
    let _spacer = child(&actions, WidgetKind::Column, "", grow())?;
    let disable = child(
        &actions,
        WidgetKind::Button,
        "停用 OSPF",
        Layout {
            width: 104,
            ..button()
        },
    )?;
    disable.set_state(disabled(config.is_none()))?;
    let status = child(&root, WidgetKind::Status, "", Layout::default())?;
    let (save, cancel) = form_buttons(&root)?;
    let mut selected = None;

    'present: loop {
        let available: Vec<_> = interfaces
            .iter()
            .copied()
            .filter(|name| !draft.links.contains_key(*name))
            .collect();
        let labels: Vec<_> = draft
            .links
            .iter()
            .map(|(name, link)| {
                let versions = match (&link.v2, &link.v3) {
                    (Some(_), Some(_)) => "v2、v3",
                    (Some(_), None) => "v2",
                    _ => "v3",
                };
                format!(
                    "{}\t{}\t{}\t{versions}\t{}",
                    name.as_str(),
                    link.area,
                    link.cost,
                    if link.passive {
                        "仅通告"
                    } else {
                        "点对点"
                    }
                )
            })
            .collect();
        let rows: Vec<_> = labels
            .iter()
            .enumerate()
            .map(|(index, label)| (index as u64 + 1, label.as_str()))
            .collect();
        list.set_rows(&rows)?;
        list.select(selected.map(|index: usize| index as u64 + 1))?;
        add.set_state(disabled(available.is_empty()))?;
        properties.set_state(disabled(selected.is_none()))?;
        remove.set_state(disabled(selected.is_none()))?;
        status.set_text(if interfaces.is_empty() {
            "请先配置路由接口或 VLAN 网关。"
        } else {
            ""
        })?;

        while let Some(event) = events.next().await? {
            match event.kind {
                EventKind::Close
                | EventKind::Key {
                    code: 1,
                    pressed: true,
                    ..
                } => return Ok(false),
                EventKind::Select { .. } if event.node == list.id() => {
                    selected = selection(&list)?;
                    properties.set_state(disabled(selected.is_none()))?;
                    remove.set_state(disabled(selected.is_none()))?;
                }
                EventKind::Activate { .. } if event.node == cancel.id() => return Ok(false),
                EventKind::Activate { .. } if event.node == disable.id() => {
                    if dialogs
                        .message(
                            &window,
                            "停用 OSPF",
                            "提交后将停用全部 OSPF 链路并撤回学到的路由。是否继续？",
                            MessageButtons::YesNo,
                            MessageIcon::Warning,
                        )?
                        .await?
                        == MessageResult::Yes
                    {
                        return Ok(config.take().is_some());
                    }
                }
                EventKind::Activate { .. } if event.node == save.id() => {
                    let Ok(id) = router_id.text()?.trim().parse::<Ipv4Addr>() else {
                        status.set_text("请输入点分十进制路由器 ID，例如 192.0.2.1。")?;
                        router_id.focus()?;
                        continue;
                    };
                    if id.is_unspecified() || draft.links.is_empty() {
                        status.set_text("路由器 ID 须非零，并须至少配置一个接口。")?;
                        continue;
                    }
                    draft.router_id = id;
                    if let Err(error) = draft.validate_links(|name| interfaces.contains(&name)) {
                        status.set_text(&error.to_string())?;
                        continue;
                    }
                    if config.as_ref() == Some(&draft) {
                        return Ok(false);
                    }
                    *config = Some(draft);
                    return Ok(true);
                }
                EventKind::Activate { .. } if event.node == add.id() && !available.is_empty() => {
                    if let Some((name, link)) =
                        edit_link(dialogs, &window, &available, None).await?
                    {
                        selected = Some(draft.links.range::<ConfigurationName, _>(..&name).count());
                        draft.links.insert(name, link);
                        continue 'present;
                    }
                }
                EventKind::Activate { .. }
                    if event.node == properties.id() || event.node == remove.id() =>
                {
                    let Some((name, link)) =
                        selected.and_then(|index| draft.links.iter().nth(index))
                    else {
                        continue;
                    };
                    if event.node == remove.id() {
                        let name = name.clone();
                        draft.links.remove(&name);
                        selected = None;
                        continue 'present;
                    }
                    if let Some((name, link)) =
                        edit_link(dialogs, &window, &[name], Some(link)).await?
                    {
                        draft.links.insert(name, link);
                        continue 'present;
                    }
                }
                _ => {}
            }
        }
        return Ok(false);
    }
}

async fn edit_link(
    dialogs: &Dialogs,
    parent: &Window,
    interfaces: &[&ConfigurationName],
    current: Option<&Link>,
) -> liteos_ui::Result<Option<(ConfigurationName, Link)>> {
    let (window, mut events, root) = form(dialogs, parent, "OSPF 接口属性", 525, 390)?;
    let interfaces_row = child(&root, WidgetKind::Row, "", row())?;
    let _label = child(
        &interfaces_row,
        WidgetKind::Label,
        "接口",
        Layout::default(),
    )?;
    let interface = choice(
        &interfaces_row,
        interfaces.iter().map(|name| name.as_str()),
        280,
    )?;
    interface.select(Some(1))?;
    let area = current.map_or(Ipv4Addr::UNSPECIFIED, |link| link.area);
    let cost = current.map_or(NonZeroU16::MIN, |link| link.cost);
    let (_, area) = field(&root, "区域 ID", &area.to_string())?;
    let (_, cost) = field(&root, "链路代价", &cost.to_string())?;
    let passive = checkbox(
        &root,
        "仅通告本网段",
        current.is_some_and(|link| link.passive),
    )?;
    let mut v2_auth = current.and_then(|link| link.v2_authentication.clone());
    let mut v3_auth = current.and_then(|link| link.v3_authentication.clone());
    let v2 = current.and_then(|link| link.v2);
    let v3 = current.and_then(|link| link.v3);
    let default_timers = Timers {
        hello: 10,
        dead: 40,
    };
    let v2_fields = version_fields(
        &root,
        "OSPFv2",
        current.is_none() || v2.is_some(),
        v2.unwrap_or(default_timers),
    )?;
    let v3_fields = version_fields(
        &root,
        "OSPFv3",
        v3.is_some(),
        v3.map_or(default_timers, |v3| v3.timers),
    )?;
    let (_, instance) = field(
        &root,
        "v3 实例号",
        &v3.map_or(0, |v3| v3.instance).to_string(),
    )?;
    let status = child(&root, WidgetKind::Status, "", Layout::default())?;
    let (save, cancel) = form_buttons(&root)?;
    let update_availability = || -> liteos_ui::Result<()> {
        let passive_mode = checked(&passive)?;
        v2_fields.set_availability(passive_mode)?;
        v3_fields.set_availability(passive_mode)?;
        instance.set_state(disabled(!checked(&v3_fields.enabled)?))
    };
    update_availability()?;
    v2_fields.show_auth(v2_auth.is_some())?;
    v3_fields.show_auth(v3_auth.is_some())?;

    while let Some(event) = events.next().await? {
        match event.kind {
            EventKind::Close
            | EventKind::Key {
                code: 1,
                pressed: true,
                ..
            } => return Ok(None),
            EventKind::Activate { .. } if event.node == cancel.id() => return Ok(None),
            EventKind::Activate { .. }
                if event.node == passive.id()
                    || event.node == v2_fields.enabled.id()
                    || event.node == v3_fields.enabled.id() =>
            {
                update_availability()?;
            }
            EventKind::Activate { .. } if event.node == v2_fields.auth.id() => {
                if let Some(next) = authentication::edit(
                    dialogs,
                    &window,
                    "OSPFv2 认证",
                    u8::MAX.into(),
                    v2_auth.as_ref(),
                )
                .await?
                {
                    v2_auth = next;
                    v2_fields.show_auth(v2_auth.is_some())?;
                }
            }
            EventKind::Activate { .. } if event.node == v3_fields.auth.id() => {
                if let Some(next) = authentication::edit(
                    dialogs,
                    &window,
                    "OSPFv3 认证",
                    u16::MAX,
                    v3_auth.as_ref(),
                )
                .await?
                {
                    v3_auth = next;
                    v3_fields.show_auth(v3_auth.is_some())?;
                }
            }
            EventKind::Activate { .. } if event.node == save.id() => {
                let Some(name) = selection(&interface)?.and_then(|index| interfaces.get(index))
                else {
                    status.set_text("请选择一个接口。")?;
                    continue;
                };
                let (Ok(area), Ok(cost)) =
                    (area.text()?.trim().parse(), cost.text()?.trim().parse())
                else {
                    status.set_text("区域 ID 须为点分十进制地址，链路代价须为 1–65535。")?;
                    continue;
                };
                let v2_enabled = checked(&v2_fields.enabled)?;
                let v3_enabled = checked(&v3_fields.enabled)?;
                let passive_mode = checked(&passive)?;
                let v2 = if v2_enabled {
                    v2_fields.timers()?
                } else {
                    None
                };
                let v3 = if v3_enabled {
                    v3_fields
                        .timers()?
                        .zip(instance.text()?.trim().parse().ok())
                        .map(|(timers, instance)| V3 { timers, instance })
                } else {
                    None
                };
                let next = Link {
                    area,
                    cost,
                    passive: passive_mode,
                    v2,
                    v3,
                    v2_authentication: (v2_enabled && !passive_mode)
                        .then(|| v2_auth.clone())
                        .flatten(),
                    v3_authentication: (v3_enabled && !passive_mode)
                        .then(|| v3_auth.clone())
                        .flatten(),
                };
                if v2_enabled != v2.is_some() || v3_enabled != v3.is_some() || !next.valid() {
                    status.set_text("请启用至少一个版本。Hello 须大于零，失效间隔须更大。v3 失效间隔最多 65535 秒，实例号为 0–31。")?;
                    continue;
                }
                return Ok(Some(((*name).clone(), next)));
            }
            _ => {}
        }
    }
    Ok(None)
}

struct VersionFields {
    enabled: Widget,
    hello: Widget,
    dead: Widget,
    auth: Widget,
}

impl VersionFields {
    fn set_availability(&self, passive: bool) -> liteos_ui::Result<()> {
        let enabled = checked(&self.enabled)?;
        self.hello.set_state(disabled(!enabled))?;
        self.dead.set_state(disabled(!enabled))?;
        self.auth.set_state(disabled(!enabled || passive))
    }

    fn show_auth(&self, configured: bool) -> liteos_ui::Result<()> {
        self.auth.set_text(if configured {
            "认证已设置…"
        } else {
            "认证…"
        })
    }

    fn timers(&self) -> liteos_ui::Result<Option<Timers>> {
        let hello = self.hello.text()?.trim().parse().ok();
        let dead = self.dead.text()?.trim().parse().ok();
        Ok(hello.zip(dead).map(|(hello, dead)| Timers { hello, dead }))
    }
}

fn version_fields(
    parent: &Widget,
    version: &str,
    enabled: bool,
    timers: Timers,
) -> liteos_ui::Result<VersionFields> {
    let line = child(parent, WidgetKind::Row, "", row())?;
    let enabled = checkbox(&line, version, enabled)?;
    let layout = Layout {
        width: 55,
        height: 23,
        ..Layout::default()
    };
    let _hello_label = child(&line, WidgetKind::Label, "Hello 秒", Layout::default())?;
    let hello = child(&line, WidgetKind::Edit, &timers.hello.to_string(), layout)?;
    let _dead_label = child(&line, WidgetKind::Label, "失效秒数", Layout::default())?;
    let dead = child(&line, WidgetKind::Edit, &timers.dead.to_string(), layout)?;
    let auth = child(
        &line,
        WidgetKind::Button,
        "认证…",
        Layout {
            width: 112,
            ..button()
        },
    )?;
    Ok(VersionFields {
        enabled,
        hello,
        dead,
        auth,
    })
}
