//! 所有 mount view 共用的真实 filesystem 生命周期与物理设备租约。
use super::super::FileSystem;
use super::super::{BlockNode, DeviceNumber, devnode};
use super::{FileSystemError, Inode};
use crate::{fallible_tree::FallibleMap, sync::TaskMutex};
use alloc::sync::{Arc, Weak};
use spin::Mutex;

// OWNER: 只索引已取得的实际 filesystem，供 sync 精确访问；弱 membership 不延长其寿命。
// 注册节点预分配、最后实例析构/节点释放均在短锁外；不存在每 namespace 的重复 filesystem。
static FILESYSTEMS: Mutex<FallibleMap<usize, Weak<MountedFilesystem>>> =
    Mutex::new(FallibleMap::new());

/// 真实物理 claim 的 RAII：创建失败与最后 filesystem 释放各恰好归还一次。
pub(in crate::fs) struct MountDeviceClaim(Arc<BlockNode>);
impl MountDeviceClaim {
    pub(in crate::fs) fn acquire(number: DeviceNumber) -> Result<Self, FileSystemError> {
        let block = devnode::block_node(number).ok_or(FileSystemError::NoDevice)?;
        block.begin_mount()?;
        Ok(Self(block))
    }
    pub(super) fn number(&self) -> DeviceNumber {
        self.0.number()
    }
}
impl Drop for MountDeviceClaim {
    fn drop(&mut self) {
        self.0.end_mount();
    }
}

pub(super) struct SuperState {
    pub(super) read_only: bool,
    // 所有 mount view 的实际 writer 总数；每 view mnt_writers 是不同的准入数量。
    pub(super) writers: usize,
}
/// 具体 adapter、superblock write admission 与物理租约的唯一 owner。
/// namespace clone 只增此 Arc；关闭一个 namespace/mount 不能停止别的 view 的 writeback。
pub(in crate::fs) struct MountedFilesystem {
    pub(super) filesystem: Arc<dyn FileSystem>,
    // OWNER: 创建 superblock 时的实际 user namespace，namespace clone/setns 不重定向其权限。
    pub(super) user: Arc<crate::namespace::UserNamespace>,
    claim: Option<MountDeviceClaim>,
    pub(super) state: Mutex<SuperState>,
    // OWNER: writer grant 与 backend reconfiguration 的唯一 sleeping gate。
    // writer release 只使用 state 短锁；若先发表 RW 再恢复 journal，会准入未准备好的写者。
    pub(super) admission: TaskMutex<()>,
    // None 是尚未注册的已创建资源；失败仍要 shutdown，但不能删除别人的 membership。
    registration: Mutex<Option<usize>>,
}
impl MountedFilesystem {
    /// device claim 必须先于 adapter create 准备；本方法消费已创建的真实 adapter。
    /// 所有失败由同一资源 Drop 停止后台工作并归还实际租约，不建立 dummy filesystem。
    pub(in crate::fs) fn new(
        filesystem: Arc<dyn FileSystem>,
        claim: Option<MountDeviceClaim>,
        read_only: bool,
        user: Arc<crate::namespace::UserNamespace>,
    ) -> Result<(Arc<Self>, Arc<dyn Inode>), FileSystemError> {
        let owner = Arc::try_new(Self {
            filesystem,
            user,
            claim,
            state: Mutex::new(SuperState {
                read_only,
                writers: 0,
            }),
            admission: TaskMutex::new(()),
            registration: Mutex::new(None),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        let root = owner.filesystem.root_inode()?;
        let identity = root.filesystem_id();
        let node = FallibleMap::try_prepare(identity, Arc::downgrade(&owner))
            .map_err(|_| FileSystemError::OutOfMemory)?;
        {
            let mut entries = FILESYSTEMS.lock();
            assert!(
                !entries.contains_key(&identity),
                "filesystem adapter mounted twice instead of sharing its owner"
            );
            entries.commit_vacant(node);
            *owner.registration.lock() = Some(identity);
        }
        Ok((owner, root))
    }
    /// sget：只 pin 索引中仍活着的 actual superblock，不在 registry 短锁内 upgrade/final Drop。
    /// Caller 持完整 mount transaction；同一块设备永远只有一个 adapter/claim owner。
    pub(in crate::fs) fn for_device(number: DeviceNumber) -> Option<Arc<Self>> {
        let mut cursor = None;
        loop {
            let next = {
                let entries = FILESYSTEMS.lock();
                match cursor {
                    Some(cursor) => entries.successor(&cursor),
                    None => entries.first_key_value(),
                }
                .map(|(id, owner)| (*id, owner.clone()))
            };
            let (id, owner) = next?;
            cursor = Some(id);
            if let Some(owner) = owner.upgrade()
                && owner.device() == Some(number)
            {
                return Some(owner);
            }
        }
    }
    pub(in crate::fs) fn reuse_root(
        &self,
        type_name: &str,
        read_only: bool,
        user: &Arc<crate::namespace::UserNamespace>,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        if self.filesystem.statistics()?.type_name != type_name
            || self.state.lock().read_only != read_only
        {
            return Err(FileSystemError::Busy);
        }
        if !Arc::ptr_eq(&self.user, user) {
            return Err(FileSystemError::PermissionDenied);
        }
        self.filesystem.root_inode()
    }
    pub(super) fn device(&self) -> Option<DeviceNumber> {
        self.claim.as_ref().map(MountDeviceClaim::number)
    }
}
impl Drop for MountedFilesystem {
    fn drop(&mut self) {
        let identity = self.registration.get_mut().take();
        if let Some(identity) = identity {
            let retired = {
                let mut entries = FILESYSTEMS.lock();
                if entries
                    .get(&identity)
                    .is_some_and(|entry| core::ptr::eq(entry.as_ptr(), self))
                {
                    entries.take_entry(&identity)
                } else {
                    None
                }
            };
            drop(retired);
            super::super::filesystem_unmounted(identity);
            if let Err(error) = super::super::page_cache::evict_filesystem(identity) {
                crate::warn!("last filesystem page-cache writeback failed: {:?}", error);
            }
        }
        // 后台线程必须停止、journal 必须排空，再由 claim 字段 Drop 解冻物理设备。
        if let Err(error) = self.filesystem.shutdown() {
            crate::warn!("last filesystem shutdown failed: {:?}", error);
        }
    }
}

/// sync 沿唯一 actual filesystem index 每次只 pin 一个 owner；不复制 namespace mount 列表。
pub(super) fn sync_all() -> Result<(), FileSystemError> {
    let mut cursor = None;
    let mut result = Ok(());
    loop {
        let next = {
            let entries = FILESYSTEMS.lock();
            match cursor {
                Some(cursor) => entries.successor(&cursor),
                None => entries.first_key_value(),
            }
            .map(|(id, owner)| (*id, owner.clone()))
        };
        let Some((id, owner)) = next else {
            break;
        };
        cursor = Some(id);
        if let Some(owner) = owner.upgrade()
            && let Err(error) = owner
                .filesystem
                .root_inode()
                .and_then(|root| root.sync_storage())
        {
            result = result.and(Err(error));
        }
    }
    result
}
