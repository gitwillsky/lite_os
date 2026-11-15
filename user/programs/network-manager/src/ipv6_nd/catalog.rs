//! ND 声明及从手动地址推导的通告前缀。配置 owner 决定持久化，NET 决定实际出帧。

use crate::configuration::{
    DraftBaseline,
    devices::{Device, DeviceMut, Layer3},
};
use network_config::{
    ManagementClient, Revision,
    configuration::{
        ConfigurationName as Name, Ipv6Address, Ipv6Configuration, RouterAdvertisements,
        RouterConfiguration, RouterInterface,
    },
};
use std::{collections::BTreeSet, io, net::Ipv6Addr};

#[derive(Clone, PartialEq, Eq)]
pub(super) enum Binding {
    Configured(Name),
    Hardware { name: Vec<u8>, mac: [u8; 6] },
}

#[derive(Clone)]
pub(super) struct Interface {
    pub(super) binding: Binding,
    pub(super) name: String,
    pub(super) policy: RouterAdvertisements,
    pub(super) addresses: Vec<Ipv6Address>,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Prefix {
    pub(super) network: Ipv6Addr,
    pub(super) interface: String,
}

pub(super) struct Catalog {
    pub(super) baseline: DraftBaseline,
    pub(super) forwarding: bool,
    pub(super) interfaces: Vec<Interface>,
    pub(super) prefixes: Vec<Prefix>,
}

impl Catalog {
    /// 读取同一代际的配置和可用于新建 ND 的接口。错误：配置正在变化、路由域消失或服务不可读。
    pub(super) fn load(scope: Option<&str>, current: Option<Revision>) -> io::Result<Option<Self>> {
        let Some((baseline, config)) = DraftBaseline::capture(current)? else {
            return Ok(None);
        };
        let forwarding = match scope {
            Some(scope) => {
                config
                    .routing_domains
                    .get(scope)
                    .ok_or_else(missing)?
                    .ipv6_forwarding
            }
            None => config.ipv6_forwarding,
        };
        let mut interfaces = Vec::new();
        let scope_data = Layer3::new(&config, scope)?;
        let mut vlan_names = BTreeSet::new();
        for device in scope_data.devices() {
            match device {
                Device::Interface(name, profile) if profile.mtu >= 1280 => {
                    interfaces.push(Interface {
                        binding: Binding::Configured(name.clone()),
                        name: name.as_str().to_owned(),
                        policy: profile.ipv6.as_ref().map_or_default(|ipv6| ipv6.advertise),
                        addresses: profile
                            .ipv6
                            .as_ref()
                            .map_or_else(Vec::new, |ipv6| ipv6.addresses.clone()),
                    });
                }
                Device::Gateway(gateway) => {
                    vlan_names.insert(gateway.name.as_str());
                    interfaces.push(Interface {
                        binding: Binding::Configured(gateway.name.clone()),
                        name: gateway.name.as_str().to_owned(),
                        policy: gateway.advertise,
                        addresses: gateway.ipv6.clone(),
                    });
                }
                Device::Interface(..) => {}
            }
        }
        if scope.is_none() {
            let snapshot = ManagementClient::local().snapshot()?;
            if snapshot.generation != baseline.revision {
                return Err(io::ErrorKind::Interrupted.into());
            }
            for device in snapshot.interfaces {
                let name = String::from_utf8_lossy(&device.name);
                if config.routing_domain_for_mac(device.mac).is_some()
                    || vlan_names.contains(name.as_ref())
                    || config
                        .interfaces
                        .values()
                        .any(|profile| profile.device.matches(&device.name, device.mac))
                {
                    continue;
                }
                interfaces.push(Interface {
                    name: name.into_owned(),
                    binding: Binding::Hardware {
                        name: device.name,
                        mac: device.mac,
                    },
                    policy: RouterAdvertisements::Disabled,
                    addresses: Vec::new(),
                });
            }
        }
        interfaces.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        let prefixes = interfaces
            .iter()
            .filter(|entry| entry.configured())
            .flat_map(|entry| {
                entry
                    .addresses
                    .iter()
                    .filter_map(|address| address.advertised_prefix())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .map(|network| Prefix {
                        network,
                        interface: entry.name.clone(),
                    })
            })
            .collect();
        Ok(Some(Self {
            baseline,
            forwarding,
            interfaces,
            prefixes,
        }))
    }

    pub(super) fn configured(&self) -> impl Iterator<Item = &Interface> {
        self.interfaces.iter().filter(|entry| entry.configured())
    }
}

impl Interface {
    fn configured(&self) -> bool {
        self.policy != RouterAdvertisements::Disabled
    }
}

fn missing() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "所选路由域或接口已不存在。")
}

/// 将设置应用到同域接口，或清除设置。硬件未有配置时使用系统已有的 MAC 绑定方式。
/// 错误：原接口已移除、改域、由网桥管理或硬件绑定冲突。
pub(super) fn set(
    config: &mut RouterConfiguration,
    scope: Option<&str>,
    binding: &Binding,
    policy: RouterAdvertisements,
) -> io::Result<()> {
    match binding {
        Binding::Configured(name) => match DeviceMut::find(config, scope, name)? {
            DeviceMut::Interface(profile) => set_profile(profile, policy),
            DeviceMut::Gateway(gateway) => gateway.advertise = policy,
        },
        Binding::Hardware { name, mac } => {
            if scope.is_some() || config.routing_domain_for_mac(*mac).is_some() {
                return Err(missing());
            }
            let profile = config
                .ensure_interface(name, Some(*mac))
                .map_err(|_| io::Error::other("设备绑定冲突，无法保存 ND 配置。"))?;
            set_profile(profile, policy);
        }
    }
    Ok(())
}

fn set_profile(profile: &mut RouterInterface, policy: RouterAdvertisements) {
    if let Some(ipv6) = &mut profile.ipv6 {
        ipv6.advertise = policy;
    } else if policy != RouterAdvertisements::Disabled {
        profile.ipv6 = Some(Ipv6Configuration {
            addresses: Vec::new(),
            accept_ra: Default::default(),
            advertise: policy,
        });
    }
}
