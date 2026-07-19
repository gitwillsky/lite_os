use std::{fs, path::Path};

const READ_SOURCE: &str = "kernel/src/syscall/fs/io/sequential/read.rs";
const CURSOR_SOURCE: &str = "kernel/src/syscall/user_iovec.rs";
const POLL_SOURCE: &str = "kernel/src/syscall/poll.rs";
const BYTES: usize = 1024 * 1024;
const LEGACY_CHUNK: usize = 512;
const POLL_FDS: usize = 1024;
const EVENT_BATCH: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ZeroReadCost {
    user_copy_transactions: usize,
}

pub(super) fn check(root: &Path, errors: &mut Vec<String>) {
    match measure(root) {
        Ok(ZeroReadCost {
            user_copy_transactions: 1,
        }) => {}
        Ok(cost) => errors.push(format!(
            "{READ_SOURCE}: scalar 1 MiB /dev/zero read must use one user-range transaction; B={BYTES}, measured {cost:?}"
        )),
        Err(error) => errors.push(error),
    }
    match measure_poll(root) {
        Ok(2) => {}
        Ok(copies) => errors.push(format!(
            "{POLL_SOURCE}: ppoll must batch pollfd import/export; N={POLL_FDS}, measured user copies={copies}"
        )),
        Err(error) => errors.push(error),
    }
    match measure_event_batch(root) {
        Ok(2) => {}
        Ok(copies) => errors.push(format!(
            "{READ_SOURCE}: one DRM event batch must validate once and copy once; E={EVENT_BATCH}, measured user transactions={copies}"
        )),
        Err(error) => errors.push(error),
    }
    match measure_input_batch(root) {
        Ok(2) => {}
        Ok(copies) => errors.push(format!(
            "{READ_SOURCE}: one evdev batch must validate once and copy once; E={EVENT_BATCH}, measured user transactions={copies}"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure_input_batch(root: &Path) -> Result<usize, String> {
    let source = read(root, READ_SOURCE)?;
    if source.contains("let mut encoded_events = [0u8; 16 * EVENT_SIZE]")
        && source.contains("&encoded_events[..read * EVENT_SIZE]")
    {
        return Ok(2);
    }
    if source.contains("for event in events.iter().take(read)")
        && source.contains("cursor.copy_to_user(task, &event.encode())")
    {
        return Ok(EVENT_BATCH + 1);
    }
    Err(format!(
        "{READ_SOURCE}: evdev event copy seam is not recognized"
    ))
}

fn measure_event_batch(root: &Path) -> Result<usize, String> {
    let source = read(root, READ_SOURCE)?;
    if source.contains("let mut encoded = [0u8; 16 * EVENT_SIZE]")
        && source.contains("&encoded[..read * EVENT_SIZE]")
    {
        return Ok(2);
    }
    if source.contains("for event in events.iter().take(read)")
        && source.contains("cursor.copy_to_user(task, &event.encode())")
    {
        return Ok(EVENT_BATCH + 1);
    }
    Err(format!(
        "{READ_SOURCE}: DRM event copy seam is not recognized"
    ))
}

fn measure_poll(root: &Path) -> Result<usize, String> {
    let source = read(root, POLL_SOURCE)?;
    if source.contains("for index in 0..count")
        && source.contains("task.copy_from_user(address, &mut bytes)")
        && source.contains("for descriptor in descriptors")
        && source.contains("task.copy_to_user(descriptor.address + 6")
    {
        return Ok(POLL_FDS * 2);
    }
    if source.contains("task.copy_from_user(poll_fds, &mut raw)")
        && source.contains("task.copy_to_user(poll_fds, raw)")
        && !source.contains("task.copy_to_user(descriptor.address + 6")
    {
        return Ok(2);
    }
    Err(format!(
        "{POLL_SOURCE}: ppoll user-copy seam is not recognized"
    ))
}

fn measure(root: &Path) -> Result<ZeroReadCost, String> {
    let read_source = read(root, READ_SOURCE)?;
    let cursor = read(root, CURSOR_SOURCE)?;
    if read_source.contains("let zeroes = [0u8; 512]")
        && read_source.contains("cursor.copy_to_user(task, &zeroes[..count])")
    {
        return Ok(ZeroReadCost {
            user_copy_transactions: BYTES / LEGACY_CHUNK,
        });
    }
    if read_source.contains("cursor.zero_to_user(task)")
        && cursor.contains("pub(super) fn zero_to_user(")
        && cursor.contains("task.zero_user(address, count)")
    {
        return Ok(ZeroReadCost {
            user_copy_transactions: 1,
        });
    }
    Err(format!(
        "{READ_SOURCE}: /dev/zero user-write seam is not recognized"
    ))
}

fn read(root: &Path, relative: &str) -> Result<String, String> {
    fs::read_to_string(root.join(relative)).map_err(|error| format!("{relative}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_dev_zero_uses_one_user_transaction() {
        let root = super::super::repository_root();
        let cost = measure(&root).expect("production zero-read cost must be measurable");
        assert_eq!(
            cost.user_copy_transactions, 1,
            "B={BYTES}, measured {cost:?}"
        );
    }

    #[test]
    fn ppoll_batches_user_array_copy() {
        let root = super::super::repository_root();
        let copies = measure_poll(&root).expect("production ppoll cost must be measurable");
        assert_eq!(copies, 2, "N={POLL_FDS}, measured user copies={copies}");
    }

    #[test]
    fn drm_event_batch_has_two_user_transactions() {
        let root = super::super::repository_root();
        let copies = measure_event_batch(&root).expect("DRM event cost must be measurable");
        assert_eq!(copies, 2, "E={EVENT_BATCH}, measured transactions={copies}");
    }

    #[test]
    fn evdev_event_batch_has_two_user_transactions() {
        let root = super::super::repository_root();
        let copies = measure_input_batch(&root).expect("evdev event cost must be measurable");
        assert_eq!(copies, 2, "E={EVENT_BATCH}, measured transactions={copies}");
    }
}
