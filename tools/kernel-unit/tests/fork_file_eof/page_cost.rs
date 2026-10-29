// Diagnostic CPU measurements of production Page/registry; physical allocation and TaskMutex use host seams.
#[test]
#[ignore = "diagnostic CPU measurement; run alone in release"]
fn canonical_page_pin_and_bounded_registry_cpu_cost() {
    use std::{hint::black_box, time::Instant};
    let _serial = SWAP_TEST.lock().unwrap();
    let page = crate::memory::SwappablePage::new(
        crate::memory::SharedFrame::from_frame(alloc().unwrap()),
        true,
    )
    .unwrap();
    for _ in 0..100_000 {
        drop(black_box(page.pin().unwrap()));
    }
    let mut samples = std::array::from_fn::<_, 5, _>(|_| {
        let start = Instant::now();
        for _ in 0..100_000 {
            drop(black_box(page.pin().unwrap()));
        }
        start.elapsed()
    });
    samples.sort_unstable();
    println!(
        "canonical Page resident pin/drop: {:.2} ns/op (host TaskMutex seam)",
        samples[2].as_nanos() as f64 / 100_000.0
    );
    for baseline in [512, 4096] {
        let retained: Vec<_> = (0..baseline)
            .map(|_| crate::memory::SwappablePage::allocate_shared().unwrap())
            .collect();
        let mut samples = [std::time::Duration::ZERO; 5];
        for sample in 0..=5 {
            // Prepare physical input outside measured owner/node publication; keep all baseline
            // pages live so bounded tombstone probing cannot accidentally measure an empty tree.
            let frames: Vec<_> = (0..512).map(|_| alloc().unwrap()).collect();
            let mut pages = Vec::with_capacity(512);
            let start = Instant::now();
            for frame in frames {
                pages.push(
                    crate::memory::SwappablePage::new(
                        crate::memory::SharedFrame::from_frame(frame),
                        true,
                    )
                    .unwrap(),
                );
            }
            let elapsed = start.elapsed();
            if sample != 0 {
                samples[sample - 1] = elapsed;
            }
            black_box(&pages);
            drop(pages);
        }
        samples.sort_unstable();
        println!(
            "canonical Page/node publication, {baseline} live pages: {:.2} ns/page",
            samples[2].as_nanos() as f64 / 512.0
        );
        drop(retained);
    }
}

#[test]
#[ignore = "diagnostic CPU measurement; run alone in release"]
fn canonical_alias_revoke_and_unuse_cpu_cost() {
    use crate::memory::swap;
    use std::{hint::black_box, time::Instant};
    let _serial = SWAP_TEST.lock().unwrap();
    for aliases in [1, 64] {
        let disk = SwapDisk::new(9);
        swap::activate(disk, b"/dev/test", 0).unwrap();
        let parent = Arc::new(SwapMemory::new(swap_memory(false, 1)));
        shared_file::register_memory_mapping_owner(parent.clone()).unwrap();
        let mut owners = vec![parent.clone()];
        for _ in 1..aliases {
            let child = Arc::new(SwapMemory::new(MemorySet::empty()));
            shared_file::register_memory_mapping_owner(child.clone()).unwrap();
            parent
                .memory
                .lock()
                .unwrap()
                .try_clone_for_fork(&mut child.memory.lock().unwrap())
                .unwrap();
            shared_file::complete_memory_mapping_owner_initialization();
            owners.push(child);
        }
        let vpn = VirtualPageNumber::from_vpn(0x5_0401);
        let page = parent
            .memory
            .lock()
            .unwrap()
            .areas
            .first_key_value()
            .unwrap()
            .1
            .data_frames
            .get(&vpn)
            .unwrap()
            .mapping()
            .unwrap()
            .page()
            .clone();
        let mut samples = [std::time::Duration::ZERO; 5];
        for sample in 0..=5 {
            let start = Instant::now();
            for _ in 0..100 {
                assert!(black_box(&page).swap_out());
                for owner in &owners {
                    owner
                        .memory
                        .lock()
                        .unwrap()
                        .restore_swapped_pages(SharedFileId {
                            filesystem: 999,
                            inode: 1,
                        })
                        .unwrap();
                }
            }
            if sample != 0 {
                samples[sample - 1] = start.elapsed();
            }
        }
        samples.sort_unstable();
        println!(
            "canonical revoke/write/read/unuse with {aliases} actual MM aliases: {:.2} ns/round (host IO/fence/TaskMutex seams)",
            samples[2].as_nanos() as f64 / 100.0
        );
        let mut wait = crate::execution::sync::TaskMutexWaitPreparation;
        swap::deactivate(
            SharedFileId {
                filesystem: 999,
                inode: 1,
            },
            &mut wait,
        )
        .unwrap();
    }
}
