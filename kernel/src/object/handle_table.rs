//! 进程句柄表。
//!
//! 句柄值 = `(槽位索引 + 1) << GENERATION_BITS | generation`。槽位释放后 generation 递增，
//! 已关闭的旧句柄值因此不会命中复用该槽位的新对象（ABA）。0 永远无效。
//! 空闲槽位以槽内 `next_free` 串成链表，释放不分配；增长走锁外扩容。

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use native_abi::{Handle, Status};
use spin::Mutex;

use super::Object;
use super::signals::CancelKey;
use crate::execution::sync::lock_with_spare;

const GENERATION_BITS: u32 = 10;
const GENERATION_MASK: u32 = (1 << GENERATION_BITS) - 1;
/// 单个进程最多持有的句柄数。
const MAX_HANDLES: usize = 1 << 16;

// OWNER: 只提供 CancelKey 的全局唯一表编号；Relaxed 足够，因为它不发布任何内存内容。
static NEXT_TABLE_ID: AtomicU64 = AtomicU64::new(1);

/// 句柄所引用的对象与 rights。
#[derive(Clone)]
pub(crate) struct HandleEntry {
    /// 被引用的对象。
    pub(crate) object: Object,
    /// 句柄 rights。
    pub(crate) rights: u32,
}

struct Slot {
    generation: u32,
    entry: Option<HandleEntry>,
    next_free: Option<u32>,
}

struct TableInner {
    slots: Vec<Slot>,
    free_head: Option<u32>,
    free_count: usize,
    // FLAG: 进程终止后关闭；缺失时已终止进程仍可经 thread_start 接收句柄，形成对象引用环而永不释放。
    closed: bool,
}

/// 一个原生进程的句柄表。
pub(crate) struct HandleTable {
    id: u64,
    inner: Mutex<TableInner>,
}

impl HandleTable {
    /// 构造空表。
    pub(crate) fn new() -> Self {
        Self {
            id: NEXT_TABLE_ID.fetch_add(1, Ordering::Relaxed),
            inner: Mutex::new(TableInner {
                slots: Vec::new(),
                free_head: None,
                free_count: 0,
                closed: false,
            }),
        }
    }

    /// `handle` 在本表中的取消键。
    pub(crate) fn cancel_key(&self, handle: Handle) -> CancelKey {
        CancelKey {
            table: self.id,
            handle,
        }
    }

    /// 插入一组句柄；全部成功或全部失败。
    ///
    /// # Returns
    /// 与输入顺序一致的句柄值。
    ///
    /// # Errors
    /// 超过上限返回 `ERR_NO_RESOURCES`，分配失败返回 `ERR_NO_MEMORY`，表已关闭返回 `ERR_BAD_STATE`；
    /// 失败时 entries 原样交还调用者以便在锁外释放。
    pub(crate) fn insert_all(
        &self,
        entries: Vec<HandleEntry>,
        output: &mut Vec<Handle>,
    ) -> Result<(), (Status, Vec<HandleEntry>)> {
        if output.try_reserve_exact(entries.len()).is_err() {
            return Err((native_abi::ERR_NO_MEMORY, entries));
        }
        let count = entries.len();
        let guard = lock_with_spare(
            &self.inner,
            |inner| count.saturating_sub(inner.free_count),
            |inner| &mut inner.slots,
        );
        let Ok(mut inner) = guard else {
            return Err((native_abi::ERR_NO_MEMORY, entries));
        };
        if inner.closed {
            return Err((native_abi::ERR_BAD_STATE, entries));
        }
        if inner.slots.len() - inner.free_count + count > MAX_HANDLES {
            return Err((native_abi::ERR_NO_RESOURCES, entries));
        }
        for entry in entries {
            output.push(inner.insert(entry));
        }
        Ok(())
    }

    /// 插入一个句柄。
    ///
    /// # Errors
    /// 同 [`Self::insert_all`]；失败时 entry 在锁外释放。
    pub(crate) fn insert(&self, entry: HandleEntry) -> Result<Handle, Status> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        entries.push(entry);
        let mut output = Vec::new();
        self.insert_all(entries, &mut output)
            .map_err(|(status, _)| status)?;
        Ok(output[0])
    }

    /// 读取句柄引用的对象。
    ///
    /// # Errors
    /// 句柄不存在返回 `ERR_BAD_HANDLE`。
    pub(crate) fn get(&self, handle: Handle) -> Result<HandleEntry, Status> {
        let inner = self.inner.lock();
        let index = inner.locate(handle)?;
        Ok(inner.slots[index]
            .entry
            .clone()
            .expect("located handle slot is live"))
    }

    /// 移出句柄；调用者在锁外释放返回值。
    ///
    /// # Errors
    /// 句柄不存在返回 `ERR_BAD_HANDLE`。
    pub(crate) fn remove(&self, handle: Handle) -> Result<HandleEntry, Status> {
        let mut inner = self.inner.lock();
        let index = inner.locate(handle)?;
        Ok(inner.release(index))
    }

    /// 按顺序移出一组句柄；任一无效或重复时不移出任何句柄。
    ///
    /// # Errors
    /// `ERR_BAD_HANDLE`（不存在）、`ERR_INVALID_ARGS`（重复）、`ERR_NO_MEMORY`。
    pub(crate) fn remove_all(&self, handles: &[Handle]) -> Result<Vec<HandleEntry>, Status> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(handles.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut inner = self.inner.lock();
        for (position, &handle) in handles.iter().enumerate() {
            inner.locate(handle)?;
            if handles[..position].contains(&handle) {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
        }
        for &handle in handles {
            let index = inner.locate(handle).expect("validated handle remains live");
            entries.push(inner.release(index));
        }
        Ok(entries)
    }

    /// 关闭表并移出全部句柄；之后的插入返回 `ERR_BAD_STATE`。调用者在锁外释放返回值。
    pub(crate) fn close(&self) -> ClosedHandles {
        let mut inner = self.inner.lock();
        inner.closed = true;
        inner.free_head = None;
        inner.free_count = 0;
        ClosedHandles(core::mem::take(&mut inner.slots))
    }
}

/// 关闭句柄表时移出的全部句柄。
pub(crate) struct ClosedHandles(Vec<Slot>);

impl ClosedHandles {
    /// 逐个取出 `(句柄值, entry)`。
    pub(crate) fn into_entries(self) -> impl Iterator<Item = (Handle, HandleEntry)> {
        self.0.into_iter().enumerate().filter_map(|(index, slot)| {
            let generation = slot.generation;
            slot.entry.map(|entry| (encode(index, generation), entry))
        })
    }
}

impl TableInner {
    fn locate(&self, handle: Handle) -> Result<usize, Status> {
        let index = (handle >> GENERATION_BITS)
            .checked_sub(1)
            .ok_or(native_abi::ERR_BAD_HANDLE)? as usize;
        let slot = self.slots.get(index).ok_or(native_abi::ERR_BAD_HANDLE)?;
        if slot.entry.is_none() || slot.generation != handle & GENERATION_MASK {
            return Err(native_abi::ERR_BAD_HANDLE);
        }
        Ok(index)
    }

    /// 调用者已保证有空闲槽位或容量。
    fn insert(&mut self, entry: HandleEntry) -> Handle {
        if let Some(index) = self.free_head {
            let slot = &mut self.slots[index as usize];
            self.free_head = slot.next_free.take();
            self.free_count -= 1;
            slot.entry = Some(entry);
            return encode(index as usize, slot.generation);
        }
        debug_assert!(self.slots.len() < self.slots.capacity());
        self.slots.push(Slot {
            generation: 0,
            entry: Some(entry),
            next_free: None,
        });
        encode(self.slots.len() - 1, 0)
    }

    fn release(&mut self, index: usize) -> HandleEntry {
        let slot = &mut self.slots[index];
        let entry = slot.entry.take().expect("released handle slot is live");
        slot.generation = (slot.generation + 1) & GENERATION_MASK;
        slot.next_free = self.free_head;
        self.free_head = Some(index as u32);
        self.free_count += 1;
        entry
    }
}

fn encode(index: usize, generation: u32) -> Handle {
    ((index as u32 + 1) << GENERATION_BITS) | generation
}
