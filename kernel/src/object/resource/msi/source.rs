//! 已绑定 MSI-X 表项的唯一写 owner；参照 Fuchsia，源 mask 不依赖目标 CPU 的本地控制器寄存器。
use crate::execution::sync::IrqMutex;
use crate::object::vm_status;
use crate::vm::{KERNEL_SPACE, Vmo};
use core::sync::atomic::{AtomicI64, Ordering};
use mmio::MmioBus;
use native_abi::Status;
use pci_config::msix_table::{ADDRESS_HIGH, ADDRESS_LOW, CONTROL, DATA, ENTRY_BYTES, MASK};

/// 单一表项；退休后不再访问硬件，即使 deferred work 仍暂时保活 InterruptShared。
pub(in crate::object::resource) struct Source {
    // OWNER: IRQ、ack、初始使能、重定位与退休互斥；Retired 防止旧 shared 析构改写下一会话的表项。
    // 锁内只有固定 dword 访问与时间戳发布，不分配/睡眠；不得持本锁调用 platform（GIC → Source）。
    state: IrqMutex<State>,
}
enum State {
    Masked(MmioBus),
    Armed(MmioBus),
    // OWNER: 消息值已经在 mask 下写回，平台正提交新路由；ack/IRQ 只更新提交完成后的使能意图。
    // 缺失时并发 ack 可在地址/数据分开写入期间打开源，将 MSI 发往拼接出的错误目标。
    Updating { table: MmioBus, unmask: bool },
    Retired,
}
impl Source {
    /// 验证 Device VMO 并准备 masked 消息值；尚未绑定 Interrupt 时绝不使能源。
    /// # Errors
    /// 错误对象/对齐、越界或内核页表分配失败；失败不发表可发消息的表项。
    pub(in crate::object::resource) fn prepare(
        vmo: &Vmo,
        offset: usize,
        address: u64,
        data: u32,
    ) -> Result<Self, Status> {
        if !offset.is_multiple_of(8) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let physical = vmo.device_range(offset, ENTRY_BYTES).map_err(vm_status)?;
        // 只投影已授权的非 RAM 物理窗口。内核 direct-map Device PTE 与静态 BAR 同机器寿命，
        // 后续会话重用同一映射，不逐 IRQ 分配 VMA；不宣称支持 BAR 重分配/物理热移除。
        let base = KERNEL_SPACE
            .wait()
            .lock()
            .map_device(physical)
            .map_err(|()| native_abi::ERR_NO_MEMORY)?;
        let table = MmioBus::new(base, ENTRY_BYTES).expect("validated MSI-X entry mapping");
        set_mask(table, true);
        write_message(table, address, data);
        Ok(Self {
            state: IrqMutex::new(State::Masked(table)),
        })
    }

    /// binding 发布后解除初始 mask；若已有 pending 通知，则留给接收者 ack。
    pub(in crate::object::resource) fn activate(&self, triggered: &AtomicI64) {
        let mut state = self.state.lock();
        if let State::Masked(table) = *state
            && triggered.load(Ordering::Acquire) == 0
        {
            set_mask(table, false);
            *state = State::Armed(table);
        }
    }

    /// 硬 IRQ 内屏蔽源并发布时间戳；与初始 activate 同锁，不能先 mask 后又被初始化误开启。
    pub(in crate::object::resource) fn trigger(
        &self,
        timestamp: i64,
        triggered: &AtomicI64,
    ) -> bool {
        let mut state = self.state.lock();
        match &mut *state {
            State::Armed(table) => {
                let table = *table;
                set_mask(table, true);
                *state = State::Masked(table);
            }
            State::Masked(_) => {}
            State::Updating { unmask, .. } => *unmask = false,
            State::Retired => return false,
        }
        triggered.store(timestamp, Ordering::Release);
        true
    }

    /// port 队列空间已预留后重新允许消息；退休对象保持 mask。
    pub(in crate::object::resource) fn ack(&self) {
        let mut state = self.state.lock();
        match &mut *state {
            State::Masked(table) => {
                let table = *table;
                set_mask(table, false);
                *state = State::Armed(table);
            }
            State::Updating { unmask, .. } => *unmask = true,
            State::Armed(_) | State::Retired => {}
        }
    }

    /// 在源 mask 下改写消息，锁外提交控制器，最后恢复 IRQ/ack 决定的使能状态。
    ///
    /// # Parameters
    /// - `address`/`data`: 已经完成 BTI 映射和平台准备的新目标。
    /// - `commit`: 只调用一次的无可恢复失败提交；调用方持有覆盖本方法全程的迁移冷锁。
    ///
    /// # Panics
    /// 并发重定位/退休违反调用方的冷锁与 Interrupt 强引用契约；控制器超时必须 fail-stop。
    pub(in crate::object::resource) fn retarget(
        &self,
        address: u64,
        data: u32,
        commit: impl FnOnce(),
    ) {
        // 1. 所有可失败准备已结束；先排空 mask 写，再改地址/数据，硬件不能观察中间消息。
        let mut state = self.state.lock();
        let (table, unmask) = match *state {
            State::Armed(table) => (table, true),
            State::Masked(table) => (table, false),
            State::Updating { .. } | State::Retired => {
                panic!("MSI source migration is serialized and live")
            }
        };
        set_mask(table, true);
        write_message(table, address, data);
        *state = State::Updating { table, unmask };
        drop(state);
        // 2. 平台可等待远端控制器/同步；IRQ-safe 源锁不得跨越此边界。
        commit();
        // 3. 途中 IRQ/ack 只改变恢复意图；不能用迁移开始时的旧 bool 覆盖最新确认状态。
        let mut state = self.state.lock();
        let State::Updating { table, unmask } = *state else {
            panic!("live MSI source remains in migration until commit completes");
        };
        if unmask {
            set_mask(table, false);
            *state = State::Armed(table);
        } else {
            *state = State::Masked(table);
        }
    }

    /// 先完成源 mask，再清空旧目标；与 IRQ 串行，返回后 unbind/归还槽，不留下晚到的硬件写。
    pub(in crate::object::resource) fn retire(&self) {
        let mut state = self.state.lock();
        let table = match core::mem::replace(&mut *state, State::Retired) {
            State::Armed(table) | State::Masked(table) => table,
            State::Updating { .. } => {
                panic!("MSI migration retains a live Interrupt until completion")
            }
            State::Retired => return,
        };
        set_mask(table, true);
        write_message(table, 0, 0);
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        // 构造失败尚未交付 Interrupt 时也必须撤销；正常退休已置 Retired，延迟析构不再写设备。
        self.retire();
    }
}
fn write_message(table: MmioBus, address: u64, data: u32) {
    table
        .write_u32(ADDRESS_LOW, address as u32)
        .expect("MSI-X address low");
    table
        .write_u32(ADDRESS_HIGH, (address >> 32) as u32)
        .expect("MSI-X address high");
    table.write_u32(DATA, data).expect("MSI-X message data");
    flush(table);
}
fn set_mask(table: MmioBus, masked: bool) {
    let control = table.read_u32(CONTROL).expect("MSI-X vector control");
    let control = if masked {
        control | MASK
    } else {
        control & !MASK
    };
    table
        .write_u32(CONTROL, control)
        .expect("MSI-X vector mask");
    flush(table);
}
fn flush(table: MmioBus) {
    // 同 BAR readback 排空 PCI posted writes；CPU fence 不证明设备已看到 mask/新消息。
    table
        .read_u32(CONTROL)
        .expect("MSI-X posted-write completion");
}
