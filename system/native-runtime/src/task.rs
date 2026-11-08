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
        self.info_array(topic)
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

    /// 为已启动系统线程设置 CBS/EDF 预算，三个时间全零恢复公平类。
    ///
    /// # Parameters
    /// resource 必须覆盖 system::SCHEDULE；runtime/deadline/period 单位纳秒。
    /// # Returns
    /// 准入并替换成功返回 Ok；失败保留旧配置。
    /// # Errors
    /// 无授权、线程未启动/已退出、参数非法或带宽不足。
    pub fn set_deadline(
        &self,
        resource: &crate::Resource,
        runtime: u64,
        deadline: u64,
        period: u64,
    ) -> Result<(), Status> {
        // SAFETY: 无指针参数；系统 resource 与线程句柄由借用 owner 保活。
        check(unsafe {
            invoke(
                call::THREAD_SET_DEADLINE,
                &[
                    resource.raw() as usize,
                    self.raw() as usize,
                    runtime as usize,
                    deadline as usize,
                    period as usize,
                ],
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

    /// 为已启动系统 worker 设置独占逻辑 CPU；None 归还并恢复全部 CPU 的普通 affinity。
    ///
    /// # Parameters
    /// resource 必须覆盖 system::SCHEDULE，cpu 是逻辑 CPU 编号。
    /// # Returns
    /// 旧普通线程已撤离目标核后返回 Ok；线程退出自动归还独占核。
    /// # Errors
    /// 无授权、CPU 非 active、分区/截止预算冲突、未启动/退出或并发替换。
    /// 独占期间普通 set_affinity 被拒绝；设备中断仍须单独设置亲和。
    pub fn set_isolation(
        &self,
        resource: &crate::Resource,
        cpu: Option<u32>,
    ) -> Result<(), Status> {
        // SAFETY: 无指针参数，resource 和线程句柄由借用 owner 保活。
        check(unsafe {
            invoke(
                call::THREAD_SET_ISOLATION,
                &[
                    resource.raw() as usize,
                    self.raw() as usize,
                    cpu.map_or(usize::MAX, |cpu| cpu as usize),
                ],
            )
        })
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

/// 崩溃循环判定：[`CRASH_WINDOW`] 纳秒内终止达到 [`CRASH_LIMIT`] 次即不再重启（对应 systemd
/// `StartLimitBurst`/`StartLimitIntervalSec`）。缺失时持续崩溃的服务会让监督者无限重启它。
pub const CRASH_LIMIT: usize = 5;
/// 崩溃循环判定窗口（纳秒）。
pub const CRASH_WINDOW: i64 = 10_000_000_000;

/// 受监督进程的终止计数：[`CRASH_WINDOW`] 内达到 [`CRASH_LIMIT`] 次即判定为崩溃循环。
pub struct CrashLoop {
    // 环形记录最近 CRASH_LIMIT 次终止时刻；最旧一次仍在窗口内即为崩溃循环。
    terminations: [i64; CRASH_LIMIT],
    next: usize,
}

impl CrashLoop {
    /// 尚无终止记录。
    pub fn new() -> Self {
        Self {
            terminations: [i64::MIN; CRASH_LIMIT],
            next: 0,
        }
    }

    /// 按最旧至最新顺序导出崩溃时刻，供监督权移交。
    pub fn snapshot(&self) -> [i64; CRASH_LIMIT] {
        let mut terminations = self.terminations;
        terminations.rotate_left(self.next);
        terminations
    }

    /// 从最旧至最新排列的崩溃时刻恢复监督窗口。
    pub fn restore(terminations: [i64; CRASH_LIMIT]) -> Self {
        Self {
            terminations,
            next: 0,
        }
    }

    /// 记录一次终止。
    ///
    /// # Returns
    ///
    /// 已构成崩溃循环时为 `true`，监督者应停止重启。
    pub fn record(&mut self) -> bool {
        let now = crate::system::monotonic();
        self.terminations[self.next] = now;
        self.next = (self.next + 1) % CRASH_LIMIT;
        now.saturating_sub(self.terminations[self.next]) < CRASH_WINDOW
    }
}

impl Default for CrashLoop {
    fn default() -> Self {
        Self::new()
    }
}
