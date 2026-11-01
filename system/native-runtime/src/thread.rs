//! 线程：创建、等待、回收与停驻。
//!
//! 每个线程有一个 [`Descriptor`]，线程指针寄存器（AArch64 `TPIDR_EL0`、RISC-V `tp`）保存它的地址。
//! restricted mode 的退出恢复全部 normal mode 寄存器，因此该寄存器在服务代码中始终有效。
//!
//! # 栈与回收
//!
//! 1. 栈放在独立的子 VMAR 中，最低一页不映射作为 guard：溢出立即故障而不是覆盖相邻内存。
//! 2. 线程在自己的栈上退出，不能自己释放栈与描述符引用；join 者（或分离线程的回收者）等到线程
//!    `TERMINATED` 后销毁子 VMAR，再释放线程持有的描述符引用。
//! 3. 分离线程登记到回收表，下一次 [`spawn`] 时回收已终止者。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use native_abi::Status;

use crate::sync::Mutex;
use crate::{Thread, Vmar, Vmo, futex};

/// 默认栈大小。
pub const DEFAULT_STACK_SIZE: usize = 256 * 1024;

/// 停驻等待的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Park {
    /// 被 [`Descriptor::unpark`] 唤醒（或之前已有未消费的唤醒）。
    Unparked,
    /// 截止时间已到。
    TimedOut,
}

struct Stack {
    vmar: Vmar,
}

/// 线程描述符。
pub struct Descriptor {
    handle: Thread,
    // 0 无待消费唤醒，1 有。
    token: AtomicU32,
    // 服务自定的线程上下文字（例如当前服务任务的指针）。
    context: AtomicUsize,
    stack: Mutex<Option<Stack>>,
    start: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Descriptor {
    /// 线程句柄。
    pub fn handle(&self) -> &Thread {
        &self.handle
    }

    /// 消费一次唤醒；没有时停驻到 `deadline`。
    pub fn park(&self, deadline: i64) -> Park {
        loop {
            if self.token.swap(0, Ordering::Acquire) == 1 {
                return Park::Unparked;
            }
            // 唤醒、值已变化或被内核取消时回到循环复查令牌。
            if futex::wait(&self.token, 0, deadline, 0) == Err(native_abi::ERR_TIMED_OUT) {
                return if self.token.swap(0, Ordering::Acquire) == 1 {
                    Park::Unparked
                } else {
                    Park::TimedOut
                };
            }
        }
    }

    /// 发布一次唤醒；线程停驻时唤醒它，多次唤醒合并。
    pub fn unpark(&self) {
        if self.token.swap(1, Ordering::Release) == 0 {
            futex::wake(&self.token, 1, 0);
        }
    }

    /// 服务上下文字。
    pub fn context(&self) -> usize {
        self.context.load(Ordering::Acquire)
    }

    /// 设置服务上下文字。
    pub fn set_context(&self, value: usize) {
        self.context.store(value, Ordering::Release);
    }
}

fn set_thread_pointer(descriptor: *const Descriptor) {
    let descriptor = descriptor as usize;
    #[cfg(target_arch = "aarch64")]
    // SAFETY: 只写本线程的线程指针寄存器。
    unsafe {
        core::arch::asm!("msr tpidr_el0, {}", in(reg) descriptor, options(nomem, nostack));
    }
    #[cfg(target_arch = "riscv64")]
    // SAFETY: 只写本线程的线程指针寄存器；服务代码不使用编译器 TLS。
    unsafe {
        core::arch::asm!("mv tp, {}", in(reg) descriptor, options(nomem, nostack));
    }
}

fn thread_pointer() -> *const Descriptor {
    let value: usize;
    #[cfg(target_arch = "aarch64")]
    // SAFETY: 读取线程指针寄存器没有副作用。
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) value, options(nomem, nostack));
    }
    #[cfg(target_arch = "riscv64")]
    // SAFETY: 读取线程指针寄存器没有副作用。
    unsafe {
        core::arch::asm!("mv {}, tp", out(reg) value, options(nomem, nostack));
    }
    value as *const Descriptor
}

/// 当前线程的描述符。
///
/// # Panics
///
/// 运行时安装主线程描述符之前调用。
pub fn current() -> &'static Descriptor {
    let pointer = thread_pointer();
    assert!(!pointer.is_null(), "thread descriptor not installed");
    // SAFETY: 线程持有自身描述符的一个强引用直到退出，退出后不再执行任何代码。
    unsafe { &*pointer }
}

/// 当前线程描述符的强引用。
pub fn current_arc() -> Arc<Descriptor> {
    let pointer = thread_pointer();
    assert!(!pointer.is_null(), "thread descriptor not installed");
    // SAFETY: 指针来自 `Arc::into_raw`，线程持有的强引用保证对象存活。
    unsafe {
        Arc::increment_strong_count(pointer);
        Arc::from_raw(pointer)
    }
}

/// 安装主线程描述符（运行时启动时调用一次）。
pub(crate) fn install_main(handle: Thread) {
    let descriptor = Arc::new(Descriptor {
        handle,
        token: AtomicU32::new(0),
        context: AtomicUsize::new(0),
        stack: Mutex::new(None),
        start: Mutex::new(None),
    });
    set_thread_pointer(Arc::into_raw(descriptor));
}

extern "C" fn thread_entry(descriptor: *const Descriptor, _: usize) -> ! {
    set_thread_pointer(descriptor);
    let start = current().start.lock().take().expect("thread started twice");
    start();
    crate::task::exit_thread();
}

/// 已创建的线程；丢弃时转为分离，由回收表在其终止后释放资源。
pub struct JoinHandle<T> {
    descriptor: Option<Arc<Descriptor>>,
    result: Arc<Mutex<Option<T>>>,
}

impl<T> JoinHandle<T> {
    /// 线程描述符。
    pub fn descriptor(&self) -> &Arc<Descriptor> {
        self.descriptor.as_ref().expect("join handle consumed")
    }

    /// 等待线程结束并取得返回值；线程被终止（未正常返回）时为 `None`。
    pub fn join(mut self) -> Option<T> {
        let descriptor = self.descriptor.take().expect("join handle consumed");
        let _ = descriptor
            .handle
            .wait_one(native_abi::signals::TERMINATED, native_abi::DEADLINE_INFINITE);
        reap(&descriptor);
        self.result.lock().take()
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        if let Some(descriptor) = self.descriptor.take() {
            ZOMBIES.lock().push(descriptor);
        }
    }
}

// OWNER: 已分离、尚待回收的线程；只在 spawn 时扫描。
static ZOMBIES: Mutex<Vec<Arc<Descriptor>>> = Mutex::new(Vec::new());

/// 释放已终止线程的栈与它持有的描述符引用。
fn reap(descriptor: &Arc<Descriptor>) {
    if let Some(stack) = descriptor.stack.lock().take() {
        let _ = stack.vmar.destroy();
    }
    // SAFETY: 线程启动时以 `Arc::into_raw` 交出一个强引用；它已终止，不会再使用该引用。
    unsafe { Arc::decrement_strong_count(Arc::as_ptr(descriptor)) };
}

fn reap_zombies() {
    let mut zombies = ZOMBIES.lock();
    zombies.retain(|descriptor| {
        let terminated = descriptor
            .handle
            .wait_one(native_abi::signals::TERMINATED, 0)
            .is_ok();
        if terminated {
            reap(descriptor);
        }
        !terminated
    });
}

/// 已创建、尚未启动的线程：创建（可失败）与启动分离，调用者可以先发布引用它的对象再启动。
pub struct PreparedThread<T> {
    descriptor: Arc<Descriptor>,
    result: Arc<Mutex<Option<T>>>,
    /// 交给线程入口的描述符强引用（`Arc::into_raw`）；启动前由本对象持有。
    raw: usize,
    stack_top: usize,
    started: bool,
}

impl<T> PreparedThread<T> {
    /// 线程描述符（启动前可设置上下文字）。
    pub fn descriptor(&self) -> &Arc<Descriptor> {
        &self.descriptor
    }

    /// 启动线程。
    ///
    /// # Errors
    ///
    /// 线程所属进程已终止；此时线程资源随本对象释放。
    pub fn start(mut self) -> Result<JoinHandle<T>, Status> {
        self.descriptor
            .handle
            .start(thread_entry as *const () as usize, self.stack_top, self.raw, 0)?;
        self.started = true;
        Ok(JoinHandle {
            descriptor: Some(self.descriptor.clone()),
            result: self.result.clone(),
        })
    }
}

impl<T> Drop for PreparedThread<T> {
    fn drop(&mut self) {
        if self.started {
            return;
        }
        // SAFETY: 线程从未启动，收回准备时交出的引用。
        drop(unsafe { Arc::from_raw(self.raw as *const Descriptor) });
        if let Some(stack) = self.descriptor.stack.lock().take() {
            let _ = stack.vmar.destroy();
        }
    }
}

// SAFETY: `raw` 只是描述符强引用的数值形式；描述符本身是 Send + Sync。
unsafe impl<T: Send> Send for PreparedThread<T> {}
// SAFETY: 同上，共享引用不暴露 `raw`。
unsafe impl<T: Send> Sync for PreparedThread<T> {}

/// 在 `process` 中创建运行 `body` 的线程（不启动）；栈从运行时 VMAR 分配。
///
/// # Errors
///
/// 线程或栈创建失败。
pub fn prepare_in<T: Send + 'static>(
    process: &crate::Process,
    name: &[u8],
    stack_size: usize,
    body: impl FnOnce() -> T + Send + 'static,
) -> Result<PreparedThread<T>, Status> {
    reap_zombies();
    let page = native_abi::PAGE_SIZE;
    let stack_size = stack_size.next_multiple_of(page);
    let handle = Thread::create(process, name)?;
    let (vmar, _) = crate::heap::vmar().allocate(native_abi::vm::READ | native_abi::vm::WRITE, 0, stack_size + page)?;
    let vmo = Vmo::create(stack_size as u64, 0)?;
    let stack_top = match vmar.map(native_abi::vm::READ | native_abi::vm::WRITE | native_abi::vm::SPECIFIC, page, &vmo, 0, stack_size) {
        Ok(stack_base) => stack_base + stack_size,
        Err(status) => {
            let _ = vmar.destroy();
            return Err(status);
        }
    };
    let result = Arc::new(Mutex::new(None));
    let slot = result.clone();
    let descriptor = Arc::new(Descriptor {
        handle,
        token: AtomicU32::new(0),
        context: AtomicUsize::new(0),
        stack: Mutex::new(Some(Stack { vmar })),
        start: Mutex::new(Some(Box::new(move || {
            *slot.lock() = Some(body());
        }))),
    });
    let raw = Arc::into_raw(descriptor.clone()) as usize;
    Ok(PreparedThread {
        descriptor,
        result,
        raw,
        stack_top,
        started: false,
    })
}

/// 在 `process` 中创建并启动运行 `body` 的线程。
///
/// # Errors
///
/// 线程或栈创建失败。
pub fn spawn_in<T: Send + 'static>(
    process: &crate::Process,
    name: &[u8],
    stack_size: usize,
    body: impl FnOnce() -> T + Send + 'static,
) -> Result<JoinHandle<T>, Status> {
    prepare_in(process, name, stack_size, body)?.start()
}

/// 在本进程中创建线程。
///
/// # Errors
///
/// 线程或栈创建失败。
pub fn spawn<T: Send + 'static>(
    name: &[u8],
    body: impl FnOnce() -> T + Send + 'static,
) -> Result<JoinHandle<T>, Status> {
    spawn_in(crate::startup::self_process(), name, DEFAULT_STACK_SIZE, body)
}
