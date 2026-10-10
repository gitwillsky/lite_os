use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::any::Any;

use crate::socket::SocketError;

use super::UnixSocket;

/// AF_UNIX GC graph 使用的稳定 socket node capability。
#[derive(Clone)]
pub(crate) struct UnixNode {
    pub(super) id: u64,
    pub(super) socket: Weak<UnixSocket>,
}

/// AF_UNIX transport 可保活、但无需识别 concrete OFD 的 passed-file capability。
pub(crate) trait UnixPassedFile: Any + Send + Sync {
    /// 把 transport capability 恢复为可由 syscall seam downcast 的类型擦除 Arc。
    ///
    /// # Returns
    ///
    /// 同一 allocation 的 `Any` Arc，不复制底层 file state。
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;

    /// 若该 file 是 AF_UNIX socket，投影其稳定 GC node。
    ///
    /// # Returns
    ///
    /// AF_UNIX node；其他 OFD kind 为 None。
    fn unix_node(&self) -> Option<UnixNode>;

    /// 判断除了 inflight rights 与本次 Weak upgrade 外是否仍有 file root。
    ///
    /// # Parameters
    ///
    /// - `inflight`: 同一 AF_UNIX file 在 graph 中的 outgoing edge 数。
    ///
    /// # Returns
    ///
    /// descriptor、active syscall 或其他非-rights Arc 存在时为 true，同时返回实际 probe pin
    /// 供 graph guard 外退休；消费 Arc 时不能在 GRAPH 内触发 OFD/Socket 最后 Drop。
    fn probe_external(self: Arc<Self>, inflight: usize) -> (bool, Arc<dyn UnixPassedFile>);
}

/// Sender 实际 limits owner 的只读 capability；不 pin Task/Process/MM/PID membership。
/// 阻塞发送每次 quota commit 重新读取 RLIMIT_NOFILE，不能冻结 syscall 开始时的 soft limit。
pub(crate) trait UnixFileLimit: Send + Sync {
    fn max_files(&self) -> usize;
}

/// 已从 transport 转移或由 MSG_PEEK 复制的接收 capabilities；不拥有 sender inflight 费用。
/// Vec 中每个实际 OFD Arc 是 syscall root，GC 不能在 fd publication 前回收其 socket。
pub(crate) struct UnixReceivedRights(Vec<Arc<dyn UnixPassedFile>>);
impl UnixReceivedRights {
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
    pub(crate) fn into_files(self) -> Vec<Arc<dyn UnixPassedFile>> {
        self.0
    }
}

/// 一条 SCM_RIGHTS control message 的完整 file capability 集合。
pub(crate) struct UnixRights {
    files: Vec<Arc<dyn UnixPassedFile>>,
    nodes: Vec<UnixNode>,
    uid: u32,
    limit: Option<Arc<dyn UnixFileLimit>>,
    batch_id: u64,
    attached_target: Option<u64>,
    prepared: Option<super::rights_graph::PreparedAttach>,
}

impl UnixRights {
    /// 构造已经由 fd-table owner 一次性捕获的 SCM_RIGHTS 集合。
    ///
    /// # Parameters
    ///
    /// - `files`: 按用户 cmsg 顺序排列的共享 file capabilities。
    /// - `uid`: sender 的 real UID；Linux inflight accounting 按 real UID 隔离。
    /// - `limit`: 无 initial SYS_RESOURCE/SYS_ADMIN 时的实际 RLIMIT_NOFILE owner；有权限时为 None。
    ///
    /// # Returns
    ///
    /// 非空、最多 SCM_MAX_FD 个 rights。
    ///
    /// # Errors
    ///
    /// 空集合或超过 Linux SCM_MAX_FD 返回 Invalid。
    pub(crate) fn new(
        files: Vec<Arc<dyn UnixPassedFile>>,
        uid: u32,
        limit: Option<Arc<dyn UnixFileLimit>>,
    ) -> Result<Self, SocketError> {
        if files.is_empty() || files.len() > super::SCM_MAX_FD {
            return Err(SocketError::Invalid);
        }
        let mut nodes = Vec::new();
        nodes
            .try_reserve_exact(files.len())
            .map_err(|_| SocketError::NoMemory)?;
        nodes.extend(files.iter().filter_map(|file| file.unix_node()));
        nodes.sort_unstable_by_key(|node| node.id);
        Ok(Self {
            files,
            nodes,
            uid,
            limit,
            batch_id: crate::id::next_runtime_object_id(),
            attached_target: None,
            prepared: None,
        })
    }

    /// 返回本 control message 的 descriptor 数量。
    ///
    /// # Returns
    ///
    /// 1..=SCM_MAX_FD。
    pub(crate) fn len(&self) -> usize {
        self.files.len()
    }

    /// 把 transport ownership 转交给 syscall/fd-table publication seam。
    ///
    /// # Returns
    ///
    /// 保持 cmsg 顺序且复用原 Vec backing 的 file capabilities。
    pub(crate) fn into_files(mut self) -> Vec<Arc<dyn UnixPassedFile>> {
        self.detach();
        core::mem::take(&mut self.files)
    }

    /// 消费 transport graph ownership，返回已脱离 UID inflight ledger 的实际 syscall roots。
    /// Caller 必须已经释放 transport guard；detach/fput 不能反入持有该 guard 的 collector。
    pub(super) fn into_received(self) -> UnixReceivedRights {
        UnixReceivedRights(self.into_files())
    }

    /// 锁内只 clone 实际 OFD Arc；Vec storage 必须已在 guard 外预留并复查当前 record 长度。
    pub(super) fn copy_for_receive(
        &self,
        prepared: &mut Vec<Arc<dyn UnixPassedFile>>,
    ) -> UnixReceivedRights {
        assert!(prepared.is_empty() && prepared.capacity() >= self.files.len());
        prepared.extend(self.files.iter().cloned());
        UnixReceivedRights(core::mem::take(prepared))
    }

    /// 在 transport owner lock 内尝试把本批 rights 计入 receiver/UID graph。
    ///
    /// # Parameters
    ///
    /// - `target`: 即将拥有本 control message 的 receive endpoint。
    ///
    /// # Returns
    ///
    /// graph publication 成功；重复 attach 是内部不变量损坏。
    ///
    /// # Errors
    ///
    /// graph storage OOM，或返回内部 NeedsCollection 要求 caller 释放 transport lock。
    pub(super) fn prepare_attach(&mut self, target: &Arc<UnixSocket>) -> Result<(), SocketError> {
        assert!(self.attached_target.is_none(), "prepare attached SCM batch");
        if let Some(prepared) = self.prepared.as_mut() {
            assert_eq!(prepared.target(), target.node_id());
            prepared.refresh_scratch()
        } else {
            self.prepared = Some(super::rights_graph::PreparedAttach::new(
                Some(self.uid),
                &self.nodes,
                target,
            )?);
            Ok(())
        }
    }
    pub(super) fn try_attach(
        &mut self,
        target: &Arc<UnixSocket>,
    ) -> Result<super::rights_graph::PreparedAttach, super::rights_graph::AttachError> {
        assert!(
            self.attached_target.is_none(),
            "SCM_RIGHTS batch attached twice"
        );
        let prepared = self
            .prepared
            .as_mut()
            .expect("SCM graph must be prepared outside transport guards");
        assert_eq!(prepared.target(), target.node_id());
        super::rights_graph::try_attach(
            self.uid,
            self.batch_id,
            self.files.len(),
            &self.nodes,
            target.node_id(),
            self.limit.as_ref().map(|limit| limit.max_files()),
            prepared,
        )?;
        self.attached_target = Some(target.node_id());
        Ok(self.prepared.take().unwrap())
    }

    /// 在 transport lock 外为下一次 attach 同步执行 quota/cycle recovery。
    ///
    /// # Returns
    ///
    /// quota 已可重试，或至少一个 dead SCC 已被清空。
    ///
    /// # Errors
    ///
    /// 没有可回收 SCC 且 sender 仍超过 RLIMIT_NOFILE 时返回 TooManyReferences。
    pub(super) fn collect(&self) -> Result<(), SocketError> {
        match &self.limit {
            Some(limit) => super::rights_graph::collect(self.uid, limit.max_files()),
            None => Ok(()),
        }
    }

    /// 回滚尚未 transport commit 的 graph publication。
    ///
    /// # Returns
    ///
    /// 无返回值；未 attach 时为空操作。
    pub(super) fn detach(&mut self) {
        let Some(target) = self.attached_target.take() else {
            return;
        };
        super::rights_graph::detach(
            self.uid,
            self.batch_id,
            self.files.len(),
            &self.nodes,
            target,
        );
    }
}

impl Drop for UnixRights {
    fn drop(&mut self) {
        self.detach();
    }
}
