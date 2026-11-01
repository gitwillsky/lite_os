use super::*;
use syscall_abi::capability::CapabilitySets;

/// 标准 capget PID/TID selector；已退出 leader 与 zombie 共用原 credential owner 到 reap。
pub(crate) fn capability_sets(pid: usize) -> Option<CapabilitySets> {
    let caller = current_task().expect("capget requires current Thread");
    if pid == 0 || pid == caller.visible_tid() as usize {
        return Some(caller.capability_snapshot().sets);
    }
    let pid = caller.resolve_pid(pid)?;
    let graph = PROCESS_TABLE.graph.lock();
    // TGID 仍保留 zombie/original leader；其 alias 与 Process exec retarget 是同一对象。
    if let Some(node) = graph.nodes.get(&pid) {
        let owner = node.leader_credentials.lock();
        return Some(owner.lock().capability_snapshot().sets);
    }
    let index = graph.threads.get(&pid)?;
    let ProcessState::Live(threads) = &graph.nodes.get(&index.tgid)?.state else {
        return None;
    };
    Some(threads.get(&pid)?.capability_snapshot().sets)
}
