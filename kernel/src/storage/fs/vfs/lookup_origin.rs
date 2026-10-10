use super::OpenedFile;
use alloc::sync::Arc;

/// 单次 pathname traversal 的 fs root/cwd 或 dirfd 起点；不保存 Task 或全局隐式 root。
#[derive(Clone)]
pub(crate) struct LookupOrigin {
    pub(super) root: Arc<OpenedFile>,
    pub(super) start: Arc<OpenedFile>,
}
impl LookupOrigin {
    /// 两个 opened identity 必须来自 caller fs context 的同锁快照或显式 boot root。
    pub(crate) fn new(root: Arc<OpenedFile>, start: Arc<OpenedFile>) -> Self {
        Self { root, start }
    }
    /// dirfd 与 magic link 只替换相对起点；绝对路径与后续绝对 symlink 保留原 root。
    pub(crate) fn with_start(self, start: Arc<OpenedFile>) -> Self {
        Self {
            root: self.root,
            start,
        }
    }
    /// 为 proc viewer 投影保活 caller 的 root entry，不复制目录关系。
    pub(crate) fn root(&self) -> Arc<OpenedFile> {
        self.root.clone()
    }
    /// 消费快照，把两个保活 identity 移交 lookup/getcwd；不会重新读取 fs context。
    pub(crate) fn into_parts(self) -> (Arc<OpenedFile>, Arc<OpenedFile>) {
        (self.root, self.start)
    }
}
