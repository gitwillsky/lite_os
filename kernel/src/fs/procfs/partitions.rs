//! `/proc/partitions`：全部块设备（整盘与分区）的 `major minor #blocks name`，容量以 1 KiB 计。

use alloc::vec::Vec;

use super::{FileSystemError, proc_text};
use crate::fs::device;

/// 生成与 Linux `show_partition` 相同的文本：表头、空行，之后每个设备一行。
///
/// # Errors
///
/// 分配失败返回 `OutOfMemory`。
pub(super) fn format_partitions() -> Result<Vec<u8>, FileSystemError> {
    let mut text = proc_text(format_args!("major minor  #blocks  name\n\n"))?;
    for node in device::block_nodes()? {
        let line = proc_text(format_args!(
            "{:4} {:7} {:10} {}\n",
            node.number().major,
            node.number().minor,
            node.capacity() / 1024,
            core::str::from_utf8(node.name()).unwrap_or("?")
        ))?;
        text.try_reserve(line.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        text.extend_from_slice(&line);
    }
    Ok(text)
}
