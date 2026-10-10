//! stable PIDFD lookup、source publication 与 task-owned association 的唯一 owner。
use super::*;
use crate::{
    file::{OpenFileDescription, PreparedFileDescription},
    fs::{
        self,
        pidfs::{PidFdPoll, PidFdSource, PidFdTarget},
    },
    namespace::PidIdentity,
};

/// PIDFD 域错误；user-copy 与 raw Linux flags 仍由 syscall 拥有。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PidFdError {
    NotFound,
    NotLeader,
    Permission,
    Interrupted,
    NoMemory,
    BadDescriptor,
    Fault,
    File(crate::file::FileDescriptorError),
}

struct PidTarget {
    identity: Arc<PidIdentity>,
    source: Arc<PidFdSource>,
}
impl PidFdTarget for PidTarget {
    fn identity(&self) -> &Arc<PidIdentity> {
        &self.identity
    }
    fn source(&self) -> &Arc<PidFdSource> {
        &self.source
    }
    fn poll(&self) -> PidFdPoll {
        let graph = PROCESS_TABLE.graph.lock();
        if let Some(node) = graph
            .nodes
            .get(&self.identity.key())
            .filter(|node| Arc::ptr_eq(node.pid.identity(), &self.identity))
        {
            match node.state {
                ProcessState::Live(_) | ProcessState::Exited(_, ExitRetirement::Retiring) => {
                    PidFdPoll::Running
                }
                ProcessState::Exited(..) => PidFdPoll::Exited,
            }
        } else if graph
            .threads
            .get(&self.identity.key())
            .is_some_and(|index| Arc::ptr_eq(index.pid.identity(), &self.identity))
            || graph
                .kernel_threads
                .get(&self.identity.key())
                .is_some_and(|thread| Arc::ptr_eq(&thread.tid_identity(), &self.identity))
        {
            PidFdPoll::Running
        } else {
            PidFdPoll::Reaped
        }
    }
    fn namespace(
        &self,
        selector: Option<crate::namespace::NamespaceSelector>,
        has_argument: bool,
    ) -> Result<crate::namespace::NamespaceReference, fs::device::DeviceError> {
        use fs::device::DeviceError;
        use syscall_abi::errno;
        let target = {
            let graph = PROCESS_TABLE.graph.lock();
            super::thread_selector::execution_by_tid(&graph, self.identity.key())
                .filter(|thread| Arc::ptr_eq(&thread.tid_identity(), &self.identity))
        }
        .ok_or(DeviceError::Errno(errno::ESRCH))?;
        if has_argument {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        let caller = current_task().expect("pidfd namespace query requires current Thread");
        target.pidfd_namespace(&caller, selector)
    }
    fn information(&self, view: &crate::namespace::PidNamespace) -> Option<[u32; 11]> {
        let caller = current_task().expect("pidfd info requires current Thread");
        let user = caller.user_namespace();
        let snapshot = {
            let graph = PROCESS_TABLE.graph.lock();
            let pid = self.identity.number_in(view);
            if let Some(node) = graph
                .nodes
                .get(&self.identity.key())
                .filter(|node| Arc::ptr_eq(node.pid.identity(), &self.identity))
            {
                let ppid = node
                    .parent
                    .and_then(|parent| graph.nodes.get(&parent))
                    .map_or(0, |parent| parent.pid.identity().number_in(view));
                Some((
                    pid,
                    pid,
                    ppid,
                    super::super::model::leader_credential_snapshot(&node.leader_credentials),
                ))
            } else if let Some(thread) =
                super::thread_selector::execution_by_tid(&graph, self.identity.key())
                    .filter(|thread| Arc::ptr_eq(&thread.tid_identity(), &self.identity))
            {
                let tgid = thread.tgid_identity().number_in(view);
                let ppid = graph
                    .nodes
                    .get(&thread.tgid())
                    .and_then(|node| node.parent)
                    .and_then(|parent| graph.nodes.get(&parent))
                    .map_or(0, |parent| parent.pid.identity().number_in(view));
                Some((pid, tgid, ppid, thread.credential_snapshot()))
            } else {
                None
            }
        };
        snapshot.map(|(pid, tgid, ppid, credentials)| {
            [
                pid,
                tgid,
                ppid,
                user.display_id(true, credentials.uids[0]),
                user.display_id(false, credentials.gids[0]),
                user.display_id(true, credentials.uids[1]),
                user.display_id(false, credentials.gids[1]),
                user.display_id(true, credentials.uids[2]),
                user.display_id(false, credentials.gids[2]),
                user.display_id(true, credentials.fsids[0]),
                user.display_id(false, credentials.fsids[1]),
            ]
        })
    }
}

/// 根据实际 graph association 选择唯一 source；不查询号码 ladder 的当前数字映射。
fn source_slot<'a>(
    graph: &'a mut ProcessGraph,
    identity: &Arc<PidIdentity>,
    thread: bool,
) -> Result<Option<&'a mut alloc::sync::Weak<PidFdSource>>, PidFdError> {
    let key = identity.key();
    if graph.nodes.contains_key(&key) {
        let node = graph.nodes.get_mut(&key).expect("selected leader PID");
        return if Arc::ptr_eq(node.pid.identity(), identity) {
            Ok(Some(&mut node.pidfd_source))
        } else {
            Err(PidFdError::NotFound)
        };
    }
    if let Some(index) = graph.threads.get_mut(&key) {
        if !Arc::ptr_eq(index.pid.identity(), identity) {
            return Err(PidFdError::NotFound);
        }
        if !thread {
            return Err(PidFdError::NotLeader);
        }
        return Ok(Some(&mut index.pidfd_source));
    }
    if graph
        .kernel_threads
        .get(&key)
        .is_some_and(|entity| Arc::ptr_eq(&entity.tid_identity(), identity))
    {
        return Ok(None);
    }
    Err(PidFdError::NotFound)
}

/// 同一真实 PIDFS preparation 供 pidfd_open 和 CLONE_PIDFD 消费；未发表 token 的 Drop 撤销 fd。
pub(super) struct PreparedPidFd {
    reservation: super::super::model::OpenFdReservation,
    description: Arc<OpenFileDescription>,
    source: Arc<PidFdSource>,
}
impl PreparedPidFd {
    /// child 尚未进入 graph 时只准备真实 inode/OFD；调用者必须先完成 child files unshare。
    pub(super) fn for_clone(
        identity: Arc<PidIdentity>,
        thread: bool,
        caller: &TaskControlBlock,
        address: usize,
    ) -> Result<Self, PidFdError> {
        let prepared = Self::prepare(identity, if thread { 0x80 } else { 0 }, caller, None)?;
        caller
            .copy_to_user(
                address,
                &(prepared.reservation.descriptors()[0] as i32).to_ne_bytes(),
            )
            .map_err(|_| PidFdError::Fault)?;
        Ok(prepared)
    }

    fn prepare(
        identity: Arc<PidIdentity>,
        flags: u32,
        caller: &TaskControlBlock,
        source: Option<Arc<PidFdSource>>,
    ) -> Result<Self, PidFdError> {
        let reservation = caller.fd_prepare_open(true).map_err(PidFdError::File)?;
        let source = match source {
            Some(source) => source,
            None => PidFdSource::try_new().map_err(|_| PidFdError::NoMemory)?,
        };
        let target =
            Arc::try_new(PidTarget { identity, source }).map_err(|_| PidFdError::NoMemory)?;
        let allocation = PreparedFileDescription::try_new().map_err(|_| PidFdError::NoMemory)?;
        let (opened, backend, source) =
            fs::pidfs::prepare_open(target, &caller.access_identity(true))
                .map_err(|_| PidFdError::NoMemory)?;
        let description = allocation.finish_backend(opened, backend, fs::O_RDWR | flags);
        Ok(Self {
            reservation,
            description,
            source,
        })
    }
    pub(super) fn source(&self) -> alloc::sync::Weak<PidFdSource> {
        Arc::downgrade(&self.source)
    }
    pub(super) fn publish(self) -> usize {
        self.reservation.publish(self.description)
    }
}

/// 标准 pidfd_open 的完整 OFD preparation 与 fd publication。
/// source/inode/description 全部锁外准备，最终 graph recheck 胜出才发表 weak source。
pub(crate) fn open_pidfd(
    identity: Arc<PidIdentity>,
    thread: bool,
    flags: u32,
    caller: &TaskControlBlock,
) -> Result<usize, PidFdError> {
    let source = {
        let mut graph = PROCESS_TABLE.graph.lock();
        match source_slot(&mut graph, &identity, thread)? {
            Some(slot) => slot.upgrade(),
            None => {
                graph.kernel_threads[&identity.key()]
                    .kernel_pidfd_source(None)
                    .0
            }
        }
    };
    let prepared = PreparedPidFd::prepare(identity.clone(), flags, caller, source)?;
    let source = &prepared.source;
    let retired = {
        let mut graph = PROCESS_TABLE.graph.lock();
        match source_slot(&mut graph, &identity, thread)? {
            Some(slot) => {
                if let Some(existing) = slot.upgrade() {
                    assert!(
                        Arc::ptr_eq(&existing, source),
                        "one PID acquired two live pidfs sources"
                    );
                }
                core::mem::replace(slot, Arc::downgrade(source))
            }
            None => {
                let (existing, retired) =
                    graph.kernel_threads[&identity.key()].kernel_pidfd_source(Some(source));
                if let Some(existing) = existing {
                    assert!(
                        Arc::ptr_eq(&existing, source),
                        "kernel PID acquired two live sources"
                    );
                }
                retired.unwrap_or_default()
            }
        }
    };
    drop(retired);
    Ok(prepared.publish())
}

/// 正数 ABI PID 在调用 namespace 解析为实际 graph PID observer；号码重用不能 retarget 旧 Arc。
pub(crate) fn pidfd_identity(
    pid: i32,
    caller: &TaskControlBlock,
) -> Result<Arc<PidIdentity>, PidFdError> {
    let key = caller
        .resolve_pid(pid as usize)
        .ok_or(PidFdError::NotFound)?;
    let graph = PROCESS_TABLE.graph.lock();
    if let Some(node) = graph.nodes.get(&key) {
        return Ok(node.pid.identity().clone());
    }
    if let Some(index) = graph.threads.get(&key) {
        return Ok(index.pid.identity().clone());
    }
    graph
        .kernel_threads
        .get(&key)
        .map(|thread| thread.tid_identity())
        .ok_or(PidFdError::NotFound)
}

/// PF_EXITING 的实际 graph projection；不得从是否还可读 proc 或 PID 号码占用猜测。
pub(in crate::task) fn file_accessible(target: &TaskControlBlock) -> bool {
    let graph = PROCESS_TABLE.graph.lock();
    graph
        .threads
        .get(&target.tid())
        .is_some_and(|index| !index.exit_in_progress)
}

/// 稳定 PID identity 捕获实际目标 OFD；same Process exec guard、REAL policy 与 fd owner 各自只查询一次。
pub(crate) fn pidfd_get_file(
    identity: &Arc<PidIdentity>,
    descriptor: usize,
    caller: &TaskControlBlock,
) -> Result<Arc<OpenFileDescription>, PidFdError> {
    let (thread, retired) = {
        let graph = PROCESS_TABLE.graph.lock();
        let thread = super::thread_selector::execution_by_tid(&graph, identity.key())
            .filter(|thread| Arc::ptr_eq(&thread.tid_identity(), identity));
        let retired = if thread.is_none() {
            graph
                .nodes
                .get(&identity.key())
                .filter(|node| Arc::ptr_eq(node.pid.identity(), identity))
                .map(|node| node.process.retired_inspection_target())
        } else {
            None
        };
        (thread, retired)
    };
    if let Some(thread) = thread {
        return thread.capture_pidfd_file(caller, descriptor);
    }
    if let Some(retired) = retired {
        retired.authorize(caller).map_err(|error| match error {
            crate::task::RobustListError::NotFound => PidFdError::NotFound,
            crate::task::RobustListError::Permission => PidFdError::Permission,
            crate::task::RobustListError::OutOfMemory => PidFdError::NoMemory,
            crate::task::RobustListError::Interrupted => PidFdError::Interrupted,
        })?;
    }
    Err(PidFdError::NotFound)
}
