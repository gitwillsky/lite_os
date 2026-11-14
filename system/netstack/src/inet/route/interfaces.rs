//! 只借用当前域的接口。候选配置也沿同一查找路径读取设备身份。
use super::super::configuration::InterfaceChange;
use super::super::interface::NetworkInterface;
use super::{InterfaceFacts, InterfaceState, Ipv6InterfaceFacts};

pub(in crate::inet) type Ipv4Interfaces<'a> = Interfaces<'a, InterfaceFacts>;
pub(in crate::inet) type Ipv6Interfaces<'a> = Interfaces<'a, Ipv6InterfaceFacts<'a>>;

pub(in crate::inet) trait Facts<'a>: Copy + 'a {
    type Context: Copy + Default + 'a;

    fn index(self) -> u32;
    fn project(interface: &'a NetworkInterface, context: Self::Context) -> Self;
}

/// 有 RX 借用时，入接口的事实由调用者提供，其他接口仍从有序设备表读取。
#[derive(Clone, Copy)]
pub(in crate::inet) struct Interfaces<'a, F: Facts<'a>> {
    before: &'a [NetworkInterface],
    after: &'a [NetworkInterface],
    current: Option<F>,
    context: F::Context,
}

impl<'a, F: Facts<'a>> Interfaces<'a, F> {
    pub(in crate::inet) fn live(interfaces: &'a [NetworkInterface]) -> Self {
        Self {
            before: interfaces,
            after: &[],
            current: None,
            context: F::Context::default(),
        }
    }

    pub(in crate::inet) fn split(
        before: &'a [NetworkInterface],
        after: &'a [NetworkInterface],
        current: F,
    ) -> Self {
        Self {
            before,
            after,
            current: Some(current),
            context: F::Context::default(),
        }
    }

    pub(in crate::inet) fn get(self, index: u32) -> Option<F> {
        let interfaces = match self.current {
            Some(current) if index == current.index() => return Some(current),
            Some(current) if index > current.index() => self.after,
            _ => self.before,
        };
        NetworkInterface::position(interfaces, index)
            .map(|position| F::project(&interfaces[position], self.context))
    }

    pub(in crate::inet) fn iter(self) -> impl Iterator<Item = F> + 'a {
        self.scoped(None)
    }

    /// 枚举未绑定的全部接口，或直接定位指定的 scope。
    pub(in crate::inet) fn scoped(self, index: Option<u32>) -> impl Iterator<Item = F> + 'a {
        let (before, after, current) = if let Some(index) = index {
            (&[][..], &[][..], self.get(index))
        } else {
            (self.before, self.after, self.current)
        };
        before
            .iter()
            .chain(after)
            .map(move |interface| F::project(interface, self.context))
            .chain(current)
    }
}

#[derive(Clone, Copy, Default)]
pub(in crate::inet) enum Ipv4Projection<'a> {
    #[default]
    Live,
    Configured(&'a [InterfaceChange]),
    AddressChange(u32, InterfaceState),
}

impl<'a> Ipv4Interfaces<'a> {
    pub(super) fn configured(
        interfaces: &'a [NetworkInterface],
        changes: &'a [InterfaceChange],
    ) -> Self {
        Self {
            context: Ipv4Projection::Configured(changes),
            ..Self::live(interfaces)
        }
    }

    pub(super) fn address_change(
        interfaces: &'a [NetworkInterface],
        index: u32,
        state: InterfaceState,
    ) -> Self {
        Self {
            context: Ipv4Projection::AddressChange(index, state),
            ..Self::live(interfaces)
        }
    }
}

impl<'a> Facts<'a> for InterfaceFacts {
    type Context = Ipv4Projection<'a>;

    fn index(self) -> u32 {
        self.index
    }

    fn project(interface: &'a NetworkInterface, context: Self::Context) -> Self {
        let index = interface.index;
        let (state, carrier, mtu) = match context {
            Ipv4Projection::Live => return Self::capture(interface),
            Ipv4Projection::Configured(changes) => {
                let change = changes.iter().find(|change| change.index == index);
                // 准入验证配置可达性，只有执行时检查载波。
                (
                    change.map_or(interface.state, |change| change.next),
                    true,
                    change.map_or_else(|| interface.device.ip_mtu(), InterfaceChange::next_mtu),
                )
            }
            Ipv4Projection::AddressChange(changed, state) => (
                if index == changed {
                    state
                } else {
                    interface.state
                },
                interface.has_carrier(),
                interface.device.ip_mtu(),
            ),
        };
        Self {
            index,
            address: state.address,
            up: state.up,
            carrier,
            mtu,
        }
    }
}

impl<'a> Facts<'a> for Ipv6InterfaceFacts<'a> {
    type Context = ();

    fn index(self) -> u32 {
        self.index
    }

    fn project(interface: &'a NetworkInterface, (): ()) -> Self {
        Self::capture(interface)
    }
}
