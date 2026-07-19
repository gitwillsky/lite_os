use std::{fs, path::Path};

const COW_SOURCE: &str = "kernel/src/memory/mm/cow.rs";
const ALLOCATOR_SOURCE: &str = "kernel/src/memory/frame_allocator.rs";
const PAGE_SIZE: usize = 4096;
const COW_PAGES: usize = 256;
const HEAP_SLAB_GROWTHS: usize = 256;
const HEAP_DIRECT_GROWTHS: usize = 16;
const HEAP_DIRECT_PAGES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MemoryCopyCost {
    cow_bytes_written: usize,
    heap_dead_zero_bytes: usize,
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(MemoryCopyCost {
            cow_bytes_written,
            heap_dead_zero_bytes,
        }) if cow_bytes_written == COW_PAGES * PAGE_SIZE && heap_dead_zero_bytes == 0 => {}
        Ok(cost) => errors.push(format!(
            "memory full-overwrite paths must not zero before publication; COW P={COW_PAGES}, heap slab/direct={HEAP_SLAB_GROWTHS}/{HEAP_DIRECT_GROWTHS}x{HEAP_DIRECT_PAGES} pages, measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure(root: &Path) -> Result<MemoryCopyCost, String> {
    let cow = read(root, COW_SOURCE)?;
    let allocator = read(root, ALLOCATOR_SOURCE)?;
    let heap = read(root, "kernel/src/memory/heap_allocator.rs")?;
    let cow_bytes_written = if cow.contains("let mut replacement = alloc()")
        && cow.contains("replacement.bytes_mut().copy_from_slice(frame.bytes())")
    {
        COW_PAGES * PAGE_SIZE * 2
    } else if cow.contains("let replacement = alloc_copy(frame.bytes())")
        && allocator.contains("pub(crate) fn alloc_copy(source: &[u8])")
        && allocator.contains("frame.bytes_mut().copy_from_slice(source)")
    {
        COW_PAGES * PAGE_SIZE
    } else {
        return Err(format!(
            "{COW_SOURCE}: COW full-overwrite allocation seam is not recognized"
        ));
    };
    let heap_dead_zero_bytes = if heap.matches("frame_allocator::alloc_heap_extent(").count() == 2
        && !heap.contains("frame_allocator::alloc_contiguous(")
        && allocator.contains("fn alloc_contiguous_uninitialized(")
    {
        0
    } else {
        (HEAP_SLAB_GROWTHS + HEAP_DIRECT_GROWTHS * HEAP_DIRECT_PAGES) * PAGE_SIZE
    };
    Ok(MemoryCopyCost {
        cow_bytes_written,
        heap_dead_zero_bytes,
    })
}

fn read(root: &Path, relative: &str) -> Result<String, String> {
    fs::read_to_string(root.join(relative)).map_err(|error| format!("{relative}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cow_full_page_copy_writes_each_byte_once() {
        let root = super::super::repository_root();
        let cost = measure(&root).expect("production COW cost must be measurable");
        assert_eq!(cost.cow_bytes_written, COW_PAGES * PAGE_SIZE);
        assert_eq!(cost.heap_dead_zero_bytes, 0, "measured {cost:?}");
    }
}
