use core::{
    cell::UnsafeCell,
    ffi::c_void,
    marker::PhantomPinned,
    mem::MaybeUninit,
    pin::Pin,
    ptr,
    sync::atomic::{AtomicU8, Ordering},
};

use crate::{display::DamageRequest, ffi};

const IDLE: u8 = 0;
const SUBMITTED: u8 = 1;
const COMPLETED: u8 = 2;
const STOP: u8 = 3;

struct Slot {
    request: MaybeUninit<DamageRequest>,
    error: i32,
}

/// @description 以固定单槽 SPSC protocol 隔离 blocking DIRTYFB 的唯一 presenter owner。
pub(super) struct Presenter {
    command: i32,
    completion: i32,
    // OWNER: status 是 main producer 与唯一 worker consumer 的 publication fact。main 只在
    // IDLE 写 slot 后 Release→SUBMITTED；worker Acquire 后执行，写 error 再
    // Release→COMPLETED；main Acquire 后读取并回到 IDLE。缺失该顺序会数据竞争 request。
    status: AtomicU8,
    // OWNER: slot 只在上述 status epoch 内换手；UnsafeCell 不代表第二份同步机制。
    slot: UnsafeCell<Slot>,
    // OWNER: thread 存在即表示 pinned self 的 raw pointer 仍被 worker 借用；stop 必须 join
    // 后才能清除。若 Drop 越过该 owner，worker 会解引用已失效的栈地址。
    thread: Option<ffi::Pthread>,
    _pin: PhantomPinned,
}

// SAFETY: `status` 的 Release/Acquire epoch 独占 `slot`；其他字段要么 immutable，要么只由
// pinned main thread 在 worker start 前或 join 后修改。任一时刻最多一个 request 在途。
unsafe impl Sync for Presenter {}

impl Presenter {
    /// @description 创建两个 close-on-exec eventfd；尚不把 self 地址交给 worker。
    /// @return 固定单槽 presenter owner。
    /// @errors eventfd allocation 失败返回 unit error，并关闭已创建 descriptor。
    pub(super) fn new() -> Result<Self, ()> {
        let command = unsafe { ffi::eventfd(0, ffi::EFD_CLOEXEC) };
        if command < 0 {
            return Err(());
        }
        let completion = unsafe { ffi::eventfd(0, ffi::EFD_CLOEXEC) };
        if completion < 0 {
            unsafe { ffi::close(command) };
            return Err(());
        }
        Ok(Self {
            command,
            completion,
            status: AtomicU8::new(IDLE),
            slot: UnsafeCell::new(Slot {
                request: MaybeUninit::uninit(),
                error: 0,
            }),
            thread: None,
            _pin: PhantomPinned,
        })
    }

    /// @description 启动唯一 worker，并把 pinned presenter 地址借给它直到 stop/join。
    /// @param self 调用者保证后续不移动且在 Drop 前调用 stop 的 pinned owner。
    /// @return worker 成功创建时返回 unit。
    /// @errors pthread control block/stack allocation 失败返回 unit error。
    pub(super) fn start(self: Pin<&mut Self>) -> Result<(), ()> {
        // SAFETY: PhantomPinned 保证 start 后地址不变；thread 只在 join 后清除 raw borrow。
        let presenter = unsafe { self.get_unchecked_mut() };
        assert!(presenter.thread.is_none());
        let mut thread = ptr::null_mut();
        if unsafe {
            ffi::pthread_create(
                &mut thread,
                ptr::null(),
                worker,
                (presenter as *mut Self).cast(),
            )
        } != 0
        {
            return Err(());
        }
        presenter.thread = Some(thread);
        Ok(())
    }

    /// @description 返回 reactor 唯一需要 poll 的 presenter completion eventfd。
    /// @return worker 完成一个 DIRTYFB 后变为 readable 的 descriptor。
    pub(super) fn completion_fd(&self) -> i32 {
        self.completion
    }

    /// @description 向空闲固定槽发布一个完全自包含的 DIRTYFB request。
    /// @param request 不借用 Display/Scene 的固定 request copy。
    /// @return 成功发布为 true；已有 request 在途为 false。
    /// @errors eventfd publication failure 返回 unit error；request 尚未交给 worker。
    pub(super) fn submit(&self, request: DamageRequest) -> Result<bool, ()> {
        if self.status.load(Ordering::Acquire) != IDLE {
            return Ok(false);
        }
        // SAFETY: main 是唯一 producer，且 IDLE 证明 worker 不读写 slot。
        unsafe {
            let slot = &mut *self.slot.get();
            slot.request.write(request);
            slot.error = 0;
        }
        self.status.store(SUBMITTED, Ordering::Release);
        if !write_event(self.command) {
            self.status.store(IDLE, Ordering::Release);
            return Err(());
        }
        Ok(true)
    }

    /// @description 接收 worker completion，并把固定槽 ownership 归还给 producer。
    /// @param wait true 用于 revoke/exit cleanup，可等待在途 ioctl；false 只消费已完成状态。
    /// @return 空闲或非阻塞尚未完成为 None；完成时返回原 request 与 errno（成功为 0）。
    /// @errors eventfd/state protocol 损坏返回 unit error。
    pub(super) fn completion(&self, wait: bool) -> Result<Option<(DamageRequest, i32)>, ()> {
        match self.status.load(Ordering::Acquire) {
            IDLE => return Ok(None),
            SUBMITTED if !wait => return Ok(None),
            SUBMITTED | COMPLETED => {}
            _ => return Err(()),
        }
        if !read_event(self.completion) || self.status.load(Ordering::Acquire) != COMPLETED {
            return Err(());
        }
        // SAFETY: COMPLETED Acquire observes worker 的 slot.error write；request 是 Copy 且
        // 直到下次 SUBMITTED 前保持 initialized。
        let (request, error) = unsafe {
            let slot = &*self.slot.get();
            (slot.request.assume_init_read(), slot.error)
        };
        self.status.store(IDLE, Ordering::Release);
        Ok(Some((request, error)))
    }

    /// @description 停止并 join worker，再关闭两个 eventfd。
    /// @param self pinned owner；caller 必须已用 completion(true) 收回在途 request。
    pub(super) fn stop(self: Pin<&mut Self>) {
        // SAFETY: join 完成前 presenter 保持 pinned；之后 worker 不再持 raw pointer。
        let presenter = unsafe { self.get_unchecked_mut() };
        assert_eq!(presenter.status.load(Ordering::Acquire), IDLE);
        let thread = presenter
            .thread
            .take()
            .expect("presenter worker was not started");
        presenter.status.store(STOP, Ordering::Release);
        assert!(write_event(presenter.command));
        assert_eq!(unsafe { ffi::pthread_join(thread, ptr::null_mut()) }, 0);
        presenter.close_descriptors();
    }

    fn close_descriptors(&mut self) {
        for descriptor in [&mut self.command, &mut self.completion] {
            if *descriptor >= 0 {
                unsafe { ffi::close(*descriptor) };
                *descriptor = -1;
            }
        }
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        assert!(
            self.thread.is_none(),
            "presenter dropped before worker join"
        );
        self.close_descriptors();
    }
}

unsafe extern "C" fn worker(argument: *mut c_void) -> *mut c_void {
    // SAFETY: start passes a pinned Presenter and stop joins this worker before that stack frame ends.
    let presenter = unsafe { &*(argument.cast::<Presenter>()) };
    loop {
        if !read_event(presenter.command) {
            unsafe { ffi::_exit(126) };
        }
        match presenter.status.load(Ordering::Acquire) {
            STOP => return ptr::null_mut(),
            SUBMITTED => {}
            _ => unsafe { ffi::_exit(126) },
        }
        // SAFETY: SUBMITTED Acquire observes producer initialization；worker 是唯一 consumer，
        // main 在 COMPLETED 前不会读取或覆盖 slot。
        let slot = unsafe { &mut *presenter.slot.get() };
        slot.error = unsafe { slot.request.assume_init_mut() }.execute();
        presenter.status.store(COMPLETED, Ordering::Release);
        if !write_event(presenter.completion) {
            unsafe { ffi::_exit(126) };
        }
    }
}

fn read_event(descriptor: i32) -> bool {
    let mut value = 0u64;
    loop {
        let count = unsafe {
            ffi::read(
                descriptor,
                (&mut value as *mut u64).cast(),
                core::mem::size_of::<u64>(),
            )
        };
        if count == core::mem::size_of::<u64>() as isize {
            return value == 1;
        }
        if count < 0 && ffi::errno() == ffi::EINTR {
            continue;
        }
        return false;
    }
}

fn write_event(descriptor: i32) -> bool {
    let value = 1u64;
    loop {
        let count = unsafe {
            ffi::write(
                descriptor,
                (&value as *const u64).cast(),
                core::mem::size_of::<u64>(),
            )
        };
        if count == core::mem::size_of::<u64>() as isize {
            return true;
        }
        if count < 0 && ffi::errno() == ffi::EINTR {
            continue;
        }
        return false;
    }
}
