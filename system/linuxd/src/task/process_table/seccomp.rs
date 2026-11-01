//! graph 是 policy commit、clone membership 与退出开始的唯一线性化 owner。

use super::*;
use crate::task::SeccompError;
use crate::task::model::{RetiredSecurity, ValidatedFilter};
use alloc::vec::Vec;

/// 对 calling Thread 启用 strict；与远端 TSYNC 和 clone publication 使用同一提交锁。
pub(crate) fn enable_strict_seccomp() -> Result<(), SeccompError> {
    let current = current_task().expect("seccomp requires current task");
    let _graph = PROCESS_TABLE.graph.lock();
    current.security_owner().enable_strict()
}

/// 追加一个 validated cBPF policy；TSYNC 失败返回阻止同步的 TID，且没有部分发布。
///
/// 所有 allocation、program final Drop 都在 graph 锁外；克隆线程增长导致容量不足时
/// 重新锁外准备。calling mode 可被远端 TSYNC 推进，因此每次提交必须复查准备期 identity。
pub(crate) fn install_seccomp_filter(
    bytes: &[u8],
    flags: u32,
) -> Result<Option<usize>, SeccompError> {
    let current = current_task().expect("seccomp requires current task");
    let program = ValidatedFilter::prepare(bytes, flags)?;
    let listener = if let Some(file) = program.listener()? {
        let reservation = current
            .fd_prepare_open(true)
            .map_err(SeccompError::Descriptor)?;
        let prepared = crate::file::PreparedDeviceDescription::try_new(
            file.anonymous_name(),
            crate::fs::O_RDWR,
        )
        .map_err(|_| SeccompError::OutOfMemory)?;
        let ofd = prepared.finish(file);
        Some((reservation, ofd))
    } else {
        None
    };
    PROCESS_TABLE.install_filter(
        &current,
        &program,
        flags & syscall_abi::seccomp::FILTER_FLAG_TSYNC != 0,
    )?;
    Ok(listener.map(|(reservation, file)| reservation.publish(file)))
}

impl ProcessTable {
    pub(super) fn install_filter(
        &self,
        current: &TaskControlBlock,
        program: &ValidatedFilter,
        synchronize: bool,
    ) -> Result<(), SeccompError> {
        // 与 exec credentials/auxv preparation 串行；killable wait 使 de_thread 可终止 TSYNC sibling。
        let _credentials = if synchronize {
            Some(
                current
                    .credential_guard()
                    .lock_killable()
                    .map_err(|error| match error {
                        crate::sync::TaskMutexLockError::OutOfMemory => SeccompError::OutOfMemory,
                        crate::sync::TaskMutexLockError::Interrupted => SeccompError::Interrupted,
                    })?,
            )
        } else {
            None
        };
        // caller 已为一次 syscall 准备唯一新叶及 listener FD；失败重试复用同一 ancestry。
        let mut retired: Vec<RetiredSecurity> = Vec::new();
        let mut required = 1;
        loop {
            retired
                .try_reserve_exact(required)
                .map_err(|_| SeccompError::OutOfMemory)?;
            let prepared = current.security_owner().prepare_filter(program)?;
            let graph = self.graph.lock();
            if !prepared.matches(current.security_owner()) {
                drop(graph);
                continue;
            }
            let node = graph
                .nodes
                .get(&current.tgid())
                .expect("seccomp caller is not live");
            let ProcessState::Live(threads) = &node.state else {
                panic!("seccomp caller process exited");
            };
            required = if synchronize { threads.len() } else { 1 };
            if retired.capacity() < required {
                drop(graph);
                continue;
            }
            if synchronize {
                for (&tid, target) in threads.iter() {
                    if tid != current.tid()
                        && !graph
                            .threads
                            .get(&tid)
                            .expect("seccomp target is not indexed")
                            .exit_in_progress
                        && !prepared.accepts(target.security_owner())
                    {
                        return Err(SeccompError::Unsynchronized(
                            target.tid_identity().number_in(&current.pid_namespace()) as usize,
                        ));
                    }
                }
            }
            // 同一 graph commit 重读 NNP，不能覆盖 target 自己已经完成的 prctl 置位。
            let no_new_privileges = synchronize && current.security_owner().no_new_privileges();
            if synchronize && node.group_exit.is_none() {
                for (&tid, target) in threads.iter() {
                    if !graph
                        .threads
                        .get(&tid)
                        .expect("seccomp target is not indexed")
                        .exit_in_progress
                    {
                        // capacity 已在任何 mutation 前确认；push/Arc clone 都不会分配或 final Drop。
                        retired.push(prepared.publish(target.security_owner(), no_new_privileges));
                    }
                }
            } else {
                retired.push(prepared.publish(current.security_owner(), no_new_privileges));
            }
            drop(graph);
            drop(retired);
            return Ok(());
        }
    }
}
