//! proc 网络表的只读格式化；数据来自 open 捕获的实际 NET，不能借全局 system snapshot。
use super::{FileSystemError, ProcNode, ProcText};
use alloc::vec::Vec;
use core::fmt::Write;

/// 实际接口配置与 device counter 的窄投影；不在 proc 保存可变配置。
#[derive(Clone, Copy)]
pub(crate) struct ProcNetworkSnapshot {
    pub(crate) name: [u8; 16],
    pub(crate) received_bytes: u64,
    pub(crate) received_packets: u64,
    pub(crate) received_errors: u64,
    pub(crate) received_dropped: u64,
    pub(crate) transmitted_bytes: u64,
    pub(crate) transmitted_packets: u64,
    pub(crate) transmitted_errors: u64,
    pub(crate) transmitted_dropped: u64,
}
/// 当前MAIN FIB record的只读窄投影；不根据接口地址猜测用户路由。
#[derive(Clone, Copy)]
pub(crate) struct ProcRouteSnapshot {
    pub(crate) name: [u8; 16],
    pub(crate) destination: [u8; 4],
    pub(crate) gateway: [u8; 4],
    pub(crate) prefix_length: u8,
    pub(crate) metric: u32,
    pub(crate) flags: u16,
    pub(crate) mtu: u32,
    pub(crate) window: u32,
    pub(crate) initial_rtt: u32,
}
#[derive(Clone, Copy)]
pub(super) enum NetworkTable {
    Devices,
    Routes,
}
impl NetworkTable {
    pub(super) fn from_node(node: ProcNode) -> Option<Self> {
        match node {
            ProcNode::NetDev => Some(Self::Devices),
            ProcNode::NetRoute => Some(Self::Routes),
            _ => None,
        }
    }
}

/// 渲染一个实际域的全部接口；只使用锁外快照，分配失败完整返回 OOM。
pub(super) fn contents(
    table: NetworkTable,
    interfaces: &[ProcNetworkSnapshot],
    routes: &[ProcRouteSnapshot],
) -> Result<Vec<u8>, FileSystemError> {
    let mut output = ProcText::new();
    match table {
        NetworkTable::Devices => {
            output.write_str(
                "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n",
            ).map_err(|_| FileSystemError::OutOfMemory)?;
            for interface in interfaces {
                let name = &interface.name[..interface
                    .name
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(interface.name.len())];
                for _ in name.len()..6 {
                    output
                        .write_str(" ")
                        .map_err(|_| FileSystemError::OutOfMemory)?;
                }
                output.write_bytes(name)?;
                writeln!(
                    output,
                    ": {:8} {:7} {:4} {:4}    0     0          0         0 {:8} {:7} {:4} {:4}    0     0       0          0",
                    interface.received_bytes,
                    interface.received_packets,
                    interface.received_errors,
                    interface.received_dropped,
                    interface.transmitted_bytes,
                    interface.transmitted_packets,
                    interface.transmitted_errors,
                    interface.transmitted_dropped,
                ).map_err(|_| FileSystemError::OutOfMemory)?;
            }
        }
        NetworkTable::Routes => {
            output.write_str(
                "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n",
            ).map_err(|_| FileSystemError::OutOfMemory)?;
            for route in routes {
                let name = &route.name[..route
                    .name
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(route.name.len())];
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(route.prefix_length))
                    .unwrap_or(0);
                if name.is_empty() {
                    output
                        .write_str("*")
                        .map_err(|_| FileSystemError::OutOfMemory)?;
                } else {
                    output.write_bytes(name)?;
                }
                writeln!(
                    output,
                    "\t{:08X}\t{:08X}\t{:04X}\t0\t0\t{}\t{:08X}\t{}\t{}\t{}",
                    u32::from_be_bytes(route.destination).swap_bytes(),
                    u32::from_be_bytes(route.gateway).swap_bytes(),
                    route.flags,
                    route.metric,
                    mask.swap_bytes(),
                    route.mtu,
                    route.window,
                    route.initial_rtt
                )
                .map_err(|_| FileSystemError::OutOfMemory)?;
            }
        }
    }
    Ok(output.finish())
}
