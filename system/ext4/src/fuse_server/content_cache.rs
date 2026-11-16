//! 同一挂载内，每个 ext4 inode 只有一个已发布的内容 VMO。

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use native_runtime::sync::Mutex;

use crate::{
    content::Content,
    ext4::Ext4Inode,
    vfs::{FileSystemError, InodeType},
};

/// lookup、fd 与 mmap 共用的内容 VMO 集合，由最后的 inode 引用和写回结果决定退休。
#[derive(Default)]
pub(super) struct ContentCache {
    // 复用挂载原有的内容 map 锁。只同步发布、引用复查和移除，写回不持此锁。
    contents: Mutex<BTreeMap<u64, Arc<Content>>>,
}

impl ContentCache {
    /// 返回文件内容；需要时首次发布。非普通文件没有内容 VMO。
    ///
    /// # Errors
    ///
    /// VMO 创建或补页来源登记失败时返回错误。
    pub(super) fn get_or_create(
        &self,
        inode: &Arc<Ext4Inode>,
    ) -> Result<Option<Arc<Content>>, FileSystemError> {
        if inode.inode_type() != InodeType::File {
            return Ok(None);
        }
        let mut contents = self.contents.lock();
        if let Some(content) = contents.get(&inode.number()) {
            return Ok(Some(content.clone()));
        }
        let content = Content::new(inode.clone())?;
        contents.insert(inode.number(), content.clone());
        Ok(Some(content))
    }

    /// 返回已发布的 VMO，缺失时返回 None。
    pub(super) fn get(&self, number: u64) -> Option<Arc<Content>> {
        self.contents.lock().get(&number).cloned()
    }

    /// FORGET 或 RELEASE 撤销一个 inode 引用后，尝试写回并退休内容。
    ///
    /// # Errors
    ///
    /// 写回失败会返回错误并保留待重试的条目。
    pub(super) fn retire(&self, number: u64) -> Result<(), FileSystemError> {
        if let Some(content) = self.get(number)
            && content.is_unpinned()
        {
            self.writeback_entry(&content)?;
        }
        Ok(())
    }

    /// 写回当前全部内容，并重试退休先前仍在使用的条目。
    ///
    /// # Errors
    ///
    /// 返回首个写回错误，失败条目保持可访问。
    pub(super) fn writeback(&self) -> Result<(), FileSystemError> {
        let contents: Vec<_> = self.contents.lock().values().cloned().collect();
        contents
            .iter()
            .try_for_each(|entry| self.writeback_entry(entry))
    }

    /// 挂载拆除时移交全部内容；调用方先排空请求，再执行最终写回。
    pub(super) fn take_all(&self) -> BTreeMap<u64, Arc<Content>> {
        core::mem::take(&mut *self.contents.lock())
    }

    fn writeback_entry(&self, content: &Arc<Content>) -> Result<(), FileSystemError> {
        content.writeback_then_retire(|number| {
            let mut contents = self.contents.lock();
            // 写回期间同一 inode 的新 lookup 必须复用原 VMO。持内容事务锁复查引用，避免
            // 把写回之后的再次写入随最后一个缓存引用一起丢弃。
            if content.is_unpinned()
                && Arc::strong_count(content) == 2
                && contents
                    .get(&number)
                    .is_some_and(|entry| Arc::ptr_eq(entry, content))
            {
                contents.remove(&number);
            }
        })
    }
}
