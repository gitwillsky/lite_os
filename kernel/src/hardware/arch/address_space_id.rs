//! 静态 MMU backend 共用的 generation ASID owner；硬件编码和 local fence 仍归 backend。

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use spin::Mutex;

const GENERATION_STEP: u64 = 1 << 16;
const ID_MASK: u64 = GENERATION_STEP - 1;
const RETIRED: u64 = u64::MAX;

/// Page-table owner 持有的软件 identity；硬件 ID 在首次 activation 时领取。
#[derive(Debug)]
pub(super) struct Context {
    identity: AtomicU64,
    // CPU-local publication footprint；重编号先清零再发布 identity。旧 activation 的迟到
    // fetch_or 可跨代，但该 CPU 已被 capture 且 active=0/pending=true，下一次必执行 full fence。
    seen: AtomicUsize,
}

impl Context {
    /// 在 task context 为新 root 准备 identity；OOM 不发布或领取硬件 ID。
    pub(super) fn try_new() -> Result<Arc<Self>, alloc::alloc::AllocError> {
        Arc::try_new(Self {
            identity: AtomicU64::new(0),
            seen: AtomicUsize::new(0),
        })
    }

    /// 仅在 root owner 已完成 all-CPU retirement fence 后调用；不提前复用 bitmap 位。
    pub(super) fn retire_after_global_fence(&self) {
        assert_ne!(
            self.identity.swap(RETIRED, Ordering::AcqRel),
            RETIRED,
            "ASID context retired twice"
        );
    }
}

/// activation 必须完成的 local translation publication consequence。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flush {
    All,
    Identifier(usize),
}

struct CpuReservation {
    identity: u64,
    flush_pending: bool,
}

struct Reservations {
    bitmap: Vec<usize>,
    cpus: Vec<CpuReservation>,
    cursor: usize,
}

/// 唯一 hardware-ID/generation owner；所有 storage 在 boot 期间、IRQ lock 外分配。
pub(super) struct Allocator {
    capacity: usize,
    generation: AtomicU64,
    active: Vec<AtomicU64>,
    reservations: Mutex<Reservations>,
}

impl Allocator {
    /// 从已验证硬件容量与真实 logical CPU topology 构造 allocator。
    ///
    /// 要求非零 ID 数量超过两倍 possible CPU；这是 tagged ASID profile 的 boot 条件。
    /// storage OOM 返回错误；无效硬件事实 fail-stop。
    pub(super) fn try_new(
        capacity: usize,
        cpu_count: usize,
    ) -> Result<Self, alloc::alloc::AllocError> {
        assert!(capacity.is_power_of_two() && capacity <= GENERATION_STEP as usize);
        assert!(
            cpu_count > 0 && cpu_count <= usize::BITS as usize && capacity > 2 * cpu_count,
            "hardware ASIDs cannot cover possible CPU reservations"
        );
        let mut bitmap = Vec::new();
        bitmap
            .try_reserve_exact(capacity.div_ceil(usize::BITS as usize))
            .map_err(|_| alloc::alloc::AllocError)?;
        bitmap.resize(capacity.div_ceil(usize::BITS as usize), 0);
        bitmap[0] = 1;
        let mut active = Vec::new();
        active
            .try_reserve_exact(cpu_count)
            .map_err(|_| alloc::alloc::AllocError)?;
        active.extend((0..cpu_count).map(|_| AtomicU64::new(0)));
        let mut cpus = Vec::new();
        cpus.try_reserve_exact(cpu_count)
            .map_err(|_| alloc::alloc::AllocError)?;
        cpus.extend((0..cpu_count).map(|_| CpuReservation {
            identity: 0,
            flush_pending: true,
        }));
        Ok(Self {
            capacity,
            generation: AtomicU64::new(GENERATION_STEP),
            active,
            reservations: Mutex::new(Reservations {
                bitmap,
                cpus,
                cursor: 1,
            }),
        })
    }

    /// 在 local IRQ-disabled transfer window 解析 live root 的非零硬件 ASID。
    ///
    /// 同一 CPU 从返回到硬件安装期间不得调度；caller 保活 page-table root。
    /// `flush` 是静态 backend 的 bounded local fence，不得分配、获取领域锁或阻塞。
    /// 快路仅执行 atomics；慢路持 reservations spinlock（caller 已屏蔽 local IRQ），不分配且不远程等待。
    pub(super) fn activate(
        &self,
        context: &Context,
        cpu: usize,
        mut flush: impl FnMut(Flush),
    ) -> usize {
        let active = &self.active[cpu];
        let mut identity = context.identity.load(Ordering::Acquire);
        assert_ne!(identity, RETIRED, "retired root activated");
        let old_active = active.load(Ordering::Acquire);
        if old_active != 0
            && identity & !ID_MASK == self.generation.load(Ordering::Acquire)
            && active
                .compare_exchange(old_active, identity, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            // Rollover 或 capture 了此次 CAS 的 identity 并保留其编号，或先把 active 清零
            // 使 CAS 失败进入慢路；因此返回到硬件安装期间该编号不会给另一个 live root。
            self.publish_local(context, cpu, identity, &mut flush);
            return (identity & ID_MASK) as usize;
        }
        let mut state = self.reservations.lock();
        identity = context.identity.load(Ordering::Acquire);
        assert_ne!(identity, RETIRED, "retired root activated");
        if identity & !ID_MASK != self.generation.load(Ordering::Acquire) {
            identity = self.assign(&mut state, identity);
            context.seen.store(0, Ordering::Relaxed);
            context.identity.store(identity, Ordering::Release);
        }
        if state.cpus[cpu].flush_pending {
            flush(Flush::All);
            state.cpus[cpu].flush_pending = false;
            context.seen.fetch_or(1usize << cpu, Ordering::Release);
        } else {
            self.publish_local(context, cpu, identity, &mut flush);
        }
        active.store(identity, Ordering::Release);
        (identity & ID_MASK) as usize
    }

    fn publish_local(
        &self,
        context: &Context,
        cpu: usize,
        identity: u64,
        flush: &mut impl FnMut(Flush),
    ) {
        let bit = 1usize << cpu;
        if context.seen.load(Ordering::Acquire) & bit == 0 {
            flush(Flush::Identifier((identity & ID_MASK) as usize));
            context.seen.fetch_or(bit, Ordering::Release);
        }
    }

    fn assign(&self, state: &mut Reservations, old: u64) -> u64 {
        let generation = self.generation.load(Ordering::Relaxed);
        if old != 0 {
            let renewed = generation | (old & ID_MASK);
            let mut reserved = false;
            // 一个 mm 可同时在多 CPU 上活跃；必须更新所有匹配 reservation，否则下一轮
            // capture 会丢失仍在执行的旧 identity，并把其硬件 ID 交给其他 root。
            for cpu in &mut state.cpus {
                if cpu.identity == old {
                    cpu.identity = renewed;
                    reserved = true;
                }
            }
            if reserved || state.claim((old & ID_MASK) as usize) {
                return renewed;
            }
        }
        if let Some(identifier) = state.allocate(self.capacity) {
            return generation | identifier as u64;
        }
        let generation = generation
            .checked_add(GENERATION_STEP)
            .filter(|value| *value != !ID_MASK)
            .expect("ASID generation exhausted");
        self.generation.store(generation, Ordering::Release);
        state.bitmap.fill(0);
        state.bitmap[0] = 1;
        state.cursor = 1;
        for (active, cpu) in self.active.iter().zip(&mut state.cpus) {
            let captured = active.swap(0, Ordering::AcqRel);
            // 多次 rollover 之间未切换的 CPU 仍在使用上轮 reservation；不可将其遗忘。
            if captured != 0 {
                cpu.identity = captured;
            }
            let identifier = (cpu.identity & ID_MASK) as usize;
            state.bitmap[identifier / usize::BITS as usize] |=
                1usize << (identifier % usize::BITS as usize);
            cpu.flush_pending = true;
        }
        generation
            | state
                .allocate(self.capacity)
                .expect("ASID reservations exceed hardware capacity") as u64
    }
}

impl Reservations {
    fn claim(&mut self, identifier: usize) -> bool {
        let bit = 1usize << (identifier % usize::BITS as usize);
        let word = &mut self.bitmap[identifier / usize::BITS as usize];
        let available = *word & bit == 0;
        *word |= bit;
        available
    }

    fn allocate(&mut self, capacity: usize) -> Option<usize> {
        let bits = usize::BITS as usize;
        for index in self.cursor / bits..self.bitmap.len() {
            let first = self.cursor.saturating_sub(index * bits).min(bits);
            if first == bits {
                continue;
            }
            let usable = (capacity - index * bits).min(bits);
            let available =
                !self.bitmap[index] & (usize::MAX >> (bits - usable)) & (usize::MAX << first);
            if available != 0 {
                let identifier = index * bits + available.trailing_zeros() as usize;
                assert!(self.claim(identifier));
                self.cursor = identifier + 1;
                return Some(identifier);
            }
        }
        None
    }
}
