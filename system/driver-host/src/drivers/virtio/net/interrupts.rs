//! 一个物理网卡的 MSI-X port：队列通知定向投递，native binding 随活动会话退休。
use super::{ActiveNetwork, VirtIONetworkDevice};
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use native_runtime::{
    Interrupt, Port,
    native_abi::{self, Status, rights},
};

impl VirtIONetworkDevice {
    /// 安装唯一的 IRQ 等待线程；MSI-X 共用 port，MMIO/没有 MSI authority 的 PCI 使用实际 INTx。
    pub(crate) fn bind_interrupts(self: &Arc<Self>, wired: Option<u32>) {
        if let Some(port) = &self.message_port {
            let port = port
                .duplicate(rights::SAME)
                .expect("network interrupt port handle");
            // OWNER: 物理控制器已登记到全进程 completion owner；线程与同一物理节点同寿命。
            // 只在投递时按 generation upgrade 活动会话，不跨 port_wait 保活旧会话，也不为每队列开线程。
            let owner = self.clone();
            let thread = native_runtime::thread::spawn(b"irq:virtio-net", move || {
                loop {
                    let packet = match port.wait(native_abi::DEADLINE_INFINITE) {
                        Ok(packet) => packet,
                        Err(native_abi::ERR_CANCELED) => return,
                        Err(status) => panic!("network MSI-X port wait: {status}"),
                    };
                    assert!(
                        packet.kind == native_abi::packet::INTERRUPT
                            && packet.status == native_abi::OK,
                        "network port only accepts bound Interrupt packets"
                    );
                    owner.message_interrupt(packet.key);
                }
            })
            .expect("network interrupt thread");
            drop(thread);
        } else {
            crate::platform::register_device_interrupt(
                wired.expect("network node lacks an interrupt route"),
                self.irq_handler_for(),
                "virtio-net",
            );
        }
    }

    fn message_interrupt(&self, key: u64) {
        let generation = (key >> 32) as u32;
        let index = key as u32 as usize;
        let active = {
            let lease = self.lease.lock();
            if generation != lease.generation {
                return;
            }
            lease.active.as_ref().and_then(Weak::upgrade)
        };
        let Some(active) = active else {
            return;
        };
        let Some(interrupt) = active.interrupts.get(index) else {
            return;
        };
        if active.failed.load(core::sync::atomic::Ordering::Acquire) == ActiveNetwork::LIVE {
            if index == 0 {
                // MSI-X 不消费共享 ISR；config 向量检查 NEEDS_RESET，并向全部 pair 发布链路变化。
                match self.device.get_status() {
                    Ok(status)
                        if status & super::super::transport::VIRTIO_CONFIG_S_DEVICE_NEEDS_RESET
                            == 0 =>
                    {
                        self.notify_all_activity();
                        self.notify_link_activity();
                    }
                    _ => self.interrupt_failure(),
                }
            } else {
                self.notify_activity(((index - 1) / 2) as u16);
            }
        }
        if interrupt.ack().is_err() {
            self.interrupt_failure();
        }
        // active 在 lease 锁外释放；它可能是最后一个引用，Drop 会重取 lease 撤销设备。
    }
    fn interrupt_failure(&self) {
        self.completion_irq.record_transport_error();
        crate::completion::raise();
    }

    /// 创建实际队列所需的组与绑定；所有 native/Vec 分配和表项在 function-mask 下完成；随后各队列在 enable 前安装索引。
    /// # Errors
    /// 分配、绑定或设备拒绝索引时返回错误；不退回其他 IRQ 模式，也不缩减用户请求。
    pub(super) fn prepare_interrupts(
        &self,
        count: u16,
        generation: u32,
    ) -> Result<Vec<Interrupt>, Status> {
        let Some(msix) = self.device.message_interrupts() else {
            return Ok(Vec::new());
        };
        let port: &Port = self
            .message_port
            .as_ref()
            .ok_or(native_abi::ERR_BAD_STATE)?;
        let vectors = u32::from(count) * 2 + 1;
        if vectors > u32::from(msix.capacity()) {
            return Err(native_abi::ERR_NO_RESOURCES);
        }
        let mut interrupts = Vec::new();
        interrupts
            .try_reserve_exact(vectors as usize)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let group = crate::platform::allocate_message_interrupts(vectors)?;
        let info: native_abi::MsiInfo = group.info(native_abi::info::MSI)?;
        if info.count != vectors {
            return Err(native_abi::ERR_BAD_STATE);
        }
        for index in 0..vectors {
            let interrupt = msix.create_interrupt(&group, index)?;
            interrupt.bind(port, u64::from(generation) << 32 | u64::from(index))?;
            interrupts.push(interrupt);
        }
        self.device
            .assign_config_interrupt(0)
            .map_err(|_| native_abi::ERR_IO)?;
        // control virtqueue 的 ACK 由既有冷路径轮询，保持 NO_VECTOR，不占额外 IRQ 或转发线程。
        // group 句柄可关闭；每个子 Interrupt 在内核保活同一分配，直到会话 reset 后清空本 Vec。
        Ok(interrupts)
    }
}
