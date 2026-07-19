//! @description VirtIO block status 与 logical block validation policy。

const BLOCK_BYTES: u32 = 4096;

/// request descriptor chain 中 device-writable prefix 的领域形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RequestOperation {
    Read,
    Write,
    Flush,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompletionStatus {
    Ok,
    IoError,
    DeviceError,
}

pub(super) fn decode_status(status: u8) -> CompletionStatus {
    match status {
        0 => CompletionStatus::Ok,
        1 => CompletionStatus::IoError,
        _ => CompletionStatus::DeviceError,
    }
}

/// 验证 used `len` 是否覆盖该操作全部且仅有的 device-writable bytes。
///
/// Read 的 status descriptor 排在 4 KiB data 之后，必须报告 4097；Write/Flush 只有一个
/// writable status byte，必须报告 1。短值不能读取其后的 status/data，超长值证明 device
/// 声称越过了 driver 提供的 writable capacity。
pub(super) const fn completion_length_is_valid(
    operation: RequestOperation,
    used_length: u32,
) -> bool {
    let expected = match operation {
        RequestOperation::Read => BLOCK_BYTES + 1,
        RequestOperation::Write | RequestOperation::Flush => 1,
    };
    used_length == expected
}

pub(super) fn valid_block(capacity_sectors: u64, block: usize, length: usize) -> bool {
    length == 4096
        && u64::try_from(block).is_ok_and(|block| block < capacity_sectors / (4096 / 512))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_status_errors_are_typed_and_unknown_is_device_error() {
        assert_eq!(decode_status(0), CompletionStatus::Ok);
        assert_eq!(decode_status(1), CompletionStatus::IoError);
        assert_eq!(decode_status(2), CompletionStatus::DeviceError);
        assert_eq!(decode_status(0xff), CompletionStatus::DeviceError);
    }

    #[test]
    fn capacity_validation_accepts_only_complete_in_range_blocks() {
        assert!(valid_block(16, 0, 4096));
        assert!(valid_block(16, 1, 4096));
        assert!(!valid_block(16, 2, 4096));
        assert!(!valid_block(16, 0, 4095));
        assert!(!valid_block(u64::MAX, usize::MAX, 4096));
    }

    #[test]
    fn read_completion_must_cover_data_before_trailing_status() {
        assert!(completion_length_is_valid(RequestOperation::Read, 4097));
        assert!(!completion_length_is_valid(RequestOperation::Read, 0));
        assert!(!completion_length_is_valid(RequestOperation::Read, 1));
        assert!(!completion_length_is_valid(RequestOperation::Read, 4096));
        assert!(!completion_length_is_valid(RequestOperation::Read, 4098));
    }

    #[test]
    fn write_and_flush_completion_cover_only_status() {
        for operation in [RequestOperation::Write, RequestOperation::Flush] {
            assert!(completion_length_is_valid(operation, 1));
            assert!(!completion_length_is_valid(operation, 0));
            assert!(!completion_length_is_valid(operation, 2));
            assert!(!completion_length_is_valid(operation, 4097));
        }
    }
}
