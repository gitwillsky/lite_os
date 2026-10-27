//! exec survivor 的 graph identity 转移，不生成虚假的 creator 退出事件。
use super::*;

/// 在 PONR 后推进实际 exec epoch；原 leader 已退出时把唯一 caller 提升到原 TGID。
///
/// # Parameters
///
/// - `task`: 已完成所有 fallible preparation/旧 TID robust cleanup 的唯一 live survivor。
///
/// # Returns
///
/// 无失败/分配；两个 Thread 索引和 creator-child relation 在同一 graph guard 下重发布。
pub(in crate::task) fn mark_process_exec(
    task: &TaskControlBlock,
    link: Option<PidLinkSlot>,
    trace_link: Option<Arc<PidRegistration>>,
) {
    let (retired, trace_retirement) = {
        let mut graph = PROCESS_TABLE.graph.lock();
        let old_tid = task.tid();
        let retired = commit_exec_identity(&mut graph, task, link);
        let trace_retirement = super::ptrace::promote_exec(&mut graph, task, old_tid, trace_link);
        (retired, trace_retirement)
    };
    if let Some((_, _, source)) = &retired
        && let Some(source) = source.upgrade()
    {
        source.notify();
    }
    drop(trace_retirement);
    drop(retired);
}

fn commit_exec_identity(
    graph: &mut ProcessGraph,
    task: &TaskControlBlock,
    mut link: Option<PidLinkSlot>,
) -> Option<(
    Arc<super::super::pid::PidIdentity>,
    PidRegistration,
    alloc::sync::Weak<crate::fs::pidfs::PidFdSource>,
)> {
    let tgid = task.tgid();
    let old_tid = task.tid();
    let node = graph
        .nodes
        .get_mut(&tgid)
        .expect("exec process missing from graph");
    let ProcessState::Live(threads) = &mut node.state else {
        panic!("exec process is not live");
    };
    assert_eq!(threads.len(), 1, "exec requires unique survivor");
    assert!(
        threads
            .get(&old_tid)
            .is_some_and(|member| core::ptr::eq(member.as_ref(), task))
    );
    let mut promoted = None;
    if old_tid != tgid {
        let registration = node
            .pid
            .clone_in(link.take().expect("nonleader exec prepared PID link"));
        assert!(
            !threads.contains_key(&tgid),
            "old leader still live during promotion"
        );
        let mut entry = threads
            .take_entry(&old_tid)
            .expect("exec survivor disappeared");
        entry.set_key(tgid);
        promoted = Some((task.promote_exec_identity(), registration));
        threads.commit_vacant(entry);
        // 已 pending 的事件保存在 Thread owner；重扫不能因较小的新 TID 而跳过它。
        node.pdeath_cursor = 0;
    }
    node.exec_generation = node
        .exec_generation
        .checked_add(1)
        .expect("exec epoch overflow");
    node.exit_signal = syscall_abi::signal::SIGCHLD as u8;
    if old_tid == tgid {
        assert!(link.is_none(), "leader exec does not require identity link");
        return None;
    }
    assert!(
        !graph.threads.contains_key(&tgid),
        "old leader index survived graph removal"
    );
    let mut index = graph
        .threads
        .take_entry(&old_tid)
        .expect("exec Thread index missing");
    assert_eq!(index.value().tgid, tgid);
    for (&child_pid, ()) in &index.value().created_children {
        let child = graph
            .nodes
            .get_mut(&child_pid)
            .expect("creator-child references missing Process");
        assert_eq!(child.parent_thread, Some(old_tid));
        child.parent_thread = Some(tgid);
    }
    let (retired_identity, registration) = promoted.expect("nonleader identity promoted");
    let retired_registration = core::mem::replace(&mut index.value_mut().pid, registration);
    let retired_source = core::mem::replace(
        &mut index.value_mut().pidfd_source,
        alloc::sync::Weak::new(),
    );
    index.set_key(tgid);
    graph.threads.commit_vacant(index);
    Some((retired_identity, retired_registration, retired_source))
}
