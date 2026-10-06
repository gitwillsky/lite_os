use std::{fs, path::Path};

const CURSOR_SOURCE: &str = "kernel/src/syscall/user_iovec.rs";
const POLL_SOURCE: &str = "kernel/src/syscall/poll.rs";
const DEVICE_CURSOR_SOURCE: &str = "kernel/src/syscall/device.rs";
const ZERO_SOURCE: &str = "kernel/src/fs/mem.rs";
const EVDEV_SOURCE: &str = "kernel/src/input/evdev_file.rs";
const DRM_SOURCE: &str = "kernel/src/drm/card_file.rs";
const BYTES: usize = 1024 * 1024;
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
            "{ZERO_SOURCE}: scalar 1 MiB /dev/zero read must use one user-range transaction; B={BYTES}, measured {cost:?}"
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
            "{DRM_SOURCE}: one DRM event batch must validate once and copy once; E={EVENT_BATCH}, measured user transactions={copies}"
        )),
        Err(error) => errors.push(error),
    }
    match measure_input_batch(root) {
        Ok(2) => {}
        Ok(copies) => errors.push(format!(
            "{EVDEV_SOURCE}: one evdev batch must validate once and copy once; E={EVENT_BATCH}, measured user transactions={copies}"
        )),
        Err(error) => errors.push(error),
    }
}

fn measure_input_batch(root: &Path) -> Result<usize, String> {
    measure_device_batch(root, EVDEV_SOURCE)
}

fn measure_event_batch(root: &Path) -> Result<usize, String> {
    measure_device_batch(root, DRM_SOURCE)
}

/// 去除全部空白，使锚点不受 rustfmt 换行影响。
fn compact(source: &str) -> String {
    source.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 度量设备 read 一批 `EVENT_BATCH` 个事件的用户事务数。
///
/// 1. 设备在出队前对整批目标 `reserve` 一次，编码进 kernel 批缓冲后 `write` 一次；
/// 2. syscall 游标把 `reserve` 实现为一次 range validate、`write` 实现为一次 range copy；
/// 3. 逐事件 `write` 的旧形状每批需要 `EVENT_BATCH` 次 copy 加一次 validate。
fn measure_device_batch(root: &Path, device_source: &str) -> Result<usize, String> {
    let device = compact(&read(root, device_source)?);
    let cursor = compact(&read(root, DEVICE_CURSOR_SOURCE)?);
    if !cursor.contains("fnreserve(&self,length:usize)->Result<(),UserFault>{self.cursor.validate_write_prefix(self.task,length)")
        || !cursor.contains("fnwrite(&mutself,bytes:&[u8])->Result<(),UserFault>{debug_assert!(bytes.len()<=self.remaining());self.cursor.copy_to_user(self.task,bytes)")
    {
        return Err(format!(
            "{DEVICE_CURSOR_SOURCE}: device user-output seam is not recognized"
        ));
    }
    if device.contains(".reserve(requested*EVENT_SIZE)")
        && device.contains(".write(&encoded[..read*EVENT_SIZE])")
    {
        return Ok(2);
    }
    if device.contains(".write(&event.encode())") {
        return Ok(EVENT_BATCH + 1);
    }
    Err(format!(
        "{device_source}: event batch copy seam is not recognized"
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
    let zero = compact(&read(root, ZERO_SOURCE)?);
    let device_cursor = compact(&read(root, DEVICE_CURSOR_SOURCE)?);
    let cursor = read(root, CURSOR_SOURCE)?;
    if zero.contains("output.zero_remaining()")
        && device_cursor.contains(
            "fnzero_remaining(&mutself)->Result<(),UserFault>{self.cursor.zero_to_user(self.task)",
        )
        && cursor.contains("pub(super) fn zero_to_user(")
        && cursor.contains("task.zero_user(address, count)")
    {
        return Ok(ZeroReadCost {
            user_copy_transactions: 1,
        });
    }
    if zero.contains("output.write(&zeroes") {
        return Ok(ZeroReadCost {
            user_copy_transactions: BYTES / 4096,
        });
    }
    Err(format!(
        "{ZERO_SOURCE}: /dev/zero user-write seam is not recognized"
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
