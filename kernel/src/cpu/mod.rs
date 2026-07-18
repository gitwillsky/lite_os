//! @description 架构无关的 logical CPU identity、topology 与 lifecycle owner。

use alloc::{boxed::Box, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Once;

mod deferred;
pub(crate) use deferred::{DeferredWork, raise as raise_deferred, take as take_deferred};

/// @description Platform/firmware 使用的 opaque hardware CPU identity。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct HardwareCpuId(usize);

impl HardwareCpuId {
    /// @description 从已验证 platform description 构造 hardware identity。
    pub(crate) fn from_raw(raw: usize) -> Self {
        Self(raw)
    }

    /// @description 仅供 arch/platform backend 编码 firmware identity。
    pub(crate) fn raw(self) -> usize {
        self.0
    }
}

/// @description Kernel domain 使用的紧凑 logical CPU identity。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CpuId(usize);

impl CpuId {
    /// @description 获取只用于数组索引或标准 Linux CPU number 投影的值。
    pub(crate) fn index(self) -> usize {
        self.0
    }
}

/// @description 只包含 logical CPU identity 的 bounded 集合。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CpuSet(usize);

impl CpuSet {
    pub(crate) const EMPTY: Self = Self(0);

    pub(crate) fn singleton(cpu: CpuId) -> Self {
        Self(1usize << cpu.index())
    }

    pub(crate) fn insert(&mut self, cpu: CpuId) {
        self.0 |= Self::singleton(cpu).0;
    }

    pub(crate) fn remove(&mut self, cpu: CpuId) {
        self.0 &= !Self::singleton(cpu).0;
    }

    pub(crate) fn contains(self, cpu: CpuId) -> bool {
        self.0 & Self::singleton(cpu).0 != 0
    }

    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub(crate) fn iter(self) -> CpuSetIter {
        CpuSetIter(self.0)
    }

    /// @description 从 Linux native-word logical CPU bitmap 构造集合。
    ///
    /// @param bits bit N 表示 logical CPU N。
    /// @return 丢弃 topology 范围外 bit 后的集合。
    pub(crate) fn from_native_word(bits: usize) -> Self {
        Self(bits) & possible()
    }

    /// @description 投影 Linux native-word CPU mask；只允许 ABI codec 使用。
    pub(crate) fn native_word(self) -> usize {
        self.0
    }
}

impl core::ops::BitAnd for CpuSet {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

pub(crate) struct CpuSetIter(usize);

impl Iterator for CpuSetIter {
    type Item = CpuId;

    fn next(&mut self) -> Option<Self::Item> {
        if self.0 == 0 {
            return None;
        }
        let index = self.0.trailing_zeros() as usize;
        self.0 &= self.0 - 1;
        Some(CpuId(index))
    }
}

struct CpuState {
    id: CpuId,
    hardware_id: HardwareCpuId,
    online: AtomicBool,
    active: AtomicBool,
}

struct CpuTopology {
    boot: CpuId,
    states: Box<[CpuState]>,
}

// OWNER: cpu module uniquely publishes hardware/logical identity and CPU lifecycle state.
static CPU_TOPOLOGY: Once<CpuTopology> = Once::new();

/// @description 构造 logical topology 并发布 arch startup table。
///
/// @param hardware_ids platform discovery 顺序中的所有 enabled CPU identities。
/// @param boot_hardware_id 首个进入 kernel 的 hardware identity。
/// @return 无返回值。
/// @errors 空/重复/过宽 topology、boot CPU 缺失或 allocation failure 时 fail-stop。
pub(crate) fn initialize(
    hardware_ids: impl IntoIterator<Item = HardwareCpuId>,
    boot_hardware_id: HardwareCpuId,
) {
    assert!(
        CPU_TOPOLOGY.get().is_none(),
        "CPU topology initialized twice"
    );
    let mut hardware_ids = hardware_ids.into_iter().collect::<Vec<_>>();
    hardware_ids.sort_unstable();
    assert!(!hardware_ids.is_empty(), "platform contains no enabled CPU");
    assert!(
        hardware_ids.windows(2).all(|pair| pair[0] != pair[1]),
        "platform contains duplicate hardware CPU identities"
    );
    assert!(
        hardware_ids.len() <= usize::BITS as usize,
        "logical CPU set exceeds native-word capacity"
    );
    let boot_index = hardware_ids
        .binary_search(&boot_hardware_id)
        .expect("boot CPU is absent from platform topology");
    let mut states = Vec::new();
    states
        .try_reserve_exact(hardware_ids.len())
        .expect("CPU topology allocation failed");
    states.extend(
        hardware_ids
            .iter()
            .copied()
            .enumerate()
            .map(|(index, hardware_id)| CpuState {
                id: CpuId(index),
                hardware_id,
                online: AtomicBool::new(false),
                active: AtomicBool::new(false),
            }),
    );
    let topology = CPU_TOPOLOGY.call_once(|| CpuTopology {
        boot: CpuId(boot_index),
        states: states.into_boxed_slice(),
    });

    deferred::initialize(topology.states.len());

    crate::arch::cpu::initialize_startup(
        topology.states.iter().map(|state| {
            crate::arch::cpu::StartupCpu::new(state.hardware_id.raw(), state.id.index())
        }),
    );
    crate::arch::cpu::install_boot_cpu(topology.boot.index());
}

pub(crate) fn is_initialized() -> bool {
    CPU_TOPOLOGY.get().is_some()
}

fn topology() -> &'static CpuTopology {
    CPU_TOPOLOGY.wait()
}

pub(crate) fn current_id() -> CpuId {
    let index = crate::arch::cpu::current_logical_id();
    topology()
        .states
        .get(index)
        .map(|state| state.id)
        .unwrap_or_else(|| panic!("logical CPU {index} is absent from topology"))
}

/// @description 获取当前 execution context 对应的 hardware CPU identity。
///
/// @return topology 发布后从 logical identity 映射；cold boot 早期封装 arch entry identity。
pub(crate) fn executing_hardware_id() -> HardwareCpuId {
    if is_initialized() {
        hardware_id(current_id())
    } else {
        HardwareCpuId::from_raw(crate::arch::cpu::entry_identity())
    }
}

/// @description 将已验证的 logical index 映射为 CPU identity。
///
/// @param index topology 中的零基 logical index。
/// @return topology 中存在时返回对应 identity，否则返回 `None`。
pub(crate) fn id_at(index: usize) -> Option<CpuId> {
    topology().states.get(index).map(|state| state.id)
}

pub(crate) fn boot_id() -> CpuId {
    topology().boot
}

pub(crate) fn count() -> usize {
    topology().states.len()
}

pub(crate) fn possible() -> CpuSet {
    let mut cpus = CpuSet::EMPTY;
    for state in topology().states.iter() {
        cpus.insert(state.id);
    }
    cpus
}

pub(crate) fn hardware_id(cpu: CpuId) -> HardwareCpuId {
    topology()
        .states
        .get(cpu.index())
        .map(|state| state.hardware_id)
        .unwrap_or_else(|| panic!("logical CPU {} is absent", cpu.index()))
}

pub(crate) fn mark_online() {
    topology().states[current_id().index()]
        .online
        .store(true, Ordering::Release);
}

pub(crate) fn online() -> CpuSet {
    let mut cpus = CpuSet::EMPTY;
    for state in topology().states.iter() {
        if state.online.load(Ordering::Acquire) {
            cpus.insert(state.id);
        }
    }
    cpus
}

pub(crate) fn mark_active() {
    topology().states[current_id().index()]
        .active
        .store(true, Ordering::Release);
}

pub(crate) fn active() -> CpuSet {
    let mut cpus = CpuSet::EMPTY;
    for state in topology().states.iter() {
        if state.active.load(Ordering::Acquire) {
            cpus.insert(state.id);
        }
    }
    cpus
}

pub(crate) fn is_active(cpu: CpuId) -> bool {
    topology()
        .states
        .get(cpu.index())
        .is_some_and(|state| state.active.load(Ordering::Acquire))
}
