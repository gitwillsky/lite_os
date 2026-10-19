//! seccomp notify file_operations 与固定 Linux UAPI；syscall 只提供标准 UserMemory。
use super::*;
use crate::fs::chardev::{DeviceError, DeviceFile, DeviceWaitSources, IoctlCall, wait_ready};

pub(in super::super) struct ListenerFile {
    queue: Arc<NotificationQueue>,
}
impl ListenerFile {
    pub(in super::super) fn new(queue: Arc<NotificationQueue>) -> Self {
        Self { queue }
    }
    fn receive(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        let mut bytes = [0u8; 80];
        call.user
            .read(call.argument, &mut bytes)
            .map_err(|_| error(errno::EFAULT))?;
        if bytes.iter().any(|byte| *byte != 0) {
            return Err(error(errno::EINVAL));
        }
        // 在把请求置 Sent 前准备 recovery；copyout 失败后的不可回滚等待不再分配。
        let recovery = addfd::RecoveryWaiter::prepare()?;
        loop {
            let selected = {
                let queue = self.queue.state.lock();
                queue.requests.iter().find_map(|request| {
                    let mut state = request.state.lock();
                    if !state.active || !matches!(state.phase, Phase::Init) {
                        return None;
                    }
                    state.phase = Phase::Sent;
                    Some(request.clone())
                })
            };
            if let Some(request) = selected {
                bytes[..8].copy_from_slice(&request.id.to_ne_bytes());
                // RECV 的 calling view 决定编号；listener 传递和 setns 后均重读，request 不保活 TCB。
                let tid = request.tid.number_in(&call.user.pid_namespace());
                bytes[8..12].copy_from_slice(&tid.to_ne_bytes());
                for (chunk, word) in bytes[16..]
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(request.data)
                {
                    *chunk = word.to_ne_bytes();
                }
                self.queue.changed();
                if call.user.write(call.argument, &bytes).is_err() {
                    let wake = loop {
                        let installing = {
                            let mut state = request.state.lock();
                            if !state.active {
                                break false;
                            }
                            if let Some(command) = &state.installing {
                                Some(command.clone())
                            } else {
                                state.phase = Phase::Init;
                                break true;
                            }
                        };
                        // Linux 持 notify_lock 安装并提交；锁外安装的实际 operation pin
                        // 提供同一顺序，等待结束后重查 active，不凭旧 phase 重开 reply。
                        installing.unwrap().wait_finished(&recovery);
                    };
                    if wake {
                        request.ready.signal();
                        self.queue.changed();
                    }
                    return Err(error(errno::EFAULT));
                }
                return Ok(0);
            }
            if !self.queue.has_users() {
                return Err(error(errno::ENOENT));
            }
            // 固定 Linux notify_recv 忽略 O_NONBLOCK，始终 interruptible wait。
            wait_ready(self, 1 | 16, false)?;
        }
    }
    fn send(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        let mut bytes = [0u8; 24];
        call.user
            .read(call.argument, &mut bytes)
            .map_err(|_| error(errno::EFAULT))?;
        let id = u64::from_ne_bytes(bytes[..8].try_into().unwrap());
        let response = Response {
            value: i64::from_ne_bytes(bytes[8..16].try_into().unwrap()),
            error: i32::from_ne_bytes(bytes[16..20].try_into().unwrap()),
            flags: u32::from_ne_bytes(bytes[20..].try_into().unwrap()),
        };
        if response.flags & !USER_NOTIF_FLAG_CONTINUE != 0
            || (response.flags & USER_NOTIF_FLAG_CONTINUE != 0
                && (response.error != 0 || response.value != 0))
        {
            return Err(error(errno::EINVAL));
        }
        let request = self.queue.find(id).ok_or_else(|| error(errno::ENOENT))?;
        {
            let mut state = request.state.lock();
            if !state.active {
                return Err(error(errno::ENOENT));
            }
            if !matches!(state.phase, Phase::Sent) {
                return Err(error(errno::EINPROGRESS));
            }
            state.phase = Phase::Replied(response);
        }
        self.queue.wake_request(&request);
        self.queue.changed();
        Ok(0)
    }
    fn valid(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        let mut bytes = [0u8; 8];
        call.user
            .read(call.argument, &mut bytes)
            .map_err(|_| error(errno::EFAULT))?;
        let request = self
            .queue
            .find(u64::from_ne_bytes(bytes))
            .ok_or_else(|| error(errno::ENOENT))?;
        let state = request.state.lock();
        if state.active && matches!(state.phase, Phase::Sent) {
            Ok(0)
        } else {
            Err(error(errno::ENOENT))
        }
    }
}
impl Drop for ListenerFile {
    fn drop(&mut self) {
        self.queue.detach_listener();
    }
}
fn error(value: isize) -> DeviceError {
    DeviceError::Errno(value)
}
impl DeviceFile for ListenerFile {
    fn supports_read(&self) -> bool {
        false
    }
    fn supports_write(&self) -> bool {
        false
    }
    fn release(&self) {
        self.queue.detach_listener();
    }
    fn anonymous_name(&self) -> &'static [u8] {
        b"anon_inode:seccomp notify"
    }
    fn poll(&self, events: i16) -> i16 {
        let mut ready = if self.queue.has_users() { 0 } else { 16 };
        {
            let queue = self.queue.state.lock();
            for request in &queue.requests {
                let state = request.state.lock();
                if !state.active {
                    continue;
                }
                match state.phase {
                    Phase::Init => ready |= 1 | 64,
                    Phase::Sent => ready |= 4 | 256,
                    Phase::AddFdReplyPending | Phase::Replied(_) => {}
                }
            }
        }
        ready & (events | 16)
    }
    fn wait_sources(&self, events: i16) -> DeviceWaitSources {
        DeviceWaitSources::pipe(self.queue.notification.0.pipe(), events | 16)
    }
    fn readiness_generation(&self) -> u64 {
        self.queue
            .notification
            .0
            .pipe()
            .readiness_generation(crate::ipc::PipeDirection::Read)
    }
    fn prepare_wait(&self, events: i16) -> Option<Arc<Pipe>> {
        if self.poll(events) != 0 {
            return None;
        }
        self.queue.notification.0.drain_readiness();
        (self.poll(events) == 0).then(|| self.queue.notification.0.pipe())
    }
    fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        match call.request {
            IOCTL_NOTIF_RECV => self.receive(call),
            IOCTL_NOTIF_SEND => self.send(call),
            IOCTL_NOTIF_ID_VALID | IOCTL_NOTIF_ID_VALID_WRONG_DIR => self.valid(call),
            IOCTL_NOTIF_SET_FLAGS => {
                if call.argument & !USER_NOTIF_FD_SYNC_WAKE_UP != 0 {
                    return Err(error(errno::EINVAL));
                }
                self.queue
                    .synchronous_wake
                    .store(call.argument != 0, Ordering::Release);
                Ok(0)
            }
            // ADDFD 参数可扩展：direction/size 可变，magic 和 ordinal 必须相同。
            request if request & !0xffff_0000 == IOCTL_NOTIF_ADDFD & !0xffff_0000 => {
                self.queue.add_fd(call, (request >> 16) & 0x3fff)
            }
            _ => Err(error(errno::EINVAL)),
        }
    }
}
