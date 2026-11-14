use fallible_tree::FallibleMap;

mod binding;
mod bitmap;
mod occupancy;
pub(super) use binding::PortBinding;
use bitmap::EphemeralPorts;
pub(super) use bitmap::{EPHEMERAL_END, EPHEMERAL_START};
use occupancy::{BindingStorage, Occupancy};

/// endpoint 在唯一 local-port namespace 中的精确 membership token。
///
/// token 同时保留地址空间、SO_REUSEADDR 分类与 listener claim；不能把双栈绑定拆为两份租约。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PortLease {
    port: u16,
    binding: PortBinding,
    reuse_address: bool,
    listener: bool,
}

impl PortLease {
    /// 返回已取得的 host-order local port。
    pub(super) const fn port(self) -> u16 {
        self.port
    }
    /// 当前 membership 的完整地址/接口身份，只供同一 owner 的 policy 迁移准备使用。
    pub(super) const fn binding(self) -> PortBinding {
        self.binding
    }
}

/// local-port membership 取得失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PortError {
    /// local tuple 与现有 wildcard/exact/reuse/listener policy 冲突。
    AddressInUse,
    /// V6ONLY 不允许绑定 IPv4-mapped 地址。
    InvalidBinding,
    /// 在发布 membership 前无法预留 AVL node。
    NoMemory,
}

/// accepted TCP membership 的未发布 token；析构不改变 namespace。
pub(super) struct PreparedPortLease {
    lease: PortLease,
    storage: BindingStorage,
}

/// 地址精确化或接口 policy 迁移所需存储；发布前可无副作用放弃。
pub(super) struct PreparedPortReaddress {
    previous: PortLease,
    lease: PortLease,
    storage: BindingStorage,
}

/// UDP 或 TCP 双族 local-port namespace 的唯一占用 owner。
///
/// entries 拥有地址/reuse/listener membership；ephemeral 只是端口完全空闲的位图投影。
/// IPv4、IPv6-only 和双栈绑定共用一次原子准入，不建立需要跨 owner 回滚的第二份租约。
pub(super) struct PortNamespace {
    ephemeral: EphemeralPorts,
    entries: FallibleMap<u16, Occupancy>,
}

impl PortNamespace {
    /// 构造空 namespace；ephemeral cursor 从 dynamic/private 范围起点开始。
    pub(super) const fn new() -> Self {
        Self {
            ephemeral: EphemeralPorts::new(),
            entries: FallibleMap::new(),
        }
    }

    /// 原子取得指定端口与地址空间的 membership。
    ///
    /// # Parameters
    /// - `port`: host-order 非零本地端口，由调用者处理 port 0。
    /// - `binding`: 规范化的通配或精确地址空间。
    /// - `reuse_address`: endpoint 当前 SO_REUSEADDR 分类。
    /// # Returns
    /// 成功返回由 endpoint 持有并精确释放的唯一 token。
    /// # Errors
    /// 冲突返回 AddressInUse；节点 OOM 返回 NoMemory，均不发布状态。
    pub(super) fn acquire(
        &mut self,
        port: u16,
        binding: PortBinding,
        reuse_address: bool,
    ) -> Result<PortLease, PortError> {
        assert_ne!(port, 0, "port 0 must use ephemeral admission");
        if self
            .entries
            .get(&port)
            .is_some_and(|occupancy| occupancy.conflicts(binding, reuse_address))
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
        let empty = Occupancy::default();
        let storage = self
            .entries
            .get(&port)
            .unwrap_or(&empty)
            .prepare_address(binding)?;
        // 1. 地址与设备索引节点先全部准备，避免 OOM 留下无 membership 的外层端口。
        // 2. 以下仅无失败发布，位图最后更新；双栈通配不需要第二次 acquire。
        if let Some(slot) = outer_slot {
            self.entries
                .commit_vacant(slot.fill(port, Occupancy::default()));
        }
        let occupancy = self
            .entries
            .get_mut(&port)
            .expect("prepared port disappeared");
        occupancy.install_address(binding, storage);
        let lease = PortLease {
            port,
            binding,
            reuse_address,
            listener: false,
        };
        occupancy.change_membership(lease, true);
        self.publish_port(port);
        Ok(lease)
    }

    /// 从 cursor 开始最多读 257 个 bitmap word，取得完全空闲的 ephemeral 端口。
    ///
    /// # Parameters
    /// - `binding`: 最终本地地址空间，不将精确 port-0 绑定降级为通配。
    /// - `reuse_address`: 当前 SO_REUSEADDR 分类。
    /// # Returns
    /// 与显式端口相同的唯一 membership token。
    /// # Errors
    /// 无空闲端口返回 AddressInUse；预留失败返回 NoMemory，不发布 membership。
    pub(super) fn acquire_ephemeral(
        &mut self,
        binding: PortBinding,
        reuse_address: bool,
    ) -> Result<PortLease, PortError> {
        self.acquire_ephemeral_matching(binding, reuse_address, |_| true)
    }

    /// 分配本地未占用且通过线上连接冲突检查的端口。
    /// # Errors
    /// 无可用端口返回 AddressInUse，租约存储不足返回 NoMemory。
    pub(super) fn acquire_ephemeral_matching(
        &mut self,
        binding: PortBinding,
        reuse_address: bool,
        available: impl FnMut(u16) -> bool,
    ) -> Result<PortLease, PortError> {
        let port = self
            .ephemeral
            .next_candidate(binding.families, available)
            .ok_or(PortError::AddressInUse)?;
        self.acquire(port, binding, reuse_address)
    }

    /// 返回已绑定 socket 是否覆盖该本机地址和端口，包括通配与任何接口作用域。
    pub(super) fn covers(&self, address: core::net::IpAddr, port: u16) -> bool {
        self.entries.get(&port).is_some_and(|occupancy| {
            PortBinding::new(address, false)
                .is_ok_and(|binding| occupancy.conflicts(binding, false))
        })
    }

    /// 预留 accepted TCP child 的精确 membership；只在同一 NET guard 内准备和提交。
    ///
    /// # Parameters
    /// - `lease`: 存活 listener 的 membership。
    /// - `binding`: listener 地址空间内的实际精确本地地址。
    /// # Returns
    /// 可无分配提交的 token；丢弃 token 不发布 membership。
    /// # Errors
    /// 精确节点 OOM 返回 NoMemory；调用者传入不属于 listener 的地址会 fail-stop。
    pub(super) fn prepare_retain_for_address(
        &self,
        lease: PortLease,
        binding: PortBinding,
    ) -> Result<PreparedPortLease, PortError> {
        assert!(
            lease.binding.contains(binding),
            "child binding is outside parent address space"
        );
        let occupancy = self
            .entries
            .get(&lease.port)
            .expect("retained port disappeared");
        Ok(PreparedPortLease {
            lease: PortLease {
                binding,
                listener: false,
                ..lease
            },
            storage: occupancy.prepare_address(binding)?,
        })
    }

    /// 无分配发布 prepared child；token 必须来自同一 owner guard，返回需精确释放的 child 租约。
    pub(super) fn commit_retained(&mut self, prepared: PreparedPortLease) -> PortLease {
        let PreparedPortLease { lease, storage } = prepared;
        let occupancy = self
            .entries
            .get_mut(&lease.port)
            .expect("retained port disappeared");
        occupancy.install_address(lease.binding, storage);
        occupancy.change_membership(lease, true);
        lease
    }

    /// 为 connect 将本地地址空间精确化预留节点；尚不改变原 membership。
    ///
    /// # Parameters
    /// - `lease`: 尚未监听的 endpoint 当前 membership。
    /// - `binding`: 已准入的精确本地地址，必须属于原地址空间。
    /// # Returns
    /// 可在 connect 成功后同 guard 无分配提交的 token。
    /// # Errors
    /// OOM 返回 NoMemory；不合法迁移会 fail-stop，避免借源地址选择扩大绑定权限。
    pub(super) fn prepare_readdress(
        &self,
        lease: PortLease,
        binding: PortBinding,
    ) -> Result<PreparedPortReaddress, PortError> {
        assert!(
            !lease.listener && lease.binding.contains(binding),
            "invalid connect binding refinement"
        );
        self.prepare_rebind(lease, binding)
    }

    /// 为同一端口的 policy/地址空间迁移准备存储，不重新准入已拥有的 membership。
    /// # Parameters
    /// `lease` 是当前租约，`binding` 由调用者的绑定锁、地址和接口授权验证；可用于已监听端点。
    /// # Returns
    /// 未发布的迁移 token；原租约和 listener claim 均不改变。
    /// # Errors
    /// 任一索引预留 OOM 返回 NoMemory，所有已预留存储自动释放。
    pub(super) fn prepare_rebind(
        &self,
        lease: PortLease,
        binding: PortBinding,
    ) -> Result<PreparedPortReaddress, PortError> {
        let occupancy = self
            .entries
            .get(&lease.port)
            .expect("rebound port disappeared");
        Ok(PreparedPortReaddress {
            previous: lease,
            lease: PortLease { binding, ..lease },
            storage: occupancy.prepare_address(binding)?,
        })
    }

    /// 无分配提交 prepared binding 迁移；原租约被消费，返回同一端口的新 membership。
    pub(super) fn commit_readdress(&mut self, prepared: PreparedPortReaddress) -> PortLease {
        let PreparedPortReaddress {
            previous,
            lease,
            storage,
        } = prepared;
        if previous.binding == lease.binding {
            return lease;
        }
        let occupancy = self
            .entries
            .get_mut(&lease.port)
            .expect("readdressed port disappeared");
        occupancy.install_address(lease.binding, storage);
        if previous.listener {
            occupancy.change_listener(previous.binding, false);
        }
        occupancy.change_membership(previous, false);
        occupancy.change_membership(lease, true);
        if lease.listener {
            occupancy.change_listener(lease.binding, true);
        }
        occupancy.remove_empty_address(previous.binding);
        self.publish_port(lease.port);
        lease
    }

    /// connect 失败或 UDP disconnect 恢复原通配绑定，不分配且不释放端口身份。
    ///
    /// # Parameters
    /// - `lease`: 当前非 listener membership。
    /// - `binding`: 原先已取得的通配地址空间，或不变的精确绑定。
    /// # Returns
    /// 已恢复的唯一租约；调用者必须同步回写实际 endpoint tuple。
    /// # Errors
    /// 无可恢复错误；扩大到非原绑定的地址空间是调用者违反 inverse 契约。
    pub(super) fn restore_bound_address(
        &mut self,
        lease: PortLease,
        binding: PortBinding,
    ) -> PortLease {
        assert!(
            !lease.listener,
            "connected endpoint cannot retain listener claim"
        );
        if binding == lease.binding {
            return lease;
        }
        assert!(
            (binding.interface.is_none() || binding.interface == lease.binding.interface)
                && binding.families & lease.binding.families == lease.binding.families
                && (binding.address.is_none() || binding.address == lease.binding.address),
            "restore must invert an address/interface refinement"
        );
        let restored = PortLease { binding, ..lease };
        let occupancy = self
            .entries
            .get_mut(&lease.port)
            .expect("restored binding lost port owner");
        occupancy.change_membership(lease, false);
        occupancy.change_membership(restored, true);
        occupancy.remove_empty_address(lease.binding);
        self.publish_port(lease.port);
        restored
    }

    /// listener 停止时释放 claim，但保留原本地端口 membership。
    pub(super) fn stop_listener(&mut self, lease: PortLease) -> PortLease {
        assert!(lease.listener, "stopped endpoint lost listener claim");
        self.release_listener_claim(lease);
        PortLease {
            listener: false,
            ..lease
        }
    }

    /// 将 bound TCP membership 提升为不与已有 listener 重叠的 claim。
    ///
    /// # Parameters
    /// - `lease`: 当前尚未 listening 的绑定。
    /// # Returns
    /// 带 claim 的新 token，必须回写 endpoint state。
    /// # Errors
    /// 即便两端 REUSEADDR，地址空间内已有 listener 仍返回 AddressInUse；不实现 REUSEPORT。
    pub(super) fn claim_listener(&mut self, lease: PortLease) -> Result<PortLease, PortError> {
        assert!(!lease.listener, "listener claim acquired twice");
        let occupancy = self
            .entries
            .get_mut(&lease.port)
            .expect("listener port disappeared");
        if occupancy.listener_conflicts(lease.binding) {
            return Err(PortError::AddressInUse);
        }
        occupancy.change_listener(lease.binding, true);
        Ok(PortLease {
            listener: true,
            ..lease
        })
    }

    /// 回滚尚未发布的 listener claim；base membership 保留，由调用者继续持有或释放。
    pub(super) fn release_listener_claim(&mut self, lease: PortLease) {
        assert!(lease.listener, "released token has no listener claim");
        self.entries
            .get_mut(&lease.port)
            .expect("listener port disappeared")
            .change_listener(lease.binding, false);
    }

    /// 原位切换 REUSEADDR 分类，保留 listener claim 与精确节点，不分配或重新准入。
    pub(super) fn set_reuse(&mut self, lease: PortLease, reuse_address: bool) -> PortLease {
        if lease.reuse_address == reuse_address {
            return lease;
        }
        let reclassified = PortLease {
            reuse_address,
            ..lease
        };
        let occupancy = self
            .entries
            .get_mut(&lease.port)
            .expect("reclassified port disappeared");
        occupancy.change_membership(lease, false);
        occupancy.change_membership(reclassified, true);
        reclassified
    }

    /// 精确释放 endpoint 最后返回的 token；最后一份租约同时退休端口节点和空闲位图投影。
    pub(super) fn release(&mut self, lease: PortLease) {
        if lease.listener {
            self.release_listener_claim(lease);
        }
        let occupancy = self
            .entries
            .get_mut(&lease.port)
            .expect("released port disappeared");
        occupancy.change_membership(lease, false);
        occupancy.remove_empty_address(lease.binding);
        if occupancy.occupied_families() == 0 {
            self.entries.remove(&lease.port);
        }
        self.publish_port(lease.port);
    }

    /// 位图只投影当前 family counts；地址精确化可能释放一族，但不退休另一族的端口租约。
    fn publish_port(&mut self, port: u16) {
        let families = self
            .entries
            .get(&port)
            .map_or(0, Occupancy::occupied_families);
        self.ephemeral.publish(port, families);
    }
}
