//! exec preparation claim 与不可回滚 sibling retirement；process graph 是唯一提交 owner。
use super::*;
use crate::sync::WaitResult;

/// 不可变 preparation identity；实际 Thread completion 由 Process 跨 exec claim 持续拥有。
pub(super) struct ExecDrain {
    owner_tid: usize,
}
/// 准备期允许其他线程继续运行；draining 一经提交，所有非 owner Thread 必须退出。
pub(super) struct ExecState {
    draining: bool,
    drain: Arc<ExecDrain>,
}

/// 唯一 preparation claim；准备失败/成功 commit 后，Drop 在 graph 外退休 claim。
pub(in crate::task) struct ExecPreparation<'a> {
    _credentials: Option<crate::sync::TaskMutexGuard<'a, ()>>,
    tgid: usize,
    drain: Arc<ExecDrain>,
}
impl<'a> ExecPreparation<'a> {
    /// 领取 calling Process 的唯一 exec preparation；OOM/竞争不修改 Process。
    pub(in crate::task) fn claim(
        task: &'a TaskControlBlock,
    ) -> Result<Self, super::super::model::ExecError> {
        use super::super::model::ExecError;
        let drain = Arc::try_new(ExecDrain {
            owner_tid: task.tid(),
        })
        .map_err(|_| ExecError::Image(crate::memory::ElfLoadError::OutOfMemory))?;
        let mut graph = PROCESS_TABLE.graph.lock();
        let node = graph
            .nodes
            .get_mut(&task.tgid())
            .expect("exec caller process is live");
        if node.exec.is_some() || node.group_exit.is_some() {
            return Err(ExecError::Busy);
        }
        node.exec = Some(ExecState {
            draining: false,
            drain: drain.clone(),
        });
        drop(graph);
        let mut preparation = Self {
            tgid: task.tgid(),
            drain,
            _credentials: None,
        };
        preparation._credentials = Some(task.credential_guard().lock_interruptible().map_err(
            |error| match error {
                crate::sync::TaskMutexLockError::OutOfMemory => {
                    ExecError::Image(crate::memory::ElfLoadError::OutOfMemory)
                }
                crate::sync::TaskMutexLockError::Interrupted => ExecError::Interrupted,
            },
        )?);
        Ok(preparation)
    }

    /// 映像准备成功后开始不可回滚 de_thread；不能在此后产生可恢复错误。
    /// 返回 Busy 时尚未终止 sibling。等待使用已分配事件，包含 graph 摘除后的 MM cleanup。
    pub(in crate::task) fn terminate_siblings(
        &self,
        task: &TaskControlBlock,
    ) -> Result<(), super::super::model::ExecError> {
        {
            let mut graph = PROCESS_TABLE.graph.lock();
            let node = graph
                .nodes
                .get_mut(&self.tgid)
                .expect("exec caller process is live");
            if node.group_exit.is_some() {
                return Err(super::super::model::ExecError::Busy);
            }
            let state = node.exec.as_mut().expect("exec claim disappeared");
            assert!(Arc::ptr_eq(&state.drain, &self.drain));
            let ProcessState::Live(threads) = &node.state else {
                panic!("exec process exited");
            };
            state.draining = true;
            node.job_control = JobControlState::Running;
            for (&tid, sibling) in threads {
                if tid != self.drain.owner_tid {
                    sibling.queue_group_exit_signal();
                }
            }
        }
        // 与 group-exit 一样在 graph 外进入 scheduler/wait seam；不分配 target Vec。
        // 同时创建的新 sibling 由 publication 登记 counter、activation 继承强制退出。
        let mut cursor = 0;
        loop {
            let next = {
                let graph = PROCESS_TABLE.graph.lock();
                let node = graph
                    .nodes
                    .get(&self.tgid)
                    .expect("exec caller process is live");
                let ProcessState::Live(threads) = &node.state else {
                    panic!("exec process exited");
                };
                threads
                    .iter_after(&cursor)
                    .find(|(tid, _)| **tid != self.drain.owner_tid)
                    .map(|(&tid, sibling)| (tid, sibling.clone()))
            };
            let Some((tid, sibling)) = next else {
                break;
            };
            cursor = tid;
            crate::task::processor::continue_stopped_task(sibling.clone());
            super::signal::interrupt_waiting_task(&sibling);
            crate::task::processor::request_task_reschedule(&sibling);
        }
        if task.thread_retirements().wait_for_survivor() == WaitResult::Interrupted {
            return Err(super::super::model::ExecError::FatalInterrupted);
        }
        assert_eq!(task.tid(), self.drain.owner_tid);
        Ok(())
    }
}
impl Drop for ExecPreparation<'_> {
    fn drop(&mut self) {
        let retired = {
            let mut graph = PROCESS_TABLE.graph.lock();
            graph.nodes.get_mut(&self.tgid).and_then(|node| {
                if node
                    .exec
                    .as_ref()
                    .is_some_and(|state| Arc::ptr_eq(&state.drain, &self.drain))
                {
                    node.exec.take()
                } else {
                    None
                }
            })
        };
        drop(retired);
    }
}
impl ExecState {
    pub(super) fn kills(&self, tid: usize) -> bool {
        self.draining && tid != self.drain.owner_tid
    }
}
