use std::{fs, path::Path};

const COW_SOURCE: &str = "kernel/src/memory/mm/private_fault.rs";
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
    let page = read(root, "kernel/src/memory/swappable_page.rs")?;
    let shared = read(root, "kernel/src/memory/shared_file.rs")?;
    let cow_bytes_written = cow_copy_cost(&cow, &page, &shared, &allocator)?;
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

fn cow_copy_cost(cow: &str, page: &str, shared: &str, allocator: &str) -> Result<usize, String> {
    // Follow the production canonical pin -> SharedFrame -> allocator chain. A missing hop,
    // zeroed-frame substitution or extra copy must fail rather than assume the old direct seam.
    if cow.contains("let replacement = pin.copy_frame().ok_or(MemoryError::OutOfMemory)?;")
        && page.contains("{\n        self.frame.copy_frame()\n    }")
        && shared.contains("{\n        frame_allocator::alloc_copy(self.frame.bytes())\n    }")
        && allocator.contains("pub(crate) fn alloc_copy(source: &[u8])")
        && allocator.contains("frame.bytes_mut().copy_from_slice(source)")
    {
        Ok(COW_PAGES * PAGE_SIZE)
    } else {
        Err(format!(
            "{COW_SOURCE}: COW full-overwrite allocation seam is not recognized"
        ))
    }
}

fn read(root: &Path, relative: &str) -> Result<String, String> {
    fs::read_to_string(root.join(relative)).map_err(|error| format!("{relative}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zeroed_or_missing_canonical_cow_copy_hop() {
        let root = super::super::repository_root();
        let cow = read(&root, COW_SOURCE).unwrap();
        let page = read(&root, "kernel/src/memory/swappable_page.rs").unwrap();
        let shared = read(&root, "kernel/src/memory/shared_file.rs").unwrap();
        let allocator = read(&root, ALLOCATOR_SOURCE).unwrap();
        assert!(
            cow_copy_cost(
                &cow.replace("pin.copy_frame()", "alloc()"),
                &page,
                &shared,
                &allocator
            )
            .is_err()
        );
        assert!(
            cow_copy_cost(
                &cow,
                &page.replace("self.frame.copy_frame()", "SharedFrame::allocate().ok()"),
                &shared,
                &allocator
            )
            .is_err()
        );
        assert!(
            cow_copy_cost(
                &cow,
                &page,
                &shared.replace(
                    "frame_allocator::alloc_copy(self.frame.bytes())",
                    "frame_allocator::alloc()"
                ),
                &allocator
            )
            .is_err()
        );
    }

    #[test]
    fn cow_full_page_copy_writes_each_byte_once() {
        let root = super::super::repository_root();
        let cost = measure(&root).expect("production COW cost must be measurable");
        assert_eq!(cost.cow_bytes_written, COW_PAGES * PAGE_SIZE);
        assert_eq!(cost.heap_dead_zero_bytes, 0, "measured {cost:?}");
    }
}
