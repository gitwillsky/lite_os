//! Kernel-owned regular sendfile transfer，descriptor/offset codec 留在上层。
use super::*;

/// 将一次 regular-file 到 regular-file 的 kernel-owned copy 提交给 page cache。
///
/// # Parameters
///
/// - `task`: 当前 caller，提供 RLIMIT_FSIZE 与 SIGXFSZ target。
/// - `input`: 已解析的输入 page-cache facade。
/// - `output`: 已解析的输出 page-cache facade。
/// - `input_position`: 本次操作唯一输入 offset owner。
/// - `output_position`: 本次操作唯一输出 OFD offset owner。
/// - `count`: Linux MAX_RW_COUNT 截断后的最大传输长度。
///
/// # Returns
///
/// 已传输字节数、EOF 零、首错负 errno 或已有进度后的 partial count。
///
/// # Errors
///
/// - 同一文件的实际传输区间重叠返回 `EINVAL`。
/// - 输出越过 RLIMIT_FSIZE 时返回 `EFBIG` 并投递 SIGXFSZ。
pub(super) fn copy_regular_file(
    task: &TaskControlBlock,
    input: &RegularFile,
    output: &RegularFile,
    input_position: &mut u64,
    output_position: &mut u64,
    count: usize,
) -> isize {
    let available = usize::try_from(input.size().saturating_sub(*input_position))
        .unwrap_or(usize::MAX)
        .min(count);
    if available == 0 {
        return 0;
    }
    let transferable = match bounded_regular_write(task, *output_position, available, 0) {
        Ok(count) => count,
        Err(error) => return error,
    };
    if let (Some(input_id), Some(output_id)) = (input.id(), output.id())
        && input_id == output_id
    {
        let input_end = u128::from(*input_position) + transferable as u128;
        let output_end = u128::from(*output_position) + transferable as u128;
        if output_end > u128::from(*input_position) && u128::from(*output_position) < input_end {
            return -errno::EINVAL;
        }
    }

    let writer = match output.begin_write(task.access_identity(true)) {
        Ok(writer) => writer,
        Err(error) => return ferr(error),
    };
    let mut chunk = [0u8; crate::memory::PAGE_SIZE];
    let mut total = 0usize;
    while total < transferable {
        let requested = chunk.len().min(transferable - total);
        let read = match input.read(*input_position, &mut chunk[..requested]) {
            Ok(read) => read,
            Err(error) => {
                return if total == 0 {
                    ferr(error)
                } else {
                    total as isize
                };
            }
        };
        task.account_read_storage(read.storage_bytes);
        let read = read.bytes;
        if read == 0 {
            break;
        }
        if let Err(error) = writer.prepare_content_write() {
            return if total == 0 {
                ferr(error)
            } else {
                total as isize
            };
        }
        let written = match writer.write(*output_position, &chunk[..read]) {
            Ok(written) => written,
            Err(error) => {
                return if total == 0 {
                    ferr(error)
                } else {
                    total as isize
                };
            }
        };
        task.account_write_storage(written);
        *input_position = input_position
            .checked_add(written as u64)
            .expect("sendfile input position overflow");
        *output_position = output_position
            .checked_add(written as u64)
            .expect("sendfile output position overflow");
        total += written;
        if written < read {
            break;
        }
    }
    total as isize
}
