//! 持久文件的补页政策；分派与线程池由 fs::pager 唯一拥有。
use super::CachedFile;
use crate::memory::PAGE_SIZE;
use alloc::vec::Vec;
use native_runtime::native_abi::pager as pager_op;
use native_runtime::{Pager, Vmo};

/// 一次存储预读的固定上界，避免让单个请求占用无界缓冲。
const MAX_SUPPLY_PAGES: usize = 32;

impl crate::fs::pager::PageSource for CachedFile {
    fn page_request(&self, pager: &Pager, offset: u64, length: u64, aux: &mut Option<Vmo>) {
        let file = self;
        let vmo = file.object.vmo();
        let mut cursor = offset;
        // 预读：把补页扩展到 filesystem 声明的窗口（FUSE 协商的 max_readahead），不越过 EOF 与对象大小。
        // 已驻留的页由内核保持不变，扩展部分只填补缺页。
        let window_end = offset.saturating_add((file.inode.readahead_pages() * PAGE_SIZE) as u64);
        let limit = file
            .inode
            .size()
            .next_multiple_of(PAGE_SIZE as u64)
            .min(vmo.size().unwrap_or(0));
        let end = (offset + length).max(window_end.min(limit));
        while cursor < end {
            let pages = ((end - cursor) / PAGE_SIZE as u64).min(MAX_SUPPLY_PAGES as u64) as usize;
            let chunk = (pages * PAGE_SIZE) as u64;
            if supply(pager, file, cursor, pages, aux).is_err() {
                pager
                    .op(pager_op::OP_FAIL, vmo, cursor, chunk)
                    .expect("failed storage read must retire its pager request");
            }
            cursor += chunk;
        }
    }
}

/// 读入并补入 `[offset, offset + pages * PAGE_SIZE)`。
fn supply(
    pager: &Pager,
    file: &CachedFile,
    offset: u64,
    pages: usize,
    aux: &mut Option<Vmo>,
) -> Result<(), ()> {
    let length = pages * PAGE_SIZE;
    let size = file.inode.size();
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(length).map_err(|_| ())?;
    buffer.resize(length, 0);
    if offset < size {
        let valid = ((size - offset) as usize).min(length);
        let mut slices: Vec<&mut [u8]> = Vec::new();
        slices.try_reserve_exact(pages).map_err(|_| ())?;
        let mut rest = &mut buffer[..valid];
        while !rest.is_empty() {
            let take = rest.len().min(PAGE_SIZE);
            let (head, tail) = rest.split_at_mut(take);
            slices.push(head);
            rest = tail;
        }
        let read = file
            .inode
            .read_storage_pages(offset, &mut slices)
            .map_err(|_| ())?;
        if read != valid {
            return Err(());
        }
    }
    // 工作池同时服务稀疏内存文件；其上次辅助 VMO 可能只有一页，不能假定容量。
    if aux
        .as_ref()
        .is_none_or(|vmo| vmo.size().unwrap_or(0) < length as u64)
    {
        *aux = Some(Vmo::create((MAX_SUPPLY_PAGES * PAGE_SIZE) as u64, 0).map_err(|_| ())?);
    }
    let aux = aux.as_ref().expect("auxiliary VMO created above");
    aux.write(&buffer, 0).map_err(|_| ())?;
    pager
        .supply_pages(file.object.vmo(), offset, length as u64, aux, 0)
        .map_err(|_| ())
}
