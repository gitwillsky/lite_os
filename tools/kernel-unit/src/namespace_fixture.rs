//! 显式 host boot composition：生产 namespace owner 原样加载，无初始身份 fallback 或 mock map。
#[path = "../../../kernel/src/namespace/mod.rs"]
#[allow(
    dead_code,
    unused_imports,
    reason = "此 fixture 只装配实际 initial owners，各子域由独立集成回归覆盖"
)]
mod owner;
#[allow(unused_imports, reason = "host 用例只使用共享 owner 的部分导出")]
pub(crate) use owner::*;
/// Fixture constructor 在故障注入前装配真实 initial owners。
pub(crate) fn boot() -> alloc::sync::Arc<UserNamespace> {
    initialize(i32::MAX as u32, 8192).expect("host user namespace boot");
    initialize_uts(b"liteos", b"(none)").expect("host UTS namespace boot");
    initialize_pid(1).expect("host PID namespace boot");
    initialize_time().expect("host time namespace boot");
    initial_user()
}

/// 已装配 initial owner 的 host root credential 输入；不创建另一份 namespace 状态。
#[allow(
    dead_code,
    reason = "部分 fixture 只需要 boot 输入，authority 输入供其他回归使用"
)]
pub(crate) fn root_authority() -> NamespaceAuthority {
    NamespaceAuthority {
        namespace: initial_user(),
        effective_uid: 0,
        effective_gid: 0,
        effective_capabilities: syscall_abi::capability::VALID_MASK,
    }
}

/// 每个回归分配独立的实际 PID identity；boot INIT 不能重复发布。
#[allow(dead_code, reason = "只供使用实际 PID ownership 的 fixture")]
pub(crate) fn prepared_pid() -> PreparedPid {
    static NEXT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(2);
    boot();
    let key = NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let mut pid = PreparedPid::try_new(key, initial_pid(), &[], &root_authority()).unwrap();
    pid.publish().unwrap();
    pid
}
