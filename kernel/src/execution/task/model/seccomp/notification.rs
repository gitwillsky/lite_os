//! 实际 seccomp listener、活动请求与线程 membership；临时 policy Arc 不计 users。

mod addfd;
mod listener;
pub(super) use listener::ListenerFile;

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use spin::Mutex;
use syscall_abi::{errno, seccomp::*};

use super::{SeccompError, TaskControlBlock};
use crate::{
    execution::sync::{TaskEvent, WaitResult},
    ipc::{Pipe, PipeEnd},
};
use addfd::AddFd;

#[derive(Clone, Copy)]
struct Response {
    value: i64,
    error: i32,
    flags: u32,
}
impl Response {
    const NO_LISTENER: Self = Self {
        value: 0,
        error: -errno::ENOSYS as i32,
        flags: 0,
    };
    fn result(self) -> Option<isize> {
        (self.flags & USER_NOTIF_FLAG_CONTINUE == 0).then_some(if self.error != 0 {
            self.error as isize
        } else {
            self.value as isize
        })
    }
}
#[derive(Clone, Copy)]
enum Phase {
    Init,
    Sent,
    // OWNER: ADDFD_SEND 已占据 reply 但实际目标安装尚未完成；不能作为最终 response 消费。
    // SEND/ID_VALID 不再接受它，只有对应 command 完成或 Pending 取消能发布 Replied。
    AddFdReplyPending,
    Replied(Response),
}
struct RequestState {
    active: bool,
    phase: Phase,
    addfd: Vec<Arc<AddFd>>,
    // OWNER: 从 pending 移出的实际 command，直到安装结果和请求失效一起提交。
    // RECV copyout rollback 必须等待它，不能重新开放锁外正在执行的原子注入。
    installing: Option<Arc<AddFd>>,
}
struct Request {
    id: u64,
    tid: Arc<crate::namespace::PidIdentity>,
    data: [u32; 16],
    state: Mutex<RequestState>,
    // OWNER: 只有被过滤线程等待；接收、答复、注入与关闭可 signal，绝不持其 state 锁调 scheduler。
    ready: TaskEvent,
}
struct QueueState {
    attached: bool,
    next_id: u64,
    requests: Vec<Arc<Request>>,
}
/// 一个已安装 filter 的实际通知 owner；request 不引用 TCB、filter 或 file-table。
pub(crate) struct NotificationQueue {
    state: Mutex<QueueState>,
    // OWNER: 只由实际 Thread mode attach/detach 修改；临时评估、prepare、listener pins 不计数。
    users: AtomicUsize,
    // POLICY: NOTIF_SET_FLAGS 的唯一同步 wake 提示；不复制 completion 或实际 task placement。
    synchronous_wake: AtomicBool,
    notification: (Arc<PipeEnd>, Arc<PipeEnd>),
}
impl core::fmt::Debug for NotificationQueue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NotificationQueue")
            .field("users", &self.users.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}
impl PartialEq for NotificationQueue {
    fn eq(&self, other: &Self) -> bool {
        core::ptr::eq(self, other)
    }
}
impl Eq for NotificationQueue {}

impl NotificationQueue {
    pub(super) fn new() -> Result<Arc<Self>, SeccompError> {
        let notification = Pipe::notification_pair().map_err(|_| SeccompError::OutOfMemory)?;
        Arc::try_new(Self {
            state: Mutex::new(QueueState {
                attached: true,
                next_id: 1,
                requests: Vec::new(),
            }),
            users: AtomicUsize::new(0),
            synchronous_wake: AtomicBool::new(false),
            notification,
        })
        .map_err(|_| SeccompError::OutOfMemory)
    }
    pub(super) fn is_attached(&self) -> bool {
        self.state.lock().attached
    }
    pub(super) fn attach_user(&self) {
        let previous = self.users.fetch_add(1, Ordering::Relaxed);
        assert!(previous < usize::MAX, "seccomp task users exhausted");
    }
    pub(super) fn detach_user(&self) {
        let previous = self.users.fetch_sub(1, Ordering::AcqRel);
        assert!(previous != 0, "seccomp task users underflow");
        if previous == 1 {
            self.changed();
        }
    }
    fn changed(&self) {
        if self.synchronous_wake.load(Ordering::Acquire) {
            self.notification.1.signal_readiness_on_current_cpu();
        } else {
            self.notification.1.signal_readiness();
        }
    }
    fn wake_request(&self, request: &Request) {
        if self.synchronous_wake.load(Ordering::Acquire) {
            request.ready.signal_on_current_cpu();
        } else {
            request.ready.signal();
        }
    }
    fn has_users(&self) -> bool {
        self.users.load(Ordering::Acquire) != 0
    }
    fn find(&self, id: u64) -> Option<Arc<Request>> {
        self.state
            .lock()
            .requests
            .iter()
            .find(|request| request.id == id)
            .cloned()
    }
    // 所有 Vec backing 准备在 spin guard 外；重取后容量仍足才移动已有实际 Arc 并发布。
    fn insert(
        &self,
        task: &TaskControlBlock,
        data: [u32; 16],
    ) -> Result<Option<Arc<Request>>, SeccompError> {
        let request = Arc::try_new(Request {
            id: {
                let mut queue = self.state.lock();
                let id = queue.next_id;
                queue.next_id = id
                    .checked_add(1)
                    .expect("seccomp notification ID exhausted");
                id
            },
            tid: task.tid_identity(),
            data,
            state: Mutex::new(RequestState {
                active: true,
                phase: Phase::Init,
                addfd: Vec::new(),
                installing: None,
            }),
            ready: TaskEvent::new(),
        })
        .map_err(|_| SeccompError::OutOfMemory)?;
        let mut prepared = Vec::new();
        loop {
            let mut queue = self.state.lock();
            if !queue.attached {
                return Ok(None);
            }
            let needed = queue.requests.len() + 1;
            if queue.requests.capacity() < needed && prepared.capacity() < needed {
                drop(queue);
                prepared
                    .try_reserve_exact(needed)
                    .map_err(|_| SeccompError::OutOfMemory)?;
                continue;
            }
            if queue.requests.capacity() < needed {
                prepared.append(&mut queue.requests);
                core::mem::swap(&mut prepared, &mut queue.requests);
            }
            queue.requests.push(request.clone());
            drop(queue);
            drop(prepared);
            self.changed();
            return Ok(Some(request));
        }
    }
    fn retire_locked(
        queue: &mut QueueState,
        request: &Arc<Request>,
        state: &mut RequestState,
    ) -> (Option<Arc<Request>>, Vec<Arc<AddFd>>) {
        state.active = false;
        let abandoned = core::mem::take(&mut state.addfd);
        let removed = queue
            .requests
            .iter()
            .position(|entry| Arc::ptr_eq(entry, request))
            .map(|index| queue.requests.remove(index));
        (removed, abandoned)
    }
    fn finish_retirement(&self, removed: Option<Arc<Request>>, abandoned: Vec<Arc<AddFd>>) {
        for command in abandoned {
            command.complete(-errno::ESRCH);
        }
        drop(removed);
        self.changed();
    }
    fn remove(&self, request: &Arc<Request>) {
        let (removed, abandoned) = {
            // queue→request 是唯一摘除顺序；失效与索引摘除同一事务，已取得 Arc 的 observer
            // 也必须看到 active=false，不能在两次分离锁之间答复已经取消的 ID。
            let mut queue = self.state.lock();
            let mut state = request.state.lock();
            Self::retire_locked(&mut queue, request, &mut state)
        };
        self.finish_retirement(removed, abandoned);
    }
    /// listener 的实际 file 最后释放；已接受 response 保留，其他请求按 ENOSYS 完成。
    fn detach_listener(&self) {
        let requests = {
            let mut queue = self.state.lock();
            if !queue.attached {
                return;
            }
            queue.attached = false;
            core::mem::take(&mut queue.requests)
        };
        for request in requests {
            {
                let mut state = request.state.lock();
                if !matches!(state.phase, Phase::Replied(_)) {
                    state.phase = Phase::Replied(Response::NO_LISTENER);
                }
            }
            request.ready.signal();
        }
        self.changed();
    }
    pub(super) fn notify(
        &self,
        task: &TaskControlBlock,
        data: [u32; 16],
        killable_after_receive: bool,
    ) -> Result<Option<isize>, SeccompError> {
        let Some(request) = self.insert(task, data)? else {
            return Ok(Some(-errno::ENOSYS));
        };
        let result = loop {
            let (command, response, killable) = {
                let mut state = request.state.lock();
                let command = if state.addfd.is_empty() {
                    None
                } else {
                    let command = state.addfd.remove(0);
                    command.begin();
                    assert!(state.installing.is_none());
                    state.installing = Some(command.clone());
                    Some(command)
                };
                let response = match state.phase {
                    Phase::Replied(value) => Some(value),
                    _ => None,
                };
                let killable = killable_after_receive && !matches!(state.phase, Phase::Init);
                (command, response, killable)
            };
            // 注入由目标线程执行；不能持 notification/graph guard 等待文件表或执行旧 OFD cleanup。
            if let Some(command) = command {
                let result = command.install(task);
                let (response, installed, removed, abandoned) = {
                    // ADDFD ioctl 的真实 OFD pin 一直持有到 Done；Processing 不允许提早返回。
                    // 因而安装失败复位 Sent 时 listener 仍存在，不需要复制 attached 状态。
                    let mut queue = self.state.lock();
                    let mut state = request.state.lock();
                    if command.send {
                        state.phase = if result >= 0 {
                            Phase::Replied(Response {
                                value: result as i64,
                                error: 0,
                                flags: 0,
                            })
                        } else {
                            Phase::Sent
                        };
                    }
                    // 普通注入可能与 SEND 并发；必须重读当前 phase，不能使用安装前的 snapshot，
                    // 否则 SEND 已成功后还会多安装一个等待 command，无法按 ESRCH 取消余项。
                    let response = match state.phase {
                        Phase::Replied(value) => Some(value),
                        _ => None,
                    };
                    // 成功原子 SEND 与普通注入期间已接受的 SEND 都必须先失效，再唤醒
                    // copyout rollback；否则它会在完成和 notify 退出之间把 reply 重开为 Init。
                    let (removed, abandoned) = if response.is_some() {
                        Self::retire_locked(&mut queue, &request, &mut state)
                    } else {
                        (None, Vec::new())
                    };
                    let installed = state.installing.take();
                    (response, installed, removed, abandoned)
                };
                command.complete(result);
                drop(installed);
                self.finish_retirement(removed, abandoned);
                // Linux 普通 SEND 可与已排队注入共存：先执行一个，再取消其余。
                if let Some(response) = response {
                    break Ok(response.result());
                }
                continue;
            }
            if let Some(response) = response {
                break Ok(response.result());
            }
            let waited = if killable {
                request.ready.wait_killable()
            } else {
                request.ready.wait_interruptible()
            };
            if waited == WaitResult::Interrupted {
                // RECV 可恰在 interrupt 与重取状态之间成功；此时切换 killable 而非取消通知。
                let state = request.state.lock();
                if !killable && killable_after_receive && !matches!(state.phase, Phase::Init) {
                    continue;
                }
                break Err(SeccompError::Interrupted);
            }
        };
        self.remove(&request);
        result
    }
}
