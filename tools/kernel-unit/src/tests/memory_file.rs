//! 稀疏页存储的洞、短写、截断与预分配语义。

use crate::memory_sparse::{PAGE_SIZE, PageBytes, SparsePages, Stop};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    vec,
    vec::Vec,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccessError {
    Io,
}
struct Page(Mutex<Vec<u8>>, AtomicBool);

impl PageBytes for Page {
    type Error = AccessError;
    fn read(&self, offset: usize, output: &mut [u8]) -> Result<(), Self::Error> {
        if self.1.load(Ordering::Relaxed) {
            return Err(AccessError::Io);
        }
        output.copy_from_slice(&self.0.lock().unwrap()[offset..offset + output.len()]);
        Ok(())
    }

    fn write(&self, offset: usize, input: &[u8]) -> Result<(), Self::Error> {
        if self.1.load(Ordering::Relaxed) {
            return Err(AccessError::Io);
        }
        self.0.lock().unwrap()[offset..offset + input.len()].copy_from_slice(input);
        Ok(())
    }

    fn zero_from(&self, offset: usize) -> Result<(), Self::Error> {
        if self.1.load(Ordering::Relaxed) {
            return Err(AccessError::Io);
        }
        self.0.lock().unwrap()[offset..].fill(0);
        Ok(())
    }
}

/// 最多再发放 `budget` 个页的分配器，用来模拟配额耗尽。
fn allocator(budget: Rc<RefCell<usize>>) -> impl FnMut() -> Result<Arc<Page>, ()> {
    move || {
        let mut left = budget.borrow_mut();
        if *left == 0 {
            return Err(());
        }
        *left -= 1;
        Ok(Arc::new(Page(
            Mutex::new(vec![0; PAGE_SIZE]),
            AtomicBool::new(false),
        )))
    }
}

#[test]
fn holes_read_zero_and_do_not_allocate() {
    let budget = Rc::new(RefCell::new(8));
    let mut pages = SparsePages::<Page>::new();
    let (written, stop) = pages.write(3 * PAGE_SIZE as u64, b"tail", allocator(budget));
    assert_eq!((written, stop), (4, None));
    assert_eq!(pages.len(), 3 * PAGE_SIZE as u64 + 4);
    assert_eq!(pages.allocated_pages(), 1);
    let (index, owner) = pages.next_page_from(0).unwrap();
    assert_eq!(index, 3);
    assert!(pages.next_page_from(4).is_none());
    assert!(Arc::ptr_eq(
        &owner,
        &pages.page::<()>(3, || unreachable!()).unwrap().unwrap()
    ));
    let mut output = vec![0xffu8; 2 * PAGE_SIZE];
    assert_eq!(
        pages.read(PAGE_SIZE as u64, &mut output),
        (output.len(), None)
    );
    assert!(output.iter().all(|&byte| byte == 0));
}

#[test]
fn quota_exhaustion_is_a_short_write_with_a_reason() {
    let budget = Rc::new(RefCell::new(1));
    let mut pages = SparsePages::<Page>::new();
    let input = vec![7u8; 2 * PAGE_SIZE];
    let (written, stop) = pages.write(0, &input, allocator(budget));
    assert_eq!(written, PAGE_SIZE);
    assert_eq!(stop, Some(Stop::Allocator(())));
    assert_eq!(pages.len(), PAGE_SIZE as u64);
}

#[test]
fn truncate_zeroes_the_kept_tail_before_growing_again() {
    let budget = Rc::new(RefCell::new(4));
    let mut pages = SparsePages::<Page>::new();
    pages.write(0, &vec![9u8; 2 * PAGE_SIZE], allocator(budget));
    pages.truncate(100).unwrap();
    assert_eq!(pages.allocated_pages(), 1);
    pages.truncate(PAGE_SIZE as u64).unwrap();
    let mut output = vec![1u8; PAGE_SIZE];
    assert_eq!(pages.read(0, &mut output), (PAGE_SIZE, None));
    assert!(output[..100].iter().all(|&byte| byte == 9));
    assert!(output[100..].iter().all(|&byte| byte == 0));
}

#[test]
fn allocate_range_fills_holes_and_extends_length() {
    let budget = Rc::new(RefCell::new(8));
    let mut pages = SparsePages::<Page>::new();
    pages
        .allocate_range(10, 2 * PAGE_SIZE as u64, allocator(budget.clone()))
        .unwrap();
    assert_eq!(pages.len(), 10 + 2 * PAGE_SIZE as u64);
    assert_eq!(pages.allocated_pages(), 3);

    let mut starved = SparsePages::<Page>::new();
    let result =
        starved.allocate_range(0, 2 * PAGE_SIZE as u64, allocator(Rc::new(RefCell::new(1))));
    assert_eq!(result, Err(Stop::Allocator(())));
    assert_eq!(starved.len(), 0);
}

#[test]
fn page_beyond_eof_is_none_and_hole_page_is_allocated() {
    let budget = Rc::new(RefCell::new(4));
    let mut pages = SparsePages::<Page>::new();
    pages.truncate(2 * PAGE_SIZE as u64).unwrap();
    assert!(pages.page(2, allocator(budget.clone())).is_none());
    assert!(pages.page(1, allocator(budget)).unwrap().is_ok());
    assert_eq!(pages.allocated_pages(), 1);
}

#[test]
fn page_access_failure_preserves_short_read_and_write_boundaries() {
    let budget = Rc::new(RefCell::new(2));
    let mut pages = SparsePages::<Page>::new();
    assert_eq!(
        pages.write(0, &vec![7; 2 * PAGE_SIZE], allocator(budget.clone())),
        (2 * PAGE_SIZE, None)
    );
    let second = pages.page(1, allocator(budget)).unwrap().unwrap();
    second.1.store(true, Ordering::Relaxed);
    let mut output = vec![99; 2 * PAGE_SIZE];
    assert_eq!(
        pages.read(0, &mut output),
        (PAGE_SIZE, Some(AccessError::Io))
    );
    assert!(output[..PAGE_SIZE].iter().all(|&byte| byte == 7));
    assert!(output[PAGE_SIZE..].iter().all(|&byte| byte == 99));
    assert_eq!(
        pages.write(0, &vec![9; 2 * PAGE_SIZE], || panic!("existing pages")),
        (PAGE_SIZE, Some(Stop::<(), _>::Access(AccessError::Io)))
    );
    assert_eq!(pages.len(), 2 * PAGE_SIZE as u64);
    second.1.store(false, Ordering::Relaxed);
    assert_eq!(pages.read(0, &mut output), (2 * PAGE_SIZE, None));
    assert!(output[..PAGE_SIZE].iter().all(|&byte| byte == 9));
    assert!(output[PAGE_SIZE..].iter().all(|&byte| byte == 7));
}

#[test]
fn failed_tail_restore_keeps_file_length_all_pages_and_contents() {
    let budget = Rc::new(RefCell::new(2));
    let mut pages = SparsePages::<Page>::new();
    pages.write(0, &vec![7; 2 * PAGE_SIZE], allocator(budget.clone()));
    let first = pages.page(0, allocator(budget)).unwrap().unwrap();
    first.1.store(true, Ordering::Relaxed);
    assert_eq!(pages.truncate(100), Err(AccessError::Io));
    assert_eq!(pages.len(), 2 * PAGE_SIZE as u64);
    assert_eq!(pages.allocated_pages(), 2);
    let mut output = vec![99; 2 * PAGE_SIZE];
    assert_eq!(pages.read(0, &mut output), (0, Some(AccessError::Io)));
    assert!(output.iter().all(|&byte| byte == 99));
    first.1.store(false, Ordering::Relaxed);
    assert_eq!(pages.read(0, &mut output), (2 * PAGE_SIZE, None));
    assert!(output.iter().all(|&byte| byte == 7));
    pages.truncate(100).unwrap();
    pages.truncate(PAGE_SIZE as u64).unwrap();
    assert_eq!(pages.allocated_pages(), 1);
    assert_eq!(pages.read(0, &mut output), (PAGE_SIZE, None));
    assert!(output[..100].iter().all(|&byte| byte == 7));
    assert!(output[100..PAGE_SIZE].iter().all(|&byte| byte == 0));
}
