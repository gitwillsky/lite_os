//! 任务对象：Job、进程与线程。
//!
//! # 生命周期
//!
//! 1. 进程创建时建立地址空间与根 VMAR，并登记到所属 Job。
//! 2. `start` 创建并入队执行体（`thread::Thread`）；执行体持有本线程对象作为 owner。
//! 3. 终止（`process_exit`、`task_kill`、最后一个线程退出）先 kill 全部线程；每个线程退出时通知进程。
//! 4. 最后一个线程退出后进程完成终止：关闭句柄表（打破“句柄表持有引用自身的句柄”的引用环）、
//!    销毁根 VMAR 释放全部内存、置 `TERMINATED`，再通知 Job。
//! 5. Job 被 kill 后终止全部成员；成员全部结束时 Job 置 `TERMINATED` 并通知父 Job。

use alloc::sync::{Arc, Weak};

use native_abi::register::RegisterState;
use native_abi::{ObjectType, Status, rights, signals};
use spin::Mutex;

use super::{Channel, HandleTable, KernelObject, SignalState, WAITABLE_RIGHTS, next_koid, vm_status};
use crate::fallible_tree::FallibleMap;
use crate::vm::{AddressSpace, Vmar, Vmo};

const NAME_CAPACITY: usize = 32;

/// 定长对象名。
#[derive(Clone, Copy)]
struct Name {
    bytes: [u8; NAME_CAPACITY],
    length: usize,
}

impl Name {
    fn new(name: &[u8]) -> Self {
        let length = name.len().min(NAME_CAPACITY);
        let mut bytes = [0; NAME_CAPACITY];
        bytes[..length].copy_from_slice(&name[..length]);
        Self { bytes, length }
    }

}

// ---------------------------------------------------------------------------------------------
// Job
// ---------------------------------------------------------------------------------------------

struct JobState {
    // OWNER: 子 Job 与进程的 Weak 索引；kill 时逐个终止。Weak 不延长成员寿命。
    children: FallibleMap<u64, Weak<Job>>,
    processes: FallibleMap<u64, Weak<Process>>,
    // FLAG: 已被 kill；之后拒绝新成员，成员全部结束时置 TERMINATED。
    killed: bool,
}

/// 任务树节点：kill 传播与异常处理者的作用域。
pub(crate) struct Job {
    koid: u64,
    parent: Option<Arc<Job>>,
    signals: SignalState,
    state: Mutex<JobState>,
    exception_channel: Mutex<Option<Arc<Channel>>>,
}

impl Job {
    /// 创建根 Job。
    ///
    /// # Errors
    /// 分配失败返回 `ERR_NO_MEMORY`。
    pub(crate) fn create_root() -> Result<Arc<Self>, Status> {
        Self::new(None)
    }

    fn new(parent: Option<Arc<Job>>) -> Result<Arc<Self>, Status> {
        Arc::try_new(Self {
            koid: next_koid(),
            parent,
            signals: SignalState::new(0),
            state: Mutex::new(JobState {
                children: FallibleMap::new(),
                processes: FallibleMap::new(),
                killed: false,
            }),
            exception_channel: Mutex::new(None),
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)
    }

    /// 创建子 Job。
    ///
    /// # Errors
    /// 本 Job 已被 kill 返回 `ERR_BAD_STATE`。
    pub(crate) fn create_child(self: &Arc<Self>) -> Result<Arc<Self>, Status> {
        let child = Self::new(Some(self.clone()))?;
        let entry = FallibleMap::try_prepare(child.koid, Arc::downgrade(&child))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut state = self.state.lock();
        if state.killed {
            return Err(native_abi::ERR_BAD_STATE);
        }
        state.children.commit_vacant(entry);
        Ok(child)
    }

    fn add_process(&self, process: &Arc<Process>) -> Result<(), Status> {
        let entry = FallibleMap::try_prepare(process.koid, Arc::downgrade(process))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut state = self.state.lock();
        if state.killed {
            return Err(native_abi::ERR_BAD_STATE);
        }
        state.processes.commit_vacant(entry);
        Ok(())
    }

    /// 终止 Job 子树中的全部任务。
    pub(crate) fn kill(&self) {
        {
            let mut state = self.state.lock();
            if state.killed {
                return;
            }
            state.killed = true;
        }
        // 按 koid 游标逐个提升并在锁外终止，不为终止路径分配快照。
        let mut cursor = 0;
        loop {
            let next = {
                let state = self.state.lock();
                state
                    .processes
                    .iter_after(&cursor)
                    .next()
                    .map(|(&koid, process)| (koid, process.upgrade()))
            };
            let Some((koid, process)) = next else { break };
            cursor = koid;
            if let Some(process) = process {
                process.terminate(None);
            }
        }
        let mut cursor = 0;
        loop {
            let next = {
                let state = self.state.lock();
                state
                    .children
                    .iter_after(&cursor)
                    .next()
                    .map(|(&koid, child)| (koid, child.upgrade()))
            };
            let Some((koid, child)) = next else { break };
            cursor = koid;
            if let Some(child) = child {
                child.kill();
            }
        }
        self.check_finished();
    }

    fn member_finished(&self, process: Option<u64>, child: Option<u64>) {
        {
            let mut state = self.state.lock();
            if let Some(koid) = process {
                drop(state.processes.remove(&koid));
            }
            if let Some(koid) = child {
                drop(state.children.remove(&koid));
            }
        }
        self.check_finished();
    }

    fn check_finished(&self) {
        let finished = {
            let state = self.state.lock();
            state.killed
                && state.processes.values().all(|process| process.strong_count() == 0)
                && state.children.values().all(|child| child.strong_count() == 0)
        };
        if finished && self.signals.active() & signals::TERMINATED == 0 {
            self.signals.update(0, signals::TERMINATED);
            if let Some(parent) = &self.parent {
                parent.member_finished(None, Some(self.koid));
            }
        }
    }

    /// 异常处理者 channel（若已创建）。
    pub(crate) fn exception_channel(&self) -> Option<Arc<Channel>> {
        self.exception_channel.lock().clone()
    }

    /// 父 Job。
    pub(crate) fn parent(&self) -> Option<&Arc<Job>> {
        self.parent.as_ref()
    }

    /// 绑定异常 channel 的内核端点。
    ///
    /// # Errors
    /// 已绑定返回 `ERR_ALREADY_BOUND`。
    pub(crate) fn bind_exception_channel(&self, channel: Arc<Channel>) -> Result<(), Status> {
        bind_exception(&self.exception_channel, channel)
    }
}

fn bind_exception(slot: &Mutex<Option<Arc<Channel>>>, channel: Arc<Channel>) -> Result<(), Status> {
    let mut slot = slot.lock();
    // 处理者关闭其端点后旧绑定失效，可以重新绑定。
    if slot.as_ref().is_some_and(|existing| existing.signals_active() & signals::PEER_CLOSED == 0) {
        return Err(native_abi::ERR_ALREADY_BOUND);
    }
    *slot = Some(channel);
    Ok(())
}

impl KernelObject for Job {
    fn object_type(&self) -> ObjectType {
        ObjectType::Job
    }
    fn koid(&self) -> u64 {
        self.koid
    }
    fn default_rights(&self) -> u32 {
        WAITABLE_RIGHTS | rights::MANAGE
    }
    fn signals(&self) -> Option<&SignalState> {
        Some(&self.signals)
    }
}

// ---------------------------------------------------------------------------------------------
// Process
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    Terminating,
    Dead,
}

struct Lifecycle {
    phase: Phase,
    started: bool,
    // OWNER: 已启动且尚未退出的线程；终止时逐个 kill，退出时按 koid 摘除。
    threads: FallibleMap<u64, Weak<Thread>>,
    live: usize,
    /// `process_exit` 的退出码；被 kill 时为 `None`。
    exit_code: Option<i64>,
}

/// 进程创建方式。
pub(crate) enum ProcessKind<'a> {
    /// 管理完整用户区间。
    Plain,
    /// 只管理共享区，供成员进程共享。
    Shared,
    /// 共享 `shared` 的共享区，自己管理私有区。
    Member(&'a Process),
}

/// 进程：句柄表 + 地址空间 + 线程集合。
pub(crate) struct Process {
    koid: u64,
    name: Mutex<Name>,
    job: Arc<Job>,
    handles: HandleTable,
    space: Arc<AddressSpace>,
    root_vmar: Arc<Vmar>,
    shared_root: bool,
    signals: SignalState,
    lifecycle: Mutex<Lifecycle>,
    exception_channel: Mutex<Option<Arc<Channel>>>,
}

impl Process {
    /// 在 `job` 中创建进程。
    ///
    /// # Errors
    /// 成员进程的 `shared` 不是共享进程返回 `ERR_INVALID_ARGS`；Job 已被 kill 返回 `ERR_BAD_STATE`。
    pub(crate) fn create(job: &Arc<Job>, name: &[u8], kind: ProcessKind<'_>) -> Result<Arc<Self>, Status> {
        let (space, root_vmar, shared_root) = match kind {
            ProcessKind::Plain => {
                let (space, vmar) = AddressSpace::create().map_err(vm_status)?;
                (space, vmar, false)
            }
            ProcessKind::Shared => {
                let (space, vmar) = AddressSpace::create_shared().map_err(vm_status)?;
                (space, vmar, true)
            }
            ProcessKind::Member(shared) => {
                if !shared.shared_root {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                let (space, vmar) = AddressSpace::create_member(&shared.space).map_err(vm_status)?;
                (space, vmar, false)
            }
        };
        let process = Arc::try_new(Self {
            koid: next_koid(),
            name: Mutex::new(Name::new(name)),
            job: job.clone(),
            handles: HandleTable::new(),
            space,
            root_vmar,
            shared_root,
            signals: SignalState::new(0),
            lifecycle: Mutex::new(Lifecycle {
                phase: Phase::Running,
                started: false,
                threads: FallibleMap::new(),
                live: 0,
                exit_code: None,
            }),
            exception_channel: Mutex::new(None),
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        job.add_process(&process)?;
        Ok(process)
    }

    /// 句柄表。
    pub(crate) fn handles(&self) -> &HandleTable {
        &self.handles
    }

    /// 地址空间。
    pub(crate) fn space(&self) -> &Arc<AddressSpace> {
        &self.space
    }

    /// 根 VMAR。
    pub(crate) fn root_vmar(&self) -> &Arc<Vmar> {
        &self.root_vmar
    }

    /// 所属 Job。
    pub(crate) fn job(&self) -> &Arc<Job> {
        &self.job
    }

    /// 名称。
    pub(crate) fn name(&self) -> ([u8; NAME_CAPACITY], usize) {
        let name = *self.name.lock();
        (name.bytes, name.length)
    }

    /// 设置名称。
    pub(crate) fn set_name(&self, name: &[u8]) {
        *self.name.lock() = Name::new(name);
    }

    /// 是否已终止（或正在终止）。
    pub(crate) fn is_terminating(&self) -> bool {
        self.lifecycle.lock().phase != Phase::Running
    }

    /// `(已终止, 退出码)`；被 kill 时退出码为 -1。
    pub(crate) fn exit_status(&self) -> (bool, i64) {
        let lifecycle = self.lifecycle.lock();
        (lifecycle.phase == Phase::Dead, lifecycle.exit_code.unwrap_or(-1))
    }

    /// 是否已启动首个线程。
    pub(crate) fn mark_started(&self) -> Result<(), Status> {
        let mut lifecycle = self.lifecycle.lock();
        if lifecycle.phase != Phase::Running || lifecycle.started {
            return Err(native_abi::ERR_BAD_STATE);
        }
        lifecycle.started = true;
        Ok(())
    }

    fn register_thread(&self, thread: &Arc<Thread>) -> Result<(), Status> {
        let entry = FallibleMap::try_prepare(thread.koid, Arc::downgrade(thread))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut lifecycle = self.lifecycle.lock();
        if lifecycle.phase != Phase::Running {
            return Err(native_abi::ERR_BAD_STATE);
        }
        lifecycle.threads.commit_vacant(entry);
        lifecycle.live += 1;
        Ok(())
    }

    fn unregister_thread(&self, koid: u64) {
        let finished = {
            let mut lifecycle = self.lifecycle.lock();
            drop(lifecycle.threads.remove(&koid));
            lifecycle.live -= 1;
            if lifecycle.live == 0 && lifecycle.phase != Phase::Dead {
                lifecycle.phase = Phase::Dead;
                true
            } else {
                false
            }
        };
        if finished {
            self.finish();
        }
    }

    /// 开始终止：之后拒绝新线程，并 kill 全部线程；没有线程时立即完成终止。
    ///
    /// # Parameters
    /// - `code`: `process_exit` 的退出码；被 kill 时为 `None`。
    pub(crate) fn terminate(&self, code: Option<i64>) {
        let finished = {
            let mut lifecycle = self.lifecycle.lock();
            if lifecycle.phase != Phase::Running {
                return;
            }
            lifecycle.exit_code = code;
            if lifecycle.live == 0 {
                lifecycle.phase = Phase::Dead;
                true
            } else {
                lifecycle.phase = Phase::Terminating;
                false
            }
        };
        if finished {
            self.finish();
            return;
        }
        let mut cursor = 0;
        loop {
            let next = {
                let lifecycle = self.lifecycle.lock();
                lifecycle
                    .threads
                    .iter_after(&cursor)
                    .next()
                    .map(|(&koid, thread)| (koid, thread.upgrade()))
            };
            let Some((koid, thread)) = next else { break };
            cursor = koid;
            if let Some(thread) = thread {
                thread.kill();
            }
        }
    }

    /// 完成终止：只执行一次。
    fn finish(&self) {
        for (handle, entry) in self.handles.close().into_entries() {
            entry.object.kernel_object().cancel(self.handles.cancel_key(handle));
            drop(entry);
        }
        // 根 VMAR 销毁释放进程的全部映射；进程对象（与句柄）仍可存活以读取退出状态。
        if let Err(error) = self.root_vmar.destroy() {
            warn!("process {} address space teardown failed: {error:?}", self.koid);
        }
        self.signals.update(0, signals::TERMINATED);
        self.job.member_finished(Some(self.koid), None);
    }

    /// 异常处理者 channel（若已创建）。
    pub(crate) fn exception_channel(&self) -> Option<Arc<Channel>> {
        self.exception_channel.lock().clone()
    }

    /// 绑定异常 channel 的内核端点。
    ///
    /// # Errors
    /// 已绑定返回 `ERR_ALREADY_BOUND`。
    pub(crate) fn bind_exception_channel(&self, channel: Arc<Channel>) -> Result<(), Status> {
        bind_exception(&self.exception_channel, channel)
    }

    /// 已消耗的 CPU 时间（全部存活线程）。
    pub(crate) fn runtime_us(&self) -> u64 {
        let mut total = 0;
        let mut cursor = 0;
        loop {
            let next = {
                let lifecycle = self.lifecycle.lock();
                lifecycle
                    .threads
                    .iter_after(&cursor)
                    .next()
                    .map(|(&koid, thread)| (koid, thread.upgrade()))
            };
            let Some((koid, thread)) = next else { break };
            cursor = koid;
            if let Some(thread) = thread {
                total += thread.runtime_us();
            }
        }
        total
    }
}

impl KernelObject for Process {
    fn object_type(&self) -> ObjectType {
        ObjectType::Process
    }
    fn koid(&self) -> u64 {
        self.koid
    }
    fn default_rights(&self) -> u32 {
        WAITABLE_RIGHTS | rights::MANAGE | rights::READ | rights::WRITE
    }
    fn signals(&self) -> Option<&SignalState> {
        Some(&self.signals)
    }
}

// ---------------------------------------------------------------------------------------------
// Thread
// ---------------------------------------------------------------------------------------------

enum Execution {
    Created,
    // OWNER: 运行期执行体；执行体以本对象为 owner，退出时在这里释放以打断引用环。
    Running(Arc<crate::thread::Thread>),
    Exited,
}

/// 线程所处的执行模式。
#[derive(Clone, Copy)]
#[expect(
    clippy::large_enum_variant,
    reason = "restricted entry is saved and restored on every restricted transition; boxing it would allocate on that hot path"
)]
pub(crate) enum Mode {
    /// 普通模式：syscall 按 native ABI 解释。
    Normal,
    /// restricted mode：任何 syscall、未解决异常或 kick 都退回 normal mode 的 `vector`。
    Restricted(RestrictedEntry),
}

/// 进入 restricted mode 时保存的 normal mode 返回信息。
#[derive(Clone, Copy)]
pub(crate) struct RestrictedEntry {
    pub(crate) vector: usize,
    pub(crate) context: usize,
    /// 进入时的 normal mode 寄存器；退出时恢复。
    pub(crate) normal: RegisterState,
}

struct Restricted {
    // OWNER: `restricted_bind_state` 绑定的状态 VMO；进入时读取、退出时写回。
    state: Option<Arc<Vmo>>,
    mode: Mode,
}

/// 线程对象。
pub(crate) struct Thread {
    koid: u64,
    name: Mutex<Name>,
    process: Arc<Process>,
    signals: SignalState,
    execution: Mutex<Execution>,
    restricted: Mutex<Restricted>,
}

impl Thread {
    /// 在进程中创建未启动的线程。
    ///
    /// # Errors
    /// 进程已终止返回 `ERR_BAD_STATE`。
    pub(crate) fn create(process: &Arc<Process>, name: &[u8]) -> Result<Arc<Self>, Status> {
        if process.is_terminating() {
            return Err(native_abi::ERR_BAD_STATE);
        }
        Arc::try_new(Self {
            koid: next_koid(),
            name: Mutex::new(Name::new(name)),
            process: process.clone(),
            signals: SignalState::new(0),
            execution: Mutex::new(Execution::Created),
            restricted: Mutex::new(Restricted {
                state: None,
                mode: Mode::Normal,
            }),
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)
    }

    /// 所属进程。
    pub(crate) fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// 名称。
    pub(crate) fn name(&self) -> ([u8; NAME_CAPACITY], usize) {
        let name = *self.name.lock();
        (name.bytes, name.length)
    }

    /// 设置名称。
    pub(crate) fn set_name(&self, name: &[u8]) {
        *self.name.lock() = Name::new(name);
    }

    /// 启动线程。`owner` 是本对象作为执行体 owner 的 trait 视图。
    ///
    /// # Errors
    /// 已启动或进程已终止返回 `ERR_BAD_STATE`；执行体分配失败返回 `ERR_NO_MEMORY`。
    pub(crate) fn start(
        self: &Arc<Self>,
        owner: Arc<dyn crate::thread::ThreadOwner>,
        start: crate::thread::UserStart,
    ) -> Result<(), Status> {
        if !start.stack.is_multiple_of(16) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        {
            let execution = self.execution.lock();
            if !matches!(*execution, Execution::Created) {
                return Err(native_abi::ERR_BAD_STATE);
            }
        }
        self.process.register_thread(self)?;
        let thread = match crate::thread::Thread::create_user(
            self.process.space.clone(),
            owner,
            start,
        ) {
            Ok(thread) => thread,
            Err(()) => {
                self.process.unregister_thread(self.koid);
                return Err(native_abi::ERR_NO_MEMORY);
            }
        };
        {
            let mut execution = self.execution.lock();
            if !matches!(*execution, Execution::Created) {
                drop(execution);
                drop(thread);
                self.process.unregister_thread(self.koid);
                return Err(native_abi::ERR_BAD_STATE);
            }
            *execution = Execution::Running(thread.clone());
        }
        // 进程可能在登记与发布之间开始终止：此时它的 kill 遍历已看到本线程，但执行体尚未发布。
        if self.process.is_terminating() {
            thread.kill();
        }
        thread.start();
        Ok(())
    }

    /// 终止线程。
    pub(crate) fn kill(self: &Arc<Self>) {
        let running = {
            let mut execution = self.execution.lock();
            match &*execution {
                Execution::Running(thread) => Some(thread.clone()),
                Execution::Created => {
                    *execution = Execution::Exited;
                    None
                }
                Execution::Exited => None,
            }
        };
        match running {
            Some(thread) => thread.kill(),
            None => self.signals.update(0, signals::TERMINATED),
        }
    }

    /// 执行体退出后调用一次（由 `ThreadOwner::exited`）。
    pub(crate) fn exited(&self) {
        let retired = core::mem::replace(&mut *self.execution.lock(), Execution::Exited);
        drop(retired);
        self.restricted.lock().state = None;
        self.signals.update(0, signals::TERMINATED);
        self.process.unregister_thread(self.koid);
    }

    /// 运行中的执行体。
    pub(crate) fn execution(&self) -> Option<Arc<crate::thread::Thread>> {
        match &*self.execution.lock() {
            Execution::Running(thread) => Some(thread.clone()),
            Execution::Created | Execution::Exited => None,
        }
    }

    /// 已消耗的 CPU 时间。
    pub(crate) fn runtime_us(&self) -> u64 {
        self.execution().map_or(0, |thread| thread.runtime_us())
    }

    /// 绑定 restricted 状态 VMO。
    pub(crate) fn bind_restricted_state(&self, vmo: Arc<Vmo>) {
        self.restricted.lock().state = Some(vmo);
    }

    /// 解除绑定。
    pub(crate) fn unbind_restricted_state(&self) {
        self.restricted.lock().state = None;
    }

    /// 绑定的 restricted 状态 VMO。
    pub(crate) fn restricted_state(&self) -> Option<Arc<Vmo>> {
        self.restricted.lock().state.clone()
    }

    /// 当前执行模式。
    pub(crate) fn mode(&self) -> Mode {
        self.restricted.lock().mode
    }

    /// 切换执行模式。
    pub(crate) fn set_mode(&self, mode: Mode) {
        self.restricted.lock().mode = mode;
    }
}

impl KernelObject for Thread {
    fn object_type(&self) -> ObjectType {
        ObjectType::Thread
    }
    fn koid(&self) -> u64 {
        self.koid
    }
    fn default_rights(&self) -> u32 {
        WAITABLE_RIGHTS | rights::MANAGE
    }
    fn signals(&self) -> Option<&SignalState> {
        Some(&self.signals)
    }
}
