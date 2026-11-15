//! 编辑当前路由域的接口，返回时保留在上层路由设置草稿中。

use super::{button, disabled, form, grow, row, selection};
use crate::child;
use liteos_ui::{Column, Dialogs, EventKind, Layout, ListOptions, ListStyle, WidgetKind, Window};
use network_config::{
    ManagementClient, Revision,
    configuration::{
        Bridge, ConfigurationName, DeviceMatch, InterfaceState, RouterConfiguration,
        RouterInterface, RoutingDomain,
    },
    format_mac,
    traffic::Row,
};
use std::{collections::BTreeMap, io};

mod profile;

pub(crate) use profile::{
    ADMIN_STATES, Field, Fields, admin_index, apply_fields, edit_ipv4_of, edit_ipv6_of,
};

/// 一块网卡在所选路由域中的已保存配置及定位信息。读取时校验网卡仍属于该域并能唯一定位，
/// 提交时以读取时的配置代际为基准，期间配置被他人修改会被拒绝。
/// 克隆用于把同一份基线交给后台线程编码候选。
#[derive(Clone)]
pub(crate) struct LiveInterface {
    pub(crate) revision: Revision,
    config: RouterConfiguration,
    domain: Option<ConfigurationName>,
    name: Option<ConfigurationName>,
    mac: [u8; 6],
    device_name: Vec<u8>,
    /// 保存的接口配置；尚未保存过时是按 MAC 绑定的默认值。
    pub(crate) initial: RouterInterface,
    kind: profile::Kind,
    /// 默认域中该网卡的运行状态，具名域不提供。
    pub(crate) device: Option<InterfaceState>,
}

impl LiveInterface {
    /// 在后台线程读取当前配置并定位 `row` 对应的网卡配置。
    ///
    /// 错误：网卡没有以太网身份、已移出所选域、由网桥 VLAN 网关管理、无法唯一定位，或管理服务不可用。
    pub(crate) fn load(domain: Option<&str>, row: &Row) -> io::Result<Self> {
        let mac = row.ethernet_address.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "此接口没有可管理的以太网设备身份。",
            )
        })?;
        let mut client = ManagementClient::local();
        let (revision, bytes) = client.export()?;
        let config = RouterConfiguration::decode(&bytes).map_err(io::Error::other)?;
        let snapshot = if domain.is_none() {
            let snapshot = client.snapshot()?;
            if snapshot.generation != revision {
                return Err(io::Error::other("配置或接口已变化，请重新选择。"));
            }
            Some(snapshot)
        } else {
            None
        };
        let bound = config.routing_domain_for_mac(mac).cloned();
        if bound.as_ref().map(ConfigurationName::as_str) != domain {
            return Err(io::Error::other("接口所属路由域已变化，请重新选择。"));
        }
        let (interfaces, bridges, device) = if let Some(scope) = &bound {
            let domain = &config.routing_domains[scope];
            (&domain.interfaces, &domain.bridges, None)
        } else {
            let device = snapshot
                .as_ref()
                .and_then(|snapshot| {
                    snapshot
                        .interfaces
                        .iter()
                        .find(|device| device.mac == mac && device.name == row.name())
                })
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "该设备已移除或不在所选路由域。")
                })?;
            (&config.interfaces, &config.bridges, Some(device.clone()))
        };
        if bridges
            .values()
            .flat_map(|bridge| bridge.gateways.values())
            .any(|gateway| gateway.name.as_str().as_bytes() == row.name())
        {
            return Err(io::Error::other("VLAN 网关由「网桥」窗口管理。"));
        }
        let mut matches = interfaces
            .iter()
            .filter(|(_, interface)| interface.device.matches(row.name(), mac));
        let matched = matches.next();
        if matches.next().is_some() || bound.is_some() && matched.is_none() {
            return Err(io::Error::other("无法唯一定位此接口配置。"));
        }
        let kind = profile_kind(bound.is_some(), bridges, matched.map(|(name, _)| name));
        let name = matched.map(|(name, _)| name.clone());
        let initial = matched.map_or_else(
            || RouterInterface::new(DeviceMatch::Mac(mac)),
            |(_, interface)| interface.clone(),
        );
        Ok(Self {
            revision,
            config,
            domain: bound,
            name,
            mac,
            device_name: row.name().to_vec(),
            initial,
            kind,
            device,
        })
    }

    /// 网桥端口的管理状态与三层设置由网桥和 VLAN 网关维护。
    pub(crate) fn bridge_port(&self) -> bool {
        self.kind == profile::Kind::BridgePort
    }

    /// 具名路由域里的网卡；IPv4 可选择不配置地址，且没有系统默认可继承。
    pub(crate) fn named(&self) -> bool {
        self.kind == profile::Kind::Named
    }

    /// 以 `next` 替换此网卡的配置并编码整份候选；在后台线程调用。
    ///
    /// 错误：默认域中设备绑定冲突，或候选无法编码。
    pub(crate) fn candidate(&self, mut next: RouterInterface) -> io::Result<Vec<u8>> {
        let mut config = self.config.clone();
        next.device = DeviceMatch::Mac(self.mac);
        if let Some(scope) = &self.domain {
            let domain = config
                .routing_domains
                .get_mut(scope)
                .expect("matched domain");
            domain.interfaces.insert(
                self.name
                    .clone()
                    .expect("named domain interface was matched"),
                next,
            );
        } else {
            *config
                .ensure_interface(&self.device_name, Some(self.mac))
                .map_err(|_| io::Error::other("设备绑定冲突，无法保存接口配置。"))? = next;
        }
        config.encode().map_err(io::Error::other)
    }
}

fn profile_kind(
    named: bool,
    bridges: &BTreeMap<ConfigurationName, Bridge>,
    name: Option<&ConfigurationName>,
) -> profile::Kind {
    if name.is_some_and(|name| {
        bridges
            .values()
            .any(|bridge| bridge.ports.contains_key(name))
    }) {
        profile::Kind::BridgePort
    } else if named {
        profile::Kind::Named
    } else {
        profile::Kind::Default
    }
}

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
                            let bridges = match &draft {
                                Draft::Default(config) => &config.bridges,
                                Draft::Named(domain) => &domain.bridges,
                            };
                            let kind = profile_kind(named, bridges, Some(name));
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
