//! 稀疏页存储的洞、短写、截断与预分配语义。

use crate::memory_sparse::{PAGE_SIZE, PageBytes, SparsePages, Stop};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
    vec,
    vec::Vec,
};

struct Page(Mutex<Vec<u8>>);

impl PageBytes for Page {
    fn read(&self, offset: usize, output: &mut [u8]) {
        output.copy_from_slice(&self.0.lock().unwrap()[offset..offset + output.len()]);
    }

    fn write(&self, offset: usize, input: &[u8]) {
        self.0.lock().unwrap()[offset..offset + input.len()].copy_from_slice(input);
    }

    fn zero_from(&self, offset: usize) {
        self.0.lock().unwrap()[offset..].fill(0);
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
        Ok(Arc::new(Page(Mutex::new(vec![0; PAGE_SIZE]))))
    }
}

#[test]
fn holes_read_zero_and_do_not_allocate() {
    let budget = Rc::new(RefCell::new(8));
    let mut pages = SparsePages::<Page>::new();
    let (written, stop) = pages.write(3 * PAGE_SIZE as u64, b"tail", allocator(budget));
    assert_eq!((written, stop), (4, None));
    assert_eq!(pages.len(), 3 * PAGE_SIZE as u64 + 4);
    assert_eq!(pages.resident_pages(), 1);
    let mut output = vec![0xffu8; 2 * PAGE_SIZE];
    assert_eq!(pages.read(PAGE_SIZE as u64, &mut output), output.len());
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
    pages.truncate(100);
    assert_eq!(pages.resident_pages(), 1);
    pages.truncate(PAGE_SIZE as u64);
    let mut output = vec![1u8; PAGE_SIZE];
    assert_eq!(pages.read(0, &mut output), PAGE_SIZE);
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
    assert_eq!(pages.resident_pages(), 3);

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
    pages.truncate(2 * PAGE_SIZE as u64);
    assert!(pages.page(2, allocator(budget.clone())).is_none());
    assert!(pages.page(1, allocator(budget)).unwrap().is_ok());
    assert_eq!(pages.resident_pages(), 1);
}
