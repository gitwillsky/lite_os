//! 按启动协议（`native_abi::startup`）启动系统服务进程。

use alloc::vec::Vec;

use native_abi::{StartupHeader, Status, rights, startup};

use crate::{Channel, Handle, Process, Thread, Vmar, Vmo};

/// 启动描述。
pub struct Launch<'a> {
    /// 已创建、尚未启动的进程。
    pub process: &'a Process,
    /// 映像各段的装载位置（静态链接地址必须落在其中）。
    pub image_vmar: &'a Vmar,
    /// 交给新进程作为 `SELF_VMAR` 的运行时 VMAR；首个线程的栈也映射在其中。
    pub runtime_vmar: Vmar,
    /// 线程名。
    pub name: &'a [u8],
    /// 已准备的静态 ELF 映像；同程序的多个实例共享只读段。
    pub image: &'a crate::elf::Image,
    /// 参数字节。
    pub arguments: &'a [u8],
    /// 角色句柄。
    pub handles: Vec<Handle>,
    /// 首个线程的栈大小。
    pub stack_size: usize,
}

/// 装载映像、映射栈、发送启动消息并启动首个线程。
///
/// # Errors
///
/// 装载、映射、句柄复制或启动失败；失败时进程仍未启动，由调用者决定是否终止。
pub fn launch(launch: Launch<'_>) -> Result<(), Status> {
    let process = launch.process;
    prepare(launch)?.start(process)
}

/// 已完成装载的首线程及其启动消息。启动前不会执行任何服务代码。
pub struct Prepared {
    thread: Thread,
    entry: usize,
    stack_top: usize,
    bootstrap: Channel,
}

impl Prepared {
    /// 启动已装载的首线程。传入准备时使用的进程。
    ///
    /// # Errors
    ///
    /// 内核拒绝启动线程或转交启动句柄。
    pub fn start(self, process: &Process) -> Result<(), Status> {
        process.start(
            &self.thread,
            self.entry,
            self.stack_top,
            self.bootstrap.into_handle(),
            0,
        )
    }
}

/// 完成 ELF 装载及启动准备，暂不执行服务代码。
///
/// # Errors
///
/// 映像无效、资源不足或启动句柄无法交付。
pub fn prepare(launch: Launch<'_>) -> Result<Prepared, Status> {
    let entry = launch.image.load(launch.image_vmar)?;
    let stack_size = launch.stack_size.next_multiple_of(native_abi::PAGE_SIZE);
    let stack = Vmo::create(stack_size as u64, 0)?;
    let stack_base = launch.runtime_vmar.map(
        native_abi::vm::READ | native_abi::vm::WRITE,
        0,
        &stack,
        0,
        stack_size,
    )?;
    let thread = Thread::create(launch.process, launch.name)?;
    let (local, remote) = Channel::create()?;

    let header = StartupHeader {
        magic: startup::MAGIC,
        arguments_length: launch.arguments.len() as u64,
    };
    let mut bytes =
        Vec::with_capacity(core::mem::size_of::<StartupHeader>() + launch.arguments.len());
    bytes.extend_from_slice(crate::bytes_of(&header));
    bytes.extend_from_slice(launch.arguments);

    let mut handles = Vec::with_capacity(startup::ROLE_HANDLES + launch.handles.len());
    handles.push(launch.process.duplicate(rights::SAME)?.into_handle());
    handles.push(launch.runtime_vmar.into_handle());
    handles.push(thread.duplicate(rights::SAME)?.into_handle());
    handles.extend(launch.handles);
    local.write(&bytes, handles)?;
    Ok(Prepared {
        thread,
        entry,
        stack_top: stack_base + stack_size,
        bootstrap: remote,
    })
}
