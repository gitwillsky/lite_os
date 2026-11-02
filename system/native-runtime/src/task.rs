//! Job、进程、线程与异常。

use native_abi::{ProcessBasic, Status, TaskRuntime, call};

use crate::handle::object;
use crate::syscall::{check, invoke};
use crate::{Channel, Handle, Vmar};

object!(
    /// 任务资源树节点。
    Job
);
object!(
    /// 进程：句柄表 + 地址空间 + 线程集合。
    Process
);
object!(
    /// 线程。
    Thread
);
object!(
    /// 随异常报告送达的异常令牌；关闭即交给上一级处理者。
    Exception
);

fn new_handle(status: Status, out: native_abi::Handle) -> Result<Handle, Status> {
    check(status)?;
    // SAFETY: 调用成功时内核写入的新句柄由调用者独占。
    Ok(unsafe { Handle::from_raw(out) })
}

/// 终止任务（进程、线程或 Job 子树）。
///
/// # Errors
///
/// 缺少 `MANAGE`。
pub fn kill(task: &Handle) -> Result<(), Status> {
    // SAFETY: 无指针参数。
    check(unsafe { invoke(call::TASK_KILL, &[task.raw() as usize]) })
}

/// 为进程或 Job 创建异常 channel。
///
/// # Errors
///
/// `ERR_ALREADY_BOUND`。
pub fn create_exception_channel(task: &Handle) -> Result<Channel, Status> {
    let mut out = native_abi::HANDLE_INVALID;
    // SAFETY: 输出位置有效。
    let status = unsafe {
        invoke(
            call::TASK_CREATE_EXCEPTION_CHANNEL,
            &[task.raw() as usize, (&raw mut out) as usize],
        )
    };
    new_handle(status, out).map(Channel::from_handle)
}

impl Job {
    /// 创建子 Job。
    ///
    /// # Errors
    ///
    /// 父 Job 已终止或内存不足。
    pub fn create(parent: &Self) -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 输出位置有效。
        let status = unsafe {
            invoke(
                call::JOB_CREATE,
                &[parent.raw() as usize, (&raw mut out) as usize],
            )
        };
        new_handle(status, out).map(Self)
    }

    /// 存活成员的 KOID（升序）：`children` 为真取子 Job，否则取直属进程。
    ///
    /// 成员在两次查询之间可能增加：内核以 `ERR_BUFFER_TOO_SMALL` 回报新的所需大小，这里按它扩容重试。
    ///
    /// # Errors
    ///
    /// 句柄缺少 `INSPECT` 或 `ENUMERATE`、结果缓冲分配失败（`ERR_NO_MEMORY`）。
    pub fn member_koids(&self, children: bool) -> Result<alloc::vec::Vec<u64>, Status> {
        let topic = if children {
            native_abi::info::JOB_CHILDREN
        } else {
            native_abi::info::JOB_PROCESSES
        };
        let mut koids = alloc::vec::Vec::new();
        loop {
            let mut actual = 0usize;
            // SAFETY: 缓冲区按 u64 数组的字节长度传入，输出位置有效。
            let status = unsafe {
                invoke(
                    call::OBJECT_GET_INFO,
                    &[
                        self.raw() as usize,
                        topic as usize,
                        koids.as_mut_ptr() as usize,
                        koids.capacity() * 8,
                        (&raw mut actual) as usize,
                    ],
                )
            };
            match check(status) {
                Ok(()) => {
                    // SAFETY: 内核写入了 `actual` 字节（8 的倍数且不超过容量）的 KOID。
                    unsafe { koids.set_len(actual / 8) };
                    return Ok(koids);
                }
                Err(native_abi::ERR_BUFFER_TOO_SMALL) => koids
                    .try_reserve_exact(actual / 8)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?,
                Err(status) => return Err(status),
            }
        }
    }

    /// 按 KOID 取直属成员（子 Job 或进程）句柄，rights 不超过本句柄。
    ///
    /// # Errors
    ///
    /// 缺少 `ENUMERATE`、`ERR_NOT_FOUND`（不是存活的直属成员）、`ERR_INVALID_ARGS`（rights 超出）。
    pub fn member(&self, koid: u64, rights: u32) -> Result<Handle, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 输出位置有效。
        let status = unsafe {
            invoke(
                call::OBJECT_GET_CHILD,
                &[
                    self.raw() as usize,
                    koid as usize,
                    rights as usize,
                    (&raw mut out) as usize,
                ],
            )
        };
        new_handle(status, out)
    }
}

impl Process {
    /// 在 `job` 中创建进程，返回进程与其根 VMAR。
    ///
    /// # Errors
    ///
    /// Job 已终止、选项非法或内存不足。
    pub fn create(job: &Job, name: &[u8], options: u32) -> Result<(Self, Vmar), Status> {
        let (mut process, mut vmar) = (native_abi::HANDLE_INVALID, native_abi::HANDLE_INVALID);
        // SAFETY: 名称与输出位置有效。
        let status = unsafe {
            invoke(
                call::PROCESS_CREATE,
                &[
                    job.raw() as usize,
                    name.as_ptr() as usize,
                    name.len(),
                    options as usize,
                    (&raw mut process) as usize,
                    (&raw mut vmar) as usize,
                ],
            )
        };
        let process = new_handle(status, process)?;
        // SAFETY: 成功时两个句柄都已写入。
        Ok((
            Self(process),
            Vmar::from_handle(unsafe { Handle::from_raw(vmar) }),
        ))
    }

    /// 创建与共享进程 `self` 共享高半区的成员进程，返回进程与其私有区根 VMAR。
    ///
    /// # Errors
    ///
    /// `self` 不是共享进程或内存不足。
    pub fn create_shared(&self, name: &[u8]) -> Result<(Self, Vmar), Status> {
        let (mut process, mut vmar) = (native_abi::HANDLE_INVALID, native_abi::HANDLE_INVALID);
        // SAFETY: 名称与输出位置有效。
        let status = unsafe {
            invoke(
                call::PROCESS_CREATE_SHARED,
                &[
                    self.raw() as usize,
                    name.as_ptr() as usize,
                    name.len(),
                    (&raw mut process) as usize,
                    (&raw mut vmar) as usize,
                ],
            )
        };
        let process = new_handle(status, process)?;
        // SAFETY: 成功时两个句柄都已写入。
        Ok((
            Self(process),
            Vmar::from_handle(unsafe { Handle::from_raw(vmar) }),
        ))
    }

    /// 启动首个线程；`argument` 转移进目标进程作为第一个参数。
    ///
    /// # Errors
    ///
    /// `ERR_BAD_STATE`（已启动或已终止）。
    pub fn start(
        &self,
        thread: &Thread,
        entry: usize,
        stack: usize,
        argument: Handle,
        second: usize,
    ) -> Result<(), Status> {
        // SAFETY: 无用户指针；参数句柄所有权交给内核。
        check(unsafe {
            invoke(
                call::PROCESS_START,
                &[
                    self.raw() as usize,
                    thread.raw() as usize,
                    entry,
                    stack,
                    argument.into_raw() as usize,
                    second,
                ],
            )
        })
    }

    /// 读取目标进程内存，返回实际字节数。
    ///
    /// # Errors
    ///
    /// `ERR_NOT_FOUND`/`ERR_INVALID_POINTER`（地址未映射）。
    pub fn read_memory(&self, address: usize, buffer: &mut [u8]) -> Result<usize, Status> {
        let mut actual = 0usize;
        // SAFETY: 缓冲区与输出位置有效。
        check(unsafe {
            invoke(
                call::PROCESS_READ_MEMORY,
                &[
                    self.raw() as usize,
                    address,
                    buffer.as_mut_ptr() as usize,
                    buffer.len(),
                    (&raw mut actual) as usize,
                ],
            )
        })?;
        Ok(actual)
    }

    /// 写入目标进程内存，返回实际字节数。
    ///
    /// # Errors
    ///
    /// 同 [`Self::read_memory`]。
    pub fn write_memory(&self, address: usize, buffer: &[u8]) -> Result<usize, Status> {
        let mut actual = 0usize;
        // SAFETY: 缓冲区与输出位置有效。
        check(unsafe {
            invoke(
                call::PROCESS_WRITE_MEMORY,
                &[
                    self.raw() as usize,
                    address,
                    buffer.as_ptr() as usize,
                    buffer.len(),
                    (&raw mut actual) as usize,
                ],
            )
        })?;
        Ok(actual)
    }

    /// 退出状态。
    ///
    /// # Errors
    ///
    /// 句柄无效。
    pub fn basic(&self) -> Result<ProcessBasic, Status> {
        self.info(native_abi::info::PROCESS)
    }

    /// 返回进程映射驻留统计与存活线程数，查询不提交物理页。
    ///
    /// # Errors
    /// 缺少 INSPECT 权限，或统计临时内存不足。
    pub fn stats(&self) -> Result<native_abi::TaskStats, Status> {
        self.info(native_abi::info::TASK_STATS)
    }

    /// 已消耗的 CPU 时间，包含已退出线程。
    ///
    /// # Errors
    /// 句柄无效或缺少 INSPECT 权限。
    pub fn runtime(&self) -> Result<TaskRuntime, Status> {
        self.info(native_abi::info::TASK_RUNTIME)
    }
}

/// 终止调用者所在进程。
pub fn exit_process(code: i64) -> ! {
    // SAFETY: 无指针参数；调用不返回。
    unsafe { invoke(call::PROCESS_EXIT, &[code as usize]) };
    unreachable!("process_exit returned");
}

impl Thread {
    /// 在进程中创建未启动的线程。
    ///
    /// # Errors
    ///
    /// 进程已终止或内存不足。
    pub fn create(process: &Process, name: &[u8]) -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 名称与输出位置有效。
        let status = unsafe {
            invoke(
                call::THREAD_CREATE,
                &[
                    process.raw() as usize,
                    name.as_ptr() as usize,
                    name.len(),
                    (&raw mut out) as usize,
                ],
            )
        };
        new_handle(status, out).map(Self)
    }

    /// 启动线程：`entry(first, second)`，栈顶为 `stack`。
    ///
    /// # Errors
    ///
    /// `ERR_BAD_STATE`。
    pub fn start(
        &self,
        entry: usize,
        stack: usize,
        first: usize,
        second: usize,
    ) -> Result<(), Status> {
        // SAFETY: 无用户指针。
        check(unsafe {
            invoke(
                call::THREAD_START,
                &[self.raw() as usize, entry, stack, first, second],
            )
        })
    }

    /// 设置权重等级（`0..40`，小者权重大）。
    ///
    /// # Errors
    ///
    /// 等级越界。
    pub fn set_priority(&self, priority: u32) -> Result<(), Status> {
        // SAFETY: 无指针参数。
        check(unsafe {
            invoke(
                call::THREAD_SET_PRIORITY,
                &[self.raw() as usize, priority as usize],
            )
        })
    }

    /// 限制可运行的逻辑 CPU（bit N = CPU N）；未启动的线程在启动时应用。
    ///
    /// # Errors
    ///
    /// 与存在的 CPU 不相交或线程已退出。
    pub fn set_affinity(&self, mask: usize) -> Result<(), Status> {
        // SAFETY: 无指针参数。
        check(unsafe { invoke(call::THREAD_SET_AFFINITY, &[self.raw() as usize, mask]) })
    }

    /// 让线程尽快退出 restricted mode。
    ///
    /// # Errors
    ///
    /// 缺少 `MANAGE`。
    pub fn kick(&self) -> Result<(), Status> {
        // SAFETY: 无指针参数。
        check(unsafe { invoke(call::RESTRICTED_KICK, &[self.raw() as usize]) })
    }

    /// 已消耗的 CPU 时间。
    ///
    /// # Errors
    ///
    /// 句柄无效。
    pub fn runtime(&self) -> Result<TaskRuntime, Status> {
        self.info(native_abi::info::TASK_RUNTIME)
    }
}

/// 终止调用线程。
pub fn exit_thread() -> ! {
    // SAFETY: 无指针参数；调用不返回。
    unsafe { invoke(call::THREAD_EXIT, &[]) };
    unreachable!("thread_exit returned");
}

impl Exception {
    /// 结束异常处理：`resume` 时线程以 `state`（为 `None` 时不修改）继续，否则交给上一级。
    ///
    /// # Errors
    ///
    /// 寄存器状态非法。
    pub fn resolve(
        self,
        resume: bool,
        state: Option<&native_abi::register::RegisterState>,
    ) -> Result<(), Status> {
        let state = state.map_or(0, |state| core::ptr::from_ref(state) as usize);
        // SAFETY: 状态指针为 0 或指向有效的寄存器快照。
        check(unsafe {
            invoke(
                call::EXCEPTION_RESOLVE,
                &[self.raw() as usize, usize::from(resume), state],
            )
        })
    }
}
