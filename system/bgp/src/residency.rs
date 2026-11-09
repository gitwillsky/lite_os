//! BGP 规范内容的唯一驻留机制；路径与完整属性使用不同的类型化实例，不复制引用计数/回收协议。
use alloc::rc::{Rc, Weak};
use core::{cell::RefCell, cmp::Ordering, fmt};
use fallible_tree::{FallibleMap, OutOfMemory};

struct Index<T: Ord> {
    // OWNER: 一个 actor 的不可变规范内容索引；AVL 给不可信输入确定的比较次数上界。
    // key 保活规范值，value 只弱引用句柄；Resident 弱引用 Index，最后引用立即删除，无扫描和引用环。
    entries: FallibleMap<Rc<T>, Weak<Resident<T>>>,
}
struct Resident<T: Ord> {
    value: Rc<T>,
    owner: Weak<RefCell<Index<T>>>,
}
impl<T: Ord> Drop for Resident<T> {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.borrow_mut().entries.remove(self.value.as_ref());
        }
    }
}
/// actor 私有的类型化驻留表；所有借用不跨分配、外部回调或 Resident 析构。
pub(crate) struct Pool<T: Ord>(Rc<RefCell<Index<T>>>);
impl<T: Ord> Pool<T> {
    pub(crate) fn try_new() -> Result<Self, OutOfMemory> {
        Rc::try_new(RefCell::new(Index {
            entries: FallibleMap::new(),
        }))
        .map(Self)
        .map_err(|_| OutOfMemory)
    }
    pub(crate) fn len(&self) -> usize {
        self.0.borrow().entries.len()
    }
    pub(crate) fn intern(&mut self, value: T) -> Result<Shared<T>, OutOfMemory> {
        // 1. 借用结束后才返回句柄；lookup 到 commit 是不调用用户代码的同线程同步流程。
        let existing = self
            .0
            .borrow()
            .entries
            .get(&value)
            .map(|value| value.upgrade().expect("indexed value has a live reference"));
        if let Some(existing) = existing {
            return Ok(Shared(existing));
        }
        // 2. 所有分配在借用外完成；未发表 Resident 析构只会移除不存在的 key。
        let value = Rc::try_new(value).map_err(|_| OutOfMemory)?;
        let resident = Rc::try_new(Resident {
            value: value.clone(),
            owner: Rc::downgrade(&self.0),
        })
        .map_err(|_| OutOfMemory)?;
        let prepared = FallibleMap::try_prepare(value, Rc::downgrade(&resident))?;
        // 3. commit 不分配、不执行回调；key 的规范值不含该池自身句柄，删除不会回借同一个 RefCell。
        self.0.borrow_mut().entries.commit_vacant(prepared);
        Ok(Shared(resident))
    }
}
/// 不可变规范内容的一指针引用；池退出后仍保活内容，不把池生命周期绑到路由上。
pub(crate) struct Shared<T: Ord>(Rc<Resident<T>>);
impl<T: Ord> Shared<T> {
    pub(crate) fn get(&self) -> &T {
        &self.0.value
    }
}
impl<T: Ord> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T: Ord> PartialEq for Shared<T> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl<T: Ord> Eq for Shared<T> {}
impl<T: Ord> PartialOrd for Shared<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<T: Ord> Ord for Shared<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        if Rc::ptr_eq(&self.0, &other.0) {
            Ordering::Equal
        } else {
            self.get().cmp(other.get())
        }
    }
}
impl<T: Ord + fmt::Debug> fmt::Debug for Shared<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(formatter)
    }
}
