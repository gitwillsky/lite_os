use crate::fallible_tree::{FallibleMap, VacantEntry};

/// DRM identity reservation 的稳定失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReservationError {
    /// rollback token 的唯一节点无法预留。
    OutOfMemory,
    /// monotonic identity 空间已经耗尽且没有可复用的未发布 identity。
    NoSpace,
}

/// DRM allocator 支持的整数 identity mechanism。
pub(super) trait Identity: Copy + Ord {
    fn checked_next(self) -> Option<Self>;
}

impl Identity for u32 {
    fn checked_next(self) -> Option<Self> {
        self.checked_add(1)
    }
}

impl Identity for u64 {
    fn checked_next(self) -> Option<Self> {
        self.checked_add(1)
    }
}

/// 尚未发布的 DRM identity 与无分配 rollback storage。
pub(super) struct UnpublishedId<T> {
    id: T,
    rollback: VacantEntry<T, ()>,
}

impl<T: Copy> UnpublishedId<T> {
    /// 返回本 transaction 独占、尚未发布的 identity。
    pub(super) fn id(&self) -> T {
        self.id
    }
}

/// 只复用 publication 前失败 identity 的 DRM namespace allocator。
///
/// published identity 永不回收；失败 identity 的节点在 reserve 时已存在，所以 copyout
/// failure 的 rollback 不分配且不会因并发 reservation 顺序留下永久空洞。
pub(super) struct IdAllocator<T> {
    next: T,
    reusable: FallibleMap<T, ()>,
}

impl<T: Identity> IdAllocator<T> {
    /// 从第一个可签发 identity 构造空 allocator。
    pub(super) const fn new(first: T) -> Self {
        Self {
            next: first,
            reusable: FallibleMap::new(),
        }
    }

    /// 预留唯一 identity 及其无分配 rollback storage。
    ///
    /// @return 优先返回最小 reusable identity，否则签发 monotonic identity。
    /// @errors rollback node OOM 返回 OutOfMemory；identity 耗尽返回 NoSpace。
    pub(super) fn reserve(&mut self) -> Result<UnpublishedId<T>, ReservationError> {
        if let Some((&id, ())) = self.reusable.first_key_value() {
            let rollback = self
                .reusable
                .take_entry(&id)
                .expect("reusable DRM identity disappeared under owner lock");
            return Ok(UnpublishedId { id, rollback });
        }

        let next = self.next.checked_next().ok_or(ReservationError::NoSpace)?;
        let id = self.next;
        let rollback =
            FallibleMap::try_prepare(id, ()).map_err(|_| ReservationError::OutOfMemory)?;
        self.next = next;
        Ok(UnpublishedId { id, rollback })
    }

    /// 标记 identity 已由 namespace publication 接管，永不再复用。
    pub(super) fn publish(&mut self, reservation: UnpublishedId<T>) {
        drop(reservation);
    }

    /// 无分配回收尚未发布的 identity，供任意后续 reservation 复用。
    pub(super) fn rollback(&mut self, reservation: UnpublishedId<T>) {
        self.reusable.commit_vacant(reservation.rollback);
    }
}

/// @description 保证 prepared DRM object 只在完整 UAPI copyout 成功后发布。
/// @param prepared 已预留 identity、owner storage 与 backing 的领域 transaction。
/// @param copyout 完成 ioctl output structure 用户输出。
/// @param publish 消费 transaction 的无失败 namespace publication。
/// @return copyout 与 publication 均成功。
/// @errors 原样转发 copyout 错误；错误路径先析构 transaction 并触发资源回收。
pub(super) fn after_copyout<T, E>(
    prepared: T,
    copyout: impl FnOnce(&T) -> Result<(), E>,
    publish: impl FnOnce(T),
) -> Result<(), E> {
    copyout(&prepared)?;
    publish(prepared);
    Ok(())
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::{IdAllocator, after_copyout};

    #[test]
    fn arbitrary_rollback_order_reuses_every_u32_identity() {
        for reverse in [false, true] {
            let mut allocator = IdAllocator::new(4u32);
            let first = allocator.reserve().unwrap();
            let second = allocator.reserve().unwrap();
            assert_eq!((first.id(), second.id()), (4, 5));

            if reverse {
                allocator.rollback(second);
                allocator.rollback(first);
            } else {
                allocator.rollback(first);
                allocator.rollback(second);
            }

            let reused_first = allocator.reserve().unwrap();
            let reused_second = allocator.reserve().unwrap();
            assert_eq!((reused_first.id(), reused_second.id()), (4, 5));
        }
    }

    #[test]
    fn rollback_never_reuses_concurrently_published_identity() {
        let mut allocator = IdAllocator::new(4u32);
        let failed = allocator.reserve().unwrap();
        let published = allocator.reserve().unwrap();
        allocator.rollback(failed);
        allocator.publish(published);

        let reused = allocator.reserve().unwrap();
        let fresh = allocator.reserve().unwrap();
        assert_eq!((reused.id(), fresh.id()), (4, 6));
    }

    #[test]
    fn arbitrary_rollback_order_reuses_every_u64_identity() {
        let mut allocator = IdAllocator::new(1u64);
        let first = allocator.reserve().unwrap();
        let second = allocator.reserve().unwrap();
        allocator.rollback(first);
        allocator.rollback(second);

        assert_eq!(allocator.reserve().unwrap().id(), 1);
        assert_eq!(allocator.reserve().unwrap().id(), 2);
    }

    #[test]
    fn exhausted_fresh_space_still_reuses_rolled_back_identity() {
        let mut allocator = IdAllocator::new(u32::MAX - 1);
        let rollback = allocator.reserve().unwrap();
        assert_eq!(rollback.id(), u32::MAX - 1);
        assert!(matches!(
            allocator.reserve(),
            Err(super::ReservationError::NoSpace)
        ));

        allocator.rollback(rollback);
        assert_eq!(allocator.reserve().unwrap().id(), u32::MAX - 1);
    }

    #[test]
    fn copyout_failure_does_not_publish_prepared_value() {
        let published = Cell::new(false);
        let result = after_copyout((), |_| Err::<(), _>(()), |_| published.set(true));

        assert_eq!(result, Err(()));
        assert!(!published.get());
    }

    #[test]
    fn successful_copyout_publishes_exactly_once() {
        let publications = Cell::new(0);
        after_copyout(
            (),
            |_| Ok::<(), ()>(()),
            |_| publications.set(publications.get() + 1),
        )
        .unwrap();

        assert_eq!(publications.get(), 1);
    }
}
