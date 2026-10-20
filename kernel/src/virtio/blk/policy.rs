//! VirtIO block status 与 logical block validation policy。

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

/// 请求完成时对该槽的处置：句柄仍在等结果则发布，已被放弃（detached）则由完成路径自己释放槽。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompletionAction {
    Publish,
    ReleaseSlot,
}

pub(super) const fn on_completion(detached: bool) -> CompletionAction {
    if detached {
        CompletionAction::ReleaseSlot
    } else {
        CompletionAction::Publish
    }
}

/// 句柄被丢弃时对该槽的处置：结果已发布则丢弃者释放槽；请求仍在设备里则标记 detached，交给完成路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AbandonAction {
    ReleaseNow,
    Detach,
}

pub(super) const fn on_abandon(result_published: bool) -> AbandonAction {
    if result_published {
        AbandonAction::ReleaseNow
    } else {
        AbandonAction::Detach
    }
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
    fn abandoned_request_slot_is_released_exactly_once() {
        // 放弃发生在完成之前：标记 detached，完成路径释放；放弃之后不会再有第二次释放。
        assert_eq!(on_abandon(false), AbandonAction::Detach);
        assert_eq!(on_completion(true), CompletionAction::ReleaseSlot);
        // 放弃发生在完成之后：丢弃者释放；完成路径当时看到 detached == false，只发布结果。
        assert_eq!(on_completion(false), CompletionAction::Publish);
        assert_eq!(on_abandon(true), AbandonAction::ReleaseNow);
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
