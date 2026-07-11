use core::sync::atomic::{AtomicUsize, Ordering};

use rustsbi::{Fence as RfenceExtension, HartMask, SbiRet};
use spin::{Mutex, MutexGuard};

use crate::{clint, constants::MAX_SUPPORTED_HARTS, hart::hart_id, trap_stack::remote_hsm};

pub(crate) const REQUEST_FENCE_I: usize = 1 << 0;
pub(crate) const REQUEST_SFENCE_VMA: usize = 1 << 1;

pub(crate) static REQUESTS: [AtomicUsize; MAX_SUPPORTED_HARTS] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

pub(crate) static ACKNOWLEDGED: [AtomicUsize; MAX_SUPPORTED_HARTS] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

static RFENCE_LOCK: Mutex<()> = Mutex::new(());

fn dtb_hart_mask() -> usize {
    crate::BOARD_INFO.wait().hart_mask
}

/// @description SBI RFENCE 的同步 M-mode 实现。
pub(crate) struct Rfence;

impl Rfence {
    fn service_local_request() {
        let current = hart_id();
        let request = REQUESTS[current].swap(0, Ordering::AcqRel);
        if request == 0 {
            return;
        }
        Self::execute_local(request);
        ACKNOWLEDGED[current].store(1, Ordering::Release);
    }

    fn lock_with_progress() -> MutexGuard<'static, ()> {
        loop {
            if let Some(guard) = RFENCE_LOCK.try_lock() {
                return guard;
            }
            // M-mode trap 默认关闭 MIE。若当前 hart 同时是持锁 RFENCE 的目标，单纯
            // 自旋会阻止它处理 MSIP并造成环形等待，因此等待锁时必须主动消费 mailbox。
            Self::service_local_request();
            core::hint::spin_loop();
        }
    }

    fn selected_harts(hart_mask: HartMask) -> Result<usize, SbiRet> {
        let (mask, base) = hart_mask.into_inner();
        let possible = dtb_hart_mask();
        let selected = if base == usize::MAX {
            possible
        } else if mask == 0 {
            0
        } else if base >= MAX_SUPPORTED_HARTS {
            return Err(SbiRet::invalid_param());
        } else {
            let valid_bits = MAX_SUPPORTED_HARTS - base;
            if valid_bits < usize::BITS as usize && (mask >> valid_bits) != 0 {
                return Err(SbiRet::invalid_param());
            }
            let selected = mask << base;
            if selected & !possible != 0 {
                return Err(SbiRet::invalid_param());
            }
            selected
        };

        for target in 0..MAX_SUPPORTED_HARTS {
            if selected & (1usize << target) == 0 || target == hart_id() {
                continue;
            }
            if !remote_hsm(target).is_some_and(|hsm| hsm.allow_ipi()) {
                return Err(SbiRet::invalid_param());
            }
        }
        Ok(selected)
    }

    fn execute_local(request: usize) {
        unsafe {
            if request & REQUEST_FENCE_I != 0 {
                core::arch::asm!("fence.i", options(nostack));
            }
            if request & REQUEST_SFENCE_VMA != 0 {
                core::arch::asm!("sfence.vma", options(nostack));
            }
        }
    }

    fn remote_fence(&self, hart_mask: HartMask, request: usize) -> SbiRet {
        // 1. 序列化 RFENCE，避免同一目标 hart 的单槽 request/ack 被并发调用覆盖。
        let _guard = Self::lock_with_progress();
        let selected = match Self::selected_harts(hart_mask) {
            Ok(selected) => selected,
            Err(error) => return error,
        };
        let current = hart_id();

        // 2. Release request 发布调用 SBI 之前的页表/指令写；目标 hart 的 aq swap 消费这些写。
        for target in 0..MAX_SUPPORTED_HARTS {
            if selected & (1usize << target) == 0 {
                continue;
            }
            if target == current {
                Self::execute_local(request);
            } else {
                // 该清零只在 RFENCE_LOCK 内由唯一 sender 写；后续 request Release
                // 把它排在目标 hart 的 ack 之前，因此无需单独承担发布语义。
                ACKNOWLEDGED[target].store(0, Ordering::Relaxed);
                REQUESTS[target].store(request, Ordering::Release);
                clint::set_msip(target);
            }
        }

        // 3. Acquire ack 与目标 hart 的 rl swap 配对；SBI 只有在所有远端 fence 完成后才返回。
        for target in 0..MAX_SUPPORTED_HARTS {
            if target == current || selected & (1usize << target) == 0 {
                continue;
            }
            while ACKNOWLEDGED[target].load(Ordering::Acquire) == 0 {
                core::hint::spin_loop();
            }
        }
        SbiRet::success(0)
    }
}

impl RfenceExtension for Rfence {
    fn remote_fence_i(&self, hart_mask: HartMask) -> SbiRet {
        self.remote_fence(hart_mask, REQUEST_FENCE_I)
    }

    fn remote_sfence_vma(&self, hart_mask: HartMask, _start_addr: usize, _size: usize) -> SbiRet {
        // 全局 sfence.vma 覆盖任意请求区间；当前 ASID=0 模型无需保留范围优化。
        self.remote_fence(hart_mask, REQUEST_SFENCE_VMA)
    }

    fn remote_sfence_vma_asid(
        &self,
        hart_mask: HartMask,
        _start_addr: usize,
        _size: usize,
        _asid: usize,
    ) -> SbiRet {
        self.remote_fence(hart_mask, REQUEST_SFENCE_VMA)
    }
}
