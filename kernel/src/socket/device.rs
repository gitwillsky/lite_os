use alloc::sync::Arc;
use core::cell::Cell;

use smoltcp::{
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    time::Instant,
};

use super::super::packet;
use super::device_error;
use crate::drivers::network::{
    NetworkCompletion, NetworkDevice, NetworkError, NetworkStatistics, NetworkTransmit,
};

const ETHERNET_MTU: usize = 1514;
const RECEIVE_CAPACITY: usize = 2048;

/// @description 将 kernel Ethernet device seam 适配为 smoltcp token device。
pub(super) struct EthernetDevice {
    device: Arc<dyn NetworkDevice>,
    // OWNER: adapter 独占 callback 无法返回的首个 typed error；外层唯一 NetworkStack
    // mutex 串行化全部访问，因此 Cell 不需要第二把锁。syscall seam 消费该状态；缺失
    // latch 会迫使 smoltcp Device 回调 panic 或静默丢错。
    pending_error: Cell<Option<NetworkError>>,
}

impl EthernetDevice {
    /// @description 创建不复制硬件状态的协议栈 adapter。
    ///
    /// @param device DTB 选中的唯一 Ethernet device。
    /// @return 只持共享设备 Arc 的 adapter。
    pub(super) fn new(device: Arc<dyn NetworkDevice>) -> Self {
        Self {
            device,
            pending_error: Cell::new(None),
        }
    }

    pub(super) fn mac_address(&self) -> [u8; 6] {
        self.device.mac_address()
    }

    pub(super) fn statistics(&self) -> NetworkStatistics {
        self.device.statistics()
    }

    /// @description 有界回收设备 TX completion。
    ///
    /// @param budget 本轮最多回收的 descriptor head 数。
    /// @return backlog 与 capacity transition。
    /// @errors 设备或 used ring 损坏时返回错误。
    pub(super) fn poll_completions(
        &self,
        budget: usize,
    ) -> Result<NetworkCompletion, NetworkError> {
        self.capture(self.device.poll_completions(budget))
    }

    /// @description 把本轮重新发布的 RX buffers 一次通知给设备。
    ///
    /// @return 成功或 transport 错误。
    pub(super) fn finish_receive_batch(&self) -> Result<(), NetworkError> {
        self.capture(self.device.finish_receive_batch())
    }

    /// @description 读取但不消费 callback 锁存的首个 adapter error。
    /// @return pending error；没有错误返回 `None`。
    pub(super) fn pending_error(&self) -> Option<NetworkError> {
        self.pending_error.get()
    }

    /// @description 在 syscall seam 消费 callback 锁存的首个 adapter error。
    /// @return pending error；没有错误返回 `None`。
    pub(super) fn take_error(&self) -> Option<NetworkError> {
        self.pending_error.replace(None)
    }

    fn record_error(&self, error: NetworkError) {
        if error == NetworkError::WouldBlock {
            return;
        }
        if self.pending_error.get().is_none() {
            self.pending_error.set(Some(error));
        }
    }

    fn capture<T>(&self, result: Result<T, NetworkError>) -> Result<T, NetworkError> {
        result.inspect_err(|error| {
            self.record_error(*error);
        })
    }
}

/// 已完成接收并由一次 smoltcp ingress callback 消费的 frame token。
pub(super) struct EthernetRxToken {
    frame: [u8; RECEIVE_CAPACITY],
    length: usize,
}

impl RxToken for EthernetRxToken {
    fn consume<R, F>(self, operation: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let frame = &self.frame[..self.length];
        packet::deliver(frame);
        operation(frame)
    }
}

/// 持有唯一 adapter reservation，并把异步 submit error 锁存回 device owner 的 TX token。
pub(super) struct EthernetTxToken<'a> {
    reservation: NetworkTransmit,
    pending_error: &'a Cell<Option<NetworkError>>,
}

impl TxToken for EthernetTxToken<'_> {
    fn consume<R, F>(self, length: usize, operation: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        assert!(length <= ETHERNET_MTU, "smoltcp TX exceeds Ethernet MTU");
        let mut frame = [0u8; ETHERNET_MTU];
        let result = operation(&mut frame[..length]);
        if let Err(error) = self.reservation.submit(&frame[..length])
            && self.pending_error.get().is_none()
        {
            self.pending_error.set(Some(error));
        }
        result
    }
}

impl Device for EthernetDevice {
    type RxToken<'a> = EthernetRxToken;
    type TxToken<'a> = EthernetTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let reservation = match device_error::classify_optional(
            NetworkTransmit::reserve(self.device.clone()),
            |error| *error == NetworkError::WouldBlock,
        ) {
            Ok(reservation) => reservation?,
            Err(error) => {
                self.record_error(error);
                return None;
            }
        };
        let mut frame = [0u8; RECEIVE_CAPACITY];
        match device_error::classify_optional(self.device.receive(&mut frame), |error| {
            *error == NetworkError::WouldBlock
        }) {
            Ok(Some(length)) => Some((
                EthernetRxToken { frame, length },
                EthernetTxToken {
                    reservation,
                    pending_error: &self.pending_error,
                },
            )),
            Ok(None) => None,
            Err(error) => {
                self.record_error(error);
                None
            }
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        match device_error::classify_optional(
            NetworkTransmit::reserve(self.device.clone()),
            |error| *error == NetworkError::WouldBlock,
        ) {
            Ok(reservation) => reservation.map(|reservation| EthernetTxToken {
                reservation,
                pending_error: &self.pending_error,
            }),
            Err(error) => {
                self.record_error(error);
                None
            }
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ethernet;
        capabilities.max_transmission_unit = ETHERNET_MTU;
        capabilities
    }
}
