//! 一个路由域内能够承载三层服务的已配置接口和 VLAN 网关。

use network_config::configuration::{
    Bridge, ConfigurationName as Name, RouterConfiguration, RouterInterface, VlanGateway,
};
use std::{collections::BTreeMap, io};

pub(crate) struct Layer3<'a> {
    interfaces: &'a BTreeMap<Name, RouterInterface>,
    bridges: &'a BTreeMap<Name, Bridge>,
}

pub(crate) enum Device<'a> {
    Interface(&'a Name, &'a RouterInterface),
    Gateway(&'a VlanGateway),
}

pub(crate) enum DeviceMut<'a> {
    Interface(&'a mut RouterInterface),
    Gateway(&'a mut VlanGateway),
}

impl<'a> Layer3<'a> {
    /// 读取默认域或具名域的配置对象。错误：所选域已删除。
    pub(crate) fn new(config: &'a RouterConfiguration, domain: Option<&str>) -> io::Result<Self> {
        let (interfaces, bridges) = match domain {
            Some(name) => {
                let domain = config.routing_domains.get(name).ok_or_else(scope_missing)?;
                (&domain.interfaces, &domain.bridges)
            }
            None => (&config.interfaces, &config.bridges),
        };
        Ok(Self {
            interfaces,
            bridges,
        })
    }

    /// 物理接口排除二层网桥成员，VLAN 三层配置由网关记录持有。
    pub(crate) fn devices(&self) -> impl Iterator<Item = Device<'a>> {
        self.interfaces
            .iter()
            .filter_map(|(name, profile)| {
                (!has_bridge_port(self.bridges, name)).then_some(Device::Interface(name, profile))
            })
            .chain(
                self.bridges
                    .values()
                    .flat_map(|bridge| bridge.gateways.values())
                    .map(Device::Gateway),
            )
    }
}

impl<'a> DeviceMut<'a> {
    /// 在同一域和相同条件下定位欲修改的接口。错误：设备已移除、改域或成为二层网桥成员。
    pub(crate) fn find(
        config: &'a mut RouterConfiguration,
        domain: Option<&str>,
        name: &Name,
    ) -> io::Result<Self> {
        let (interfaces, bridges) = match domain {
            Some(name) => {
                let domain = config
                    .routing_domains
                    .get_mut(name)
                    .ok_or_else(scope_missing)?;
                (&mut domain.interfaces, &mut domain.bridges)
            }
            None => (&mut config.interfaces, &mut config.bridges),
        };
        if let Some(profile) = interfaces.get_mut(name)
            && !has_bridge_port(bridges, name)
        {
            return Ok(Self::Interface(profile));
        }
        bridges
            .values_mut()
            .flat_map(|bridge| bridge.gateways.values_mut())
            .find(|gateway| &gateway.name == name)
            .map(Self::Gateway)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "所选三层接口已不存在。"))
    }
}

fn has_bridge_port(bridges: &BTreeMap<Name, Bridge>, name: &Name) -> bool {
    bridges
        .values()
        .any(|bridge| bridge.ports.contains_key(name))
}

fn scope_missing() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "所选路由域已删除。")
}
