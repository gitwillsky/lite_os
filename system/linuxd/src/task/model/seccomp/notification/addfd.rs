//! 观察者提交实际 OFD；只有被过滤线程操作其真实文件表。
use super::*;
use crate::{
    file::{FileDescriptorError, OpenFileDescription},
    fs::chardev::{DeviceError, IoctlCall},
};

#[derive(Clone, Copy)]
enum AddFdPhase {
    Pending,
    Processing,
    Done(isize),
}
struct AddFdState {
    phase: AddFdPhase,
    recovery: Option<Arc<RecoveryWaiter>>,
}
// OWNER: 每次 RECV 自己的单等待者事件。只在等待实际安装时挂入 command，完成时迭代摘除。
// 不能共用 ioctl observer 的 TaskEvent；多个延迟 copyout 可能同时等待同一次安装。
pub(super) struct RecoveryWaiter {
    ready: TaskEvent,
    next: Mutex<Option<Arc<RecoveryWaiter>>>,
}
impl RecoveryWaiter {
    pub(super) fn prepare() -> Result<Arc<Self>, DeviceError> {
        Arc::try_new(Self {
            ready: TaskEvent::new(),
            next: Mutex::new(None),
        })
        .map_err(|_| DeviceError::Errno(errno::ENOMEM))
    }
}
pub(super) struct AddFd {
    file: Arc<OpenFileDescription>,
    destination: Option<usize>,
    cloexec: bool,
    pub(super) send: bool,
    state: Mutex<AddFdState>,
    completed: TaskEvent,
}
impl AddFd {
    pub(super) fn wait_finished(&self, recovery: &Arc<RecoveryWaiter>) {
        {
            let mut state = self.state.lock();
            if matches!(state.phase, AddFdPhase::Done(_)) {
                return;
            }
            assert!(recovery.next.lock().is_none());
            *recovery.next.lock() = state.recovery.take();
            state.recovery = Some(recovery.clone());
        }
        recovery.ready.wait();
    }
    pub(super) fn begin(&self) {
        let mut state = self.state.lock();
        assert!(matches!(state.phase, AddFdPhase::Pending));
        state.phase = AddFdPhase::Processing;
    }
    pub(super) fn complete(&self, result: isize) {
        let mut recovery = {
            let mut state = self.state.lock();
            assert!(!matches!(state.phase, AddFdPhase::Done(_)));
            state.phase = AddFdPhase::Done(result);
            state.recovery.take()
        };
        // 不持 operation/next spin guard 调 scheduler，链的释放不递归。
        while let Some(waiter) = recovery {
            recovery = waiter.next.lock().take();
            waiter.ready.signal();
        }
        self.completed.signal();
    }
    pub(super) fn install(&self, task: &TaskControlBlock) -> isize {
        task.fd_receive(self.file.clone(), self.destination, self.cloexec)
            .map(|fd| fd as isize)
            .unwrap_or_else(|error| -match error {
                FileDescriptorError::NotFound => errno::EBADF,
                FileDescriptorError::Limit => errno::EMFILE,
                FileDescriptorError::OutOfMemory => errno::ENOMEM,
                FileDescriptorError::Busy => errno::EBUSY,
            })
    }
}
impl NotificationQueue {
    pub(super) fn add_fd(&self, call: &IoctlCall<'_>, size: usize) -> Result<isize, DeviceError> {
        let error = |value| DeviceError::Errno(value);
        if !(24..4096).contains(&size) {
            return Err(error(errno::EINVAL));
        }
        let mut bytes = [0u8; 24];
        call.user
            .read(call.argument, &mut bytes)
            .map_err(|_| error(errno::EFAULT))?;
        // copy_struct_from_user: unknown extension bytes must all be zero, before any fd lookup.
        let mut position = 24;
        while position < size {
            let mut extra = [0u8; 64];
            let count = (size - position).min(extra.len());
            call.user
                .read(
                    call.argument
                        .checked_add(position)
                        .ok_or_else(|| error(errno::EFAULT))?,
                    &mut extra[..count],
                )
                .map_err(|_| error(errno::EFAULT))?;
            if extra[..count].iter().any(|byte| *byte != 0) {
                return Err(error(errno::E2BIG));
            }
            position += count;
        }
        let id = u64::from_ne_bytes(bytes[..8].try_into().unwrap());
        let flags = u32::from_ne_bytes(bytes[8..12].try_into().unwrap());
        let source = u32::from_ne_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let destination = u32::from_ne_bytes(bytes[16..20].try_into().unwrap()) as usize;
        let fd_flags = u32::from_ne_bytes(bytes[20..].try_into().unwrap());
        if flags & !(ADDFD_FLAG_SETFD | ADDFD_FLAG_SEND) != 0
            || fd_flags & !crate::fs::O_CLOEXEC != 0
            || (destination != 0 && flags & ADDFD_FLAG_SETFD == 0)
        {
            return Err(error(errno::EINVAL));
        }
        let caller = crate::task::current_task().expect("seccomp listener requires task context");
        let file = caller.fd_get(source).ok_or_else(|| error(errno::EBADF))?;
        let command = Arc::try_new(AddFd {
            file,
            destination: (flags & ADDFD_FLAG_SETFD != 0).then_some(destination),
            cloexec: fd_flags != 0,
            send: flags & ADDFD_FLAG_SEND != 0,
            state: Mutex::new(AddFdState {
                phase: AddFdPhase::Pending,
                recovery: None,
            }),
            completed: TaskEvent::new(),
        })
        .map_err(|_| error(errno::ENOMEM))?;
        let request = self.find(id).ok_or_else(|| error(errno::ENOENT))?;
        let mut prepared = Vec::new();
        loop {
            let mut state = request.state.lock();
            if !state.active {
                return Err(error(errno::ENOENT));
            }
            if !matches!(state.phase, Phase::Sent) {
                return Err(error(errno::EINPROGRESS));
            }
            if command.send && !state.addfd.is_empty() {
                return Err(error(errno::EBUSY));
            }
            let needed = state.addfd.len() + 1;
            if state.addfd.capacity() < needed && prepared.capacity() < needed {
                drop(state);
                prepared
                    .try_reserve_exact(needed)
                    .map_err(|_| error(errno::ENOMEM))?;
                continue;
            }
            if state.addfd.capacity() < needed {
                prepared.append(&mut state.addfd);
                core::mem::swap(&mut prepared, &mut state.addfd);
            }
            if command.send {
                state.phase = Phase::AddFdReplyPending;
            }
            state.addfd.insert(0, command.clone());
            drop(state);
            drop(prepared);
            request.ready.signal();
            self.changed();
            break;
        }
        loop {
            if let AddFdPhase::Done(result) = command.state.lock().phase {
                return Ok(result);
            }
            if command.completed.wait_interruptible() == WaitResult::Interrupted {
                let mut state = request.state.lock();
                let result = command.state.lock().phase;
                match result {
                    AddFdPhase::Done(result) => return Ok(result),
                    AddFdPhase::Processing => {
                        // 已交给目标线程，不能撤回可能已发布的 fd；等待无分配完成再返回实际结果。
                        drop(state);
                        command.completed.wait();
                        continue;
                    }
                    AddFdPhase::Pending => {
                        let removed = state
                            .addfd
                            .iter()
                            .position(|item| Arc::ptr_eq(item, &command))
                            .map(|index| state.addfd.remove(index));
                        if command.send {
                            // 只有 Pending 的对应 command 可以把预占变成固定 Linux 的零值 reply；
                            // 普通注入完成不能误把尚未安装的 SEND 预占当作实际 response。
                            state.phase = Phase::Replied(Response {
                                value: 0,
                                error: 0,
                                flags: 0,
                            });
                        }
                        drop(state);
                        drop(removed);
                        request.ready.signal();
                        self.changed();
                        // Linux 的 pending SEND 取消保留已占用 reply（初始返回值为0），目标自行完成。
                        return Err(DeviceError::Restart);
                    }
                }
            }
        }
    }
}
