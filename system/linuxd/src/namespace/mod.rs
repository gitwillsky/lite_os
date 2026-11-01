//! Linux namespace identities；任务 membership、namespace 内容与权限只各有一个 owner。
mod id_map;
mod rlimit_counts;
pub(crate) use rlimit_counts::{
    IO_URING_BYTES_PER_UID, NamespaceRlimits, RlimitCharge, RlimitResource,
};
mod pid;
mod pid_identity;
pub(crate) use pid::{PidAllocationError, PidExitWake, PidKey, PidNamespace, PidReservation};
pub(crate) use pid_identity::{
    PidIdentity, PidLinkSlot, PidRegistration, PreparedPid, RetiredPidRegistrations,
};
mod quota;
pub(crate) use quota::{
    UserResource, UserResourceCharge, initial_namespace_limit, set_initial_inotify_limit,
};
mod reference;
mod time;
mod user;
mod uts;
use alloc::sync::Arc;
pub(crate) use id_map::IdMap;
pub(crate) use reference::{NamespaceOwner, NamespaceReference, NamespaceSelector, NamespaceType};
use spin::Once;
pub(crate) use time::{TimeClock, TimeNamespace, TimeOffset, TimeOffsetError, TimeOffsets};
pub(crate) use user::{NamespaceAuthority, UserNamespace, UserNamespaceMap};
pub(crate) use uts::UtsNamespace;

/// namespace 准备与标准 ID map publication 的失败分类；syscall/procfs 各自在 ABI seam 编码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamespaceError {
    Invalid,
    Permission,
    OutOfMemory,
    NestingLimit,
    QuotaLimit,
    Multithreaded,
}

// OWNER: boot 唯一 initial user namespace；所有根凭据及 root-owned kernel resource 引用同一对象。
// Once 在首个 Task 前安装，失败在 composition root 显式终止；绝不以 UID0 或临时 namespace 替代。
static INITIAL_USER: Once<Arc<UserNamespace>> = Once::new();
pub(crate) fn initialize(limit: u32, inotify_watches: u32) -> Result<(), NamespaceError> {
    if limit > i32::MAX as u32 || !(8192..=1_048_576).contains(&inotify_watches) {
        return Err(NamespaceError::Invalid);
    }
    if INITIAL_USER.get().is_some() {
        return Ok(());
    }
    let initial = UserNamespace::initial(limit)?;
    initial.quotas.initialize_inotify(inotify_watches);
    INITIAL_USER.call_once(|| initial);
    Ok(())
}
pub(crate) fn initial_user() -> Arc<UserNamespace> {
    INITIAL_USER
        .get()
        .expect("namespace initialize precedes Task creation")
        .clone()
}

// OWNER: boot 安装唯一 initial UTS namespace；Task membership 决定是否继续共享此对象。
static INITIAL_UTS: Once<Arc<UtsNamespace>> = Once::new();
pub(crate) fn initialize_uts(hostname: &[u8], domainname: &[u8]) -> Result<(), NamespaceError> {
    if INITIAL_UTS.get().is_some() {
        return Ok(());
    }
    let initial = UtsNamespace::initial(initial_user(), hostname, domainname)?;
    INITIAL_UTS.call_once(|| initial);
    Ok(())
}
pub(crate) fn initial_uts() -> Arc<UtsNamespace> {
    INITIAL_UTS
        .get()
        .expect("UTS initialize precedes Task creation")
        .clone()
}

// OWNER: boot 安装唯一 initial time namespace，所有初始 membership 指向同一冻结 owner。
static INITIAL_TIME: Once<Arc<TimeNamespace>> = Once::new();
pub(crate) fn initialize_time() -> Result<(), NamespaceError> {
    if INITIAL_TIME.get().is_some() {
        return Ok(());
    }
    let initial = TimeNamespace::initial(initial_user())?;
    INITIAL_TIME.call_once(|| initial);
    Ok(())
}
pub(crate) fn initial_time() -> Arc<TimeNamespace> {
    INITIAL_TIME
        .get()
        .expect("time initialize precedes Task creation")
        .clone()
}

// OWNER: boot 唯一 initial PID namespace；possible CPU policy 由 composition root 显式注入。
struct InitialPid {
    namespace: Arc<PidNamespace>,
    // OWNER: actual init_struct_pid；boot 构造唯一 PID0，idle/current-none 只 pin 同一对象。
    idle: Arc<PidIdentity>,
}
static INITIAL_PID: Once<InitialPid> = Once::new();
pub(crate) fn initialize_pid(possible_cpus: usize) -> Result<(), NamespaceError> {
    if possible_cpus == 0 {
        return Err(NamespaceError::Invalid);
    }
    if INITIAL_PID.get().is_some() {
        return Ok(());
    }
    let namespace = PidNamespace::initial(initial_user(), possible_cpus)?;
    let idle = PidIdentity::idle(namespace.clone()).map_err(|_| NamespaceError::OutOfMemory)?;
    INITIAL_PID.call_once(|| InitialPid { namespace, idle });
    Ok(())
}
pub(crate) fn initial_pid() -> Arc<PidNamespace> {
    INITIAL_PID
        .get()
        .expect("PID initialize precedes Task creation")
        .namespace
        .clone()
}

/// 已装配 boot/idle 的唯一 initial PID0；不新建身份、不占用用户编号。
pub(crate) fn idle_pid_identity() -> Arc<PidIdentity> {
    INITIAL_PID
        .get()
        .expect("PID initialize precedes execution observers")
        .idle
        .clone()
}

// OWNER: ns_id 是跨 namespace 类型唯一的身份；1..=8 保留固定 Linux initial namespace IDs。
// fetch_update 不允许 wrap/重复；与 pipe/socket object ID 分属不同领域，绝不借用其序列。
static NEXT_NAMESPACE_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(9);
pub(crate) fn next_namespace_id() -> u64 {
    NEXT_NAMESPACE_ID
        .try_update(
            core::sync::atomic::Ordering::Relaxed,
            core::sync::atomic::Ordering::Relaxed,
            |next| next.checked_add(1),
        )
        .expect("namespace identity exhausted")
}
