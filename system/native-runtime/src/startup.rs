//! 进程入口：读取启动消息（布局见 `native_abi::startup`），安装运行时，再调用服务的 `main`。

use alloc::vec::Vec;

use native_abi::{StartupHeader, startup};

use crate::sync::Once;
use crate::{Channel, Handle, Process, Thread, Vmar};

/// 启动消息中的参数与角色句柄。
pub struct Startup {
    /// 启动者给出的参数字节（根服务为内核启动参数文本）。
    pub arguments: Vec<u8>,
    /// 角色句柄（启动消息中第 [`startup::ROLE_HANDLES`] 个起），按启动者约定的顺序。
    pub handles: Vec<Handle>,
}

impl Startup {
    /// 取出第 `index` 个角色句柄（`index` 为 `native_abi::startup` 中的绝对下标）。
    ///
    /// # Panics
    ///
    /// 下标不是角色句柄或已被取出。
    pub fn take(&mut self, index: usize) -> Handle {
        let slot = self
            .handles
            .get_mut(index - startup::ROLE_HANDLES)
            .expect("startup handle index out of range");
        let handle = core::mem::replace(slot, Handle::invalid());
        assert!(!handle.is_invalid(), "startup handle {index} missing or taken twice");
        handle
    }
}

// OWNER: 本进程句柄；启动时安装一次，供线程创建等使用。
static SELF_PROCESS: Once<Process> = Once::new();
// OWNER: panic 时终止的任务；缺省为本进程。服务若跨多个进程运行线程（共享进程成员），
// 必须换成覆盖全部成员的 Job，否则 panic 只终止出错线程所在的成员进程，其余部分带着损坏状态继续运行。
static FATAL_SCOPE: Once<Handle> = Once::new();

/// 本进程句柄。
///
/// # Panics
///
/// 运行时启动前调用。
pub fn self_process() -> &'static Process {
    SELF_PROCESS.get().expect("runtime not started")
}

/// 设置 panic 时终止的任务（只能设置一次）。
pub fn set_fatal_scope(task: Handle) {
    FATAL_SCOPE.call_once(|| task);
}

/// 服务无法继续时终止其范围内的全部任务。
pub(crate) fn fatal() -> ! {
    if let Some(scope) = FATAL_SCOPE.get() {
        let _ = crate::task::kill(scope);
    }
    crate::task::exit_process(-1)
}

unsafe extern "Rust" {
    /// 服务入口，由 [`crate::entry!`] 定义。
    fn __native_runtime_main(startup: Startup) -> i64;
}

/// 定义服务入口：`fn main(startup: Startup) -> i64`。
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[unsafe(no_mangle)]
        fn __native_runtime_main(startup: $crate::Startup) -> i64 {
            $main(startup)
        }
    };
}

const ARGUMENT_CAPACITY: usize = 16 * 1024;

#[unsafe(no_mangle)]
extern "C" fn _start(channel: native_abi::Handle, _: usize) -> ! {
    // SAFETY: 首个参数是内核转移给本进程的 startup channel 句柄。
    let channel = Channel::from_handle(unsafe { Handle::from_raw(channel) });
    let mut bytes = [0u8; ARGUMENT_CAPACITY];
    let mut raw = [native_abi::HANDLE_INVALID; native_abi::CHANNEL_MAX_HANDLES];
    let actual = channel
        .read_raw(&mut bytes, &mut raw)
        .unwrap_or_else(|_| crate::task::exit_process(-2));
    drop(channel);
    let header_size = core::mem::size_of::<StartupHeader>();
    let (byte_count, handle_count) = (actual.bytes as usize, actual.handles as usize);
    if byte_count < header_size || handle_count < startup::ROLE_HANDLES {
        crate::task::exit_process(-3);
    }
    let mut header = StartupHeader::default();
    crate::bytes_of_mut(&mut header).copy_from_slice(&bytes[..header_size]);
    let argument_end = header_size.saturating_add(header.arguments_length as usize);
    if header.magic != startup::MAGIC || argument_end > byte_count {
        crate::task::exit_process(-3);
    }
    // SAFETY: 以下句柄由内核刚交给本进程，各自只被接管一次。
    let take = |index: usize| unsafe { Handle::from_raw(raw[index]) };
    crate::heap::initialize(Vmar::from_handle(take(startup::SELF_VMAR)));
    crate::thread::install_main(Thread::from_handle(take(startup::SELF_THREAD)));
    SELF_PROCESS.call_once(|| Process::from_handle(take(startup::SELF_PROCESS)));
    let startup = Startup {
        arguments: bytes[header_size..argument_end].to_vec(),
        handles: (startup::ROLE_HANDLES..handle_count).map(take).collect(),
    };
    // SAFETY: 入口由服务以 `entry!` 定义，签名一致。
    let code = unsafe { __native_runtime_main(startup) };
    crate::task::exit_process(code)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    crate::log::write(crate::system::Severity::Error, format_args!("panic: {info}"));
    fatal()
}
