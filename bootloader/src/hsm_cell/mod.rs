use core::{
    cell::UnsafeCell,
    hint::spin_loop,
    sync::atomic::{AtomicUsize, Ordering},
};
use sbi_spec::hsm::*;

/// 硬件线程状态和受状态保护的线程间共享数据。
pub(crate) struct HsmCell<T> {
    status: AtomicUsize,
    val: UnsafeCell<Option<T>>,
}

/// 当前硬件线程的共享对象。
pub(crate) struct LocalHsmCell<'a, T>(&'a HsmCell<T>);

/// 任意硬件线程的共享对象。
pub(crate) struct RemoteHsmCell<'a, T>(&'a HsmCell<T>);

// SAFETY: status atomics serialize access to UnsafeCell<T>; transfer occurs only through the
// START_PENDING state machine and T: Send permits ownership to move between harts.
unsafe impl<T: Send> Sync for HsmCell<T> {}
// SAFETY: HsmCell owns T and moving the cell moves no active borrow; T: Send permits transfer.
unsafe impl<T: Send> Send for HsmCell<T> {}

const HART_STATE_START_PENDING_EXT: usize = usize::MAX;

impl<T> HsmCell<T> {
    /// 创建一个新的共享对象。
    pub(crate) const fn new() -> Self {
        Self {
            status: AtomicUsize::new(hart_state::STOPPED),
            val: UnsafeCell::new(None),
        }
    }

    /// 从当前硬件线程的状态中获取线程间共享对象。
    ///
    /// # Safety
    ///
    /// 用户需要确保对象属于当前硬件线程。
    #[inline]
    // SAFETY: caller must invoke this only for the cell indexed by the executing hart; that
    // invariant grants local operations their single-hart authority.
    pub(crate) unsafe fn local(&self) -> LocalHsmCell<'_, T> {
        LocalHsmCell(self)
    }

    /// 取出共享对象。
    #[inline]
    pub(crate) fn remote(&self) -> RemoteHsmCell<'_, T> {
        RemoteHsmCell(self)
    }
}

impl<T> LocalHsmCell<'_, T> {
    /// 从启动挂起状态的硬件线程取出共享数据，并将其状态设置为启动，如果成功返回取出的数据，否则返回当前状态。
    #[inline]
    pub(crate) fn start(&self) -> Result<T, usize> {
        loop {
            match self.0.status.compare_exchange(
                hart_state::START_PENDING,
                hart_state::STARTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                // SAFETY: successful AcqRel transition gives this local hart exclusive ownership
                // of the value published before START_PENDING with Release ordering.
                Ok(_) => break Ok(unsafe { (*self.0.val.get()).take().unwrap() }),
                Err(HART_STATE_START_PENDING_EXT) => spin_loop(),
                Err(s) => break Err(s),
            }
        }
    }

    /// 关闭。
    #[inline]
    pub(crate) fn stop(&self) {
        self.0.status.store(hart_state::STOPPED, Ordering::Release)
    }

    /// 关闭。
    #[inline]
    pub(crate) fn suspend(&self) {
        self.0
            .status
            .store(hart_state::SUSPENDED, Ordering::Release)
    }

    /// 关闭。
    #[inline]
    pub(crate) fn resume(&self) {
        self.0.status.store(hart_state::STARTED, Ordering::Release)
    }
}

impl<T> RemoteHsmCell<'_, T> {
    /// 向关闭状态的硬件线程传入共享数据，并将其状态设置为启动挂起，返回是否放入成功。
    #[inline]
    pub(crate) fn start(self, t: T) -> bool {
        if self
            .0
            .status
            .compare_exchange(
                hart_state::STOPPED,
                HART_STATE_START_PENDING_EXT,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            // SAFETY: successful transition to the private EXT state excludes local and remote
            // readers until the following Release publication of START_PENDING.
            unsafe { *self.0.val.get() = Some(t) };
            self.0
                .status
                .store(hart_state::START_PENDING, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// 取出当前状态。
    #[inline]
    pub(crate) fn sbi_get_status(&self) -> usize {
        match self.0.status.load(Ordering::Acquire) {
            HART_STATE_START_PENDING_EXT => hart_state::START_PENDING,
            normal => normal,
        }
    }

    /// 判断这个 HART 能否接收 IPI。
    #[inline]
    pub(crate) fn allow_ipi(&self) -> bool {
        matches!(
            self.0.status.load(Ordering::Acquire),
            hart_state::STARTED | hart_state::SUSPENDED
        )
    }
}
