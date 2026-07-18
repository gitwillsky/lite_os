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

/// @description 回收尚未发布且仍位于 allocator 尾部的 u32 identity。
/// @param next allocator 当前下一个 identity。
/// @param reserved 本 transaction 预留的 identity。
pub(super) fn rollback_latest_u32(next: &mut u32, reserved: u32) {
    if reserved
        .checked_add(1)
        .is_some_and(|expected| *next == expected)
    {
        *next = reserved;
    }
}

/// @description 回收尚未发布且仍位于 allocator 尾部的 u64 identity。
/// @param next allocator 当前下一个 identity。
/// @param reserved 本 transaction 预留的 identity。
pub(super) fn rollback_latest_u64(next: &mut u64, reserved: u64) {
    if reserved
        .checked_add(1)
        .is_some_and(|expected| *next == expected)
    {
        *next = reserved;
    }
}

#[cfg(test)]
mod tests {
    use alloc::{rc::Rc, vec, vec::Vec};
    use core::cell::RefCell;

    use super::{after_copyout, rollback_latest_u32, rollback_latest_u64};

    #[derive(Default)]
    struct FakeNamespace {
        next_id: u32,
        allocated: usize,
        published: Vec<u32>,
    }

    struct FakePrepared {
        namespace: Rc<RefCell<FakeNamespace>>,
        ids: Vec<u32>,
    }

    impl FakePrepared {
        fn new(namespace: Rc<RefCell<FakeNamespace>>) -> Self {
            let id = {
                let mut namespace = namespace.borrow_mut();
                let id = namespace.next_id;
                namespace.next_id += 1;
                namespace.allocated += 1;
                id
            };
            Self {
                namespace,
                ids: vec![id],
            }
        }

        fn id(&self) -> u32 {
            self.ids[0]
        }

        fn publish(mut self) {
            let id = self.ids.pop().unwrap();
            self.namespace.borrow_mut().published.push(id);
        }
    }

    impl Drop for FakePrepared {
        fn drop(&mut self) {
            let Some(id) = self.ids.pop() else {
                return;
            };
            let mut namespace = self.namespace.borrow_mut();
            namespace.allocated -= 1;
            rollback_latest_u32(&mut namespace.next_id, id);
        }
    }

    #[test]
    fn copyout_fault_releases_object_and_immediately_reuses_id() {
        let namespace = Rc::new(RefCell::new(FakeNamespace {
            next_id: 4,
            ..FakeNamespace::default()
        }));
        let prepared = FakePrepared::new(namespace.clone());
        assert_eq!(prepared.id(), 4);

        let result = after_copyout(prepared, |_| Err::<(), _>(()), FakePrepared::publish);
        assert_eq!(result, Err(()));
        assert_eq!(namespace.borrow().allocated, 0);
        assert_eq!(namespace.borrow().next_id, 4);
        assert!(namespace.borrow().published.is_empty());

        let reused = FakePrepared::new(namespace.clone());
        assert_eq!(reused.id(), 4);
    }

    #[test]
    fn successful_copyout_publishes_exactly_once() {
        let namespace = Rc::new(RefCell::new(FakeNamespace {
            next_id: 1,
            ..FakeNamespace::default()
        }));
        let prepared = FakePrepared::new(namespace.clone());
        after_copyout(prepared, |_| Ok::<(), ()>(()), FakePrepared::publish).unwrap();

        let namespace = namespace.borrow();
        assert_eq!(namespace.allocated, 1);
        assert_eq!(namespace.published, [1]);
        assert_eq!(namespace.next_id, 2);

        let mut next_identity = 8u64;
        rollback_latest_u64(&mut next_identity, 7);
        assert_eq!(next_identity, 7);
    }
}
