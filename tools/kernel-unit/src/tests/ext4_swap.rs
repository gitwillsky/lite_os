//! Actual ext4 inode freeze, storage mutation, journal drain, extent export and lease retirement.
use super::*;
use crate::{
    devices::block::BLOCK_SIZE,
    ext4_cost_tests::{COST_TEST_LOCK, mounted},
};
use std::sync::atomic::Ordering;

const FILE: CreateMetadata = CreateMetadata {
    mode: 0o600,
    uid: 0,
    gid: 0,
};

#[test]
fn swapfile_freezes_all_hardlinks_drains_old_journal_and_retires_once() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, fs) = mounted();
    let root = fs.root_inode().unwrap();
    let file = root.create(b"swap", InodeType::File, FILE).unwrap();
    file.allocate_storage(0, 8 * BLOCK_SIZE as u64, None)
        .unwrap();
    file.write_storage(0, &[0x51; BLOCK_SIZE], None).unwrap();
    root.link(b"alias", file.clone()).unwrap();
    let alias = root.find_child(b"alias").unwrap();
    image.reset_writes();
    let lease = file.clone().pin_swap_file().unwrap();
    assert!(alias.is_swap_file());
    assert!(matches!(
        alias.clone().pin_swap_file(),
        Err(FileSystemError::Busy)
    ));
    assert_eq!(
        alias.write_storage(0, &[1], None),
        Err(FileSystemError::TextBusy)
    );
    assert_eq!(
        file.append_storage(&[1], None),
        Err(FileSystemError::TextBusy)
    );
    assert_eq!(
        file.truncate_storage(0, None),
        Err(FileSystemError::TextBusy)
    );
    assert_eq!(
        file.allocate_storage(0, BLOCK_SIZE as u64, None),
        Err(FileSystemError::TextBusy)
    );
    assert_eq!(
        root.unlink(b"alias", false),
        Err(FileSystemError::PermissionDenied)
    );
    assert_eq!(
        root.rename(b"swap", root.metadata().unwrap().inode, b"new", false),
        Err(FileSystemError::PermissionDenied)
    );
    let layout = lease.activate().unwrap();
    assert_eq!(layout.size_bytes, 8 * BLOCK_SIZE as u64);
    assert!(
        image.writes() > 0 && image.flushes() > 0,
        "old running data must reach home before layout publication"
    );
    assert_eq!(layout.extents.first().unwrap().start, 0);
    assert_eq!(layout.extents.last().unwrap().end, 8);
    let physical = layout.extents.first().unwrap().physical;
    let mut raw = [0; BLOCK_SIZE];
    layout.device.read_block(physical, &mut raw).unwrap();
    assert_eq!(raw, [0x51; BLOCK_SIZE]);
    layout
        .device
        .write_block(physical, &[0xa7; BLOCK_SIZE])
        .unwrap();
    // Metadata mutation remains legal, but no older data transaction may later overwrite swap DMA.
    file.set_times(Some(1), Some(1)).unwrap();
    file.sync_storage().unwrap();
    layout.device.read_block(physical, &mut raw).unwrap();
    assert_eq!(raw, [0xa7; BLOCK_SIZE]);
    assert_eq!(
        file.writeback_storage_batch(&mut |writer| writer.write(0, &[1]).map(|_| ())),
        Err(FileSystemError::TextBusy)
    );
    lease.close();
    assert!(!file.is_swap_file());
    let successor = file.clone().pin_swap_file().unwrap();
    drop(lease);
    assert!(
        file.is_swap_file(),
        "old lease Drop cannot clear its successor"
    );
    successor.close();
    assert_eq!(file.write_storage(0, &[0x33], None), Ok(1));
    root.unlink(b"alias", false).unwrap();
}

#[test]
fn sparse_swapfile_failure_returns_the_same_inode_to_free_without_changing_layout() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (_, fs) = mounted();
    let root = fs.root_inode().unwrap();
    let file = root.create(b"sparse", InodeType::File, FILE).unwrap();
    file.truncate_storage(8 * BLOCK_SIZE as u64, None).unwrap();
    file.write_storage(0, &[0x61; BLOCK_SIZE], None).unwrap();
    let lease = file.clone().pin_swap_file().unwrap();
    assert!(matches!(
        lease.activate(),
        Err(FileSystemError::InvalidOperation)
    ));
    assert!(file.is_swap_file());
    drop(lease);
    assert!(!file.is_swap_file());
    assert_eq!(
        file.write_storage(BLOCK_SIZE as u64, &[0x72; BLOCK_SIZE], None),
        Ok(BLOCK_SIZE)
    );
    let mut page = [0; BLOCK_SIZE];
    file.read_storage(0, &mut page).unwrap();
    assert_eq!(page, [0x61; BLOCK_SIZE]);
}

#[test]
fn swapfile_journal_io_failure_cleans_freeze_without_releasing_it_early() {
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let (image, fs) = mounted();
    let root = fs.root_inode().unwrap();
    let file = root.create(b"swap-error", InodeType::File, FILE).unwrap();
    file.allocate_storage(0, 2 * BLOCK_SIZE as u64, None)
        .unwrap();
    let lease = file.clone().pin_swap_file().unwrap();
    image.fail_next_flush.store(true, Ordering::Release);
    assert!(lease.activate().is_err());
    assert!(file.is_swap_file());
    drop(lease);
    assert!(!file.is_swap_file());
}

#[test]
fn immutable_swap_extent_lookup_rejects_holes_tail_and_overflow() {
    use super::swap::SwapExtent;
    let extents = [
        SwapExtent {
            start: 0,
            end: 2,
            physical: 40,
        },
        SwapExtent {
            start: 4,
            end: 6,
            physical: 90,
        },
    ];
    assert_eq!(SwapExtent::physical_page(&extents, 0), Some(40));
    assert_eq!(SwapExtent::physical_page(&extents, 1), Some(41));
    assert_eq!(SwapExtent::physical_page(&extents, 2), None);
    assert_eq!(SwapExtent::physical_page(&extents, 4), Some(90));
    assert_eq!(SwapExtent::physical_page(&extents, 6), None);
    assert_eq!(
        SwapExtent::physical_page(
            &[SwapExtent {
                start: 0,
                end: 2,
                physical: usize::MAX
            }],
            1
        ),
        None
    );
}

#[test]
#[ignore = "diagnostic CPU measurement; run alone in release"]
fn production_swap_extent_and_writer_retirement_cpu_cost() {
    use super::swap::SwapExtent;
    use std::{hint::black_box, time::Instant};
    const COUNT: usize = 100_000;
    for count in [1usize, 512] {
        let extents: Vec<_> = (0..count)
            .map(|index| SwapExtent {
                start: index,
                end: index + 1,
                physical: index * 2 + 7,
            })
            .collect();
        for page in 0..COUNT {
            black_box(SwapExtent::physical_page(&extents, page % count));
        }
        let mut samples = [0f64; 5];
        for sample in &mut samples {
            let start = Instant::now();
            for page in 0..COUNT {
                black_box(SwapExtent::physical_page(&extents, page % count));
            }
            *sample = start.elapsed().as_nanos() as f64 / COUNT as f64;
        }
        samples.sort_by(f64::total_cmp);
        eprintln!(
            "SWAP_EXTENT_COST extents={count} median_ns={:.2}",
            samples[2]
        );
    }
    // Actual TaskEvent.signal no-waiter path used by normal last writer retirement;
    // scheduler wake/contended waiter cost is outside this diagnostic.
    let event = crate::execution::sync::TaskEvent::new();
    for _ in 0..COUNT {
        event.signal();
    }
    let mut samples = [0f64; 5];
    for sample in &mut samples {
        let start = Instant::now();
        for _ in 0..COUNT {
            event.signal();
        }
        *sample = start.elapsed().as_nanos() as f64 / COUNT as f64;
    }
    samples.sort_by(f64::total_cmp);
    eprintln!(
        "SWAP_WRITER_EVENT_COST no_waiter_median_ns={:.2}",
        samples[2]
    );
}

#[test]
fn allocated_unwritten_extents_are_exported_without_changing_logical_zero_reads() {
    use crate::ext4_cost_tests::{CountingImage, ext4_fixture_path};
    use std::{fs, process::Command};
    let _serial = COST_TEST_LOCK.lock().unwrap();
    let image = std::env::temp_dir().join(format!("liteos-unwritten-{}.img", std::process::id()));
    let header_file = image.with_extension("header");
    fs::copy(ext4_fixture_path(), &image).unwrap();
    let mut header = [0u8; BLOCK_SIZE];
    header[1024..1028].copy_from_slice(&1u32.to_le_bytes());
    header[1028..1032].copy_from_slice(&3u32.to_le_bytes());
    header[4086..].copy_from_slice(b"SWAPSPACE2");
    fs::write(&header_file, header).unwrap();
    let debugfs =
        std::path::PathBuf::from(std::env::var_os("LITEOS_E2FSCK").expect("run make verify-unit"))
            .with_file_name("debugfs");
    for request in [
        format!("write {} /unwritten-swap", header_file.display()),
        "fallocate /unwritten-swap 1 3".into(),
        "sif /unwritten-swap size 16384".into(),
    ] {
        let output = Command::new(&debugfs)
            .args(["-w", "-R", &request])
            .arg(&image)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "debugfs: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let disk = CountingImage::open_path(&image);
    let filesystem =
        super::ext4::Ext4FileSystem::new(disk, crate::fs::DeviceNumber::new(8, 0), false).unwrap();
    let inode = filesystem
        .root_inode()
        .unwrap()
        .find_child(b"unwritten-swap")
        .unwrap();
    let mut page = [0xffu8; BLOCK_SIZE];
    assert_eq!(
        inode.read_storage(BLOCK_SIZE as u64, &mut page),
        Ok(BLOCK_SIZE)
    );
    assert_eq!(page, [0; BLOCK_SIZE]);
    let lease = inode.clone().pin_swap_file().unwrap();
    let layout = lease.activate().unwrap();
    let physical = super::swap::SwapExtent::physical_page(&layout.extents, 1).unwrap();
    layout
        .device
        .write_block(physical, &[0x82; BLOCK_SIZE])
        .unwrap();
    layout.device.read_block(physical, &mut page).unwrap();
    assert_eq!(page, [0x82; BLOCK_SIZE]);
    inode.read_storage(BLOCK_SIZE as u64, &mut page).unwrap();
    assert_eq!(
        page, [0; BLOCK_SIZE],
        "unwritten file semantics must not be replaced by direct swap I/O"
    );
    lease.close();
    drop(layout);
    drop(lease);
    drop(inode);
    drop(filesystem);
    fs::remove_file(image).unwrap();
    fs::remove_file(header_file).unwrap();
}
