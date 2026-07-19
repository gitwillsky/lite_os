use core::{
    marker::PhantomData,
    ops::{Deref, DerefMut},
};

use crate::fs::ext2::{Ext2Inode, Ext2InodeDisk};

/// @description mutation owner 的 inode working copy；Drop 时用短 spin 临界区发布 live state。
///
/// ext2 的唯一 mutation mutex 已排除并发 writer，因此 working copy 不需要在 journal/block
/// I/O 期间保留 inode spin lock。读者在发布前看到旧 snapshot，发布后看到完整新 snapshot，
/// abort 则由 `MutationGuard` 恢复首次写入前的 preimage。
pub(in crate::fs::ext2) struct InodeMutation<'mutation, 'inode> {
    inode: &'inode Ext2Inode,
    disk: Ext2InodeDisk,
    transaction: PhantomData<&'mutation mut ()>,
}

impl<'mutation, 'inode> InodeMutation<'mutation, 'inode> {
    pub(super) const fn new(inode: &'inode Ext2Inode, disk: Ext2InodeDisk) -> Self {
        Self {
            inode,
            disk,
            transaction: PhantomData,
        }
    }
}

impl Deref for InodeMutation<'_, '_> {
    type Target = Ext2InodeDisk;

    fn deref(&self) -> &Self::Target {
        &self.disk
    }
}

impl DerefMut for InodeMutation<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.disk
    }
}

impl Drop for InodeMutation<'_, '_> {
    fn drop(&mut self) {
        *self.inode.disk.lock() = self.disk;
    }
}
