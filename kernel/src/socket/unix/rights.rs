use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::any::Any;

use crate::socket::SocketError;

use super::UnixSocket;

/// @description AF_UNIX GC graph 使用的稳定 socket node capability。
#[derive(Clone)]
pub(crate) struct UnixNode {
    pub(super) id: u64,
    pub(super) socket: Weak<UnixSocket>,
}

/// @description AF_UNIX transport 可保活、但无需识别 concrete OFD 的 passed-file capability。
pub(crate) trait UnixPassedFile: Any + Send + Sync {
    /// @description 把 transport capability 恢复为可由 syscall seam downcast 的类型擦除 Arc。
    /// @return 同一 allocation 的 `Any` Arc，不复制底层 file state。
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;

    /// @description 若该 file 是 AF_UNIX socket，投影其稳定 GC node。
    /// @return AF_UNIX node；其他 OFD kind 为 None。
    fn unix_node(&self) -> Option<UnixNode>;

    /// @description 判断除了 inflight rights 与本次 Weak upgrade 外是否仍有 file root。
    /// @param inflight 同一 AF_UNIX file 在 graph 中的 outgoing edge 数。
    /// @return descriptor、active syscall 或其他非-rights Arc 存在时为 true。
    fn externally_referenced(self: Arc<Self>, inflight: usize) -> bool;
}

/// @description 一条 SCM_RIGHTS control message 的完整 file capability 集合。
pub(crate) struct UnixRights {
    files: Vec<Arc<dyn UnixPassedFile>>,
    nodes: Vec<UnixNode>,
    uid: u32,
    limit: usize,
    batch_id: u64,
    attached_target: Option<u64>,
}

impl UnixRights {
    /// @description 构造已经由 fd-table owner 一次性捕获的 SCM_RIGHTS 集合。
    /// @param files 按用户 cmsg 顺序排列的共享 file capabilities。
    /// @param uid sender 的 real UID；Linux inflight accounting 按 real UID 隔离。
    /// @param limit sender 当前 RLIMIT_NOFILE soft limit。
    /// @return 非空、最多 SCM_MAX_FD 个 rights。
    /// @errors 空集合或超过 Linux SCM_MAX_FD 返回 Invalid。
    pub(crate) fn new(
        files: Vec<Arc<dyn UnixPassedFile>>,
        uid: u32,
        limit: usize,
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
        })
    }

    /// @description 返回本 control message 的 descriptor 数量。
    /// @return 1..=SCM_MAX_FD。
    pub(crate) fn len(&self) -> usize {
        self.files.len()
    }

    /// @description 把 transport ownership 转交给 syscall/fd-table publication seam。
    /// @return 保持 cmsg 顺序且复用原 Vec backing 的 file capabilities。
    pub(crate) fn into_files(mut self) -> Vec<Arc<dyn UnixPassedFile>> {
        self.detach();
        core::mem::take(&mut self.files)
    }

    /// @description 在 transport owner lock 内尝试把本批 rights 计入 receiver/UID graph。
    /// @param target 即将拥有本 control message 的 receive endpoint。
    /// @return graph publication 成功；重复 attach 是内部不变量损坏。
    /// @errors graph storage OOM，或返回内部 NeedsCollection 要求 caller 释放 transport lock。
    pub(super) fn try_attach(
        &mut self,
        target: &Arc<UnixSocket>,
    ) -> Result<(), super::rights_graph::AttachError> {
        assert!(
            self.attached_target.is_none(),
            "SCM_RIGHTS batch attached twice"
        );
        super::rights_graph::try_attach(
            self.uid,
            self.limit,
            self.batch_id,
            self.files.len(),
            &self.nodes,
            target,
        )?;
        self.attached_target = Some(target.node_id());
        Ok(())
    }

    /// @description 在 transport lock 外为下一次 attach 同步执行 quota/cycle recovery。
    /// @return quota 已可重试，或至少一个 dead SCC 已被清空。
    /// @errors 没有可回收 SCC 且 sender 仍超过 RLIMIT_NOFILE 时返回 TooManyReferences。
    pub(super) fn collect(&self) -> Result<(), SocketError> {
        super::rights_graph::collect(self.uid, self.limit, self.files.len())
    }

    /// @description 回滚尚未 transport commit 的 graph publication。
    /// @return 无返回值；未 attach 时为空操作。
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
