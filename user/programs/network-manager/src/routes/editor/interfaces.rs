//! 编辑当前路由域的接口及转发开关，返回时保留在上层路由设置草稿中。

use super::{button, checkbox, checked, disabled, form, grow, row, selection};
use crate::child;
use liteos_ui::{Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, WidgetKind, Window};
use network_config::{
    configuration::{
        ConfigurationName, DeviceMatch, InterfaceState, RouterConfiguration, RouterInterface,
        RoutingDomain,
    },
    format_mac,
};
use std::collections::BTreeMap;

mod profile;

pub(super) enum Draft<'a> {
    Default(&'a mut RouterConfiguration),
    Named(&'a mut RoutingDomain),
}

impl Draft<'_> {
    fn interfaces(&self) -> &BTreeMap<ConfigurationName, RouterInterface> {
        match self {
            Self::Default(config) => &config.interfaces,
            Self::Named(domain) => &domain.interfaces,
        }
    }

    fn forwarding(&mut self) -> (&mut bool, &mut bool) {
        match self {
            Self::Default(config) => (&mut config.ipv4_forwarding, &mut config.ipv6_forwarding),
            Self::Named(domain) => (&mut domain.ipv4_forwarding, &mut domain.ipv6_forwarding),
        }
    }
}

enum Target<'a> {
    Saved(ConfigurationName, Option<&'a InterfaceState>),
    Device(&'a InterfaceState),
}

pub(super) async fn edit(
    dialogs: &Dialogs,
    parent: &Window,
    devices: &[InterfaceState],
    mut draft: Draft<'_>,
) -> liteos_ui::Result<bool> {
    let (window, mut events, root) = form(dialogs, parent, "路由接口", 690, 405)?;
    let forwarding = child(&root, WidgetKind::Row, "", row())?;
    let (v4, v6) = draft.forwarding();
    let ipv4 = checkbox(&forwarding, "启用 IPv4 转发", *v4)?;
    let ipv6 = checkbox(&forwarding, "启用 IPv6 转发", *v6)?;
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
            (1, "配置名称", 159),
            (2, "设备", 112),
            (3, "设备绑定", 174),
            (4, "状态", 180),
        ]
        .map(|(id, title, width)| Column { id, title, width }),
    )?;
    let buttons = child(&root, WidgetKind::Row, "", row())?;
    let edit = child(&buttons, WidgetKind::Button, "属性…", button())?;
    let _spacer = child(&buttons, WidgetKind::Column, "", grow())?;
    let back = child(&buttons, WidgetKind::Button, "返回", button())?;
    let status = child(&root, WidgetKind::Status, "", Layout::default())?;
    let mut changed = false;
    let mut selected = None;

    'refresh: loop {
        let named = matches!(draft, Draft::Named(_));
        let entries = targets(draft.interfaces(), devices);
        let labels: Vec<_> = entries
            .iter()
            .map(|target| target.label(draft.interfaces(), named))
            .collect();
        let rows: Vec<_> = labels
            .iter()
            .enumerate()
            .map(|(index, label)| (index as u64 + 1, label.as_str()))
            .collect();
        list.set_rows(&rows)?;
        list.select(selected.map(|index: usize| index as u64 + 1))?;
        edit.set_state(disabled(selected.is_none()))?;

        while let Some(event) = events.next().await? {
            match event.kind {
                EventKind::Close
                | EventKind::Key {
                    code: 1,
                    pressed: true,
                    ..
                } => break,
                EventKind::Select { .. } if event.node == list.id() => {
                    selected = selection(&list)?;
                    edit.set_state(disabled(selected.is_none()))?;
                }
                EventKind::Activate { .. } if event.node == back.id() => break,
                EventKind::Activate { .. } if event.node == edit.id() => {
                    let Some(target) = selected.and_then(|index| entries.get(index)) else {
                        continue;
                    };
                    let result = match target {
                        Target::Saved(name, device) => {
                            let kind = match &draft {
                                Draft::Default(config)
                                    if config
                                        .bridges
                                        .values()
                                        .any(|bridge| bridge.ports.contains_key(name)) =>
                                {
                                    profile::Kind::BridgePort
                                }
                                Draft::Default(_) => profile::Kind::Default,
                                Draft::Named(_) => profile::Kind::Named,
                            };
                            let old = &draft.interfaces()[name];
                            let Some(mut next) = profile::edit(
                                dialogs,
                                &window,
                                name.as_str(),
                                old.clone(),
                                *device,
                                kind,
                            )
                            .await?
                            else {
                                continue;
                            };
                            if let Some(device) = device {
                                next.device = DeviceMatch::Mac(device.mac);
                            }
                            if *old == next {
                                continue;
                            }
                            match &mut draft {
                                Draft::Default(config) => &mut config.interfaces,
                                Draft::Named(domain) => &mut domain.interfaces,
                            }
                            .insert(name.clone(), next);
                            Ok(())
                        }
                        Target::Device(device) => {
                            let Draft::Default(config) = &mut draft else {
                                continue;
                            };
                            let name = String::from_utf8_lossy(&device.name);
                            let initial = RouterInterface::new(DeviceMatch::Mac(device.mac));
                            let Some(next) = profile::edit(
                                dialogs,
                                &window,
                                &name,
                                initial,
                                Some(device),
                                profile::Kind::Default,
                            )
                            .await?
                            else {
                                continue;
                            };
                            config
                                .ensure_interface(&device.name, Some(device.mac))
                                .map(|profile| *profile = next)
                        }
                    };
                    if result.is_err() {
                        status.set_text("设备绑定冲突，无法创建接口配置。")?;
                        continue;
                    }
                    changed = true;
                    // 从设备创建的配置会移至具名项，重读列表后清除旧行选择。
                    selected = None;
                    status.set_text("")?;
                    continue 'refresh;
                }
                _ => {}
            }
        }
        let (next_ipv4, next_ipv6) = (checked(&ipv4)?, checked(&ipv6)?);
        let (v4, v6) = draft.forwarding();
        changed |= (*v4, *v6) != (next_ipv4, next_ipv6);
        (*v4, *v6) = (next_ipv4, next_ipv6);
        return Ok(changed);
    }
}

fn targets<'a>(
    interfaces: &BTreeMap<ConfigurationName, RouterInterface>,
    devices: &'a [InterfaceState],
) -> Vec<Target<'a>> {
    let mut unconfigured: Vec<_> = devices.iter().map(Some).collect();
    let mut targets = Vec::with_capacity(interfaces.len() + devices.len());
    for (name, interface) in interfaces {
        let mut current = None;
        for (device, unconfigured) in devices.iter().zip(&mut unconfigured) {
            if interface.device.matches(&device.name, device.mac) {
                current = Some(device);
                *unconfigured = None;
            }
        }
        targets.push(Target::Saved(name.clone(), current));
    }
    targets.extend(unconfigured.into_iter().flatten().map(Target::Device));
    targets
}

impl Target<'_> {
    fn label(
        &self,
        interfaces: &BTreeMap<ConfigurationName, RouterInterface>,
        named: bool,
    ) -> String {
        match self {
            Self::Saved(name, device) => {
                let interface = &interfaces[name];
                let binding = match &interface.device {
                    DeviceMatch::Name(name) => name.as_str().to_owned(),
                    DeviceMatch::Mac(mac) => format_mac(*mac),
                };
                let device_name =
                    device.map_or("—".into(), |device| String::from_utf8_lossy(&device.name));
                let status = if named {
                    "已分配至此域"
                } else if device.is_none() {
                    "设备未连接"
                } else {
                    "已匹配设备"
                };
                format!("{}\t{device_name}\t{binding}\t{status}", name.as_str())
            }
            Self::Device(device) => format!(
                "—\t{}\t{}\t继承系统默认",
                String::from_utf8_lossy(&device.name),
                format_mac(device.mac)
            ),
        }
    }
}
