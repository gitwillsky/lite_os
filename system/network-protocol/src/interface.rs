//! 原生接口事实的版本化 codec；不持有地址、设备或 DAD 状态。
use super::{
    DriverName, InterfaceSnapshot, Reader, Status, Writer, fixed, get_ipv4, get_option_ipv4,
    native_abi, put_ipv4, put_option_ipv4,
};

// 版本 3 只保留轻量接口事实；完整地址从地址快照查询，避免收发路径复制动态列表。
const INTERFACE_VERSION: u32 = 4;

pub(super) fn put_interface(writer: &mut Writer, snapshot: &InterfaceSnapshot) {
    writer
        .u32(INTERFACE_VERSION)
        .u32(snapshot.index)
        .bytes(&snapshot.name)
        .bool(snapshot.loopback)
        .i32(snapshot.mtu)
        .bytes(&snapshot.mac);
    put_option_ipv4(writer, snapshot.address);
    writer.u8(snapshot.prefix_length);
    put_ipv4(writer, snapshot.broadcast);
    writer.bool(snapshot.up);
    writer.bytes(
        snapshot
            .driver
            .as_ref()
            .map_or("", DriverName::as_str)
            .as_bytes(),
    );
    writer.bool(snapshot.driver.is_some());
    writer.bool(snapshot.link.is_some());
    if let Some(link) = snapshot.link {
        writer
            .bool(link.carrier)
            .u32(link.carrier_epoch)
            .u32(link.speed_mbps.unwrap_or(u32::MAX));
    }
}

pub(super) fn get_interface(reader: &mut Reader<'_>) -> Result<InterfaceSnapshot, Status> {
    if reader.u32()? != INTERFACE_VERSION {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let index = reader.u32()?;
    let name = fixed::<16>(reader)?;
    let loopback = reader.bool()?;
    let mtu = reader.i32()?;
    let mac = fixed::<6>(reader)?;
    let address = get_option_ipv4(reader)?;
    let prefix_length = reader.u8()?;
    let broadcast = get_ipv4(reader)?;
    let up = reader.bool()?;
    let driver = core::str::from_utf8(reader.bytes()?).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    let driver = reader.bool()?.then(|| DriverName::new(driver));
    let link = if reader.bool()? {
        let carrier = reader.bool()?;
        let carrier_epoch = reader.u32()?;
        let speed = reader.u32()?;
        Some(device_model::network::NetworkLink {
            carrier,
            carrier_epoch,
            speed_mbps: (speed <= i32::MAX as u32).then_some(speed),
        })
    } else {
        None
    };
    Ok(InterfaceSnapshot {
        index,
        name,
        loopback,
        mtu,
        mac,
        address,
        prefix_length,
        broadcast,
        up,
        link,
        driver,
    })
}
