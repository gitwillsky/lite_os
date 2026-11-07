//! VirtIO block status 与 logical block validation policy。

const BLOCK_BYTES: usize = 4096;

/// request descriptor chain 中 device-writable prefix 的领域形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RequestOperation {
    Read,
    Write,
    Flush,
}

impl From<device_model::BlockOperation> for RequestOperation {
    fn from(operation: device_model::BlockOperation) -> Self {
        match operation {
            device_model::BlockOperation::Read => Self::Read,
            device_model::BlockOperation::Write => Self::Write,
        }
    }
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
/// Read 的 status descriptor 排在本次 data 之后，必须报告 requested + 1；Write/Flush 只有一个
/// writable status byte，必须报告 1。短值不能读取其后的 status/data，超长值证明 device
/// 声称越过了 driver 提供的 writable capacity。
pub(super) const fn completion_length_is_valid(
    operation: RequestOperation,
    requested: usize,
    used_length: u32,
) -> bool {
    let expected = match operation {
        RequestOperation::Read => {
            if requested == 0 || requested >= u32::MAX as usize {
                return false;
            }
            requested as u32 + 1
        }
        RequestOperation::Write | RequestOperation::Flush => 1,
    };
    used_length == expected
}

pub(super) fn valid_transfer(capacity_sectors: u64, block: u64, length: usize) -> bool {
    length != 0
        && length.is_multiple_of(BLOCK_BYTES)
        && block
            .checked_add((length / BLOCK_BYTES) as u64)
            .is_some_and(|end| end <= capacity_sectors / (BLOCK_BYTES as u64 / 512))
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
        assert!(valid_transfer(16, 0, 4096));
        assert!(valid_transfer(16, 1, 4096));
        assert!(!valid_transfer(16, 2, 4096));
        assert!(!valid_transfer(16, 0, 4095));
        assert!(!valid_transfer(u64::MAX, u64::MAX, 4096));
    }

    #[test]
    fn read_completion_must_cover_data_before_trailing_status() {
        assert!(completion_length_is_valid(
            RequestOperation::Read,
            4096,
            4097
        ));
        assert!(!completion_length_is_valid(RequestOperation::Read, 4096, 0));
        assert!(!completion_length_is_valid(RequestOperation::Read, 4096, 1));
        assert!(!completion_length_is_valid(
            RequestOperation::Read,
            4096,
            4096
        ));
        assert!(!completion_length_is_valid(
            RequestOperation::Read,
            4096,
            4098
        ));
    }

    #[test]
    fn write_and_flush_completion_cover_only_status() {
        for operation in [RequestOperation::Write, RequestOperation::Flush] {
            assert!(completion_length_is_valid(operation, 0, 1));
            assert!(!completion_length_is_valid(operation, 0, 0));
            assert!(!completion_length_is_valid(operation, 0, 2));
            assert!(!completion_length_is_valid(operation, 0, 4097));
        }
    }
}
