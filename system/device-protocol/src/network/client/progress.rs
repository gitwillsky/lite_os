//! 网卡队列线程到单一协议所有者的进度交接。

use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use device_model::network::{NetworkCompletion, NetworkDeviceProgress, NetworkError};
use native_runtime::native_abi;

use super::{decode_error, encode_error};

/// 在同一网卡的所有队列间合并可写跃迁与故障。
#[derive(Default)]
pub(super) struct DeviceProgress {
    // 队列线程先发表再唤醒。三项互不覆盖，某个队列的故障不会吞掉其他队列的可写跃迁。
    transmit_ready: AtomicBool,
    receive_error: AtomicI32,
    transmit_error: AtomicI32,
}

impl DeviceProgress {
    /// 记录接收队列报告的故障。
    pub(super) fn receive_failed(&self, error: NetworkError) {
        publish_error(&self.receive_error, error);
    }

    /// 从本轮发送推进中记录可写跃迁或故障。
    pub(super) fn transmit_completed(&self, result: Result<NetworkCompletion, NetworkError>) {
        match result {
            Ok(done) if done.transmit_became_available => {
                self.transmit_ready.store(true, Ordering::Release);
            }
            Err(error) => publish_error(&self.transmit_error, error),
            Ok(_) => {}
        }
    }

    /// 唯一协议所有者消费已发表的进度。
    pub(super) fn take(&self) -> NetworkDeviceProgress {
        NetworkDeviceProgress {
            transmit_ready: self.transmit_ready.load(Ordering::Acquire)
                && self.transmit_ready.swap(false, Ordering::AcqRel),
            receive_error: take_error(&self.receive_error),
            transmit_error: take_error(&self.transmit_error),
        }
    }
}

fn publish_error(target: &AtomicI32, error: NetworkError) {
    if error == NetworkError::WouldBlock {
        return;
    }
    let status = encode_error(error);
    // 断开必须优先传给域，才能退休会话。其他故障保留最先发现的一项。
    let _ = target.try_update(Ordering::Release, Ordering::Relaxed, |pending| {
        (pending == 0
            || (pending != native_abi::ERR_PEER_CLOSED && status == native_abi::ERR_PEER_CLOSED))
            .then_some(status)
    });
}

fn take_error(source: &AtomicI32) -> Option<NetworkError> {
    // 空闲网卡只读；若发表晚于读取，队列线程的唤醒会安排下一轮。
    (source.load(Ordering::Acquire) != 0).then(|| decode_error(source.swap(0, Ordering::AcqRel)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_progress_preserves_writability_and_prioritizes_disconnect() {
        let progress = DeviceProgress::default();
        progress.receive_failed(NetworkError::NoMemory);
        progress.receive_failed(NetworkError::Device);
        progress.transmit_completed(Ok(NetworkCompletion {
            transmit_became_available: true,
            ..Default::default()
        }));
        progress.transmit_completed(Err(NetworkError::Device));
        progress.transmit_completed(Err(NetworkError::Disconnected));
        progress.transmit_completed(Err(NetworkError::NoMemory));

        assert_eq!(
            progress.take(),
            NetworkDeviceProgress {
                transmit_ready: true,
                receive_error: Some(NetworkError::NoMemory),
                transmit_error: Some(NetworkError::Disconnected),
            }
        );
        assert_eq!(progress.take(), NetworkDeviceProgress::default());
    }
}
