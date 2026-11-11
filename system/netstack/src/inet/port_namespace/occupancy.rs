//! 同一端口的地址/接口交集索引；计数只在一个 NET transaction 中发布，不保存第二份 lease。
use super::{PortBinding, PortError, PortLease};
use core::net::IpAddr;
use fallible_tree::{FallibleMap, NodeSlot};

#[derive(Default)]
struct Counts {
    exclusive: usize,
    reusable: usize,
    listeners: usize,
}
fn change_count(count: &mut usize, add: bool) {
    *count = if add {
        count.checked_add(1)
    } else {
        count.checked_sub(1)
    }
    .expect("port membership overflow or unowned release");
}
impl Counts {
    fn change(&mut self, reuse: bool, add: bool) {
        change_count(
            if reuse {
                &mut self.reusable
            } else {
                &mut self.exclusive
            },
            add,
        );
    }
    fn total(&self) -> usize {
        self.exclusive + self.reusable
    }
    fn conflicts(&self, reuse: bool) -> bool {
        self.listeners != 0
            || if reuse {
                self.exclusive != 0
            } else {
                self.total() != 0
            }
    }
}
#[derive(Default)]
struct FamilyOccupancy {
    wildcard: Counts,
    specific: Counts,
}
impl FamilyOccupancy {
    fn selected(&mut self, exact: bool) -> &mut Counts {
        if exact {
            &mut self.specific
        } else {
            &mut self.wildcard
        }
    }
}
/// 全局 exact node 同时含 unbound 子集；解除接口绑定时复用该 node，不能在 disconnect 追加分配。
#[derive(Default)]
struct AddressOccupancy {
    all: Counts,
    unbound: Counts,
}
#[derive(Default)]
struct DeviceOccupancy {
    families: [FamilyOccupancy; 2],
    specific: FallibleMap<IpAddr, Counts>,
}
impl DeviceOccupancy {
    fn conflicts(&self, binding: PortBinding, reuse: bool, listener: bool) -> bool {
        binding.families().any(|index| {
            let family = &self.families[index];
            let exact = match binding.address {
                Some(address) => self.specific.get(&address),
                None => Some(&family.specific),
            };
            overlaps(&family.wildcard, exact, reuse, listener)
        })
    }
    fn empty(&self) -> bool {
        self.families
            .iter()
            .all(|family| family.wildcard.total() + family.specific.total() == 0)
    }
    fn change(&mut self, binding: PortBinding, reuse: bool, add: bool, listener: bool) {
        if let Some(address) = binding.address {
            update(
                self.specific
                    .get_mut(&address)
                    .expect("device exact node was not prepared"),
                reuse,
                add,
                listener,
            );
        }
        for index in binding.families() {
            update(
                self.families[index].selected(binding.address.is_some()),
                reuse,
                add,
                listener,
            );
        }
    }
}
fn overlaps(wildcard: &Counts, exact: Option<&Counts>, reuse: bool, listener: bool) -> bool {
    if listener {
        wildcard.listeners != 0 || exact.is_some_and(|counts| counts.listeners != 0)
    } else {
        wildcard.conflicts(reuse) || exact.is_some_and(|counts| counts.conflicts(reuse))
    }
}
fn update(counts: &mut Counts, reuse: bool, add: bool, listener: bool) {
    if listener {
        change_count(&mut counts.listeners, add);
    } else {
        counts.change(reuse, add);
    }
}

#[derive(Default)]
pub(super) struct Occupancy {
    // OWNER: 全接口投影及未绑定接口的子集。每份 lease 只有一个身份；投影与设备 index 在同一提交中更新。
    // 缺失 unbound 子集会让绑定不同接口的端点错误冲突，缺失全局投影会让无接口绑定漏掉已有占用。
    families: [FamilyOccupancy; 2],
    unbound: [FamilyOccupancy; 2],
    specific: FallibleMap<IpAddr, AddressOccupancy>,
    devices: FallibleMap<u32, DeviceOccupancy>,
}
#[derive(Default)]
pub(super) struct BindingStorage {
    address: Option<NodeSlot<IpAddr, AddressOccupancy>>,
    device: Option<NodeSlot<u32, DeviceOccupancy>>,
    device_address: Option<NodeSlot<IpAddr, Counts>>,
}
impl Occupancy {
    fn overlaps(&self, binding: PortBinding, reuse: bool, listener: bool) -> bool {
        let unbound_only = binding.interface.is_some();
        let shared = binding.families().any(|index| {
            let family = if unbound_only {
                &self.unbound[index]
            } else {
                &self.families[index]
            };
            let exact = match binding.address {
                Some(address) => self.specific.get(&address).map(|counts| {
                    if unbound_only {
                        &counts.unbound
                    } else {
                        &counts.all
                    }
                }),
                None => Some(&family.specific),
            };
            overlaps(&family.wildcard, exact, reuse, listener)
        });
        shared
            || binding
                .interface
                .and_then(|index| self.devices.get(&index))
                .is_some_and(|device| device.conflicts(binding, reuse, listener))
    }
    pub(super) fn conflicts(&self, binding: PortBinding, reuse: bool) -> bool {
        self.overlaps(binding, reuse, false)
    }
    pub(super) fn listener_conflicts(&self, binding: PortBinding) -> bool {
        self.overlaps(binding, false, true)
    }
    pub(super) fn change_membership(&mut self, lease: PortLease, add: bool) {
        self.change(lease.binding, lease.reuse_address, add, false);
    }
    pub(super) fn change_listener(&mut self, binding: PortBinding, add: bool) {
        self.change(binding, false, add, true);
    }
    fn change(&mut self, binding: PortBinding, reuse: bool, add: bool, listener: bool) {
        if let Some(address) = binding.address {
            let exact = self
                .specific
                .get_mut(&address)
                .expect("exact port node was not prepared");
            update(&mut exact.all, reuse, add, listener);
            if binding.interface.is_none() {
                update(&mut exact.unbound, reuse, add, listener);
            }
        }
        for index in binding.families() {
            update(
                self.families[index].selected(binding.address.is_some()),
                reuse,
                add,
                listener,
            );
            if binding.interface.is_none() {
                update(
                    self.unbound[index].selected(binding.address.is_some()),
                    reuse,
                    add,
                    listener,
                );
            }
        }
        if let Some(index) = binding.interface {
            self.devices
                .get_mut(&index)
                .expect("bound device port node was not prepared")
                .change(binding, reuse, add, listener);
        }
    }
    pub(super) fn prepare_address(
        &self,
        binding: PortBinding,
    ) -> Result<BindingStorage, PortError> {
        let address = if binding
            .address
            .is_some_and(|address| self.specific.get(&address).is_none())
        {
            Some(FallibleMap::try_reserve_node().map_err(|_| PortError::NoMemory)?)
        } else {
            None
        };
        let device = if binding
            .interface
            .is_some_and(|index| self.devices.get(&index).is_none())
        {
            Some(FallibleMap::try_reserve_node().map_err(|_| PortError::NoMemory)?)
        } else {
            None
        };
        let device_address =
            if let (Some(index), Some(address)) = (binding.interface, binding.address) {
                if self
                    .devices
                    .get(&index)
                    .is_none_or(|device| device.specific.get(&address).is_none())
                {
                    Some(FallibleMap::try_reserve_node().map_err(|_| PortError::NoMemory)?)
                } else {
                    None
                }
            } else {
                None
            };
        Ok(BindingStorage {
            address,
            device,
            device_address,
        })
    }
    pub(super) fn install_address(&mut self, binding: PortBinding, storage: BindingStorage) {
        if let Some(slot) = storage.address {
            self.specific.commit_vacant(slot.fill(
                binding.address.expect("exact binding lost address"),
                AddressOccupancy::default(),
            ));
        }
        if let Some(slot) = storage.device {
            self.devices.commit_vacant(slot.fill(
                binding.interface.expect("scoped binding lost device"),
                DeviceOccupancy::default(),
            ));
        }
        if let Some(slot) = storage.device_address {
            self.devices
                .get_mut(&binding.interface.expect("device exact binding lost scope"))
                .expect("prepared device disappeared")
                .specific
                .commit_vacant(slot.fill(
                    binding.address.expect("device exact binding lost address"),
                    Counts::default(),
                ));
        }
    }
    pub(super) fn remove_empty_address(&mut self, binding: PortBinding) {
        if let Some(address) = binding.address {
            if self
                .specific
                .get(&address)
                .is_some_and(|counts| counts.all.total() == 0)
            {
                let counts = self
                    .specific
                    .remove(&address)
                    .expect("empty global exact node disappeared");
                assert_eq!(
                    counts.all.listeners + counts.unbound.listeners + counts.unbound.total(),
                    0,
                    "port claim outlived membership"
                );
            }
            if let Some(index) = binding.interface {
                let device = self
                    .devices
                    .get_mut(&index)
                    .expect("released device node disappeared");
                if device
                    .specific
                    .get(&address)
                    .is_some_and(|counts| counts.total() == 0)
                {
                    let counts = device
                        .specific
                        .remove(&address)
                        .expect("empty device exact node disappeared");
                    assert_eq!(counts.listeners, 0, "device listener outlived membership");
                }
            }
        }
        if let Some(index) = binding.interface
            && self.devices.get(&index).is_some_and(DeviceOccupancy::empty)
        {
            self.devices.remove(&index);
        }
    }
    pub(super) fn occupied_families(&self) -> u8 {
        self.families
            .iter()
            .enumerate()
            .fold(0, |mask, (index, family)| {
                mask | if family.wildcard.total() + family.specific.total() != 0 {
                    1 << index
                } else {
                    0
                }
            })
    }
}
