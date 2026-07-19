use core::net::Ipv4Addr;

use crate::fallible_tree::FallibleMap;

#[path = "port_namespace/bitmap.rs"]
mod bitmap;
use bitmap::EphemeralPorts;
#[cfg(test)]
pub(super) use bitmap::{EPHEMERAL_END, EPHEMERAL_START};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 一个 endpoint 在唯一 local-port namespace 中的精确 membership token。
///
/// token 记录 port、wildcard/exact address、`SO_REUSEADDR` 分类与 TCP listener
/// claim；缺少任一字段都会让 drop 从错误计数器释放，并破坏位图与 map 的单 owner 不变量。
pub(super) struct PortLease {
    port: u16,
    address: Option<Ipv4Addr>,
    reuse_address: bool,
    listener: bool,
}

impl PortLease {
    /// 返回已取得的 host-order local port。
    pub(super) const fn port(self) -> u16 {
        self.port
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// local-port membership 取得失败原因。
pub(super) enum PortError {
    /// local tuple 与现有 wildcard/exact/reuse/listener policy 冲突。
    AddressInUse,
    /// 在发布 membership 前无法预留 AVL node。
    NoMemory,
}

#[derive(Default)]
struct AddressOccupancy {
    exclusive: usize,
    reusable: usize,
    listeners: usize,
}

impl AddressOccupancy {
    fn increment(&mut self, reuse_address: bool) {
        let count = if reuse_address {
            &mut self.reusable
        } else {
            &mut self.exclusive
        };
        *count = count.checked_add(1).expect("address occupancy overflow");
    }

    fn decrement(&mut self, reuse_address: bool) {
        let count = if reuse_address {
            &mut self.reusable
        } else {
            &mut self.exclusive
        };
        *count = count
            .checked_sub(1)
            .expect("released unowned address lease");
    }

    const fn total(&self) -> usize {
        self.exclusive + self.reusable
    }
}

#[derive(Default)]
struct Occupancy {
    wildcard_exclusive: usize,
    wildcard_reusable: usize,
    specific_exclusive: usize,
    specific_reusable: usize,
    wildcard_listeners: usize,
    specific_listeners: usize,
    specific: FallibleMap<Ipv4Addr, AddressOccupancy>,
}

impl Occupancy {
    fn conflicts(&self, address: Option<Ipv4Addr>, reuse_address: bool) -> bool {
        match address {
            None if !reuse_address => self.total() != 0,
            None => self.wildcard_exclusive + self.specific_exclusive != 0,
            Some(address) => {
                let exact = self.specific.get(&address);
                if reuse_address {
                    self.wildcard_exclusive + exact.map_or(0, |occupancy| occupancy.exclusive) != 0
                } else {
                    self.wildcard_exclusive
                        + self.wildcard_reusable
                        + exact.map_or(0, AddressOccupancy::total)
                        != 0
                }
            }
        }
    }

    fn increment(&mut self, lease: PortLease) {
        match lease.address {
            None => {
                let count = if lease.reuse_address {
                    &mut self.wildcard_reusable
                } else {
                    &mut self.wildcard_exclusive
                };
                *count = count.checked_add(1).expect("port occupancy overflow");
            }
            Some(address) => {
                self.specific
                    .get_mut(&address)
                    .expect("specific port occupancy was not prepared")
                    .increment(lease.reuse_address);
                let count = if lease.reuse_address {
                    &mut self.specific_reusable
                } else {
                    &mut self.specific_exclusive
                };
                *count = count.checked_add(1).expect("port occupancy overflow");
            }
        }
    }

    fn decrement(&mut self, lease: PortLease) {
        match lease.address {
            None => {
                let count = if lease.reuse_address {
                    &mut self.wildcard_reusable
                } else {
                    &mut self.wildcard_exclusive
                };
                *count = count.checked_sub(1).expect("released unowned port lease");
            }
            Some(address) => {
                let exact = self
                    .specific
                    .get_mut(&address)
                    .expect("released port lease lost specific address");
                exact.decrement(lease.reuse_address);
                let empty = exact.total() == 0;
                let count = if lease.reuse_address {
                    &mut self.specific_reusable
                } else {
                    &mut self.specific_exclusive
                };
                *count = count.checked_sub(1).expect("released unowned port lease");
                if empty {
                    self.specific.remove(&address);
                }
            }
        }
    }

    const fn total(&self) -> usize {
        self.wildcard_exclusive
            + self.wildcard_reusable
            + self.specific_exclusive
            + self.specific_reusable
    }

    fn listener_conflicts(&self, address: Option<Ipv4Addr>) -> bool {
        match address {
            None => self.wildcard_listeners + self.specific_listeners != 0,
            Some(address) => {
                self.wildcard_listeners
                    + self
                        .specific
                        .get(&address)
                        .map_or(0, |occupancy| occupancy.listeners)
                    != 0
            }
        }
    }
}

/// accepted TCP membership 所需 AVL storage 与 exact tuple 的未发布 token。
///
/// token 析构不改变 namespace，因此 accept 可在 replacement socket 发布前安全返回 OOM。
pub(super) struct PreparedPortLease {
    lease: PortLease,
    address_slot: Option<crate::fallible_tree::NodeSlot<Ipv4Addr, AddressOccupancy>>,
}

/// active TCP connect 所需 exact-address storage 与尚未发布的 membership 迁移。
pub(super) struct PreparedPortReaddress {
    previous: PortLease,
    lease: PortLease,
    address_slot: Option<crate::fallible_tree::NodeSlot<Ipv4Addr, AddressOccupancy>>,
}

/// UDP 或 TCP local-port namespace 的唯一占用 owner。
///
/// `entries` 是 wildcard/exact/reuse/listener 语义 owner，`occupied` 仅是“该 port 是否
/// 完全空闲”的 ephemeral lookup 投影。两者只能由 acquire/retain/release 同时更新。
pub(super) struct PortNamespace {
    ephemeral: EphemeralPorts,
    entries: FallibleMap<u16, Occupancy>,
}

impl PortNamespace {
    /// 构造空 namespace，ephemeral cursor 从 Linux dynamic/private 范围起点开始。
    pub(super) const fn new() -> Self {
        Self {
            ephemeral: EphemeralPorts::new(),
            entries: FallibleMap::new(),
        }
    }

    /// 原子取得指定 local tuple membership。
    ///
    /// @param port host-order local port。
    /// @param address `None` 表示 wildcard，`Some` 表示 exact IPv4。
    /// @param reuse_address endpoint 当前 `SO_REUSEADDR` 分类。
    /// @return 成功返回必须由 endpoint 持有并精确释放的 token。
    /// @errors tuple 冲突返回 `AddressInUse`；AVL 预留失败返回 `NoMemory`，两者均不发布状态。
    pub(super) fn acquire(
        &mut self,
        port: u16,
        address: Option<Ipv4Addr>,
        reuse_address: bool,
    ) -> Result<PortLease, PortError> {
        if self
            .entries
            .get(&port)
            .is_some_and(|occupancy| occupancy.conflicts(address, reuse_address))
        {
            return Err(PortError::AddressInUse);
        }
        let outer_slot = if self.entries.get(&port).is_none() {
            Some(
                FallibleMap::<u16, Occupancy>::try_reserve_node()
                    .map_err(|_| PortError::NoMemory)?,
            )
        } else {
            None
        };
        let needs_address = address.is_some_and(|address| {
            self.entries
                .get(&port)
                .is_none_or(|occupancy| occupancy.specific.get(&address).is_none())
        });
        let address_slot = if needs_address {
            Some(
                FallibleMap::<Ipv4Addr, AddressOccupancy>::try_reserve_node()
                    .map_err(|_| PortError::NoMemory)?,
            )
        } else {
            None
        };
        if let Some(slot) = outer_slot {
            self.entries
                .commit_vacant(slot.fill(port, Occupancy::default()));
        }
        let occupancy = self
            .entries
            .get_mut(&port)
            .expect("prepared port occupancy disappeared");
        if let (Some(address), Some(slot)) = (address, address_slot) {
            occupancy
                .specific
                .commit_vacant(slot.fill(address, AddressOccupancy::default()));
        }
        let lease = PortLease {
            port,
            address,
            reuse_address,
            listener: false,
        };
        occupancy.increment(lease);
        self.ephemeral.mark_occupied(port);
        Ok(lease)
    }

    /// 从 cursor 开始按 word 查找一个完全空闲的 ephemeral port。
    ///
    /// @param address 最终 bind 的 wildcard/exact IPv4；不能将 specific port-0 降级为 wildcard。
    /// @param reuse_address endpoint 当前 `SO_REUSEADDR` 分类。
    /// @return 最多读取 257 个 bitmap word 后返回已取得 token。
    /// @errors 范围耗尽返回 `AddressInUse`；membership AVL 预留失败返回 `NoMemory`。
    pub(super) fn acquire_ephemeral(
        &mut self,
        address: Option<Ipv4Addr>,
        reuse_address: bool,
    ) -> Result<PortLease, PortError> {
        let port = self
            .ephemeral
            .next_candidate()
            .ok_or(PortError::AddressInUse)?;
        self.acquire(port, address, reuse_address)
    }

    /// 预留 accepted TCP endpoint 的 exact local tuple membership storage。
    ///
    /// @param lease listener 当前 membership，只读借用且不改变其 claim。
    /// @param address established child 的权威 local IPv4。
    /// @return 可在同一 stack owner 临界区内无分配提交的 token。
    /// @errors exact-address node OOM 返回 `NoMemory`，namespace 不变。
    pub(super) fn prepare_retain_for_address(
        &self,
        lease: PortLease,
        address: Ipv4Addr,
    ) -> Result<PreparedPortLease, PortError> {
        let needs_address = self
            .entries
            .get(&lease.port)
            .expect("retained port lease lost namespace entry")
            .specific
            .get(&address)
            .is_none();
        let address_slot = if needs_address {
            Some(
                FallibleMap::<Ipv4Addr, AddressOccupancy>::try_reserve_node()
                    .map_err(|_| PortError::NoMemory)?,
            )
        } else {
            None
        };
        Ok(PreparedPortLease {
            lease: PortLease {
                address: Some(address),
                listener: false,
                ..lease
            },
            address_slot,
        })
    }

    /// 无分配发布已预留的 accepted TCP exact membership。
    ///
    /// @param prepared 在同一 `NetworkStack` lock 周期内产生的未发布 token。
    /// @return listener claim 已清除、address 已精确化的 accepted lease。
    /// @errors 无可恢复错误；跨 owner 或中途修改 namespace 会 fail-stop，否则会发布无 owner membership。
    pub(super) fn commit_retained(&mut self, prepared: PreparedPortLease) -> PortLease {
        let PreparedPortLease {
            lease,
            address_slot,
        } = prepared;
        let occupancy = self
            .entries
            .get_mut(&lease.port)
            .expect("retained port lease lost namespace entry");
        if let Some(slot) = address_slot {
            occupancy.specific.commit_vacant(slot.fill(
                lease.address.expect("prepared exact lease lost address"),
                AddressOccupancy::default(),
            ));
        }
        occupancy.increment(lease);
        lease
    }

    /// 预留 active connect 把 wildcard membership 迁移到 exact source IPv4 的 storage。
    ///
    /// @param lease fresh TCP endpoint 当前 membership。
    /// @param address 已验证的单 interface source IPv4。
    /// @return 可在 connect 成功后无分配提交的 token。
    /// @errors exact-address node OOM 返回 `NoMemory`，原 wildcard membership 不变。
    pub(super) fn prepare_readdress(
        &self,
        lease: PortLease,
        address: Ipv4Addr,
    ) -> Result<PreparedPortReaddress, PortError> {
        let address_slot = if lease.address == Some(address)
            || self
                .entries
                .get(&lease.port)
                .expect("readdressed port lease lost namespace entry")
                .specific
                .get(&address)
                .is_some()
        {
            None
        } else {
            Some(
                FallibleMap::<Ipv4Addr, AddressOccupancy>::try_reserve_node()
                    .map_err(|_| PortError::NoMemory)?,
            )
        };
        Ok(PreparedPortReaddress {
            previous: lease,
            lease: PortLease {
                address: Some(address),
                ..lease
            },
            address_slot,
        })
    }

    /// 无分配把 active TCP membership 迁移到已选定的 exact source IPv4。
    ///
    /// @param prepared 在同一 stack owner 临界区内、connect 之前产生的 token。
    /// @return 已精确化的 lease，caller 必须回写 endpoint state。
    /// @errors 无可恢复错误；跨 owner 或重复提交会 fail-stop，否则会破坏 exact occupancy。
    pub(super) fn commit_readdress(&mut self, prepared: PreparedPortReaddress) -> PortLease {
        let PreparedPortReaddress {
            previous,
            lease,
            address_slot,
        } = prepared;
        if previous.address == lease.address {
            return lease;
        }
        let occupancy = self
            .entries
            .get_mut(&lease.port)
            .expect("readdressed port lease lost namespace entry");
        if let Some(slot) = address_slot {
            occupancy.specific.commit_vacant(slot.fill(
                lease.address.expect("prepared exact lease lost address"),
                AddressOccupancy::default(),
            ));
        }
        occupancy.decrement(previous);
        occupancy.increment(lease);
        lease
    }

    /// 把 bound TCP membership 原子提升为唯一的重叠 listener claim。
    ///
    /// @param lease 尚未 listening 的 TCP membership。
    /// @return 带 listener claim 的新 token，必须回写 endpoint state。
    /// @errors wildcard 或同 exact address 已有 listener 时返回 `AddressInUse`；不依赖 `SO_REUSEADDR`。
    pub(super) fn claim_listener(&mut self, lease: PortLease) -> Result<PortLease, PortError> {
        let occupancy = self
            .entries
            .get_mut(&lease.port)
            .expect("listener port lease lost namespace entry");
        if occupancy.listener_conflicts(lease.address) {
            return Err(PortError::AddressInUse);
        }
        match lease.address {
            None => {
                occupancy.wildcard_listeners = occupancy
                    .wildcard_listeners
                    .checked_add(1)
                    .expect("wildcard listener occupancy overflow");
            }
            Some(address) => {
                occupancy.specific_listeners = occupancy
                    .specific_listeners
                    .checked_add(1)
                    .expect("specific listener occupancy overflow");
                let exact = occupancy
                    .specific
                    .get_mut(&address)
                    .expect("listener lease lost specific address");
                exact.listeners = exact
                    .listeners
                    .checked_add(1)
                    .expect("exact listener occupancy overflow");
            }
        }
        Ok(PortLease {
            listener: true,
            ..lease
        })
    }

    /// 回滚尚未发布到 endpoint mode 的 listener claim。
    ///
    /// @param lease `claim_listener` 返回但尚未写入 endpoint 的 token。
    /// @return 无返回值；base membership 仍存在，需要时由 caller 另行 release。
    /// @errors token 不属于该 namespace 时 fail-stop，避免静默损坏 listener 计数。
    pub(super) fn release_listener_claim(&mut self, lease: PortLease) {
        let occupancy = self
            .entries
            .get_mut(&lease.port)
            .expect("listener port lease lost namespace entry");
        match lease.address {
            None => {
                occupancy.wildcard_listeners = occupancy
                    .wildcard_listeners
                    .checked_sub(1)
                    .expect("released unowned wildcard listener");
            }
            Some(address) => {
                occupancy.specific_listeners = occupancy
                    .specific_listeners
                    .checked_sub(1)
                    .expect("released unowned specific listener");
                let exact = occupancy
                    .specific
                    .get_mut(&address)
                    .expect("listener lease lost specific address");
                exact.listeners = exact
                    .listeners
                    .checked_sub(1)
                    .expect("released unowned exact listener");
            }
        }
    }

    /// 更新已绑定 endpoint 的 `SO_REUSEADDR` classification。
    ///
    /// @param lease endpoint 当前持有的 token。
    /// @param reuse_address 新分类。
    /// @return 已切换分类的 token，caller 必须回写，否则 drop 会从错误计数器释放。
    /// @errors 无可恢复错误；不属于该 namespace 的 token 会 fail-stop。
    pub(super) fn set_reuse(&mut self, lease: PortLease, reuse_address: bool) -> PortLease {
        if lease.reuse_address == reuse_address {
            return lease;
        }
        let occupancy = self
            .entries
            .get_mut(&lease.port)
            .expect("reclassified port lease lost namespace entry");
        match lease.address {
            None => {
                let old = if lease.reuse_address {
                    &mut occupancy.wildcard_reusable
                } else {
                    &mut occupancy.wildcard_exclusive
                };
                *old = old.checked_sub(1).expect("reclassified unowned lease");
                let new = if reuse_address {
                    &mut occupancy.wildcard_reusable
                } else {
                    &mut occupancy.wildcard_exclusive
                };
                *new = new.checked_add(1).expect("port occupancy overflow");
            }
            Some(address) => {
                let exact = occupancy
                    .specific
                    .get_mut(&address)
                    .expect("reclassified lease lost specific address");
                exact.decrement(lease.reuse_address);
                exact.increment(reuse_address);
                let old = if lease.reuse_address {
                    &mut occupancy.specific_reusable
                } else {
                    &mut occupancy.specific_exclusive
                };
                *old = old.checked_sub(1).expect("reclassified unowned lease");
                let new = if reuse_address {
                    &mut occupancy.specific_reusable
                } else {
                    &mut occupancy.specific_exclusive
                };
                *new = new.checked_add(1).expect("port occupancy overflow");
            }
        }
        PortLease {
            reuse_address,
            ..lease
        }
    }

    /// 精确释放 endpoint 持有的一份 membership。
    ///
    /// @param lease 从 acquire/commit/set_reuse/claim 最后返回并由该 endpoint 持有的 token。
    /// @return 无返回值；最后一份会同时清除 bitmap 与 map entry。
    /// @errors 重复释放或过期 token 会 fail-stop，防止把仍在使用的 port 标记为空闲。
    pub(super) fn release(&mut self, lease: PortLease) {
        if lease.listener {
            self.release_listener_claim(lease);
        }
        let empty = {
            let occupancy = self
                .entries
                .get_mut(&lease.port)
                .expect("released port lease lost namespace entry");
            occupancy.decrement(lease);
            occupancy.total() == 0
        };
        if empty {
            self.entries.remove(&lease.port);
            self.ephemeral.clear_occupied(lease.port);
        }
    }
}
