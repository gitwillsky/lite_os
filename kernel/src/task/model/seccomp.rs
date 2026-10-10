//! Thread 独占的 Linux no_new_privs 与 seccomp 状态。

#[path = "seccomp/filter.rs"]
mod filter;
#[path = "seccomp/notification.rs"]
mod notification;
use notification::{ListenerFile, NotificationQueue};

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;
use syscall_abi::{seccomp::*, signal::SIGKILL};

use super::TaskControlBlock;
use filter::Program;

/// seccomp installation 的领域失败；syscall owner 负责 errno 投影。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SeccompError {
    Invalid,
    OutOfMemory,
    Unsynchronized(usize),
    Interrupted,
    Busy,
    Descriptor(crate::file::FileDescriptorError),
}

/// syscall 执行前的领域决策；通知仅保活实际 queue，不能保活 TCB 或 program ancestry。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SeccompDecision {
    Allow,
    KillThread(usize),
    KillProcess,
    Trap(u16),
    Errno(u16),
    NoObserver,
    Notify {
        queue: Arc<NotificationQueue>,
        data: [u32; 16],
        killable_after_receive: bool,
    },
    Log,
}

#[derive(Debug, Clone)]
enum Mode {
    Disabled,
    Strict,
    Filter(Arc<FilterSet>),
}

#[derive(Debug)]
struct FilterSet {
    // OWNER: oldest→newest immutable program references；共享 program，不链接 ancestor set。
    // 数千次追加后的 final Drop 只线性释放 Vec，不能递归销毁 Arc ancestor 耗尽 kernel stack。
    programs: Vec<Arc<InstalledFilter>>,
}

/// 单个不可变 filter 及其实际 listener；policy snapshots 不计入 listener 的 task users。
#[derive(Debug)]
struct InstalledFilter {
    program: Program,
    flags: u32,
    notifications: Option<Arc<NotificationQueue>>,
}
impl Mode {
    // graph publication 仅增加原子 membership；解除可能唤醒 observer，必须移到 graph 外。
    fn attach(&self) {
        if let Self::Filter(set) = self {
            for filter in &set.programs {
                if let Some(queue) = &filter.notifications {
                    queue.attach_user();
                }
            }
        }
    }
    fn detach(&self) {
        if let Self::Filter(set) = self {
            for filter in &set.programs {
                if let Some(queue) = &filter.notifications {
                    queue.detach_user();
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ThreadSecurity {
    no_new_privileges: bool,
    mode: Mode,
}

/// 每个 Thread 的唯一 security owner；sibling 从不共享可变 policy。
#[derive(Debug)]
pub(in super::super) struct ThreadSecurityOwner {
    state: Mutex<ThreadSecurity>,
    // CACHE: false 精确表示尚未启用 seccomp，disabled syscall 因此不获取 state lock。
    // graph 串行化所有 mode publication；state 提交后 Release=true。
    // 无撤销路径；远端 TSYNC 返回前已发布，使之后的 syscall Acquire 不绕过 policy。
    active: AtomicBool,
}

impl Default for ThreadSecurityOwner {
    fn default() -> Self {
        Self::from_state(ThreadSecurity {
            no_new_privileges: false,
            mode: Mode::Disabled,
        })
    }
}

impl ThreadSecurityOwner {
    fn from_state(state: ThreadSecurity) -> Self {
        let active = !matches!(state.mode, Mode::Disabled);
        Self {
            state: Mutex::new(state),
            active: AtomicBool::new(active),
        }
    }

    pub(in super::super) fn enable_no_new_privileges(&self) {
        self.state.lock().no_new_privileges = true;
    }

    pub(in super::super) fn no_new_privileges(&self) -> bool {
        self.state.lock().no_new_privileges
    }

    pub(in super::super) fn mode(&self) -> u32 {
        match &self.state.lock().mode {
            Mode::Disabled => MODE_DISABLED,
            Mode::Strict => MODE_STRICT,
            Mode::Filter(_) => MODE_FILTER,
        }
    }

    pub(in super::super) fn enable_strict(&self) -> Result<(), SeccompError> {
        let mut state = self.state.lock();
        if matches!(state.mode, Mode::Filter(_)) {
            return Err(SeccompError::Invalid);
        }
        state.mode = Mode::Strict;
        self.active.store(true, Ordering::Release);
        Ok(())
    }

    /// 在 graph 锁外准备追加；后续提交必须复查同一旧 mode，远端 TSYNC 可使它过期。
    pub(in super::super) fn prepare_filter(
        &self,
        program: &ValidatedFilter,
    ) -> Result<PreparedFilter, SeccompError> {
        let previous = self.state.lock().mode.clone();
        let existing = match &previous {
            Mode::Disabled => &[][..],
            Mode::Filter(set) => set.programs.as_slice(),
            Mode::Strict => return Err(SeccompError::Invalid),
        };
        if program.0.notifications.is_some()
            && existing.iter().any(|filter| {
                filter
                    .notifications
                    .as_ref()
                    .is_some_and(|queue| queue.is_attached())
            })
        {
            return Err(SeccompError::Busy);
        }
        let cost = program.0.program.length()
            + existing
                .iter()
                .map(|filter| filter.program.length() + 4)
                .sum::<usize>();
        if cost > 32768 {
            return Err(SeccompError::OutOfMemory);
        }
        let mut programs = Vec::new();
        programs
            .try_reserve_exact(existing.len() + 1)
            .map_err(|_| SeccompError::OutOfMemory)?;
        programs.extend(existing.iter().cloned());
        programs.push(program.0.clone());
        let next = Arc::try_new(FilterSet { programs }).map_err(|_| SeccompError::OutOfMemory)?;
        Ok(PreparedFilter { previous, next })
    }

    /// clone publication 在 graph 内从真实 creator 重取 policy；返回旧 owner 锁外释放。
    pub(in super::super) fn inherit_from(&self, creator: &Self) -> RetiredSecurity {
        let inherited = creator.state.lock().clone();
        let active = !matches!(inherited.mode, Mode::Disabled);
        inherited.mode.attach();
        let old = core::mem::replace(&mut *self.state.lock(), inherited);
        self.active.store(active, Ordering::Release);
        RetiredSecurity { _state: old }
    }

    /// exit 已被 graph 标记后退休实际 membership；保留的 TCB 不能阻止 listener HUP。
    pub(in super::super) fn retire(&self) {
        let old = core::mem::replace(&mut self.state.lock().mode, Mode::Disabled);
        self.active.store(false, Ordering::Release);
        old.detach();
    }

    /// 以 Linux seccomp_data 评估 syscall；disabled 路径只有一次原子读取，不读取 PC。
    ///
    /// `number`/`args` 为 raw registers，`architecture` 为静态 AUDIT_ARCH，`pc` 返回 syscall 后 PC。
    /// 所有 program Arc 在返回前释放；USER_NOTIF 仅返回实际 queue 与 seccomp_data。
    pub(in super::super) fn evaluate(
        &self,
        number: usize,
        args: [usize; 6],
        architecture: u32,
        pc: impl FnOnce() -> usize,
        log_action: impl FnOnce(u32),
    ) -> SeccompDecision {
        if !self.active.load(Ordering::Acquire) {
            return SeccompDecision::Allow;
        }
        let mode = self.state.lock().mode.clone();
        match mode {
            Mode::Disabled => unreachable!("active seccomp cannot be disabled"),
            Mode::Strict => {
                if matches!(
                    number as u32 as usize,
                    syscall_abi::SYSCALL_READ
                        | syscall_abi::SYSCALL_WRITE
                        | syscall_abi::SYSCALL_EXIT
                        | syscall_abi::SYSCALL_RT_SIGRETURN
                ) {
                    SeccompDecision::Allow
                } else {
                    SeccompDecision::KillThread(SIGKILL)
                }
            }
            Mode::Filter(set) => {
                let mut data = [0u32; 16];
                data[0] = number as u32;
                data[1] = architecture;
                let pointer = pc() as u64;
                data[2] = pointer as u32;
                data[3] = (pointer >> 32) as u32;
                for (index, argument) in args.iter().enumerate() {
                    data[4 + index * 2] = *argument as u32;
                    data[5 + index * 2] = (*argument as u64 >> 32) as u32;
                }
                let mut result = RET_ALLOW;
                let mut matched = None;
                for program in set.programs.iter().rev() {
                    let candidate = program.program.evaluate(&data);
                    // Linux action precedence 是 signed；同级保留 newest 的 DATA。
                    if ((candidate & RET_ACTION_FULL) as i32) < ((result & RET_ACTION_FULL) as i32)
                    {
                        result = candidate;
                        matched = Some(program);
                    }
                }
                let action = result & RET_ACTION_FULL;
                if action == RET_LOG
                    || (action != RET_ALLOW
                        && matched.is_some_and(|filter| filter.flags & FILTER_FLAG_LOG != 0))
                {
                    log_action(action);
                }
                let value = (result & RET_DATA) as u16;
                match result & RET_ACTION_FULL {
                    RET_ALLOW => SeccompDecision::Allow,
                    RET_KILL_THREAD => SeccompDecision::KillThread(syscall_abi::signal::SIGSYS),
                    RET_TRAP => SeccompDecision::Trap(value),
                    RET_ERRNO => SeccompDecision::Errno(value.min(4095)),
                    RET_TRACE => SeccompDecision::NoObserver,
                    RET_USER_NOTIF => match matched.and_then(|filter| filter.notifications.clone())
                    {
                        Some(queue) => SeccompDecision::Notify {
                            queue,
                            data,
                            killable_after_receive: matched.unwrap().flags
                                & FILTER_FLAG_WAIT_KILLABLE_RECV
                                != 0,
                        },
                        None => SeccompDecision::NoObserver,
                    },
                    RET_LOG => SeccompDecision::Log,
                    _ => SeccompDecision::KillProcess,
                }
            }
        }
    }
}

/// 验证完成但尚未发布的新叶；installation 事务至多成功提交一次。
/// 此约束使 leaf Arc 唯一确定父 prefix，TSYNC ancestry 能按叶 identity 常数时间比较。
pub(in super::super) struct ValidatedFilter(Arc<InstalledFilter>);
impl ValidatedFilter {
    pub(in super::super) fn prepare(bytes: &[u8], flags: u32) -> Result<Self, SeccompError> {
        let program = Program::prepare(bytes)?;
        // 固定 Linux generic arch_seccomp_spec_mitigate 是空 weak hook。当前两个静态 backend
        // 都未提供 per-task speculation override；SPEC_ALLOW 因而不改变 architecture profile，
        // 不能据此宣称已增加硬件缓解或让 child policy 撤销既有缓解。
        let notifications = if flags & FILTER_FLAG_NEW_LISTENER != 0 {
            Some(NotificationQueue::new()?)
        } else {
            None
        };
        Ok(Self(
            Arc::try_new(InstalledFilter {
                program,
                flags,
                notifications,
            })
            .map_err(|_| SeccompError::OutOfMemory)?,
        ))
    }
    pub(in super::super) fn listener(
        &self,
    ) -> Result<Option<Arc<dyn crate::fs::device::DeviceFile>>, SeccompError> {
        self.0
            .notifications
            .as_ref()
            .map(|queue| {
                Arc::try_new(ListenerFile::new(queue.clone()))
                    .map(|file| file as Arc<dyn crate::fs::device::DeviceFile>)
                    .map_err(|_| SeccompError::OutOfMemory)
            })
            .transpose()
    }
}

/// 不可变 prepared append；graph owner 复查后一次无失败发布。
pub(in super::super) struct PreparedFilter {
    previous: Mode,
    next: Arc<FilterSet>,
}

/// 被替换的 owner；必须移出 graph/process_creation 锁后才释放最后一个 program Arc。
pub(in super::super) struct RetiredSecurity {
    _state: ThreadSecurity,
}

impl Drop for RetiredSecurity {
    fn drop(&mut self) {
        self._state.mode.detach();
    }
}
impl Drop for ThreadSecurityOwner {
    fn drop(&mut self) {
        self.state.get_mut().mode.detach();
    }
}

impl PreparedFilter {
    /// 只能在 graph 锁内复查调用者；同一 FilterSet Arc 是精确 policy identity。
    pub(in super::super) fn matches(&self, caller: &ThreadSecurityOwner) -> bool {
        match (&self.previous, &caller.state.lock().mode) {
            (Mode::Disabled, Mode::Disabled) => true,
            (Mode::Filter(old), Mode::Filter(current)) => Arc::ptr_eq(old, current),
            _ => false,
        }
    }

    /// graph 锁内校验 TSYNC sibling；disabled 或 calling policy 的 ancestor 才可同步。
    pub(in super::super) fn accepts(&self, sibling: &ThreadSecurityOwner) -> bool {
        match (&sibling.state.lock().mode, &self.previous) {
            (Mode::Disabled, _) => true,
            (Mode::Filter(target), Mode::Filter(caller)) => {
                let length = target.programs.len();
                length <= caller.programs.len()
                    && Arc::ptr_eq(&target.programs[length - 1], &caller.programs[length - 1])
            }
            _ => false,
        }
    }

    /// 所有 targets 校验后才调用；只替换 mode，NNP 与并发 prctl 的单向置位合并。
    pub(in super::super) fn publish(
        &self,
        target: &ThreadSecurityOwner,
        no_new_privileges: bool,
    ) -> RetiredSecurity {
        Mode::Filter(self.next.clone()).attach();
        let old = {
            let mut state = target.state.lock();
            state.no_new_privileges |= no_new_privileges;
            core::mem::replace(&mut state.mode, Mode::Filter(self.next.clone()))
        };
        target.active.store(true, Ordering::Release);
        RetiredSecurity {
            _state: ThreadSecurity {
                no_new_privileges: false,
                mode: old,
            },
        }
    }
}

impl TaskControlBlock {
    /// 取得 calling Thread 的 seccomp mode（0/1/2）。
    pub(crate) fn seccomp_mode(&self) -> u32 {
        self.thread().security.mode()
    }

    /// 仅 task 域的 graph installation/publication 事务可取得可变 policy seam。
    pub(in super::super) fn security_owner(&self) -> &ThreadSecurityOwner {
        &self.thread().security
    }

    /// 当前 Thread 阻塞于真实 notification，返回 CONTINUE 或已编码的 syscall result。
    pub(crate) fn wait_seccomp_notification(
        &self,
        queue: &Arc<NotificationQueue>,
        data: [u32; 16],
        killable_after_receive: bool,
    ) -> Result<Option<isize>, SeccompError> {
        queue.notify(self, data, killable_after_receive)
    }

    pub(super) fn retire_seccomp(&self) {
        self.thread().security.retire();
    }

    /// 不把 Thread/lock/program owner 暴露给 dispatcher。
    pub(crate) fn evaluate_seccomp(&self, number: usize, args: [usize; 6]) -> SeccompDecision {
        self.thread().security.evaluate(
            number,
            args,
            crate::arch::user::AUDIT_ARCH,
            || self.user_program_counter(),
            |action| {
                info!(
                    "seccomp tid={} syscall={} arch={:#x} action={:#x}",
                    self.tid(),
                    number,
                    crate::arch::user::AUDIT_ARCH,
                    action
                );
            },
        )
    }
}
