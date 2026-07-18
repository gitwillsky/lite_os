/// @description 保证 prepared capabilities 只在全部 recvmsg copyout 成功后发布。
/// @param transaction 可选的领域专用 prepared transaction。
/// @param copyout 执行 name、control 与 msghdr metadata 的完整用户输出。
/// @param publish 消费成功 transaction 的无失败 publication 操作。
/// @return copyout 结果；错误路径先析构 transaction 并触发其 rollback。
/// @errors 原样转发 copyout 错误；publication 本身不得失败。
pub(super) fn after_copyout<T, E>(
    transaction: Option<T>,
    copyout: impl FnOnce(Option<&T>) -> Result<(), E>,
    publish: impl FnOnce(T),
) -> Result<(), E> {
    copyout(transaction.as_ref())?;
    if let Some(transaction) = transaction {
        publish(transaction);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::{rc::Rc, vec, vec::Vec};
    use core::cell::RefCell;

    use super::after_copyout;

    struct FakeTable {
        occupied: Vec<bool>,
        published: Vec<usize>,
    }

    impl FakeTable {
        fn lowest_free(&self) -> usize {
            self.occupied
                .iter()
                .position(|occupied| !occupied)
                .unwrap_or(self.occupied.len())
        }
    }

    struct FakeTransaction {
        table: Rc<RefCell<FakeTable>>,
        descriptors: Vec<usize>,
    }

    impl FakeTransaction {
        fn publish(mut self) {
            let mut table = self.table.borrow_mut();
            table.published.extend_from_slice(&self.descriptors);
            self.descriptors.clear();
        }
    }

    impl Drop for FakeTransaction {
        fn drop(&mut self) {
            let mut table = self.table.borrow_mut();
            for descriptor in self.descriptors.drain(..) {
                table.occupied[descriptor] = false;
            }
        }
    }

    fn prepared() -> (Rc<RefCell<FakeTable>>, FakeTransaction) {
        let table = Rc::new(RefCell::new(FakeTable {
            occupied: vec![true, true, true, true, true, false],
            published: Vec::new(),
        }));
        (
            table.clone(),
            FakeTransaction {
                table,
                descriptors: vec![3, 4],
            },
        )
    }

    fn fail_at(stage: usize) -> impl FnMut() -> Result<(), ()> {
        let mut current = 0;
        move || {
            current += 1;
            if current == stage { Err(()) } else { Ok(()) }
        }
    }

    #[test]
    fn cmsg_header_copyout_failure_rolls_back_to_lowest_fd() {
        let (table, transaction) = prepared();
        let mut copy = fail_at(2);
        let result = after_copyout(
            Some(transaction),
            |transaction| {
                assert_eq!(transaction.unwrap().descriptors, [3, 4]);
                copy()?; // fd numbers
                copy()?; // cmsghdr
                Ok(())
            },
            FakeTransaction::publish,
        );
        assert_eq!(result, Err(()));
        assert_eq!(table.borrow().lowest_free(), 3);
        assert!(table.borrow().published.is_empty());
    }

    #[test]
    fn msghdr_metadata_copyout_failure_rolls_back_to_lowest_fd() {
        let (table, transaction) = prepared();
        let mut copy = fail_at(4);
        let result = after_copyout(
            Some(transaction),
            |_| {
                copy()?; // fd numbers
                copy()?; // cmsghdr
                copy()?; // msg_controllen
                copy()?; // msg_flags
                Ok(())
            },
            FakeTransaction::publish,
        );
        assert_eq!(result, Err(()));
        assert_eq!(table.borrow().lowest_free(), 3);
        assert!(table.borrow().published.is_empty());
    }

    #[test]
    fn successful_copyout_publishes_the_complete_ordered_batch() {
        let (table, transaction) = prepared();
        after_copyout(
            Some(transaction),
            |transaction| {
                assert_eq!(transaction.unwrap().descriptors, [3, 4]);
                Ok::<(), ()>(())
            },
            FakeTransaction::publish,
        )
        .unwrap();

        let table = table.borrow();
        assert_eq!(table.published, [3, 4]);
        assert_eq!(table.lowest_free(), 5);
    }
}
