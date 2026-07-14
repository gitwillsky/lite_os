use alloc::sync::Arc;

use smoltcp::{
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    time::Instant,
};

use crate::drivers::network::{NetworkDevice, NetworkError, NetworkStatistics};

use super::super::packet;

const ETHERNET_MTU: usize = 1514;
const RECEIVE_CAPACITY: usize = 2048;

/// @description 将 kernel Ethernet device seam 适配为 smoltcp token device。
pub(super) struct EthernetDevice {
    device: Arc<dyn NetworkDevice>,
}

impl EthernetDevice {
    /// @description 创建不复制硬件状态的协议栈 adapter。
    ///
    /// @param device DTB 选中的唯一 Ethernet device。
    /// @return 只持共享设备 Arc 的 adapter。
    pub(super) fn new(device: Arc<dyn NetworkDevice>) -> Self {
        Self { device }
    }

    pub(super) fn mac_address(&self) -> [u8; 6] {
        self.device.mac_address()
    }

    pub(super) fn statistics(&self) -> NetworkStatistics {
        self.device.statistics()
    }
}

pub(super) struct EthernetRxToken<'a> {
    frame: [u8; RECEIVE_CAPACITY],
    length: usize,
    _device_lifetime: core::marker::PhantomData<&'a mut EthernetDevice>,
}

impl RxToken for EthernetRxToken<'_> {
    fn consume<R, F>(self, operation: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let frame = &self.frame[..self.length];
        packet::deliver(frame);
        operation(frame)
    }
}

pub(super) struct EthernetTxToken {
    device: Arc<dyn NetworkDevice>,
}

impl TxToken for EthernetTxToken {
    fn consume<R, F>(self, length: usize, operation: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        assert!(length <= ETHERNET_MTU, "smoltcp TX exceeds Ethernet MTU");
        let mut frame = [0u8; ETHERNET_MTU];
        let result = operation(&mut frame[..length]);
        self.device
            .transmit(&frame[..length])
            .unwrap_or_else(|error| panic!("Ethernet transmit failed: {:?}", error));
        result
    }
}

impl Device for EthernetDevice {
    type RxToken<'a> = EthernetRxToken<'a>;
    type TxToken<'a> = EthernetTxToken;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut frame = [0u8; RECEIVE_CAPACITY];
        match self.device.receive(&mut frame) {
            Ok(length) => Some((
                EthernetRxToken {
                    frame,
                    length,
                    _device_lifetime: core::marker::PhantomData,
                },
                EthernetTxToken {
                    device: self.device.clone(),
                },
            )),
            Err(NetworkError::WouldBlock) => None,
            Err(error) => panic!("Ethernet receive failed: {:?}", error),
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(EthernetTxToken {
            device: self.device.clone(),
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ethernet;
        capabilities.max_transmission_unit = ETHERNET_MTU;
        capabilities
    }
}
