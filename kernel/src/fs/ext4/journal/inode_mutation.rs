use core::{
    marker::PhantomData,
    ops::{Deref, DerefMut},
};

use crate::fs::ext4::{Ext4Inode, Ext4InodeDisk};

/// mutation owner 的 inode working copy；Drop 时用短 spin 临界区发布 live state。
///
/// ext4 的唯一 mutation mutex 已排除并发 writer，因此 working copy 不需要在 journal/block
/// I/O 期间保留 inode spin lock。读者在发布前看到旧 snapshot，发布后看到完整新 snapshot，
/// abort 则由 `MutationGuard` 恢复首次写入前的 preimage。
pub(in crate::fs::ext4) struct InodeMutation<'mutation, 'inode> {
    inode: &'inode Ext4Inode,
    disk: Ext4InodeDisk,
    transaction: PhantomData<&'mutation mut ()>,
}

impl<'mutation, 'inode> InodeMutation<'mutation, 'inode> {
    pub(super) const fn new(inode: &'inode Ext4Inode, disk: Ext4InodeDisk) -> Self {
        Self {
            inode,
            disk,
            transaction: PhantomData,
        }
    }
}

impl Deref for InodeMutation<'_, '_> {
    type Target = Ext4InodeDisk;

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
