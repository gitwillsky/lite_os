//! System V 各对象类型独立的实际 ID/key registry；message/sem/shm 不复制编号算法。
use super::IpcError;
use crate::fallible_tree::{FallibleMap, NodeSlot, VacantEntry};
use alloc::sync::Arc;
use core::mem::MaybeUninit;

pub(super) const IPC_ID_SLOTS: u32 = 32768;

/// ID 是不可变身份；key 由实际 ipc_perm 拥有，shm RMID 可撤销 key 而保留 ID。
pub(super) trait IpcObject {
    fn id(&self) -> i32;
    fn key(&self) -> i32;
}
pub(super) struct IpcIds<T> {
    objects: FallibleMap<u16, Arc<T>>,
    keys: FallibleMap<i32, u16>,
    next_slot: u32,
    sequence: u32,
}
/// 锁外完整准备实际 Arc 与两个索引节点；竞争失败时 RAII 归还全部未发布 storage。
pub(super) struct PreparedIpcObject<T> {
    storage: Arc<MaybeUninit<T>>,
    object_node: NodeSlot<u16, Arc<T>>,
    key_node: Option<NodeSlot<i32, u16>>,
    key: i32,
}
impl<T> PreparedIpcObject<T> {
    pub(super) fn new(key: i32) -> Result<Self, IpcError> {
        Ok(Self {
            storage: Arc::try_new_uninit().map_err(|_| IpcError::NoMemory)?,
            object_node: FallibleMap::try_reserve_node().map_err(|_| IpcError::NoMemory)?,
            key_node: if key == 0 {
                None
            } else {
                Some(FallibleMap::try_reserve_node().map_err(|_| IpcError::NoMemory)?)
            },
            key,
        })
    }
}
/// 在外层实际 registry 锁内选择、随后无失败发布；该 token 不拥有第二份可变 allocator。
pub(super) struct IpcIdReservation {
    pub(super) id: i32,
    slot: u32,
    sequence: u32,
}
pub(super) struct RetiredIpcObject<T> {
    _object: VacantEntry<u16, Arc<T>>,
    _key: Option<VacantEntry<i32, u16>>,
}
impl<T: IpcObject> IpcIds<T> {
    pub(super) const fn new() -> Self {
        Self {
            objects: FallibleMap::new(),
            keys: FallibleMap::new(),
            next_slot: 0,
            sequence: 0,
        }
    }
    pub(super) fn len(&self) -> usize {
        self.objects.len()
    }
    pub(super) fn iter(&self) -> crate::fallible_tree::Iter<'_, u16, Arc<T>> {
        self.objects.iter()
    }
    pub(super) fn iter_from(&self, slot: u16) -> crate::fallible_tree::Iter<'_, u16, Arc<T>> {
        self.objects.iter_from(&slot)
    }
    /// 单行 proc/IPC 查询直接使用实际 slot neighbor，不构造一次性 iterator。
    pub(super) fn at_or_after(&self, slot: u16) -> Option<(&u16, &Arc<T>)> {
        self.objects.ceiling(&slot)
    }
    pub(super) fn get_key(&self, key: i32) -> Option<&Arc<T>> {
        if key == 0 {
            return None;
        }
        self.keys.get(&key).map(|slot| {
            self.objects
                .get(slot)
                .expect("IPC key references actual object")
        })
    }
    pub(super) fn get(&self, id: i32) -> Result<&Arc<T>, IpcError> {
        if id < 0 {
            return Err(IpcError::Invalid);
        }
        self.objects
            .get(&((id as u32 % IPC_ID_SLOTS) as u16))
            .filter(|object| object.id() == id)
            .ok_or(IpcError::Invalid)
    }
    pub(super) fn get_index(&self, index: i32) -> Result<&Arc<T>, IpcError> {
        let index = u16::try_from(index).map_err(|_| IpcError::Invalid)?;
        self.objects.get(&index).ok_or(IpcError::Invalid)
    }
    pub(super) fn reserve(&self, maximum: usize) -> Result<IpcIdReservation, IpcError> {
        if self.len() >= maximum || self.len() >= IPC_ID_SLOTS as usize {
            return Err(IpcError::NoSpace);
        }
        let mut slot = self.next_slot;
        let mut sequence = self.sequence;
        loop {
            if slot == IPC_ID_SLOTS {
                slot = 0;
                sequence = (sequence + 1) & u32::from(u16::MAX);
            }
            // Linux IPC64 seq 是 16-bit 环；只索引当前实际对象，observer Arc 不变成新对象。
            if !self.objects.contains_key(&(slot as u16)) {
                return Ok(IpcIdReservation {
                    id: (sequence * IPC_ID_SLOTS + slot) as i32,
                    slot,
                    sequence,
                });
            }
            slot += 1;
        }
    }
    pub(super) fn publish(
        &mut self,
        mut prepared: PreparedIpcObject<T>,
        reservation: IpcIdReservation,
        object: T,
    ) -> i32 {
        assert_eq!(object.id(), reservation.id);
        assert_eq!(object.key(), prepared.key);
        assert!(!self.objects.contains_key(&(reservation.slot as u16)));
        assert!(prepared.key == 0 || !self.keys.contains_key(&prepared.key));
        Arc::get_mut(&mut prepared.storage)
            .expect("unpublished IPC Arc")
            .write(object);
        // SAFETY: 完整对象刚初始化，Arc 尚未共享；发布节点已预留，以下提交不分配、不失败。
        let object = unsafe { prepared.storage.assume_init() };
        self.objects
            .commit_vacant(prepared.object_node.fill(reservation.slot as u16, object));
        if let Some(node) = prepared.key_node {
            self.keys
                .commit_vacant(node.fill(prepared.key, reservation.slot as u16));
        }
        self.next_slot = reservation.slot + 1;
        self.sequence = reservation.sequence;
        reservation.id
    }
    pub(super) fn remove(&mut self, id: i32) -> Result<RetiredIpcObject<T>, IpcError> {
        let key = self.get(id)?.key();
        let slot = (id as u32 % IPC_ID_SLOTS) as u16;
        Ok(RetiredIpcObject {
            _object: self
                .objects
                .take_entry(&slot)
                .expect("checked IPC identity"),
            _key: if key == 0 {
                None
            } else {
                self.keys.take_entry(&key)
            },
        })
    }
    /// 外层 registry 同锁事务撤销实际对象的 key；对象随后将 ipc_perm.key 置 IPC_PRIVATE。
    /// ID 和 payload 继续保留，摘下的 AVL storage 必须在 registry guard 外析构。
    pub(super) fn withdraw_key(
        &mut self,
        id: i32,
    ) -> Result<Option<VacantEntry<i32, u16>>, IpcError> {
        let key = self.get(id)?.key();
        Ok(if key == 0 {
            None
        } else {
            self.keys.take_entry(&key)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Object {
        id: i32,
        key: i32,
    }
    impl IpcObject for Object {
        fn id(&self) -> i32 {
            self.id
        }
        fn key(&self) -> i32 {
            self.key
        }
    }
    #[test]
    fn ipc64_sequence_wrap_does_not_exhaust_admission_or_replace_live_slot() {
        let mut ids = IpcIds::new();
        let reserved = ids.reserve(IPC_ID_SLOTS as usize).unwrap();
        let old = reserved.id;
        ids.publish(
            PreparedIpcObject::new(5).unwrap(),
            reserved,
            Object { id: old, key: 5 },
        );
        let observer = ids.get(old).unwrap().clone();
        ids.next_slot = IPC_ID_SLOTS;
        ids.sequence = u32::from(u16::MAX);
        let reserved = ids.reserve(IPC_ID_SLOTS as usize).unwrap();
        assert_eq!(reserved.id, 1);
        let id = reserved.id;
        ids.publish(
            PreparedIpcObject::new(6).unwrap(),
            reserved,
            Object { id, key: 6 },
        );
        assert!(Arc::ptr_eq(&observer, ids.get(old).unwrap()));
        assert_eq!(ids.get_key(6).unwrap().id(), 1);
    }
}
