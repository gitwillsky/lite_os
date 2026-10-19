//! tmpfs 目录项集合：按名字 O(log n) 查找，同时按稳定 cookie 顺序迭代。
//!
//! `getdents` 的 cursor 是上一项的 cookie。cookie 单调分配且永不复用，所以迭代中途删除、创建任意项
//! 都不会让已读位置漂移，也不会重复或漏掉未被修改的项（Linux `simple_offset` 的语义）。

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{borrow::Borrow, cmp::Ordering};

use crate::fallible_tree::{FallibleMap, NodeSlot, OutOfMemory};

/// `.` 与 `..` 占用的 cookie 之后的第一个可分配 cookie。
pub(super) const FIRST_ENTRY_COOKIE: u64 = 3;

struct Name(Box<[u8]>);

/// 目录项名字的共享所有权；名字索引与 cookie 索引各持一份引用，名字只存一份。
struct Key(Arc<Name>);

impl Clone for Key {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Key {
    fn bytes(&self) -> &[u8] {
        &self.0.0
    }
}

impl Borrow<[u8]> for Key {
    fn borrow(&self) -> &[u8] {
        self.bytes()
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.bytes() == other.bytes()
    }
}

impl Eq for Key {}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        self.bytes().cmp(other.bytes())
    }
}

struct Slot<T> {
    cookie: u64,
    value: T,
}

/// 已分配好全部内存、提交时不会失败的目录项。
pub(super) struct Reserved<T> {
    key: Key,
    value: T,
    name_slot: NodeSlot<Key, Slot<T>>,
    order_slot: NodeSlot<u64, Key>,
}

/// 一个目录的全部子项。
pub(super) struct Directory<T> {
    // OWNER: 名字到 (cookie, 值) 的权威映射；`order` 是它的 cookie 次序投影，只经 `insert`/`remove`
    // 一起修改。缺失任一侧会让 lookup 与 readdir 对同一目录给出不同答案。
    names: FallibleMap<Key, Slot<T>>,
    order: FallibleMap<u64, Key>,
    next_cookie: u64,
}

impl<T> Directory<T> {
    pub(super) const fn new() -> Self {
        Self {
            names: FallibleMap::new(),
            order: FallibleMap::new(),
            next_cookie: FIRST_ENTRY_COOKIE,
        }
    }

    pub(super) const fn len(&self) -> usize {
        self.names.len()
    }

    pub(super) fn get(&self, name: &[u8]) -> Option<&T> {
        self.names.get(name).map(|slot| &slot.value)
    }

    /// 预先分配一个目录项所需的全部内存。
    ///
    /// 目录变更先 `reserve` 再修改状态，所以 OOM 只会发生在任何可见变化之前。
    ///
    /// # Errors
    ///
    /// 名字或索引节点分配失败返回 [`OutOfMemory`]。
    pub(super) fn reserve(name: &[u8], value: T) -> Result<Reserved<T>, OutOfMemory> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(name.len())
            .map_err(|_| OutOfMemory)?;
        bytes.extend_from_slice(name);
        let key = Key(Arc::try_new(Name(bytes.into_boxed_slice())).map_err(|_| OutOfMemory)?);
        Ok(Reserved {
            key,
            value,
            name_slot: FallibleMap::<Key, Slot<T>>::try_reserve_node()?,
            order_slot: FallibleMap::<u64, Key>::try_reserve_node()?,
        })
    }

    /// 提交已预留的目录项，分配新 cookie；不会失败。
    ///
    /// # Panics
    ///
    /// 同名项已存在：调用者必须在同一把锁内先检查并移除。
    pub(super) fn insert(&mut self, reserved: Reserved<T>) {
        let Reserved {
            key,
            value,
            name_slot,
            order_slot,
        } = reserved;
        assert!(
            !self.names.contains_key::<[u8]>(key.bytes()),
            "directory entry committed over an existing name"
        );
        let cookie = self.next_cookie;
        self.next_cookie = cookie.checked_add(1).expect("directory cookie exhausted");
        let name_entry = name_slot.fill(key.clone(), Slot { cookie, value });
        self.names.commit_vacant(name_entry);
        self.order.commit_vacant(order_slot.fill(cookie, key));
    }

    /// 删除一项并返回它的值。
    pub(super) fn remove(&mut self, name: &[u8]) -> Option<T> {
        let slot = self.names.remove(name)?;
        self.order
            .remove(&slot.cookie)
            .expect("directory name index and cookie index diverged");
        Some(slot.value)
    }

    /// 取出并返回 cookie 最小的一项；目录拆除时逐项摘下子项，使释放不必递归。
    pub(super) fn pop_first(&mut self) -> Option<T> {
        let key = self.order.first_key_value()?.1.clone();
        self.remove(key.bytes())
    }

    /// 按 cookie 升序遍历 cookie 严格大于 `after` 的项。
    pub(super) fn entries_after(&self, after: u64) -> impl Iterator<Item = (u64, &[u8], &T)> {
        self.order.iter_after(&after).map(|(&cookie, key)| {
            let name = key.bytes();
            let slot = self
                .names
                .get(name)
                .expect("directory cookie index names a missing entry");
            (cookie, name, &slot.value)
        })
    }
}
