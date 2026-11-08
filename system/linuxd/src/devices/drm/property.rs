//! DRM property blob：不可变载荷、device-wide 弱索引与 per-OFD 创建者引用。
use super::{DrmDevice, DrmError, DrmFile, publication::ObjectIdReservation};
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use fallible_tree::{FallibleMap, VacantEntry};

/// 不可变 KMS property payload；提交者可持有 Arc，使 DESTROY 不提前退休在途数据。
pub(super) struct Blob {
    pub(super) id: u32,
    pub(super) bytes: Vec<u8>,
    // OWNER: 只用来摘除弱索引，不反向保活 device；device/active blob 之间不能形成 Arc 环。
    device: Weak<DrmDevice>,
}
impl Blob {
    /// 是否能作为一个 KMS blob 分配；零长度及超过有符号分配大小的载荷无效。
    pub(super) fn valid_length(length: usize) -> bool {
        length != 0
            && length
                .checked_add(size_of::<Self>())
                .is_some_and(|size| size <= i32::MAX as usize)
    }
}
impl Drop for Blob {
    fn drop(&mut self) {
        if let Some(device) = self.device.upgrade() {
            let removed = device.state.lock().blobs.take_entry(&self.id);
            // Weak 节点与载荷均在锁外析构；所有最后强引用也必须在 device/OFD 锁外释放。
            drop(removed);
        }
    }
}

/// 尚未发布的全局 blob；字段顺序保证载荷/弱索引退休后才回滚可重用 ID。
struct PreparedBlob<'file> {
    file: &'file DrmFile,
    blob: Arc<Blob>,
    global: VacantEntry<u32, Weak<Blob>>,
    id: ObjectIdReservation<'file>,
}
impl PreparedBlob<'_> {
    fn publish(self) -> Arc<Blob> {
        let Self {
            file,
            blob,
            global,
            id,
        } = self;
        file.device.state.lock().blobs.commit_vacant(global);
        id.commit();
        blob
    }
}
/// 用户创建者引用与全局索引的两阶段发布，copyout 失败无副作用。
pub(super) struct PreparedUserBlob<'file> {
    // 先退休创建者引用，再析构全局准备事务，防止 ID 已回滚而迟到的 Blob.Drop 摘除新对象。
    owned: VacantEntry<u32, Arc<Blob>>,
    prepared: PreparedBlob<'file>,
}
impl PreparedUserBlob<'_> {
    /// 写回 ID 后发布 OFD/global 两份索引；临时 blob 引用跨过两个阶段，防止并发 DESTROY 留下死亡弱节点。
    pub(super) fn complete<E>(self, copyout: impl FnOnce(u32) -> Result<(), E>) -> Result<(), E> {
        copyout(self.prepared.blob.id)?;
        let Self { owned, prepared } = self;
        prepared.file.state.lock().blobs.commit_vacant(owned);
        drop(prepared.publish());
        Ok(())
    }
}
impl DrmFile {
    /// 准备不可变的 property blob；载荷已由 ioctl 从用户空间复制。
    ///
    /// # Errors
    /// 空载荷/长度非法、节点/control block OOM 或 mode object ID 耗尽。
    pub(super) fn prepare_property_blob(
        &self,
        bytes: Vec<u8>,
    ) -> Result<PreparedUserBlob<'_>, DrmError> {
        let prepared = self.prepare_blob(bytes)?;
        let owned = FallibleMap::<u32, Arc<Blob>>::try_reserve_node()
            .map_err(|_| DrmError::OutOfMemory)?
            .fill(prepared.blob.id, prepared.blob.clone());
        Ok(PreparedUserBlob { owned, prepared })
    }

    /// 为 KMS 自身状态建立 blob；只由返回的状态引用保活，不创建伪用户引用或多余 OFD 节点。
    pub(super) fn state_blob(&self, bytes: Vec<u8>) -> Result<Arc<Blob>, DrmError> {
        Ok(self.prepare_blob(bytes)?.publish())
    }

    fn prepare_blob(&self, bytes: Vec<u8>) -> Result<PreparedBlob<'_>, DrmError> {
        if !Blob::valid_length(bytes.len()) {
            return Err(DrmError::Invalid);
        }
        let id = ObjectIdReservation::reserve(self)?;
        let blob = Arc::try_new(Blob {
            id: id.id,
            bytes,
            device: Arc::downgrade(&self.device),
        })
        .map_err(|_| DrmError::OutOfMemory)?;
        let global = FallibleMap::<u32, Weak<Blob>>::try_reserve_node()
            .map_err(|_| DrmError::OutOfMemory)?
            .fill(blob.id, Arc::downgrade(&blob));
        Ok(PreparedBlob {
            file: self,
            blob,
            global,
            id,
        })
    }

    /// 按 device-wide mode object ID 取得只读引用；不限于创建它的 OFD。
    ///
    /// # Errors
    /// 对象不存在、类型不同或最后引用已退休时返回 NotFound。
    pub(super) fn property_blob(&self, id: u32) -> Result<Arc<Blob>, DrmError> {
        let blob = self
            .device
            .state
            .lock()
            .blobs
            .get(&id)
            .and_then(Weak::upgrade);
        blob.ok_or(DrmError::NotFound)
    }

    /// 撤销本 OFD 的创建者引用；其他查询/提交仍可保活同一不可变 blob。
    ///
    /// # Errors
    /// 不存在返回 NotFound，存在但本 OFD 未创建或已释放返回 NotOwner。
    pub(super) fn destroy_property_blob(&self, id: u32) -> Result<(), DrmError> {
        let live = self.property_blob(id)?;
        let owned = self
            .state
            .lock()
            .blobs
            .take_entry(&id)
            .ok_or(DrmError::NotOwner)?;
        drop(owned);
        drop(live);
        Ok(())
    }
}
